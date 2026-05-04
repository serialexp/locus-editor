//! Gradient editor — preview bar, draggable stop markers, per-stop colour
//! editor, and spread-method combo. Lives in the properties panel,
//! beneath the paint-type combo for fill/stroke when the paint kind is
//! Linear or Radial.
//!
//! The editor takes a `&mut Gradient` (a local clone in the caller) and
//! returns whether anything changed. The caller (`properties_panel`)
//! applies the modified gradient via `Scene::set_gradient` and records
//! a `Command::SetGradient` undo with the previous value. On-canvas
//! gradient handles are a separate concern handled in the select tool.
//!
//! Stop semantics:
//! * Stops are kept sorted by `offset` (0..=1) at all times.
//! * Adjacent stops are forbidden from crossing — when dragging stop `i`,
//!   its offset is clamped to `(stops[i-1].offset, stops[i+1].offset)`.
//!   This dodges the index-tracking pain of stops swapping positions
//!   mid-drag.
//! * A gradient must always have ≥2 stops. The "Delete stop" button is
//!   disabled when `stops.len() <= 2`.
//!
//! Selected-stop index is persisted in `egui::Memory` keyed off the
//! editor's `id_source`, so it survives across frames without the caller
//! having to thread state through.

use egui::{Color32, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Vec2};
use vector_geom::Color;
use vector_scene::{ColorStop, Gradient, Scene, SpreadMethod};

use crate::paint_picker;
use crate::util::color_to_egui;

const BAR_HEIGHT: f32 = 28.0;
const MARKER_HEIGHT: f32 = 12.0;
const MARKER_HALF_WIDTH: f32 = 6.0;
const ROW_GAP: f32 = 4.0;

/// Show the full gradient editor for a single gradient. Returns true if
/// `gradient` was modified this frame.
pub(crate) fn show_gradient_editor(
    ui: &mut egui::Ui,
    gradient: &mut Gradient,
    scene: &Scene,
    id_source: &str,
) -> bool {
    let mut changed = false;
    let editor_id = ui.make_persistent_id(("gradient_editor", id_source));

    // Selected stop index, persisted in egui memory so a click on a
    // marker is remembered across frames without the caller threading
    // state through.
    let mut selected = ui
        .data(|d| d.get_temp::<usize>(editor_id))
        .unwrap_or(0)
        .min(gradient.stops.len().saturating_sub(1));

    // ── Preview bar + markers ──
    let bar_response = paint_preview_bar(ui, gradient);

    // Double-click on the bar inserts a new stop at the click's t.
    if bar_response.double_clicked()
        && let Some(pos) = bar_response.interact_pointer_pos()
    {
        let t = ((pos.x - bar_response.rect.left()) / bar_response.rect.width()).clamp(0.0, 1.0);
        let new_color = sample_gradient(gradient, t);
        let insert_at = gradient
            .stops
            .iter()
            .position(|s| s.offset > t)
            .unwrap_or(gradient.stops.len());
        gradient.stops.insert(
            insert_at,
            ColorStop {
                offset: t,
                color: new_color,
            },
        );
        selected = insert_at;
        changed = true;
    }

    // Stop markers (under the bar).
    let marker_strip = ui.allocate_response(
        Vec2::new(bar_response.rect.width(), MARKER_HEIGHT + 2.0),
        Sense::hover(),
    );
    let strip_rect = marker_strip.rect;

    let mut delete_idx: Option<usize> = None;
    for i in 0..gradient.stops.len() {
        let (clicked, deleted, offset_changed) = paint_stop_marker(
            ui,
            gradient,
            i,
            strip_rect,
            bar_response.rect,
            i == selected,
            editor_id,
        );
        if clicked {
            selected = i;
        }
        if deleted {
            delete_idx = Some(i);
        }
        if offset_changed {
            changed = true;
        }
    }
    if let Some(i) = delete_idx
        && gradient.stops.len() > 2
    {
        gradient.stops.remove(i);
        if selected >= gradient.stops.len() {
            selected = gradient.stops.len() - 1;
        }
        changed = true;
    }

    selected = selected.min(gradient.stops.len().saturating_sub(1));

    // ── Selected-stop editors ──
    ui.add_space(ROW_GAP);
    if selected < gradient.stops.len() {
        let stop_count = gradient.stops.len();
        ui.horizontal(|ui| {
            ui.label("Stop");
            ui.label(format!("{} of {}", selected + 1, stop_count));
        });

        // Stop colour.
        ui.horizontal(|ui| {
            ui.label("Color");
            let stop = &mut gradient.stops[selected];
            if paint_picker::solid_color_button(
                ui,
                &mut stop.color,
                scene,
                &format!("{id_source}_stop_color_{selected}"),
            ) {
                changed = true;
            }
        });

        // Stop offset (numeric — handy for typing exact values; the
        // marker drag is the primary input).
        // Clamp range to [prev.offset, next.offset] so numeric input
        // can't cross a neighbour either.
        let lo = if selected == 0 {
            0.0_f32
        } else {
            gradient.stops[selected - 1].offset
        };
        let hi = if selected + 1 >= stop_count {
            1.0_f32
        } else {
            gradient.stops[selected + 1].offset
        };
        ui.horizontal(|ui| {
            ui.label("Offset");
            let stop = &mut gradient.stops[selected];
            if ui
                .add(
                    egui::DragValue::new(&mut stop.offset)
                        .range(lo..=hi)
                        .speed(0.005)
                        .fixed_decimals(3),
                )
                .changed()
            {
                changed = true;
            }
        });
    }

    // ── Spread combo ──
    ui.horizontal(|ui| {
        ui.label("Spread");
        let mut spread = gradient.spread;
        egui::ComboBox::from_id_salt(("spread", id_source))
            .selected_text(spread_label(spread))
            .show_ui(ui, |ui| {
                for s in [
                    SpreadMethod::Pad,
                    SpreadMethod::Reflect,
                    SpreadMethod::Repeat,
                ] {
                    ui.selectable_value(&mut spread, s, spread_label(s));
                }
            });
        if spread != gradient.spread {
            gradient.spread = spread;
            changed = true;
        }
    });

    // Persist selected index for next frame.
    ui.data_mut(|d| d.insert_temp(editor_id, selected));

    changed
}

