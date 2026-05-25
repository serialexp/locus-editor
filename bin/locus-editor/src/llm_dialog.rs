//! Live-preview dialog for LLM-driven SVG generation.
//!
//! Architecturally identical to `trace_dialog`: a non-modal egui window
//! manages a *preview group* in the live scene; a background worker
//! produces fresh content (here, an SVG returned by an OpenAI-compatible
//! chat completion endpoint); each successful response replaces the
//! preview group's children. Apply commits the preview as a single
//! undoable insert; Cancel discards it.
//!
//! The single new piece beyond the trace pattern is the conversation
//! history — the dialog keeps `Vec<Message>` across turns so "make the
//! sun bigger" continues the previous exchange instead of starting over.
//!
//! Lifetime:
//! 1. [`open`] populates `EditorState::llm_dialog` with a default config
//!    (DeepSeek base URL + model) and tries to load the API key from the
//!    OS keychain. If loading fails the dialog surfaces the key-input
//!    field; otherwise the user can Send right away.
//! 2. [`poll`] runs every frame *before* the egui pass, draining any
//!    landed worker result into the preview group.
//! 3. [`show`] runs *during* the egui pass, drawing the chat UI plus
//!    Apply / Cancel buttons. Returns a [`DialogAction`] interpreted by
//!    `app.rs::draw_frame`.
//! 4. [`apply`] / [`cancel`] tear the dialog down — Apply commits the
//!    preview as `Command::InsertSubtree`, Cancel removes it silently.

use std::sync::mpsc::{self, Receiver};
use std::thread;

use locus_llm::{
    Conversation, KeyStoreError, LlmConfig, LlmError, Message, Role, delete_api_key, extract_svg,
    load_api_key, save_api_key,
};
use locus_ops::Command;
use locus_render::Renderer;
use locus_scene::NodeId;

use crate::editor_state::EditorState;
use crate::preview_group::{self, DialogAction};

/// Status of the API key as known to the dialog. Drives which UI row is
/// shown for the key field.
#[derive(Debug, Clone)]
enum KeyStatus {
    /// Loaded from the OS keychain at dialog open. The "loaded ✓" label
    /// is shown alongside a "Forget" button.
    LoadedFromKeychain,
    /// No key in the keychain (`KeyStoreError::NotFound`). Empty input
    /// row + "Save to keychain" button.
    NotStored,
    /// Keychain is unavailable or returned an error. Same input row as
    /// `NotStored`, plus an explanatory message; "Save" may still try
    /// (and may fail again) but the user can also paste a key for the
    /// session and Send without saving.
    BackendUnavailable(String),
    /// User pasted a key but hasn't saved it yet. Persists across turns
    /// for the dialog session only.
    SessionOnly,
}

/// Result coming back from a worker thread. The string in `Ok` is the
/// raw assistant text (still wrapped in markdown fences etc., if the
/// model decided to do that — extraction happens on the UI thread so we
/// stay in the same place as the conversation log).
enum LlmJobResult {
    Ok(String),
    Err(String),
}

/// One in-flight chat completion, identified by the generation it was
/// launched for. Stale results are dropped when [`poll`] drains the
/// channel.
struct LlmJob {
    generation: u64,
    rx: Receiver<LlmJobResult>,
}

pub(crate) struct LlmDialogState {
    /// Endpoint + model + key. Key is mirrored here even if we loaded
    /// from the keychain so the worker thread sees a fresh `LlmConfig`
    /// without touching the keychain again.
    config: LlmConfig,
    /// Whether the Settings collapsing header is open.
    settings_open: bool,
    /// Provenance of the API key (drives the key-row UI).
    key_status: KeyStatus,

    /// Multi-turn message log including the system prompt at index 0.
    conversation: Conversation,
    /// Bound to the bottom multiline text edit.
    input_buffer: String,

    /// NodeId of the preview group currently in the live scene, if any.
    /// Created on the first successful response.
    preview_group: Option<NodeId>,
    /// In-flight job, if any.
    job: Option<LlmJob>,
    /// Generation counter — bumps each time a job is kicked.
    generation: u64,
    /// Last error from a failed completion or SVG parse, shown in the
    /// status row. Cleared on the next successful response.
    last_error: Option<String>,
}

impl LlmDialogState {
    fn new(config: LlmConfig, key_status: KeyStatus) -> Self {
        Self {
            config,
            settings_open: false,
            key_status,
            conversation: Conversation::new(),
            input_buffer: String::new(),
            preview_group: None,
            job: None,
            generation: 0,
            last_error: None,
        }
    }
}

// ── Dialog lifecycle ────────────────────────────────────────────────

