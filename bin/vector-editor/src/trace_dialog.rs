//! Live-preview dialog for raster-to-vector tracing.
//!
//! The dialog is non-modal: while it's open the canvas remains
//! interactive (pan/zoom/select), and a single preview group node lives
//! in the live scene reflecting the current parameters. As the user
//! tweaks sliders, a debounce timer fires off background trace jobs;
//! results land in the preview group, replacing its children.
//!
//! Lifetime:
//! 1. [`open`] populates `EditorState::trace_dialog` with the loaded
//!    image bytes and the user's last-used parameters. The preview
//!    group is created lazily by the first successful trace.
//! 2. [`poll`] runs every frame *before* the egui pass: drains worker
//!    results, swaps the preview group's children, and kicks new jobs
//!    when params change and the debounce window has elapsed.
//! 3. [`show`] runs *during* the egui pass, drawing the parameter UI
//!    and returning a [`DialogAction`] that the caller (in `app.rs`)
//!    interprets as Apply/Cancel/None.
//! 4. [`apply`] / [`cancel`] tear the dialog down, committing the
//!    preview as a single undoable insert (Apply) or removing it
//!    silently (Cancel). Either way the dialog's params are saved
//!    back onto `EditorState::last_trace_params`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use vector_ops::Command;
use vector_render::Renderer;
use vector_scene::{NodeId, Scene};
use vector_trace::{
    CurveMode, HierarchicalMode, TraceColorMode, TraceError, TraceParams, TracePreset,
};

use crate::editor_state::EditorState;
use crate::preview_group;
pub(crate) use crate::preview_group::DialogAction;

/// Debounce window: param changes only kick a new trace once this much
/// time has elapsed since the last change. Tuned by feel — 250 ms is
/// long enough to coalesce a slider drag, short enough that the preview
/// doesn't feel stale.
const DEBOUNCE_MS: u64 = 250;

/// Result coming back from a worker thread.
enum TraceJobResult {
    Ok(Box<Scene>),
    Err(String),
}

/// One in-flight trace, identified by the generation it was launched
/// for. Stale results (older than the current generation) are dropped
/// when [`poll`] drains the channel.
struct TraceJob {
    generation: u64,
    rx: Receiver<TraceJobResult>,
}

/// All state for an open trace dialog.
pub(crate) struct TraceDialogState {
    /// Image bytes shared (immutably) with worker threads.
    image_bytes: Arc<Vec<u8>>,
    /// File path, used to label the dialog title.
    image_path: PathBuf,
    /// Current parameter set — bound directly to the egui sliders.
    pub(crate) params: TraceParams,
    /// `Some(t)` once `params` differs from `last_kicked_params`.
    /// Reset to `None` when a job for the current params is launched.
    params_changed_at: Option<Instant>,
    /// The params snapshot used for the most recent kicked job; we
    /// avoid re-launching when the user lands back on these values
    /// (e.g. drag a slider then drag it back).
    last_kicked_params: Option<TraceParams>,
    /// NodeId of the preview group currently in the live scene, if any.
    /// Created on the first successful trace.
    preview_group: Option<NodeId>,
    /// In-flight trace job, if any.
    job: Option<TraceJob>,
    /// Generation counter — bumps each time a job is kicked.
    generation: u64,
    /// Last error from a failed trace, shown in the dialog status row.
    /// Cleared on the next successful trace.
    last_error: Option<String>,
}

impl TraceDialogState {
    /// Build a fresh dialog state for a just-loaded raster file.
    /// `initial_params` is what `EditorState::last_trace_params` holds,
    /// so the dialog opens where the user left off.
    ///
    /// `params_changed_at` is intentionally set to a past instant so the
    /// next [`poll`] call kicks the very first trace immediately, rather
    /// than waiting for the user to touch a slider.
    pub(crate) fn new_from_open(
        image_bytes: Vec<u8>,
        image_path: PathBuf,
        initial_params: TraceParams,
    ) -> Self {
        let stale = Instant::now()
            .checked_sub(Duration::from_millis(DEBOUNCE_MS + 1))
            .unwrap_or_else(Instant::now);
        Self {
            image_bytes: Arc::new(image_bytes),
            image_path,
            params: initial_params,
            params_changed_at: Some(stale),
            last_kicked_params: None,
            preview_group: None,
            job: None,
            generation: 0,
            last_error: None,
        }
    }
}

