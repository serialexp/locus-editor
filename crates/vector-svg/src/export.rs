use std::collections::HashMap;
use std::fmt::Write;

use vector_geom::{Affine, Color, Segment};
use vector_scene::{
    FillRule, Gradient, GradientKind, LineCap, LineJoin, NodeData, NodeId, Paint, PaintRef, Scene,
    SpreadMethod,
};

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

    // Build gradient label map: NodeId → SVG id string.
    let gradient_ids = build_gradient_id_map(scene);

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
            write_node(scene, child_id, 1, &mut buf, &gradient_ids);
        }
    }

    let _ = writeln!(buf, "</svg>");
    buf
}

/// Build a map from gradient NodeIds to their SVG element id strings.
/// Ensures unique ids even if labels collide.
fn build_gradient_id_map(scene: &Scene) -> HashMap<NodeId, String> {
    let mut map = HashMap::new();
    let defs = scene.defs();
    let Some(defs_node) = scene.get(defs) else {
        return map;
    };

    let mut counter = 0usize;
    for &child_id in &defs_node.children {
        if let Some(child) = scene.get(child_id)
            && matches!(&child.data, NodeData::Paint(Paint::Gradient(_)))
        {
            let id = if child.label.is_empty() {
                counter += 1;
                format!("gradient_{counter}")
            } else {
                child.label.clone()
            };
            map.insert(child_id, id);
        }
    }
    map
}

