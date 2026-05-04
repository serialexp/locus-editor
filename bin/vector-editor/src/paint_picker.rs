//! Paint picker — a custom colour popup that adds a hex input field and a
//! document-wide palette of already-used colours on top of egui's built-in
//! HSV picker.
//!
//! Replaces the bare `ui.color_edit_button_srgba()` calls in the properties
//! panel. The widget itself stays a small swatch button; clicking it opens
//! a popup with three sections:
//!   1. egui's `color_picker_color32` HSV square + sliders
//!   2. a hex input field (`#RGB` / `#RGBA` / `#RRGGBB` / `#RRGGBBAA`)
//!   3. a palette grid of every unique solid colour currently used in the
//!      scene (fills, strokes, gradient stops)
//!
//! All colour values cross the boundary as `vector_geom::Color` (linear
//! RGBA); the picker handles the sRGB conversion internally via
//! [`color_to_egui`] / [`egui_to_color`] in `util.rs`. The hex parser /
//! formatter likewise treats the on-screen string as sRGB bytes — that's
//! the only convention people expect.

use egui::{Id, Popup, Sense, Stroke, StrokeKind, Vec2};
use std::collections::HashMap;
use vector_geom::Color;
use vector_scene::{NodeData, Paint, PaintRef, Scene};

use crate::util::{color_to_egui, egui_to_color};

// ── Layout constants ──

const SWATCH_SIZE: Vec2 = Vec2::new(36.0, 18.0);
const PALETTE_SWATCH: Vec2 = Vec2::new(20.0, 20.0);
const PALETTE_COLUMNS: usize = 8;
const PALETTE_CAP: usize = 64;

/// Maximum number of palette entries shown in the popup. Public so other
/// modules (like the scene-scan helper) can stay consistent.
pub(crate) const DOCUMENT_PALETTE_CAP: usize = PALETTE_CAP;

// ── The widget ──

/// Draw a swatch button for `color`. When clicked, opens a popup with the
/// HSV picker, a hex input field, and a palette of every solid colour
/// already used in `scene`. The palette is scanned **only when the popup
/// is actually open** — large traced-raster documents (10k+ paths) make
/// the scene walk too costly to run every frame.
///
/// `id_source` is hashed into the popup id so multiple pickers on the same
/// frame don't share state. Returns `true` if the colour changed this frame.
pub(crate) fn solid_color_button(
    ui: &mut egui::Ui,
    color: &mut Color,
    scene: &Scene,
    id_source: &str,
) -> bool {
    let mut changed = false;

    // ── Swatch button ──
    let (rect, mut resp) = ui.allocate_exact_size(SWATCH_SIZE, Sense::click());
    let visuals = ui.visuals().widgets.style(&resp);
    let painter = ui.painter();
    crate::util::paint_transparency_checkerboard(painter, rect, 4.0);
    painter.rect_filled(rect, 2.0, color_to_egui(*color));
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, visuals.fg_stroke.color),
        StrokeKind::Inside,
    );
    resp = resp.on_hover_text(color_to_hex(*color));

    // Stable id for the popup so we can close it from inside (e.g. when
    // the user clicks a palette swatch).
    let popup_id = ui.make_persistent_id(("paint_picker", id_source));

    Popup::menu(&resp)
        .id(popup_id)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(240.0);

            // ── HSV picker ──
            let mut srgba = color_to_egui(*color);
            if egui::widgets::color_picker::color_picker_color32(
                ui,
                &mut srgba,
                egui::widgets::color_picker::Alpha::OnlyBlend,
            ) {
                *color = egui_to_color(srgba);
                changed = true;
                // Force the hex field to re-sync from the new colour on
                // the next iteration of the closure (we wrote into `color`).
                set_cached_hex(ui, popup_id, color_to_hex(*color));
            }

            ui.add_space(4.0);

            // ── Hex input ──
            ui.horizontal(|ui| {
                ui.label("Hex");
                let cached = cached_hex(ui, popup_id).unwrap_or_else(|| color_to_hex(*color));

                // If the cached string parses to something different than
                // the current colour (e.g. egui picker just changed it),
                // re-sync. If the cached string is junk (parse fails),
                // leave it — the user is mid-edit.
                let mut hex_text = match parse_hex(&cached) {
                    Some(parsed) if !colors_close(parsed, *color) => color_to_hex(*color),
                    _ => cached,
                };

                let resp = ui.add(
                    egui::TextEdit::singleline(&mut hex_text)
                        .desired_width(96.0)
                        .font(egui::TextStyle::Monospace),
                );

                if resp.changed()
                    && let Some(c) = parse_hex(&hex_text)
                {
                    *color = c;
                    changed = true;
                }
                set_cached_hex(ui, popup_id, hex_text);
            });

            // ── Document palette ──
            // Scanned only while the popup body is being rendered, i.e.
            // while the popup is open. On a closed popup egui short-
            // circuits and never enters this closure — that's the whole
            // reason this is structured as `Popup::menu(...).show(...)`.
            let palette = document_palette(scene);
            if !palette.is_empty() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Document").weak().small());
                egui::Grid::new(popup_id.with("palette"))
                    .spacing([2.0, 2.0])
                    .show(ui, |ui| {
                        for (i, &c) in palette.iter().enumerate() {
                            if palette_swatch(ui, c) {
                                *color = c;
                                changed = true;
                                set_cached_hex(ui, popup_id, color_to_hex(c));
                                Popup::close_id(ui.ctx(), popup_id);
                            }
                            if (i + 1) % PALETTE_COLUMNS == 0 {
                                ui.end_row();
                            }
                        }
                    });
            }
        });

    changed
}