/// Per-frame housekeeping. Drains any landed worker result into the
/// preview group, then kicks a new job if params have changed and the
/// debounce window has elapsed.
///
/// Must run *before* the egui pass — the UI should reflect the current
/// preview state and any "Tracing…" indicator.
pub(crate) fn poll(state: &mut EditorState, renderer: &mut Renderer, egui_ctx: &egui::Context) {
    let Some(dialog) = state.trace_dialog.as_mut() else {
        return;
    };

    // 1. Drain any results that landed since the last poll.
    if let Some(job) = dialog.job.as_ref() {
        // try_recv to avoid blocking — we only consume what's ready.
        match job.rx.try_recv() {
            Ok(result) => {
                let job_gen = job.generation;
                dialog.job = None;
                if job_gen == dialog.generation {
                    apply_trace_result(state, renderer, result);
                }
                // Stale generations are silently discarded.
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                // Worker died without sending — clear the slot so we
                // can kick a replacement.
                dialog.job = None;
            }
        }
    }

    // Re-borrow after the possible mutation above.
    let Some(dialog) = state.trace_dialog.as_mut() else {
        return;
    };

    // 2. Decide whether to kick a new job. Conditions: no in-flight job,
    //    params differ from the last-kicked snapshot, debounce elapsed.
    if dialog.job.is_some() {
        return;
    }
    if dialog
        .last_kicked_params
        .as_ref()
        .is_some_and(|p| p == &dialog.params)
    {
        // Already traced these exact params — nothing to do.
        dialog.params_changed_at = None;
        return;
    }
    let Some(changed_at) = dialog.params_changed_at else {
        return;
    };
    if changed_at.elapsed() < Duration::from_millis(DEBOUNCE_MS) {
        return;
    }

    // 3. Launch.
    dialog.generation = dialog.generation.wrapping_add(1);
    dialog.last_kicked_params = Some(dialog.params.clone());
    dialog.params_changed_at = None;
    let bytes = Arc::clone(&dialog.image_bytes);
    let params = dialog.params.clone();
    let generation = dialog.generation;
    let (tx, rx) = mpsc::channel();
    let ctx = egui_ctx.clone();

    thread::Builder::new()
        .name(format!("trace-{generation}"))
        .spawn(move || {
            let result = match vector_trace::trace_image_with_params(&bytes, &params) {
                Ok(scene) => TraceJobResult::Ok(Box::new(scene)),
                Err(e) => TraceJobResult::Err(format_trace_error(&e)),
            };
            // If the receiver has been dropped (dialog closed), the send
            // simply fails — we don't need to handle that explicitly.
            let _ = tx.send(result);
            // Wake the UI even if it was idle. The editor's frame loop
            // is currently always-on, so this is belt-and-braces, but
            // we don't want to depend on that.
            ctx.request_repaint();
        })
        .expect("spawn trace worker");

    dialog.job = Some(TraceJob { generation, rx });
}