/// Construct a fresh dialog state, attempting to load the API key from
/// the OS keychain. Always succeeds — even when the keychain is missing,
/// the dialog opens in `BackendUnavailable` mode so the user can paste a
/// key and proceed.
pub(crate) fn open(state: &mut EditorState) {
    let mut config = LlmConfig::default();
    let key_status = match load_api_key() {
        Ok(key) => {
            config.api_key = key;
            KeyStatus::LoadedFromKeychain
        }
        Err(KeyStoreError::NotFound) => KeyStatus::NotStored,
        Err(KeyStoreError::NoBackend(msg)) => KeyStatus::BackendUnavailable(msg),
        Err(KeyStoreError::Other(msg)) => KeyStatus::BackendUnavailable(msg),
    };
    state.llm_dialog = Some(LlmDialogState::new(config, key_status));
}

/// Per-frame housekeeping. Drains any landed worker result into the
/// preview group. Runs *before* the egui pass.
pub(crate) fn poll(state: &mut EditorState, renderer: &mut Renderer, _egui_ctx: &egui::Context) {
    let Some(dialog) = state.llm_dialog.as_mut() else {
        return;
    };

    let Some(job) = dialog.job.as_ref() else {
        return;
    };

    match job.rx.try_recv() {
        Ok(result) => {
            let job_gen = job.generation;
            dialog.job = None;
            if job_gen == dialog.generation {
                apply_chat_result(state, renderer, result);
            }
            // Stale generations silently discarded.
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            // Worker died without sending — clear the slot so we can
            // kick a replacement.
            dialog.job = None;
        }
    }
}

/// Render the dialog UI for this frame. Returns whether the caller
/// should Apply / Cancel / keep open. Sending is handled inline (kicks
/// a worker thread and updates `dialog.job`).
pub(crate) fn show(
    state: &mut EditorState,
    ctx: &egui::Context,
    renderer: &mut Renderer,
) -> DialogAction {
    if state.llm_dialog.is_none() {
        return DialogAction::None;
    }

    let mut action = DialogAction::None;
    let mut still_open = true;
    let mut send_clicked = false;

    egui::Window::new("Generate SVG from prompt")
        .open(&mut still_open)
        .resizable(true)
        .default_width(440.0)
        .default_height(540.0)
        .show(ctx, |ui| {
            let Some(dialog) = state.llm_dialog.as_mut() else {
                return;
            };

            // ── Settings (collapsible) ──────────────────────────────
            egui::CollapsingHeader::new("Settings")
                .default_open(dialog.settings_open)
                .show(ui, |ui| {
                    egui::Grid::new("llm_settings_grid")
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Base URL");
                            ui.add(
                                egui::TextEdit::singleline(&mut dialog.config.base_url)
                                    .desired_width(280.0),
                            );
                            ui.end_row();

                            ui.label("Model");
                            ui.add(
                                egui::TextEdit::singleline(&mut dialog.config.model)
                                    .desired_width(280.0),
                            );
                            ui.end_row();

                            ui.label("API key");
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut dialog.config.api_key)
                                        .desired_width(220.0)
                                        .password(true)
                                        .hint_text("sk-…"),
                                );
                                draw_key_buttons(ui, dialog);
                            });
                            ui.end_row();
                        });

                    // Status line under the key row.
                    match &dialog.key_status {
                        KeyStatus::LoadedFromKeychain => {
                            ui.colored_label(egui::Color32::LIGHT_GREEN, "Loaded from keychain ✓");
                        }
                        KeyStatus::NotStored => {
                            ui.weak("No key stored. Paste one and click Save to persist it.");
                        }
                        KeyStatus::BackendUnavailable(msg) => {
                            ui.colored_label(
                                egui::Color32::LIGHT_YELLOW,
                                format!("Keychain unavailable: {msg}"),
                            );
                            ui.weak("You can still paste a key for this session.");
                        }
                        KeyStatus::SessionOnly => {
                            ui.weak("Session-only key (not saved to keychain).");
                        }
                    }
                });

            ui.separator();

            // ── Conversation transcript ─────────────────────────────
            ui.heading("Conversation");
            egui::ScrollArea::vertical()
                .id_salt("llm_chat_scroll")
                .auto_shrink([false, false])
                .max_height(ui.available_height() - 120.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for msg in &dialog.conversation.messages {
                        // System messages never appear in the UI — they
                        // are an implementation detail of the prompt.
                        if msg.role == Role::System {
                            continue;
                        }
                        render_message(ui, msg);
                    }
                    if dialog.job.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("waiting for response…");
                        });
                    }
                });

            ui.separator();

            // ── Input row ───────────────────────────────────────────
            let can_send = dialog.job.is_none()
                && !dialog.input_buffer.trim().is_empty()
                && !dialog.config.api_key.trim().is_empty();

            ui.horizontal(|ui| {
                let edit = egui::TextEdit::multiline(&mut dialog.input_buffer)
                    .hint_text("Describe what to draw, or how to tweak the previous result…")
                    .desired_rows(3)
                    .desired_width(ui.available_width() - 90.0);
                let resp = ui.add(edit);
                ui.vertical(|ui| {
                    let send = ui.add_enabled(can_send, egui::Button::new("Send"));
                    if send.clicked() {
                        send_clicked = true;
                    }
                    // Ctrl+Enter while focused on the input also sends.
                    if resp.has_focus()
                        && can_send
                        && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Enter))
                    {
                        send_clicked = true;
                    }
                });
            });

            // ── Status / error row ──────────────────────────────────
            ui.horizontal(|ui| {
                if let Some(err) = &dialog.last_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("Error: {err}"));
                } else if dialog.preview_group.is_some() {
                    ui.weak("Preview ready — Apply to commit, Cancel to discard.");
                } else {
                    ui.weak("Send a prompt to generate a preview.");
                }
            });

            ui.separator();

            // ── Footer ──────────────────────────────────────────────
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    action = DialogAction::Cancel;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(dialog.preview_group.is_some(), egui::Button::new("Apply"))
                        .clicked()
                    {
                        action = DialogAction::Apply;
                    }
                });
            });
        });

    // Title-bar [×] also counts as Cancel.
    if !still_open && action == DialogAction::None {
        action = DialogAction::Cancel;
    }

    // Kick a job after the closure (re-borrow). `send_clicked` was set
    // either by the Send button or by Ctrl+Enter inside the input.
    if send_clicked {
        kick_job(state, ctx, renderer);
    }

    action
}

