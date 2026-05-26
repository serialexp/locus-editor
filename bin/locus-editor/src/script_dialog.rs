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

// ── Pretty parametric ──────────────────────────────────────────────

// Heart curve from Eugene Beutel (1909): the iconic 16·sin³ / 13·cos…
// formula. Scaled to fit (WIDTH, HEIGHT). N is unused but kept in scope
// so the dialog params row stays consistent across presets.
const PRESET_HEART: &str = r#"// Classic heart curve, scaled to (WIDTH, HEIGHT).
let cx = WIDTH / 2.0;
let cy = HEIGHT / 2.0;
let pi = 3.141592653589793;
// Native curve extents: x in [-17, 17], y in [-17, 12]. Scale so the
// longer axis fills its dimension, with a touch of breathing room.
let scale_x = WIDTH / 34.0;
let scale_y = HEIGHT / 29.0;
let scale = if scale_x < scale_y { scale_x } else { scale_y };
scale = scale * 0.95;
let steps = 200;
let d = "";
for i in 0..steps {
    let t = (i.to_float() / steps.to_float()) * 2.0 * pi;
    let s = t.sin();
    let x = 16.0 * s * s * s;
    let y = -(13.0 * t.cos() - 5.0 * (2.0 * t).cos() - 2.0 * (3.0 * t).cos() - (4.0 * t).cos());
    let px = cx + x * scale;
    let py = cy + y * scale;
    d += if i == 0 { "M " } else { " L " };
    d += `${px} ${py}`;
}
d + " Z"
"#;

// Rose curve (rhodonea): r = cos(k·θ). N controls petal count —
// when N is even there are 2·N petals, when odd there are N petals.
const PRESET_ROSE: &str = r#"// Rose curve with N petals (2N if N is even).
let cx = WIDTH / 2.0;
let cy = HEIGHT / 2.0;
let r_max = if WIDTH < HEIGHT { WIDTH / 2.2 } else { HEIGHT / 2.2 };
let pi = 3.141592653589793;
let steps = N * 64;
let d = "";
for i in 0..steps {
    let t = (i.to_float() / steps.to_float()) * 2.0 * pi;
    let r = r_max * (N.to_float() * t).cos();
    let x = cx + r * t.cos();
    let y = cy + r * t.sin();
    d += if i == 0 { "M " } else { " L " };
    d += `${x} ${y}`;
}
d + " Z"
"#;

// Lissajous figure with a 3:N frequency ratio. Famous oscilloscope
// pattern — different N values produce very different curves.
const PRESET_LISSAJOUS: &str = r#"// Lissajous figure, frequency ratio 3:N.
let cx = WIDTH / 2.0;
let cy = HEIGHT / 2.0;
let ax = WIDTH / 2.2;
let ay = HEIGHT / 2.2;
let pi = 3.141592653589793;
let steps = 400;
// Phase offset of pi/2 closes the figure neatly when 3 and N are
// coprime; otherwise it's open. Either way it's pretty.
let phase = pi / 2.0;
let d = "";
for i in 0..steps {
    let t = (i.to_float() / steps.to_float()) * 2.0 * pi;
    let x = cx + ax * (3.0 * t + phase).sin();
    let y = cy + ay * (N.to_float() * t).sin();
    d += if i == 0 { "M " } else { " L " };
    d += `${x} ${y}`;
}
d + " Z"
"#;

// ── Practical UI ───────────────────────────────────────────────────

// Rounded rectangle filling (WIDTH, HEIGHT). N is the corner radius
// in canvas units (clamped to half the shorter side).
const PRESET_ROUNDED_RECT: &str = r#"// Rounded rectangle filling (WIDTH, HEIGHT), corner radius N.
let max_r = if WIDTH < HEIGHT { WIDTH / 2.0 } else { HEIGHT / 2.0 };
let r = if N.to_float() > max_r { max_r } else { N.to_float() };
let w = WIDTH;
let h = HEIGHT;
// SVG arcs: A rx ry x-axis-rotation large-arc sweep x y. Sweep = 1
// matches the "outside" turn at each corner.
let d = `M ${r} 0`;
d += ` L ${w - r} 0`;
d += ` A ${r} ${r} 0 0 1 ${w} ${r}`;
d += ` L ${w} ${h - r}`;
d += ` A ${r} ${r} 0 0 1 ${w - r} ${h}`;
d += ` L ${r} ${h}`;
d += ` A ${r} ${r} 0 0 1 0 ${h - r}`;
d += ` L 0 ${r}`;
d += ` A ${r} ${r} 0 0 1 ${r} 0`;
d += " Z";
d
"#;