/// Render the dialog's egui UI. Returns the action the caller should
/// take (apply, cancel, or do nothing). Param edits inside the dialog
/// also bump `params_changed_at` so [`poll`] knows to re-trace.
pub(crate) fn show(state: &mut EditorState, ctx: &egui::Context) -> DialogAction {
    let Some(dialog) = state.trace_dialog.as_mut() else {
        return DialogAction::None;
    };

    let title = match dialog.image_path.file_name().and_then(|s| s.to_str()) {
        Some(name) => format!("Trace raster image — {name}"),
        None => "Trace raster image".to_string(),
    };

    let mut action = DialogAction::None;
    // Window's `open` flag — if the user clicks the [×] in the title bar,
    // egui flips this to false; we treat that as Cancel.
    let mut still_open = true;
    let original_params = dialog.params.clone();

    egui::Window::new(title)
        .open(&mut still_open)
        .resizable(true)
        .default_width(320.0)
        .show(ctx, |ui| {
            // Preset row.
            ui.horizontal(|ui| {
                info_icon(
                    ui,
                    "Starting points. B&W: sharp 2-colour line art. \
                     Poster: a few flat colour regions. \
                     Photo: smoothest curves, fewest corners — best for shapes that are \
                     really circles/ellipses/blobs.",
                );
                ui.label("Load defaults:");
                ui.button("B&W")
                    .on_hover_text("Sharp two-tone line art. Low corner threshold, hard edges.")
                    .clicked()
                    .then(|| dialog.params = TraceParams::from_preset(TracePreset::Bw));
                ui.button("Poster")
                    .on_hover_text("Flat colour regions, moderate smoothing.")
                    .clicked()
                    .then(|| dialog.params = TraceParams::from_preset(TracePreset::Poster));
                ui.button("Photo")
                    .on_hover_text(
                        "Maximum smoothing (corner threshold 180°). Use this when shapes should \
                         be all curves and no corners.",
                    )
                    .clicked()
                    .then(|| dialog.params = TraceParams::from_preset(TracePreset::Photo));
            });

            ui.separator();

            // Mode & curves.
            egui::Grid::new("trace_mode_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        info_icon(
                            ui,
                            "Color: trace each colour layer separately. \
                             Binary: threshold to black/white only — much simpler output, \
                             ignores colour entirely.",
                        );
                        ui.label("Color mode");
                    });
                    egui::ComboBox::from_id_salt("color_mode")
                        .selected_text(match dialog.params.color_mode {
                            TraceColorMode::Color => "Color",
                            TraceColorMode::Binary => "Binary (B&W)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut dialog.params.color_mode,
                                TraceColorMode::Color,
                                "Color",
                            );
                            ui.selectable_value(
                                &mut dialog.params.color_mode,
                                TraceColorMode::Binary,
                                "Binary (B&W)",
                            );
                        });
                    ui.end_row();

                    ui.horizontal(|ui| {
                        info_icon(
                            ui,
                            "Stacked: each colour layer is a full opaque shape, layered on top \
                             of the others. Cutout: each layer is just the part not covered by \
                             a deeper layer (smaller files, but shapes are hole-punched).",
                        );
                        ui.label("Layers");
                    });
                    egui::ComboBox::from_id_salt("hierarchical")
                        .selected_text(match dialog.params.hierarchical {
                            HierarchicalMode::Stacked => "Stacked",
                            HierarchicalMode::Cutout => "Cutout",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut dialog.params.hierarchical,
                                HierarchicalMode::Stacked,
                                "Stacked",
                            );
                            ui.selectable_value(
                                &mut dialog.params.hierarchical,
                                HierarchicalMode::Cutout,
                                "Cutout",
                            );
                        });
                    ui.end_row();

                    ui.horizontal(|ui| {
                        info_icon(
                            ui,
                            "Pixel art: each pixel edge becomes a path edge (no smoothing). \
                             Polygon: straight-line segments only. \
                             Smooth: cubic Bézier curves — fewest vertices for organic shapes.",
                        );
                        ui.label("Curves");
                    });
                    egui::ComboBox::from_id_salt("curve_mode")
                        .selected_text(match dialog.params.curve_mode {
                            CurveMode::PixelArt => "Pixel art (no smoothing)",
                            CurveMode::Polygon => "Polygon",
                            CurveMode::Spline => "Smooth (spline)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut dialog.params.curve_mode,
                                CurveMode::PixelArt,
                                "Pixel art (no smoothing)",
                            );
                            ui.selectable_value(
                                &mut dialog.params.curve_mode,
                                CurveMode::Polygon,
                                "Polygon",
                            );
                            ui.selectable_value(
                                &mut dialog.params.curve_mode,
                                CurveMode::Spline,
                                "Smooth (spline)",
                            );
                        });
                    ui.end_row();
                });

            ui.separator();

            // Numeric knobs.
            let is_color = dialog.params.color_mode == TraceColorMode::Color;
            slider_row(
                ui,
                true,
                &mut dialog.params.filter_speckle,
                0..=128,
                "Filter speckle",
                "Drop clusters smaller than this many pixels on a side. Higher = less noise, \
                 fewer stray paths around shape edges. Bump up if you see speckle along contours.",
            );
            slider_row(
                ui,
                is_color,
                &mut dialog.params.color_precision,
                1..=8,
                "Color precision (bits)",
                "Bits per channel after quantisation. Lower = fewer distinct colours, fewer layers.",
            );
            slider_row(
                ui,
                is_color,
                &mut dialog.params.layer_difference,
                0..=128,
                "Layer difference",
                "Threshold for merging adjacent colour layers. Higher = more aggressive merging, \
                 fewer layers in the output.",
            );
            slider_row(
                ui,
                true,
                &mut dialog.params.corner_threshold,
                0..=180,
                "Corner threshold (°)",
                "Angles below this become hard corners; above, smooth curves. \
                 LOW values (≤60°) produce many corners — every pixel staircase becomes a vertex. \
                 HIGH values (160–180°) treat almost everything as smooth — best for circles, \
                 ellipses, and other organic shapes. This is the biggest knob for reducing vertex count.",
            );
            slider_row(
                ui,
                true,
                &mut dialog.params.length_threshold,
                0.0..=10.0,
                "Length threshold",
                "Minimum segment length (pixels) before splice/curve fitting kicks in. \
                 Higher = shorter wobbles get absorbed into longer curves, simpler paths. \
                 Push toward 8–10 for chunky shapes with few intended corners.",
            );
            slider_row(
                ui,
                true,
                &mut dialog.params.splice_threshold,
                0..=180,
                "Splice threshold (°)",
                "Max tangent-angle change between segments that can be spliced into a single curve. \
                 LOWER = more aggressive splicing, fewer separate curve pieces. Try 20–30° if \
                 you're seeing too many short curve segments where one long one would do.",
            );
            slider_row(
                ui,
                true,
                &mut dialog.params.path_precision,
                0..=8,
                "Path precision (decimals)",
                "Decimal places retained in coordinates. Lower = smaller files, slightly less \
                 accurate. Affects file size, not vertex count.",
            );

            ui.separator();

            // Status row.
            ui.horizontal(|ui| {
                if dialog.job.is_some() {
                    ui.spinner();
                    ui.label("Tracing…");
                } else if let Some(err) = &dialog.last_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("Trace failed: {err}"));
                } else if dialog.preview_group.is_some() {
                    ui.label("Preview up to date.");
                } else {
                    ui.label("Waiting for first trace…");
                }
            });

            // Footer.
            ui.separator();
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

    // Re-borrow because the closure took a mutable borrow.
    if let Some(dialog) = state.trace_dialog.as_mut() {
        // If the title-bar [×] was clicked, treat as Cancel.
        if !still_open && action == DialogAction::None {
            action = DialogAction::Cancel;
        }
        // Mark params dirty if anything changed inside the closure.
        if dialog.params != original_params {
            dialog.params_changed_at = Some(Instant::now());
        }
    }

    action
}

