//! Non-modal "Generate shape from script…" dialog.
//!
//! The user authors a tiny Rhai script that returns an SVG `d` string.
//! Three named globals — `WIDTH`, `HEIGHT`, `N` — are exposed as
//! parameters so the same script can produce different shapes without
//! editing the code. The returned path is parsed by routing through our
//! existing `locus_svg::import_svg` (wrapped in a minimal `<svg>` envelope),
//! so we get every path-data edge case for free.
//!
//! Architecturally a sibling of `trace_dialog` / `llm_dialog`: a live
//! preview group in the scene, replaced on every successful evaluation,
//! committed on Apply as one undoable `Command::InsertSubtree` and
//! dropped silently on Cancel. The difference is that Rhai evaluation
//! is synchronous and fast (single-digit milliseconds for trivial
//! scripts), so we don't spin up a worker thread — we just re-evaluate
//! inline whenever any of the four inputs changes.

use locus_ops::Command;
use locus_render::Renderer;
use locus_scene::NodeId;

use crate::editor_state::EditorState;
use crate::preview_group::{self, DialogAction};

/// Built-in starter scripts, populated into the editor when the user
/// clicks one of the preset buttons. All three follow the same shape:
/// they read `WIDTH`, `HEIGHT`, `N` from scope and return a single `d`
/// string. Kept here (rather than embedded in the dialog struct) so
/// they're easy to tweak without touching state.
const PRESET_STAR: &str = r#"// N-pointed star centred on (WIDTH/2, HEIGHT/2).
let cx = WIDTH / 2.0;
let cy = HEIGHT / 2.0;
let outer = if WIDTH < HEIGHT { WIDTH / 2.0 } else { HEIGHT / 2.0 };
let inner = outer * 0.4;
let pts = N * 2;
let pi = 3.141592653589793;
let d = "";
for i in 0..pts {
    let r = if i % 2 == 0 { outer } else { inner };
    let a = (i.to_float() / pts.to_float()) * 2.0 * pi - pi / 2.0;
    let x = cx + r * a.cos();
    let y = cy + r * a.sin();
    d += if i == 0 { "M " } else { " L " };
    d += `${x} ${y}`;
}
d + " Z"
"#;

const PRESET_POLYGON: &str = r#"// Regular N-gon inscribed in (WIDTH, HEIGHT).
let cx = WIDTH / 2.0;
let cy = HEIGHT / 2.0;
let r = if WIDTH < HEIGHT { WIDTH / 2.0 } else { HEIGHT / 2.0 };
let pi = 3.141592653589793;
let d = "";
for i in 0..N {
    let a = (i.to_float() / N.to_float()) * 2.0 * pi - pi / 2.0;
    let x = cx + r * a.cos();
    let y = cy + r * a.sin();
    d += if i == 0 { "M " } else { " L " };
    d += `${x} ${y}`;
}
d + " Z"
"#;

const PRESET_SPIRAL: &str = r#"// Archimedean spiral with N turns, filling (WIDTH, HEIGHT).
let cx = WIDTH / 2.0;
let cy = HEIGHT / 2.0;
let r_max = if WIDTH < HEIGHT { WIDTH / 2.0 } else { HEIGHT / 2.0 };
let pi = 3.141592653589793;
let steps = N * 32;
let d = "";
for i in 0..steps {
    let t = i.to_float() / steps.to_float();
    let r = t * r_max;
    let a = t * N.to_float() * 2.0 * pi;
    let x = cx + r * a.cos();
    let y = cy + r * a.sin();
    d += if i == 0 { "M " } else { " L " };
    d += `${x} ${y}`;
}
d
"#;

pub(crate) struct ScriptDialogState {
    /// User script. Bound to a multiline `TextEdit`.
    script: String,
    /// Exposed to scripts as `WIDTH`.
    width: f64,
    /// Exposed to scripts as `HEIGHT`.
    height: f64,
    /// Exposed to scripts as `N` — a generic integer parameter (star
    /// point count, polygon sides, spiral turns, ...).
    n: i64,

    /// NodeId of the preview group currently in the live scene, if any.
    /// Created on the first successful evaluation.
    preview_group: Option<NodeId>,