/// Render a single palette swatch. Returns true if it was clicked.
fn palette_swatch(ui: &mut egui::Ui, c: Color) -> bool {
    let (rect, resp) = ui.allocate_exact_size(PALETTE_SWATCH, Sense::click());
    let painter = ui.painter();
    crate::util::paint_transparency_checkerboard(painter, rect, 4.0);
    painter.rect_filled(rect, 2.0, color_to_egui(c));
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(0.5, ui.visuals().widgets.inactive.fg_stroke.color),
        StrokeKind::Inside,
    );
    let resp = resp.on_hover_text(color_to_hex(c));
    resp.clicked()
}

// ── Hex parsing ──

/// Format a colour as `#rrggbb` (alpha = 255) or `#rrggbbaa` lowercase.
pub(crate) fn color_to_hex(c: Color) -> String {
    let [r, g, b, a] = c.to_srgb8();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

/// Parse a hex colour string. Accepts (with or without leading `#`):
/// `RGB`, `RGBA`, `RRGGBB`, `RRGGBBAA`. Whitespace is trimmed.
pub(crate) fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (r, g, b, a) = match s.len() {
        3 => {
            let r = u8::from_str_radix(&s[0..1], 16).ok()?;
            let g = u8::from_str_radix(&s[1..2], 16).ok()?;
            let b = u8::from_str_radix(&s[2..3], 16).ok()?;
            (r * 17, g * 17, b * 17, 255)
        }
        4 => {
            let r = u8::from_str_radix(&s[0..1], 16).ok()?;
            let g = u8::from_str_radix(&s[1..2], 16).ok()?;
            let b = u8::from_str_radix(&s[2..3], 16).ok()?;
            let a = u8::from_str_radix(&s[3..4], 16).ok()?;
            (r * 17, g * 17, b * 17, a * 17)
        }
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            (r, g, b, 255)
        }
        8 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            let a = u8::from_str_radix(&s[6..8], 16).ok()?;
            (r, g, b, a)
        }
        _ => return None,
    };
    Some(Color::from_srgb8(r, g, b, a))
}

