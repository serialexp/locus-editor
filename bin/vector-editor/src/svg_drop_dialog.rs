//! Drop-choice dialog for SVG files.
//!
//! When the user drops an SVG file onto the canvas and the current
//! document already has content, we don't know whether they meant
//! "open this file" or "drop this in on top of what I have." This
//! dialog asks. If the current document is empty, the drop handler
//! skips the dialog entirely and just opens — there's nothing to
//! merge into so the question has only one answer.
//!
//! Non-modal but visually prominent (centered, anchored). Matches the
//! shape of the other editor dialogs (trace, llm): `open` populates
//! the state, `show` renders the UI and returns a `DropChoice`, and
//! the caller in `app.rs::draw_frame` routes the choice to the
//! appropriate scene-mutation path.

use std::path::PathBuf;

/// Pending drop awaiting the user's choice. Holds the raw file bytes
/// (already read from disk) so we don't have to re-read if the dialog
/// is open long enough that the source file changes.
pub(crate) struct SvgDropDialogState {
    pub(crate) bytes: Vec<u8>,
    pub(crate) path: PathBuf,
}

/// What the user picked in the drop-choice dialog. Returned by [`show`]
/// each frame; `None` means "still waiting" (or no dialog open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropChoice {
    /// Keep the dialog open.
    None,
    /// Replace the current document with the dropped SVG.
    Replace,
    /// Insert the dropped SVG as a new top-level group, preserving the
    /// existing document.
    AddAsGroup,
    /// Dismiss without doing anything.
    Cancel,
}

/// Render the drop-choice dialog when one is pending. Returns the
/// user's choice for this frame. The caller is responsible for
/// clearing `state.svg_drop_dialog` once a terminal choice
/// (Replace/AddAsGroup/Cancel) is returned.
pub(crate) fn show(
    state: &mut crate::editor_state::EditorState,
    ctx: &egui::Context,
) -> DropChoice {
    let Some(dialog) = state.svg_drop_dialog.as_ref() else {
        return DropChoice::None;
    };

    let filename = dialog
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| dialog.path.display().to_string());

    let mut choice = DropChoice::None;
    // `Window::open` flag — the [×] in the title bar maps to Cancel.
    let mut still_open = true;

    egui::Window::new("Dropped SVG")
        .open(&mut still_open)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                ui.label(format!("How should {filename} be imported?"));
                ui.add_space(4.0);
            });
            ui.separator();
            ui.add_space(6.0);

            // Three side-by-side primary actions. Order is Add-as-group
            // first (the non-destructive choice, and the reason this
            // dialog exists), Replace second, Cancel last.
            ui.horizontal(|ui| {
                if ui
                    .button("Add as group")
                    .on_hover_text(
                        "Import the SVG as a new top-level group on top of the existing \
                         document. Recorded as a single undoable insert.",
                    )
                    .clicked()
                {
                    choice = DropChoice::AddAsGroup;
                }
                if ui
                    .button("Replace document")
                    .on_hover_text(
                        "Discard the current document and open the dropped SVG in its place. \
                         Clears undo history.",
                    )
                    .clicked()
                {
                    choice = DropChoice::Replace;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Cancel").clicked() {
                        choice = DropChoice::Cancel;
                    }
                });
            });
        });

    // Title-bar [×] = Cancel.
    if !still_open && choice == DropChoice::None {
        choice = DropChoice::Cancel;
    }

    choice
}