/// Commit the current preview group as a single undoable insert and
/// close the dialog. Mirrors `trace_dialog::apply`.
pub(crate) fn apply(state: &mut EditorState, renderer: &mut Renderer) {
    let Some(dialog) = state.llm_dialog.take() else {
        return;
    };

    if let Some(group_id) = dialog.preview_group {
        let parent = state.scene.parent(group_id);
        let index = state.scene.child_index(group_id);
        let snapshot = state.scene.snapshot_subtree(group_id);
        state.scene.remove(group_id);

        if let (Some(parent), Some(index), Some(snapshot)) = (parent, index, snapshot) {
            state.history.execute(
                Command::InsertSubtree {
                    parent,
                    index,
                    snapshot: Box::new(snapshot),
                },
                &mut state.scene,
            );
        }
    }
    renderer.mark_dirty();
}

/// Discard the preview group without recording undo, then close.
pub(crate) fn cancel(state: &mut EditorState, renderer: &mut Renderer) {
    let Some(dialog) = state.llm_dialog.take() else {
        return;
    };
    if let Some(group_id) = dialog.preview_group {
        state.scene.remove(group_id);
    }
    renderer.mark_dirty();
}

// ── Internals ───────────────────────────────────────────────────────

/// Render a single non-system message in the transcript.
fn render_message(ui: &mut egui::Ui, msg: &Message) {
    let (label, color) = match msg.role {
        Role::User => ("YOU", egui::Color32::LIGHT_BLUE),
        Role::Assistant => ("ASST", egui::Color32::LIGHT_GREEN),
        Role::System => return,
    };

    ui.horizontal_top(|ui| {
        ui.colored_label(color, egui::RichText::new(label).monospace().strong());
        match msg.role {
            Role::User => {
                // User messages: display verbatim. Models can always
                // re-derive context from the system prompt.
                ui.label(&msg.content);
            }
            Role::Assistant => {
                // Assistant messages are full SVG dumps — showing them
                // raw would dominate the panel. Summarise instead.
                let bytes = msg.content.len();
                ui.weak(format!("(SVG response, {} bytes)", bytes));
            }
            Role::System => unreachable!(),
        }
    });
}

/// Render the API-key control buttons next to the key field.
fn draw_key_buttons(ui: &mut egui::Ui, dialog: &mut LlmDialogState) {
    match &dialog.key_status {
        KeyStatus::LoadedFromKeychain => {
            if ui.button("Forget").clicked() {
                match delete_api_key() {
                    Ok(()) => {
                        dialog.config.api_key.clear();
                        dialog.key_status = KeyStatus::NotStored;
                    }
                    Err(e) => {
                        dialog.key_status = KeyStatus::BackendUnavailable(e.to_string());
                    }
                }
            }
        }
        KeyStatus::NotStored | KeyStatus::SessionOnly | KeyStatus::BackendUnavailable(_) => {
            let can_save = !dialog.config.api_key.trim().is_empty();
            if ui
                .add_enabled(can_save, egui::Button::new("Save"))
                .clicked()
            {
                match save_api_key(&dialog.config.api_key) {
                    Ok(()) => {
                        dialog.key_status = KeyStatus::LoadedFromKeychain;
                    }
                    Err(e) => {
                        dialog.key_status = KeyStatus::BackendUnavailable(e.to_string());
                    }
                }
            }
        }
    }
}