/// Whether two colours render to the same sRGB+alpha bytes. We compare
/// post-quantisation because that's what hex round-trips through; comparing
/// raw f32 fields would treat `#7f7f7f` and a freshly-typed `#7f7f7f` as
/// "different" once one has gone through the egui HSV conversion.
fn colors_close(a: Color, b: Color) -> bool {
    a.to_srgb8() == b.to_srgb8()
}

// ── Cached hex string in egui memory ──
//
// Why we cache: the user might type partial / invalid hex (e.g. `#ff` while
// extending to `#ffaa00`). We want the textbox to keep showing what they
// typed even if it doesn't currently parse to anything. We invalidate the
// cache only when the colour was changed by some other path (HSV picker,
// palette click, external assignment) and the cached text no longer parses
// to that colour.

fn cached_hex(ui: &egui::Ui, popup_id: Id) -> Option<String> {
    let key = popup_id.with("hex_text");
    ui.data(|d| d.get_temp::<String>(key))
}

fn set_cached_hex(ui: &egui::Ui, popup_id: Id, text: String) {
    let key = popup_id.with("hex_text");
    ui.data_mut(|d| d.insert_temp(key, text));
}

// ── Document palette scan ──

/// Walk the scene and collect every unique solid colour referenced by any
/// node — fill paint, stroke paint, and every gradient stop colour in defs.
///
/// Ordered by descending usage count, then by HSL hue (for visual stability
/// when ties are common). Capped at [`DOCUMENT_PALETTE_CAP`] entries.
///
/// Walks defs as well as the rendered tree because gradient stops only live
/// in the defs subtree but are absolutely "colours used in this document".
pub(crate) fn document_palette(scene: &Scene) -> Vec<Color> {
    // Dedup by post-quantised sRGB bytes — same equivalence as the picker.
    let mut counts: HashMap<[u8; 4], (Color, u32)> = HashMap::new();

    let mut bump = |c: Color| {
        let key = c.to_srgb8();
        counts.entry(key).or_insert((c, 0)).1 += 1;
    };

    for (_, node) in scene.iter() {
        // Solid fill/stroke colours from any styled node.
        node.for_each_paint_ref(|paint| {
            if let PaintRef::Solid(c) = paint {
                bump(*c);
            }
        });
        // Gradient stop colours — these don't appear in `for_each_paint_ref`
        // (which only walks references on a single node's style), so handle
        // them here when we encounter the gradient definition itself.
        if let NodeData::Paint(Paint::Gradient(g)) = &node.data {
            for stop in &g.stops {
                bump(stop.color);
            }
        }
    }

    let mut entries: Vec<([u8; 4], Color, u32)> =
        counts.into_iter().map(|(k, (c, n))| (k, c, n)).collect();
    // Primary: usage descending. Secondary: hue ascending (for stable order
    // among ties).
    entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| hue(a.1).total_cmp(&hue(b.1))));
    entries.truncate(DOCUMENT_PALETTE_CAP);
    entries.into_iter().map(|(_, c, _)| c).collect()
}