// Speech bubble: a rounded rect with a triangular tail at the
// bottom-left. N controls corner radius.
const PRESET_SPEECH: &str = r#"// Speech bubble. Body fills (WIDTH, HEIGHT * 0.8) with a tail below.
let body_h = HEIGHT * 0.8;
let tail_h = HEIGHT - body_h;
let max_r = if WIDTH < body_h { WIDTH / 2.0 } else { body_h / 2.0 };
let r = if N.to_float() > max_r { max_r } else { N.to_float() };
let w = WIDTH;
let h = body_h;
// Tail anchor points along the bottom edge (about 1/4 from the left).
let tail_x1 = w * 0.20;
let tail_x2 = w * 0.30;
let tail_tip_x = w * 0.10;
let tail_tip_y = body_h + tail_h;
let d = `M ${r} 0`;
d += ` L ${w - r} 0`;
d += ` A ${r} ${r} 0 0 1 ${w} ${r}`;
d += ` L ${w} ${h - r}`;
d += ` A ${r} ${r} 0 0 1 ${w - r} ${h}`;
d += ` L ${tail_x2} ${h}`;
d += ` L ${tail_tip_x} ${tail_tip_y}`;
d += ` L ${tail_x1} ${h}`;
d += ` L ${r} ${h}`;
d += ` A ${r} ${r} 0 0 1 0 ${h - r}`;
d += ` L 0 ${r}`;
d += ` A ${r} ${r} 0 0 1 ${r} 0`;
d += " Z";
d
"#;

// ── Mechanical ─────────────────────────────────────────────────────

// Simple cog: alternating outer/inner radius around a circle with N
// teeth. Not a true involute — this is the cartoon-gear look — but
// it's a fine starting point that the user can refine.
const PRESET_GEAR: &str = r#"// Cog with N teeth, alternating outer/inner radius.
let cx = WIDTH / 2.0;
let cy = HEIGHT / 2.0;
let r_outer = (if WIDTH < HEIGHT { WIDTH / 2.0 } else { HEIGHT / 2.0 }) * 0.95;
let r_inner = r_outer * 0.78;
let teeth = N;
// Each tooth: 4 points — outer-rising, outer-falling, inner-rising,
// inner-falling — so the loop visits the rim in 4·teeth steps.
let steps = teeth * 4;
let pi = 3.141592653589793;
// Width fraction of each segment around the tooth/gap, normalised to 1.
let widths = [0.4, 0.1, 0.4, 0.1];
let radii = [r_outer, r_outer, r_inner, r_inner];
let acc = 0.0;
let d = "";
let total = teeth.to_float();
for i in 0..steps {
    let phase = widths[i % 4];
    let r = radii[i % 4];
    // Start of this segment around the circle.
    let a = (acc / (total * 1.0)) * 2.0 * pi - pi / 2.0;
    let x = cx + r * a.cos();
    let y = cy + r * a.sin();
    d += if i == 0 { "M " } else { " L " };
    d += `${x} ${y}`;
    acc += phase;
}
d + " Z"
"#;

// ── Math-y / recursive ─────────────────────────────────────────────

// Koch snowflake with N iterations. Each iteration replaces every
// segment with four shorter ones — segment count is 3·4^N, so keep N
// modest (5 = 3072 segments, perceptibly fractal but still fast).
const PRESET_KOCH: &str = r#"// Koch snowflake — N iterations of the classic 4-replacement rule.
let cx = WIDTH / 2.0;
let cy = HEIGHT / 2.0;
let radius = (if WIDTH < HEIGHT { WIDTH } else { HEIGHT }) / 2.3;
let pi = 3.141592653589793;
// Equilateral triangle vertices.
let p0 = [cx, cy - radius];
let p1 = [cx + radius * (60.0 * pi / 180.0).sin(), cy + radius * 0.5];
let p2 = [cx - radius * (60.0 * pi / 180.0).sin(), cy + radius * 0.5];
let pts = [p0, p1, p2, p0];

// Cap N so the snowflake doesn't explode. 4^6 = 4096 — beyond that
// the path gets unwieldy without adding visible detail.
let iters = if N > 6 { 6 } else { N };

for _iter in 0..iters {
    let next = [];
    let n = pts.len() - 1;
    for i in 0..n {
        let a = pts[i];
        let b = pts[i + 1];
        let dx = (b[0] - a[0]) / 3.0;
        let dy = (b[1] - a[1]) / 3.0;
        let p_third = [a[0] + dx, a[1] + dy];
        let p_two_third = [a[0] + 2.0 * dx, a[1] + 2.0 * dy];
        // Rotate (dx, dy) by -60° to find the bump's apex on the
        // outward side of the segment (Y-down: negative angle).
        let cos60 = 0.5;
        let sin60 = -0.866025403784;
        let rx = dx * cos60 - dy * sin60;
        let ry = dx * sin60 + dy * cos60;
        let apex = [p_third[0] + rx, p_third[1] + ry];
        next.push(a);
        next.push(p_third);
        next.push(apex);
        next.push(p_two_third);
    }
    next.push(pts[pts.len() - 1]);
    pts = next;
}