    /// Last successfully-evaluated inputs. We skip re-evaluation when
    /// the current inputs match this — both saves work and prevents a
    /// jitter loop where `show` re-eval'd every frame.
    last_eval: Option<(String, f64, f64, i64)>,

    /// Error from the most recent evaluation. Shown in red below the
    /// editor. Cleared on a successful re-eval.
    last_error: Option<String>,
}

impl ScriptDialogState {
    fn new() -> Self {
        Self {
            script: PRESET_STAR.to_string(),
            width: 200.0,
            height: 200.0,
            n: 5,
            preview_group: None,
            last_eval: None,
            last_error: None,
        }
    }

    /// Have the inputs changed since the last successful evaluation?
    /// `true` for the very first frame too (when `last_eval` is `None`).
    fn needs_eval(&self) -> bool {
        match &self.last_eval {
            None => true,
            Some((s, w, h, n)) => {
                s != &self.script || *w != self.width || *h != self.height || *n != self.n
            }
        }
    }
}

/// Open the dialog, populating it with the default star preset.
/// No-op if the dialog is already open (single-instance, like the
/// trace and LLM dialogs).
pub(crate) fn open(state: &mut EditorState) {
    if state.script_dialog.is_some() {
        return;
    }
    state.script_dialog = Some(ScriptDialogState::new());
}

/// Run any pending evaluation before the egui pass. Mirrors the
/// shape of `trace_dialog::poll` / `llm_dialog::poll`, but the work
/// runs on this thread because Rhai is fast.
pub(crate) fn poll(state: &mut EditorState, renderer: &mut Renderer) {
    let Some(dialog) = state.script_dialog.as_mut() else {
        return;
    };
    if !dialog.needs_eval() {
        return;
    }

    let snapshot = (dialog.script.clone(), dialog.width, dialog.height, dialog.n);

    match evaluate(&dialog.script, dialog.width, dialog.height, dialog.n) {
        Ok(produced) => {
            // Splice into the preview group. The helper creates the
            // group on first call and replaces children on subsequent
            // calls.
            preview_group::replace_children(
                &mut state.scene,
                &produced,
                &mut dialog.preview_group,
                renderer,
                "Generated shape",
            );
            dialog.last_eval = Some(snapshot);
            dialog.last_error = None;
        }
        Err(e) => {
            // Leave the preview group as-is so the user can keep their
            // last-good shape on screen while fixing the script. Record
            // the inputs so we don't re-attempt the same broken eval
            // every frame.
            dialog.last_eval = Some(snapshot);
            dialog.last_error = Some(e);
        }
    }
}