/// Commit the current preview group as a single undoable insert and
/// close the dialog. The preview node is captured via `snapshot_subtree`
/// then re-inserted via `Command::InsertSubtree` so the entire imported
/// group lands in the undo history as one atomic step.
pub(crate) fn apply(state: &mut EditorState, renderer: &mut Renderer) {
    let Some(dialog) = state.trace_dialog.take() else {
        return;
    };
    state.last_trace_params = dialog.params.clone();

    if let Some(group_id) = dialog.preview_group {
        // Capture parent + index + snapshot before removing — these are
        // the inputs `Command::InsertSubtree` needs for both apply and
        // its undo half.
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

/// Discard the preview group without recording undo, then close. Used
/// when the user clicks Cancel or the title-bar [×]. The dialog's
/// params are still saved back to `last_trace_params` so the next
/// open starts from the same place.
pub(crate) fn cancel(state: &mut EditorState, renderer: &mut Renderer) {
    let Some(dialog) = state.trace_dialog.take() else {
        return;
    };
    state.last_trace_params = dialog.params.clone();
    if let Some(group_id) = dialog.preview_group {
        state.scene.remove(group_id);
    }
    renderer.mark_dirty();
}

/// Draw a small ⓘ marker that shows `tooltip` on hover. Used to advertise
/// the per-control help text on the trace dialog so users notice that
/// hover-help exists — without it the tooltips are effectively hidden.
///
/// Rendered as weakly-coloured text so it reads as "extra info available"
/// rather than as a button. The marker itself is the hover target, but
/// the tooltip is also still attached to the control next to it so
/// hovering anywhere on the row works.
fn info_icon(ui: &mut egui::Ui, tooltip: &str) -> egui::Response {
    let color = ui.style().visuals.weak_text_color();
    ui.add(egui::Label::new(
        egui::RichText::new("ⓘ").color(color).small(),
    ))
    .on_hover_text(tooltip)
}

/// Slider row with a leading ⓘ marker, the slider itself, and the tooltip
/// attached to both. Returns the slider's response so callers can react
/// to drags if needed (currently unused — params_changed_at is set
/// elsewhere by comparing snapshots).
fn slider_row<Num: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    enabled: bool,
    value: &mut Num,
    range: std::ops::RangeInclusive<Num>,
    label: &str,
    tooltip: &str,
) -> egui::Response {
    ui.horizontal(|ui| {
        info_icon(ui, tooltip);
        ui.add_enabled(
            enabled,
            egui::Slider::new(value, range)
                .text(label)
                .clamping(egui::SliderClamping::Always),
        )
        .on_hover_text(tooltip)
    })
    .inner
}

// ── Internals ────────────────────────────────────────────────────────

/// Apply a worker result to the preview group. On success, the new
/// scene's contents are merged into the live scene as the preview
/// group's children (creating the group on first call). On error, the
/// previous preview survives and the error is stashed for display.
fn apply_trace_result(state: &mut EditorState, renderer: &mut Renderer, result: TraceJobResult) {
    match result {
        TraceJobResult::Ok(traced) => {
            // Take the preview-group slot out of the dialog so we don't
            // hold an aliasing borrow while mutating `state.scene`.
            let mut group_slot = state
                .trace_dialog
                .as_mut()
                .and_then(|d| d.preview_group.take());
            // First-call labels the new group "Traced image"; subsequent
            // calls use a transient inner label that gets flattened away.
            let label = if group_slot.is_some() {
                "trace contents"
            } else {
                "Traced image"
            };
            preview_group::replace_children(
                &mut state.scene,
                &traced,
                &mut group_slot,
                renderer,
                label,
            );
            if let Some(dialog) = state.trace_dialog.as_mut() {
                dialog.preview_group = group_slot;
                dialog.last_error = None;
            }
        }
        TraceJobResult::Err(msg) => {
            if let Some(dialog) = state.trace_dialog.as_mut() {
                dialog.last_error = Some(msg);
            }
            renderer.mark_dirty();
        }
    }
}

fn format_trace_error(e: &TraceError) -> String {
    e.to_string()
}
