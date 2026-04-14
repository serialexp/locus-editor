use std::fmt::Write;

use vector_geom::{Affine, Color, Segment};
use vector_scene::{FillRule, LineCap, LineJoin, NodeData, NodeId, PaintRef, Scene};

/// Export a scene graph to an SVG string.
pub fn export_svg(scene: &Scene) -> String {
    let bounds = scene.content_bounds();
    let (vx, vy, vw, vh) = if bounds.is_empty() {
        (0.0, 0.0, 800.0, 600.0)
    } else {
        // Add 1px margin to avoid clipping strokes at the edge.
        let margin = 1.0;
        (
            bounds.min.x - margin,
            bounds.min.y - margin,
            bounds.width() + 2.0 * margin,
            bounds.height() + 2.0 * margin,
        )
    };

    let mut buf = String::with_capacity(4096);
    let _ = writeln!(buf, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = writeln!(
        buf,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vx} {vy} {vw} {vh}">"#,
    );

    // Walk root's children (skip root itself).
    let root = scene.root();
    if let Some(root_node) = scene.get(root) {
        for &child_id in &root_node.children {
            write_node(scene, child_id, 1, &mut buf);
        }
    }

    let _ = writeln!(buf, "</svg>");
    buf
}

/// Recursively emit a node and its children as SVG elements.
fn write_node(scene: &Scene, id: NodeId, indent: usize, buf: &mut String) {
    let Some(node) = scene.get(id) else { return };
    if !node.visible {
        return;
    }

    let pad = "  ".repeat(indent);
    let transform_attr = fmt_transform(node.transform);

    match &node.data {
        NodeData::Group { is_defs: true } => {
            // Skip the defs group entirely (gradient refs not exported yet).
        }
        NodeData::Group { is_defs: false } => {
            let _ = write!(buf, "{pad}<g");
            if !node.label.is_empty() {
                let _ = write!(buf, r#" id="{}""#, xml_escape(&node.label));
            }
            if let Some(ref t) = transform_attr {
                let _ = write!(buf, r#" transform="{t}""#);
            }
            let _ = writeln!(buf, ">");

            for &child_id in &node.children {
                write_node(scene, child_id, indent + 1, buf);
            }

            let _ = writeln!(buf, "{pad}</g>");
        }
        NodeData::Path { path, style } => {
            let d = fmt_path_data(path);
            let _ = write!(buf, r#"{pad}<path d="{d}""#);

            if !node.label.is_empty() {
                let _ = write!(buf, r#" id="{}""#, xml_escape(&node.label));
            }
            if let Some(ref t) = transform_attr {
                let _ = write!(buf, r#" transform="{t}""#);
            }

            // Fill attributes.
            match &style.fill {
                None => {
                    let _ = write!(buf, r#" fill="none""#);
                }
                Some(fill) => {
                    let color = paint_color(&fill.paint);
                    let _ = write!(buf, r#" fill="{}""#, fmt_color(color));
                    if fill.rule == FillRule::EvenOdd {
                        let _ = write!(buf, r#" fill-rule="evenodd""#);
                    }
                    if fill.opacity < 1.0 - 1e-4 {
                        let _ = write!(buf, r#" fill-opacity="{:.4}""#, fill.opacity);
                    }
                }
            }

            // Stroke attributes.
            if let Some(stroke) = &style.stroke {
                let color = paint_color(&stroke.paint);
                let _ = write!(buf, r#" stroke="{}""#, fmt_color(color));
                let _ = write!(buf, r#" stroke-width="{}""#, stroke.style.width);
                if stroke.opacity < 1.0 - 1e-4 {
                    let _ = write!(buf, r#" stroke-opacity="{:.4}""#, stroke.opacity);
                }
                match stroke.style.cap {
                    LineCap::Butt => {} // SVG default
                    LineCap::Round => {
                        let _ = write!(buf, r#" stroke-linecap="round""#);
                    }
                    LineCap::Square => {
                        let _ = write!(buf, r#" stroke-linecap="square""#);
                    }
                }
                match stroke.style.join {
                    LineJoin::Miter => {} // SVG default
                    LineJoin::Round => {
                        let _ = write!(buf, r#" stroke-linejoin="round""#);
                    }
                    LineJoin::Bevel => {
                        let _ = write!(buf, r#" stroke-linejoin="bevel""#);
                    }
                }
                if (stroke.style.miter_limit - 4.0).abs() > 1e-4 {
                    let _ = write!(buf, r#" stroke-miterlimit="{}""#, stroke.style.miter_limit);
                }
                if let Some(dash) = &stroke.style.dash {
                    let vals: Vec<String> = dash.array.iter().map(|v| v.to_string()).collect();
                    let _ = write!(buf, r#" stroke-dasharray="{}""#, vals.join(","));
                    if dash.offset.abs() > 1e-6 {
                        let _ = write!(buf, r#" stroke-dashoffset="{}""#, dash.offset);
                    }
                }
            }

            let _ = writeln!(buf, "/>");
        }
        NodeData::Paint(_) => {
            // Paint definitions are in the defs group; skip here.
        }
        NodeData::Text(text) => {
            let _ = write!(buf, r#"{pad}<text"#);
            if !node.label.is_empty() {
                let _ = write!(buf, r#" id="{}""#, xml_escape(&node.label));
            }
            if let Some(ref t) = transform_attr {
                let _ = write!(buf, r#" transform="{t}""#);
            }
            let _ = write!(buf, r#" font-family="{}""#, xml_escape(&text.font_family));
            let _ = write!(buf, r#" font-size="{}""#, text.font_size);
            let _ = writeln!(buf, ">{}</text>", xml_escape(&text.content));
        }
    }
}

// ── Formatting helpers ──────────────────────────────────────────────

/// Convert a Path to an SVG `d` attribute string.
fn fmt_path_data(path: &vector_geom::Path) -> String {
    let mut d = String::new();
    for sp in &path.subpaths {
        if !d.is_empty() {
            d.push(' ');
        }
        let _ = write!(d, "M {} {}", sp.start.x, sp.start.y);
        for seg in &sp.segments {
            match seg {
                Segment::Line { to } => {
                    let _ = write!(d, " L {} {}", to.x, to.y);
                }
                Segment::Quad { ctrl, to } => {
                    let _ = write!(d, " Q {} {} {} {}", ctrl.x, ctrl.y, to.x, to.y);
                }
                Segment::Cubic { ctrl1, ctrl2, to } => {
                    let _ = write!(
                        d,
                        " C {} {} {} {} {} {}",
                        ctrl1.x, ctrl1.y, ctrl2.x, ctrl2.y, to.x, to.y
                    );
                }
                Segment::Arc {
                    radii,
                    x_rotation,
                    large_arc,
                    sweep,
                    to,
                } => {
                    let _ = write!(
                        d,
                        " A {} {} {} {} {} {} {}",
                        radii.x,
                        radii.y,
                        x_rotation.to_degrees(),
                        u8::from(*large_arc),
                        u8::from(*sweep),
                        to.x,
                        to.y,
                    );
                }
            }
        }
        if sp.closed {
            d.push_str(" Z");
        }
    }
    d
}

/// Convert a linear RGBA Color to an sRGB hex string (`#rrggbb`).
fn fmt_color(color: Color) -> String {
    let [r, g, b, _a] = color.to_srgb8();
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Format an Affine transform as an SVG `matrix(...)` string.
/// Returns `None` for the identity transform (no attribute needed).
fn fmt_transform(affine: Affine) -> Option<String> {
    if affine.is_identity() {
        return None;
    }
    // SVG matrix(a,b,c,d,e,f): x'=a*x+c*y+e, y'=b*x+d*y+f
    // Our Affine:               x'=a*x+b*y+tx, y'=c*x+d*y+ty
    // So SVG args are: (our.a, our.c, our.b, our.d, our.tx, our.ty)
    Some(format!(
        "matrix({},{},{},{},{},{})",
        affine.a, affine.c, affine.b, affine.d, affine.tx, affine.ty
    ))
}

/// Resolve a PaintRef to a solid color (gradient refs fall back to black).
fn paint_color(paint: &PaintRef) -> Color {
    match paint {
        PaintRef::Solid(c) => *c,
        PaintRef::Ref(_) => {
            // Gradient/pattern refs are not exported yet.
            Color::BLACK
        }
    }
}

/// Minimal XML escaping for attribute values and text content.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_geom::*;

    #[test]
    fn color_roundtrip() {
        let c = Color::from_srgb8(255, 128, 0, 255);
        assert_eq!(fmt_color(c), "#ff8000");
    }

    #[test]
    fn path_data_line() {
        let mut path = Path::new();
        path.subpaths.push(SubPath {
            start: Point::new(10.0, 20.0),
            segments: vec![Segment::Line {
                to: Point::new(30.0, 40.0),
            }],
            closed: false,
        });
        let d = fmt_path_data(&path);
        assert_eq!(d, "M 10 20 L 30 40");
    }

    #[test]
    fn path_data_closed() {
        let mut path = Path::new();
        path.subpaths.push(SubPath {
            start: Point::new(0.0, 0.0),
            segments: vec![
                Segment::Line {
                    to: Point::new(100.0, 0.0),
                },
                Segment::Line {
                    to: Point::new(100.0, 100.0),
                },
            ],
            closed: true,
        });
        let d = fmt_path_data(&path);
        assert_eq!(d, "M 0 0 L 100 0 L 100 100 Z");
    }

    #[test]
    fn export_contains_svg_header() {
        let scene = Scene::new();
        let svg = export_svg(&scene);
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
        assert!(svg.contains("xmlns"));
    }

    #[test]
    fn export_simple_path() {
        let mut scene = Scene::new();
        let root = scene.root();

        let mut path = Path::new();
        path.subpaths.push(SubPath {
            start: Point::new(0.0, 0.0),
            segments: vec![
                Segment::Line {
                    to: Point::new(100.0, 0.0),
                },
                Segment::Line {
                    to: Point::new(50.0, 100.0),
                },
            ],
            closed: true,
        });

        let mut node = vector_scene::Node::path("triangle", path);
        if let NodeData::Path { ref mut style, .. } = node.data {
            style.fill = Some(vector_scene::style::Fill {
                paint: PaintRef::Solid(Color::from_srgb8(255, 0, 0, 255)),
                rule: FillRule::NonZero,
                opacity: 1.0,
            });
            style.stroke = Some(vector_scene::style::Stroke {
                paint: PaintRef::Solid(Color::from_srgb8(0, 0, 0, 255)),
                style: vector_scene::StrokeStyle {
                    width: 2.0,
                    ..Default::default()
                },
                opacity: 1.0,
            });
        }
        scene.insert(root, node);

        let svg = export_svg(&scene);
        assert!(svg.contains(r#"<path d=""#));
        assert!(svg.contains(r##"fill="#ff0000""##));
        assert!(svg.contains(r##"stroke="#000000""##));
        assert!(svg.contains(r##"stroke-width="2""##));
        assert!(svg.contains(" Z"));
    }

    #[test]
    fn transform_identity_is_none() {
        assert!(fmt_transform(Affine::IDENTITY).is_none());
    }

    #[test]
    fn transform_translate() {
        let t = fmt_transform(Affine::translate(10.0, 20.0)).unwrap();
        assert!(t.contains("matrix("));
        assert!(t.contains("10"));
        assert!(t.contains("20"));
    }
}