/// HSL hue in `[0, 360)` for ordering ties in the palette. Computed in sRGB
/// (post-gamma) because that's the space humans perceive hue in.
fn hue(c: Color) -> f32 {
    let [r, g, b, _] = c.to_srgb8();
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d <= f32::EPSILON {
        return 0.0;
    }
    let h = if max == r {
        ((g - b) / d) % 6.0
    } else if max == g {
        ((b - r) / d) + 2.0
    } else {
        ((r - g) / d) + 4.0
    };
    let mut h = h * 60.0;
    if h < 0.0 {
        h += 360.0;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_accepts_short_and_long_forms() {
        assert!(parse_hex("#fff").is_some());
        assert!(parse_hex("FFF").is_some());
        assert!(parse_hex("#ffaa00").is_some());
        assert!(parse_hex("#ffaa0080").is_some());
        assert!(parse_hex("  #abc ").is_some());
        // Short form expands by repeat: f -> ff (=255).
        let c = parse_hex("#fff").unwrap();
        assert_eq!(c.to_srgb8(), [255, 255, 255, 255]);
        // 4-digit expands alpha too.
        let c = parse_hex("#fff8").unwrap();
        assert_eq!(c.to_srgb8(), [255, 255, 255, 0x88]);
    }

    #[test]
    fn parse_hex_rejects_garbage() {
        assert!(parse_hex("").is_none());
        assert!(parse_hex("#").is_none());
        assert!(parse_hex("#xyz").is_none());
        assert!(parse_hex("#fffff").is_none()); // 5 digits is invalid
        assert!(parse_hex("#fffffff").is_none()); // 7 digits is invalid
    }

    #[test]
    fn color_to_hex_drops_alpha_when_opaque() {
        assert_eq!(color_to_hex(Color::WHITE), "#ffffff");
        assert_eq!(color_to_hex(Color::BLACK), "#000000");
        let translucent = Color::from_srgb8(255, 0, 0, 128);
        assert_eq!(color_to_hex(translucent), "#ff000080");
    }

    #[test]
    fn color_to_hex_round_trips_via_parse() {
        let original = Color::from_srgb8(0x33, 0x66, 0xff, 0xff);
        let hex = color_to_hex(original);
        assert_eq!(hex, "#3366ff");
        let parsed = parse_hex(&hex).unwrap();
        assert_eq!(parsed.to_srgb8(), original.to_srgb8());
    }

    #[test]
    fn document_palette_dedups_and_orders_by_usage() {
        use vector_scene::style::{Fill, FillRule};
        use vector_scene::{
            ColorStop, Gradient, GradientKind, Node, NodeData, Paint, PaintRef, SpreadMethod,
        };

        let mut scene = Scene::new();
        let root = scene.root();

        let red_fill = Fill {
            paint: PaintRef::Solid(Color::from_srgb8(255, 0, 0, 255)),
            rule: FillRule::NonZero,
            opacity: 1.0,
        };
        let blue_fill = Fill {
            paint: PaintRef::Solid(Color::from_srgb8(0, 0, 255, 255)),
            rule: FillRule::NonZero,
            opacity: 1.0,
        };

        // Two reds, one blue.
        for fill in [&red_fill, &red_fill, &blue_fill] {
            let mut node = Node::path("p", vector_geom::Path::new());
            if let NodeData::Path { style, .. } = &mut node.data {
                style.fill = Some(fill.clone());
            }
            scene.insert(root, node);
        }

        // A green gradient stop in defs.
        let grad = Gradient {
            kind: GradientKind::Linear {
                start: vector_geom::Point::new(0.0, 0.0),
                end: vector_geom::Point::new(1.0, 0.0),
            },
            stops: vec![ColorStop {
                offset: 0.0,
                color: Color::from_srgb8(0, 255, 0, 255),
            }],
            interpolation: vector_scene::InterpolationSpace::LinearRgb,
            transform: vector_geom::Affine::IDENTITY,
            spread: SpreadMethod::Pad,
        };
        scene.insert(
            scene.defs(),
            Node {
                label: "g".into(),
                transform: vector_geom::Affine::IDENTITY,
                data: NodeData::Paint(Paint::Gradient(grad)),
                children: Vec::new(),
                visible: true,
                locked: false,
            },
        );

        let palette = document_palette(&scene);
        // Expect red first (count 2), then either blue or green (each
        // count 1) in hue order: red(0)/green(120)/blue(240) → green, blue.
        assert_eq!(palette.len(), 3);
        assert_eq!(palette[0].to_srgb8(), [255, 0, 0, 255]);
        // After red, ties broken by hue ascending: green (120°) before blue (240°).
        assert_eq!(palette[1].to_srgb8(), [0, 255, 0, 255]);
        assert_eq!(palette[2].to_srgb8(), [0, 0, 255, 255]);
    }
}