/// Render the dialog's egui window. Returns the action the caller
/// should take (apply, cancel, or keep open).
pub(crate) fn show(state: &mut EditorState, ctx: &egui::Context) -> DialogAction {
    let Some(dialog) = state.script_dialog.as_mut() else {
        return DialogAction::None;
    };

    let mut action = DialogAction::None;
    // Egui's Window `open` flag — flipped to false when the user clicks
    // the title-bar [×]. We treat that as Cancel, same as trace/llm.
    let mut still_open = true;

    egui::Window::new("Generate shape from script")
        .open(&mut still_open)
        .resizable(true)
        .default_width(480.0)
        .default_height(380.0)
        .show(ctx, |ui| {
            // Parameter row — width / height / N. Edits to any of
            // these bump the input snapshot via `needs_eval()` so the
            // next `poll` re-evaluates.
            ui.horizontal(|ui| {
                ui.label("Width:");
                ui.add(
                    egui::DragValue::new(&mut dialog.width)
                        .speed(1.0)
                        .range(1.0..=10_000.0),
                );
                ui.add_space(8.0);
                ui.label("Height:");
                ui.add(
                    egui::DragValue::new(&mut dialog.height)
                        .speed(1.0)
                        .range(1.0..=10_000.0),
                );
                ui.add_space(8.0);
                ui.label("N:");
                ui.add(egui::DragValue::new(&mut dialog.n).range(1..=10_000));
            });

            // Preset buttons — drop a working script into the editor
            // so the dialog is useful without external docs.
            ui.horizontal(|ui| {
                ui.label("Preset:");
                if ui.button("Star").clicked() {
                    dialog.script = PRESET_STAR.to_string();
                }
                if ui.button("Polygon").clicked() {
                    dialog.script = PRESET_POLYGON.to_string();
                }
                if ui.button("Spiral").clicked() {
                    dialog.script = PRESET_SPIRAL.to_string();
                }
            });

            ui.add_space(4.0);

            // The script editor itself. Multiline TextEdit with a
            // monospace font and code_editor flag for indentation /
            // bracket pairing. Filled to use most of the dialog so
            // the user has actual room to write.
            let available = ui.available_size();
            ui.add_sized(
                [available.x, available.y - 60.0],
                egui::TextEdit::multiline(&mut dialog.script)
                    .font(egui::TextStyle::Monospace)
                    .code_editor()
                    .desired_rows(12)
                    .desired_width(f32::INFINITY),
            );

            // Error message — only shown when the last eval failed. Red
            // so it actually grabs the user's attention; weak when
            // there's no error so the spacing doesn't jump around.
            if let Some(err) = &dialog.last_error {
                ui.colored_label(egui::Color32::from_rgb(0xff, 0x60, 0x60), err);
            }

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    action = DialogAction::Cancel;
                }
                // Apply is greyed out if we have nothing to commit
                // (no preview group yet — happens when the very first
                // evaluation failed and the user hasn't fixed it).
                let can_apply = dialog.preview_group.is_some() && dialog.last_error.is_none();
                if ui
                    .add_enabled(can_apply, egui::Button::new("Apply"))
                    .clicked()
                {
                    action = DialogAction::Apply;
                }
            });
        });

    if !still_open {
        action = DialogAction::Cancel;
    }
    action
}

/// Commit the current preview group as one undoable insert, then close.
pub(crate) fn apply(state: &mut EditorState, renderer: &mut Renderer) {
    let Some(dialog) = state.script_dialog.take() else {
        return;
    };
    if let Some(group_id) = dialog.preview_group {
        // Same pattern as trace_dialog::apply — capture parent + index
        // + snapshot, remove, then re-insert through the history so
        // undo turns the shape back off in one step.
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
    let Some(dialog) = state.script_dialog.take() else {
        return;
    };
    if let Some(group_id) = dialog.preview_group {
        state.scene.remove(group_id);
    }
    renderer.mark_dirty();
}

/// Evaluate a script against the given parameters and parse its
/// returned `d` string into a one-path Scene.
///
/// Errors describe what failed in human terms — "script error: ..." for
/// Rhai parse/eval failures, "import error: ..." for SVG-path-parse
/// failures of whatever the script produced. Either way the caller
/// surfaces them in the dialog's error row.
fn evaluate(script: &str, width: f64, height: f64, n: i64) -> Result<locus_scene::Scene, String> {
    let engine = rhai::Engine::new();
    let mut scope = rhai::Scope::new();
    // `push_constant` means the script can't accidentally reassign these
    // (reassignment would silently lose the parameter without an error).
    scope.push_constant("WIDTH", width);
    scope.push_constant("HEIGHT", height);
    scope.push_constant("N", n);

    let d: String = engine
        .eval_with_scope(&mut scope, script)
        .map_err(|e| format!("script error: {e}"))?;
    if d.trim().is_empty() {
        return Err("script returned an empty path".to_string());
    }

    // Wrap the returned `d` in a minimal SVG document so we can reuse
    // the main importer. The viewBox matches the script's stated
    // bounds so any width/height-relative units in the path land in
    // sensible canvas coordinates after import.
    let wrapped = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}"><path d="{d}" fill="black" stroke="none"/></svg>"#,
        width = width,
        height = height,
        // Escape the only character that would otherwise close the
        // attribute prematurely. Rhai's string interpolation can't
        // produce `"` unless the script puts one there itself, so this
        // is just a belt-and-braces guard.
        d = d.replace('"', "&quot;"),
    );
    locus_svg::import_svg(wrapped.as_bytes()).map_err(|e| format!("import error: {e}"))
}