fn spread_label(s: SpreadMethod) -> &'static str {
    match s {
        SpreadMethod::Pad => "Pad",
        SpreadMethod::Reflect => "Reflect",
        SpreadMethod::Repeat => "Repeat",
    }
}

/// Render a horizontal preview of the gradient as a series of small
/// vertical slabs, blending stop colours in linear RGBA (matching the
/// renderer's fragment shader, so what you see in the panel matches what
/// you see on canvas).
///
/// The bar is `Sense::click()` so a double-click inserts a new stop;
/// drags on the bar itself are not used (stop dragging happens in the
/// marker strip below).
fn paint_preview_bar(ui: &mut egui::Ui, gradient: &Gradient) -> Response {
    let desired = Vec2::new(ui.available_width(), BAR_HEIGHT);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());

    // Background checker so transparent stops are visually obvious.
    crate::util::paint_transparency_checkerboard(ui.painter(), rect, 6.0);

    // Slab rendering. 1 px slabs are overkill but cheap and exact.
    const SLAB: f32 = 1.0;
    let mut x = rect.left();
    while x < rect.right() {
        let t = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let c = sample_gradient(gradient, t);
        let next = (x + SLAB).min(rect.right());
        ui.painter().rect_filled(
            Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(next, rect.bottom())),
            0.0,
            color_to_egui(c),
        );
        x = next;
    }

    // Border.
    ui.painter().rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color),
        StrokeKind::Inside,
    );

    resp
}