/// Recursively emit a node and its children as SVG elements.
fn write_node(
    scene: &Scene,
    id: NodeId,
    indent: usize,
    buf: &mut String,
    gradient_ids: &HashMap<NodeId, String>,
) {
    let Some(node) = scene.get(id) else { return };
    if !node.visible {
        return;
    }

    let pad = "  ".repeat(indent);
    let transform_attr = fmt_transform(node.transform);

    match &node.data {
        NodeData::Group { is_defs: true } => {
            // Emit a <defs> block if there are any gradient definitions.
            if gradient_ids.is_empty() {
                return;
            }
            let _ = writeln!(buf, "{pad}<defs>");
            for &child_id in &node.children {
                if let Some(child) = scene.get(child_id)
                    && let NodeData::Paint(Paint::Gradient(gradient)) = &child.data
                    && let Some(svg_id) = gradient_ids.get(&child_id)
                {
                    write_gradient(gradient, svg_id, indent + 1, buf);
                }
            }
            let _ = writeln!(buf, "{pad}</defs>");
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
                write_node(scene, child_id, indent + 1, buf, gradient_ids);
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
                    let _ = write!(buf, r#" fill="{}""#, fmt_paint(&fill.paint, gradient_ids));
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
                let _ = write!(
                    buf,
                    r#" stroke="{}""#,
                    fmt_paint(&stroke.paint, gradient_ids)
                );
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
            // Paint definitions are emitted from the defs group handler; skip here.
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

/// Emit an SVG gradient definition element.
fn write_gradient(gradient: &Gradient, svg_id: &str, indent: usize, buf: &mut String) {
    let pad = "  ".repeat(indent);
    let transform_attr = fmt_transform(gradient.transform);
    let spread_attr = match gradient.spread {
        SpreadMethod::Pad => None,
        SpreadMethod::Reflect => Some("reflect"),
        SpreadMethod::Repeat => Some("repeat"),
    };

    match &gradient.kind {
        GradientKind::Linear { start, end } => {
            let _ = write!(
                buf,
                r#"{pad}<linearGradient id="{}" x1="{}" y1="{}" x2="{}" y2="{}""#,
                xml_escape(svg_id),
                start.x,
                start.y,
                end.x,
                end.y
            );
            let _ = write!(buf, r#" gradientUnits="userSpaceOnUse""#);
            if let Some(ref t) = transform_attr {
                let _ = write!(buf, r#" gradientTransform="{t}""#);
            }
            if let Some(s) = spread_attr {
                let _ = write!(buf, r#" spreadMethod="{s}""#);
            }
            let _ = writeln!(buf, ">");
            write_stops(&gradient.stops, indent + 1, buf);
            let _ = writeln!(buf, "{pad}</linearGradient>");
        }
        GradientKind::Radial {
            center,
            radius,
            focal,
            focal_radius,
        } => {
            let _ = write!(
                buf,
                r#"{pad}<radialGradient id="{}" cx="{}" cy="{}" r="{}""#,
                xml_escape(svg_id),
                center.x,
                center.y,
                radius
            );
            let _ = write!(buf, r#" fx="{}" fy="{}""#, focal.x, focal.y);
            if *focal_radius > 1e-8 {
                let _ = write!(buf, r#" fr="{}""#, focal_radius);
            }
            let _ = write!(buf, r#" gradientUnits="userSpaceOnUse""#);
            if let Some(ref t) = transform_attr {
                let _ = write!(buf, r#" gradientTransform="{t}""#);
            }
            if let Some(s) = spread_attr {
                let _ = write!(buf, r#" spreadMethod="{s}""#);
            }
            let _ = writeln!(buf, ">");
            write_stops(&gradient.stops, indent + 1, buf);
            let _ = writeln!(buf, "{pad}</radialGradient>");
        }
    }
}

/// Emit `<stop>` elements for gradient color stops.
fn write_stops(stops: &[vector_scene::ColorStop], indent: usize, buf: &mut String) {
    let pad = "  ".repeat(indent);
    for stop in stops {
        let color = Color::new(stop.color.r, stop.color.g, stop.color.b, 1.0);
        let opacity = stop.color.a;
        let _ = write!(
            buf,
            r#"{pad}<stop offset="{}" stop-color="{}""#,
            stop.offset,
            fmt_color(color)
        );
        if opacity < 1.0 - 1e-4 {
            let _ = write!(buf, r#" stop-opacity="{:.4}""#, opacity);
        }
        let _ = writeln!(buf, "/>");
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

/// Format a PaintRef as an SVG paint value string.
/// Solid colors become `#rrggbb`, gradient refs become `url(#id)`.
fn fmt_paint(paint: &PaintRef, gradient_ids: &HashMap<NodeId, String>) -> String {
    match paint {
        PaintRef::Solid(c) => fmt_color(*c),
        PaintRef::Ref(node_id) => {
            if let Some(svg_id) = gradient_ids.get(node_id) {
                format!("url(#{})", xml_escape(svg_id))
            } else {
                fmt_color(Color::BLACK)
            }
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

    #[test]
    fn export_gradient_defs() {
        use vector_scene::{ColorStop, Gradient, GradientKind, InterpolationSpace, SpreadMethod};

        let mut scene = Scene::new();
        let root = scene.root();
        let defs = scene.defs();

        // Create a linear gradient in defs.
        let gradient = Gradient {
            kind: GradientKind::Linear {
                start: Point::new(0.0, 0.0),
                end: Point::new(100.0, 0.0),
            },
            stops: vec![
                ColorStop {
                    offset: 0.0,
                    color: Color::from_srgb8(255, 0, 0, 255),
                },
                ColorStop {
                    offset: 1.0,
                    color: Color::from_srgb8(0, 0, 255, 255),
                },
            ],
            interpolation: InterpolationSpace::LinearRgb,
            transform: Affine::IDENTITY,
            spread: SpreadMethod::Pad,
        };
        let grad_node = vector_scene::Node {
            label: "my_grad".into(),
            transform: Affine::IDENTITY,
            data: NodeData::Paint(vector_scene::Paint::Gradient(gradient)),
            children: Vec::new(),
            visible: true,
            locked: false,
        };
        let grad_id = scene.insert(defs, grad_node).unwrap();

        // Create a path that references the gradient.
        let mut path = Path::new();
        path.subpaths.push(SubPath {
            start: Point::new(0.0, 0.0),
            segments: vec![Segment::Line {
                to: Point::new(100.0, 100.0),
            }],
            closed: false,
        });
        let mut node = vector_scene::Node::path("rect", path);
        if let NodeData::Path { ref mut style, .. } = node.data {
            style.fill = Some(vector_scene::style::Fill {
                paint: PaintRef::Ref(grad_id),
                rule: FillRule::NonZero,
                opacity: 1.0,
            });
        }
        scene.insert(root, node);

        let svg = export_svg(&scene);
        assert!(svg.contains("<defs>"), "should have defs section");
        assert!(
            svg.contains("linearGradient"),
            "should have linearGradient element"
        );
        assert!(
            svg.contains(r#"id="my_grad""#),
            "gradient should have correct id"
        );
        assert!(
            svg.contains("url(#my_grad)"),
            "path fill should reference gradient"
        );
        assert!(svg.contains("<stop"), "should have stop elements");
        assert!(svg.contains("</defs>"), "should close defs section");
    }
}