/// Take the current input buffer, push it onto the conversation as a
/// new user turn, snapshot the conversation + config, and spawn a
/// worker thread that calls `locus_llm::complete`.
fn kick_job(state: &mut EditorState, egui_ctx: &egui::Context, _renderer: &mut Renderer) {
    let Some(dialog) = state.llm_dialog.as_mut() else {
        return;
    };
    if dialog.job.is_some() {
        return;
    }

    let prompt = dialog.input_buffer.trim().to_string();
    if prompt.is_empty() {
        return;
    }
    dialog.input_buffer.clear();

    // If the user pasted a key in the input field but never clicked
    // Save, mark it as session-only so the status row reflects reality.
    if matches!(dialog.key_status, KeyStatus::NotStored) && !dialog.config.api_key.trim().is_empty()
    {
        dialog.key_status = KeyStatus::SessionOnly;
    }

    dialog.conversation.push_user(prompt);
    dialog.last_error = None;

    dialog.generation = dialog.generation.wrapping_add(1);
    let generation = dialog.generation;
    let cfg = dialog.config.clone();
    let conv = dialog.conversation.clone();
    let (tx, rx) = mpsc::channel();
    let ctx = egui_ctx.clone();

    thread::Builder::new()
        .name(format!("llm-{generation}"))
        .spawn(move || {
            let result = match locus_llm::complete(&cfg, &conv) {
                Ok(text) => LlmJobResult::Ok(text),
                Err(e) => LlmJobResult::Err(format_llm_error(&e)),
            };
            // Receiver may have been dropped if the dialog was closed
            // while the request was in flight; ignore the send error.
            let _ = tx.send(result);
            ctx.request_repaint();
        })
        .expect("spawn llm worker");

    dialog.job = Some(LlmJob { generation, rx });
}

/// Apply a worker result. On success, push the assistant turn onto the
/// log, extract the SVG, parse it, and replace the preview group's
/// children. On failure, stash the error.
fn apply_chat_result(state: &mut EditorState, renderer: &mut Renderer, result: LlmJobResult) {
    match result {
        LlmJobResult::Ok(text) => {
            // Always log the assistant turn — even if the SVG fails to
            // parse, the user can ask for a fix in plain language and
            // the model has the context.
            if let Some(dialog) = state.llm_dialog.as_mut() {
                dialog.conversation.push_assistant(text.clone());
            }

            let svg_text = match extract_svg(&text) {
                Some(s) => s,
                None => {
                    if let Some(dialog) = state.llm_dialog.as_mut() {
                        dialog.last_error = Some(
                            "Response contained no <svg> tag. Try asking the model to retry."
                                .to_string(),
                        );
                    }
                    return;
                }
            };

            let imported = match locus_svg::import_svg(svg_text.as_bytes()) {
                Ok(s) => s,
                Err(e) => {
                    if let Some(dialog) = state.llm_dialog.as_mut() {
                        dialog.last_error = Some(format!("SVG parse failed: {e}"));
                    }
                    return;
                }
            };

            // Take the preview-group slot out of the dialog so we can
            // mutate the scene without an aliasing borrow.
            let mut group_slot = state
                .llm_dialog
                .as_mut()
                .and_then(|d| d.preview_group.take());
            let label = if group_slot.is_some() {
                "generated contents"
            } else {
                "Generated SVG"
            };
            preview_group::replace_children(
                &mut state.scene,
                &imported,
                &mut group_slot,
                renderer,
                label,
            );
            if let Some(dialog) = state.llm_dialog.as_mut() {
                dialog.preview_group = group_slot;
                dialog.last_error = None;
            }
        }
        LlmJobResult::Err(msg) => {
            if let Some(dialog) = state.llm_dialog.as_mut() {
                dialog.last_error = Some(msg);
                // Drop the trailing user turn so the user can edit and
                // resend without the failed attempt sitting in history.
                if let Some(last) = dialog.conversation.messages.last()
                    && last.role == Role::User
                {
                    let removed = dialog.conversation.messages.pop().unwrap();
                    dialog.input_buffer = removed.content;
                }
            }
            renderer.mark_dirty();
        }
    }
}

/// Render an `LlmError` into a single-line user-facing string. The
/// `Display` impl is fine for short variants; for `Http` we strip large
/// HTML error pages down to the first 200 chars so the status row
/// doesn't blow out the dialog height.
fn format_llm_error(e: &LlmError) -> String {
    match e {
        LlmError::Http { status, body } => {
            let trimmed = body.trim();
            let snippet: String = trimmed.chars().take(200).collect();
            if trimmed.len() > snippet.len() {
                format!("HTTP {status}: {snippet}…")
            } else {
                format!("HTTP {status}: {snippet}")
            }
        }
        other => other.to_string(),
    }
}