/// Render a single stop marker (a small upward-pointing triangle with the
/// stop's colour as fill). Returns (was_clicked, was_deleted, offset_changed).
///
/// Markers are draggable horizontally; the offset is clamped to the
/// adjacent stops so neighbours can't cross.
fn paint_stop_marker(
    ui: &mut egui::Ui,
    gradient: &mut Gradient,
    i: usize,
    strip_rect: Rect,
    bar_rect: Rect,
    is_selected: bool,
    editor_id: Id,
) -> (bool, bool, bool) {
    let stop = &gradient.stops[i];
    let cx = bar_rect.left() + stop.offset * bar_rect.width();
    let top = strip_rect.top();
    let marker_rect = Rect::from_min_max(
        Pos2::new(cx - MARKER_HALF_WIDTH, top),
        Pos2::new(cx + MARKER_HALF_WIDTH, top + MARKER_HEIGHT),
    );

    let resp = ui.interact(
        marker_rect,
        editor_id.with(("marker", i)),
        Sense::click_and_drag(),
    );

    // Body.
    let painter = ui.painter();
    let triangle = [
        Pos2::new(cx, top),
        Pos2::new(cx - MARKER_HALF_WIDTH, top + MARKER_HEIGHT),
        Pos2::new(cx + MARKER_HALF_WIDTH, top + MARKER_HEIGHT),
    ];
    let border_color = if is_selected {
        Color32::from_rgb(255, 200, 0)
    } else {
        ui.visuals().widgets.inactive.bg_stroke.color
    };
    let border_width = if is_selected { 2.0 } else { 1.0 };
    painter.add(egui::Shape::convex_polygon(
        triangle.to_vec(),
        color_to_egui(stop.color),
        Stroke::new(border_width, border_color),
    ));

    // Drag → adjust offset, clamped to neighbours.
    let mut offset_changed = false;
    if resp.dragged()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let new_t = ((pos.x - bar_rect.left()) / bar_rect.width()).clamp(0.0, 1.0);
        let lo = if i == 0 {
            0.0_f32
        } else {
            gradient.stops[i - 1].offset + 1e-4
        };
        let hi = if i + 1 >= gradient.stops.len() {
            1.0_f32
        } else {
            gradient.stops[i + 1].offset - 1e-4
        };
        let clamped = new_t.clamp(lo, hi);
        if (clamped - gradient.stops[i].offset).abs() > f32::EPSILON {
            gradient.stops[i].offset = clamped;
            offset_changed = true;
        }
    }

    let resp = resp.on_hover_text(format!(
        "{}\noffset {:.3}\n(right-click to delete)",
        paint_picker::color_to_hex(gradient.stops[i].color),
        gradient.stops[i].offset
    ));

    let mut deleted = false;
    resp.context_menu(|ui| {
        let can_delete = gradient.stops.len() > 2;
        ui.add_enabled_ui(can_delete, |ui| {
            if ui.button("Delete stop").clicked() {
                deleted = true;
                ui.close();
            }
        });
        if !can_delete {
            ui.label(
                egui::RichText::new("Gradient needs ≥2 stops")
                    .weak()
                    .small(),
            );
        }
    });

    (resp.clicked(), deleted, offset_changed)
}

/// Sample the gradient at parameter `t` ∈ [0,1] in linear RGBA, ignoring
/// spread mode (preview bar shows only the [0,1] range).
fn sample_gradient(g: &Gradient, t: f32) -> Color {
    if g.stops.is_empty() {
        return Color::TRANSPARENT;
    }
    if g.stops.len() == 1 {
        return g.stops[0].color;
    }
    if t <= g.stops.first().unwrap().offset {
        return g.stops.first().unwrap().color;
    }
    if t >= g.stops.last().unwrap().offset {
        return g.stops.last().unwrap().color;
    }
    for w in g.stops.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if t >= a.offset && t <= b.offset {
            let span = (b.offset - a.offset).max(f32::EPSILON);
            let local = (t - a.offset) / span;
            return a.color.lerp(b.color, local);
        }
    }
    g.stops.last().unwrap().color
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_geom::{Affine, Point};
    use vector_scene::{GradientKind, InterpolationSpace};

    fn linear_two_stop(a: Color, b: Color) -> Gradient {
        Gradient {
            kind: GradientKind::Linear {
                start: Point::new(0.0, 0.0),
                end: Point::new(1.0, 0.0),
            },
            stops: vec![
                ColorStop {
                    offset: 0.0,
                    color: a,
                },
                ColorStop {
                    offset: 1.0,
                    color: b,
                },
            ],
            interpolation: InterpolationSpace::LinearRgb,
            transform: Affine::IDENTITY,
            spread: SpreadMethod::Pad,
        }
    }

    #[test]
    fn sample_endpoints() {
        let g = linear_two_stop(Color::BLACK, Color::WHITE);
        assert_eq!(sample_gradient(&g, 0.0).to_srgb8(), [0, 0, 0, 255]);
        assert_eq!(sample_gradient(&g, 1.0).to_srgb8(), [255, 255, 255, 255]);
    }

    #[test]
    fn sample_midpoint_is_50_percent_in_linear() {
        let g = linear_two_stop(Color::BLACK, Color::WHITE);
        let mid = sample_gradient(&g, 0.5);
        // Linear midpoint is 0.5 in linear light, which is ~187/255 in sRGB.
        let s = mid.to_srgb8();
        assert!(s[0] > 180 && s[0] < 195, "expected ~188, got {}", s[0]);
    }

    #[test]
    fn sample_outside_range_clamps() {
        let g = linear_two_stop(Color::BLACK, Color::WHITE);
        // Sampling t < 0 returns first stop's colour.
        assert_eq!(sample_gradient(&g, -0.5).to_srgb8(), [0, 0, 0, 255]);
        // Sampling t > 1 returns last stop's colour.
        assert_eq!(sample_gradient(&g, 1.5).to_srgb8(), [255, 255, 255, 255]);
    }
}