let d = "";
let count = pts.len();
for i in 0..count {
    d += if i == 0 { "M " } else { " L " };
    d += `${pts[i][0]} ${pts[i][1]}`;
}
d + " Z"
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

            // Preset dropdown — drops a working script into the editor
            // so the dialog is useful without external docs. ComboBox
            // (instead of buttons) so we can keep adding presets without
            // the row overflowing. Selecting an entry replaces the
            // script wholesale; "—" entries are inert section headers.
            ui.horizontal(|ui| {
                ui.label("Preset:");
                egui::ComboBox::from_id_salt("script_preset")
                    .selected_text("Load…")
                    .show_ui(ui, |ui| {
                        ui.label(egui::RichText::new("Basics").weak());
                        if ui.selectable_label(false, "Star").clicked() {
                            dialog.script = PRESET_STAR.to_string();
                        }
                        if ui.selectable_label(false, "Polygon").clicked() {
                            dialog.script = PRESET_POLYGON.to_string();
                        }
                        if ui.selectable_label(false, "Spiral").clicked() {
                            dialog.script = PRESET_SPIRAL.to_string();
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("Parametric curves").weak());
                        if ui.selectable_label(false, "Heart").clicked() {
                            dialog.script = PRESET_HEART.to_string();
                        }
                        if ui.selectable_label(false, "Rose (N petals)").clicked() {
                            dialog.script = PRESET_ROSE.to_string();
                        }
                        if ui.selectable_label(false, "Lissajous (3:N)").clicked() {
                            dialog.script = PRESET_LISSAJOUS.to_string();
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("UI shapes").weak());
                        if ui.selectable_label(false, "Rounded rectangle").clicked() {
                            dialog.script = PRESET_ROUNDED_RECT.to_string();
                        }
                        if ui.selectable_label(false, "Speech bubble").clicked() {
                            dialog.script = PRESET_SPEECH.to_string();
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("Mechanical").weak());
                        if ui.selectable_label(false, "Gear (N teeth)").clicked() {
                            dialog.script = PRESET_GEAR.to_string();
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("Fractal").weak());
                        if ui
                            .selectable_label(false, "Koch snowflake (N iter)")
                            .clicked()
                        {
                            dialog.script = PRESET_KOCH.to_string();
                        }
                    });
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

#[cfg(test)]
mod tests {
    use super::*;

    // Every preset is exercised through `evaluate` with sane parameters,
    // catching Rhai-syntax / runtime-typo regressions at `cargo test`
    // time instead of at user-runtime (where they'd render as an
    // unfriendly red error in the dialog and probably go un-fixed
    // because nobody asks the tests).
    fn run(preset: &str, n: i64) {
        let scene = evaluate(preset, 200.0, 200.0, n)
            .unwrap_or_else(|e| panic!("preset failed: {e}\n---script---\n{preset}"));
        // The imported scene should contain at least one path-bearing
        // node under root (defs aside). `scene.root()` always exists; we
        // just check there's content.
        let root = scene.root();
        let root_node = scene.get(root).expect("root node must exist");
        let defs = scene.defs();
        let has_content = root_node.children.iter().any(|&c| c != defs);
        assert!(has_content, "preset produced no scene content");
    }

    #[test]
    fn preset_star_evaluates() {
        run(PRESET_STAR, 5);
    }

    #[test]
    fn preset_polygon_evaluates() {
        run(PRESET_POLYGON, 6);
    }

    #[test]
    fn preset_spiral_evaluates() {
        run(PRESET_SPIRAL, 4);
    }

    #[test]
    fn preset_heart_evaluates() {
        run(PRESET_HEART, 1);
    }

    #[test]
    fn preset_rose_evaluates() {
        run(PRESET_ROSE, 5);
    }

    #[test]
    fn preset_lissajous_evaluates() {
        run(PRESET_LISSAJOUS, 4);
    }

    #[test]
    fn preset_rounded_rect_evaluates() {
        run(PRESET_ROUNDED_RECT, 20);
    }

    #[test]
    fn preset_speech_evaluates() {
        run(PRESET_SPEECH, 16);
    }

    #[test]
    fn preset_gear_evaluates() {
        run(PRESET_GEAR, 8);
    }

    #[test]
    fn preset_koch_evaluates() {
        // 4 iterations = 3·4^4 = 768 segments. Far from the 6-iter cap
        // but enough to exercise the recursion / array-mutation logic.
        run(PRESET_KOCH, 4);
    }
}
