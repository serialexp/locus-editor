use std::collections::HashMap;

use vector_geom::{Affine, Color, Path, Point, Segment, SubPath, VertexMode};
use vector_scene::{Node, TextData};
use vector_scene::{NodeData, NodeId, Scene, Style};

/// Import an SVG file from bytes into a scene graph.
pub fn import_svg(data: &[u8]) -> Result<Scene, ImportError> {
    let mut opts = usvg::Options::default();
    // Load system fonts so usvg can shape text nodes. Without this,
    // text elements are silently dropped during parsing.
    opts.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_data(data, &opts).map_err(ImportError::Parse)?;

    let mut scene = Scene::new();
    let root = scene.root();

    // Cache for gradient deduplication. usvg shares `Arc<LinearGradient>` /
    // `Arc<RadialGradient>` across paths that reference the same SVG element.
    // We key by the Arc's raw pointer to avoid creating duplicate paint nodes.
    let mut gradient_cache: HashMap<usize, NodeId> = HashMap::new();

    import_group(tree.root(), &mut scene, root, &mut gradient_cache);

    Ok(scene)
}

/// Returns true if a usvg Group carries no meaningful attributes beyond
/// being a container — i.e. identity transform, full opacity, normal blend,
/// no clip/mask/filters, and no user-assigned ID.
fn is_trivial_group(g: &usvg::Group) -> bool {
    g.transform() == usvg::Transform::identity()
        && g.opacity() == usvg::Opacity::ONE
        && g.blend_mode() == usvg::BlendMode::Normal
        && g.clip_path().is_none()
        && g.mask().is_none()
        && g.filters().is_empty()
        && g.id().is_empty()
}

fn import_group(
    group: &usvg::Group,
    scene: &mut Scene,
    parent: NodeId,
    gradient_cache: &mut HashMap<usize, NodeId>,
) {
    for child in group.children() {
        match child {
            usvg::Node::Group(g) => {
                if is_trivial_group(g) {
                    // Skip trivial wrapper groups — import children directly
                    // into the current parent.
                    import_group(g, scene, parent, gradient_cache);
                } else {
                    let mut node = Node::group(g.id().to_string());
                    // Preserve the group's transform.
                    let ut = g.transform();
                    node.transform = Affine {
                        a: ut.sx as f64,
                        b: ut.kx as f64,
                        c: ut.ky as f64,
                        d: ut.sy as f64,
                        tx: ut.tx as f64,
                        ty: ut.ty as f64,
                    };
                    if let Some(id) = scene.insert(parent, node) {
                        import_group(g, scene, id, gradient_cache);
                    }
                }
            }
            usvg::Node::Path(p) => {
                let path = convert_path(p.data());
                let style = convert_style(p, scene, gradient_cache);
                let mut node = Node::path(p.id().to_string(), path);
                if let NodeData::Path { style: s, .. } = &mut node.data {
                    *s = style;
                }
                scene.insert(parent, node);
            }
            usvg::Node::Image(_) => {
                log::warn!("Image nodes not yet supported, skipping");
            }
            usvg::Node::Text(t) => {
                let text_node = convert_text(t);
                scene.insert(parent, text_node);
            }
        }
    }
}

fn convert_path(data: &usvg::tiny_skia_path::Path) -> Path {
    let mut path = Path::new();
    let mut current_subpath: Option<SubPath> = None;

    for seg in data.segments() {
        match seg {
            usvg::tiny_skia_path::PathSegment::MoveTo(pt) => {
                if let Some(sp) = current_subpath.take() {
                    path.subpaths.push(sp);
                }
                current_subpath = Some(SubPath::new(Point::new(pt.x as f64, pt.y as f64)));
            }
            usvg::tiny_skia_path::PathSegment::LineTo(pt) => {
                if let Some(sp) = &mut current_subpath {
                    sp.push_segment(
                        Segment::Line {
                            to: Point::new(pt.x as f64, pt.y as f64),
                        },
                        VertexMode::Corner,
                    );
                }
            }
            usvg::tiny_skia_path::PathSegment::QuadTo(ctrl, to) => {
                if let Some(sp) = &mut current_subpath {
                    sp.push_segment(
                        Segment::Quad {
                            ctrl: Point::new(ctrl.x as f64, ctrl.y as f64),
                            to: Point::new(to.x as f64, to.y as f64),
                        },
                        VertexMode::Corner,
                    );
                }
            }
            usvg::tiny_skia_path::PathSegment::CubicTo(ctrl1, ctrl2, to) => {
                if let Some(sp) = &mut current_subpath {
                    sp.push_segment(
                        Segment::Cubic {
                            ctrl1: Point::new(ctrl1.x as f64, ctrl1.y as f64),
                            ctrl2: Point::new(ctrl2.x as f64, ctrl2.y as f64),
                            to: Point::new(to.x as f64, to.y as f64),
                        },
                        VertexMode::Corner,
                    );
                }
            }
            usvg::tiny_skia_path::PathSegment::Close => {
                if let Some(sp) = &mut current_subpath {
                    sp.closed = true;
                }
                if let Some(sp) = current_subpath.take() {
                    path.subpaths.push(sp);
                }
            }
        }
    }

    if let Some(sp) = current_subpath {
        path.subpaths.push(sp);
    }

    path
}

fn convert_style(
    path: &usvg::Path,
    scene: &mut Scene,
    gradient_cache: &mut HashMap<usize, NodeId>,
) -> Style {
    let fill = path.fill().map(|f| {
        let paint = convert_paint(f.paint(), scene, gradient_cache);
        vector_scene::style::Fill {
            paint,
            rule: match f.rule() {
                usvg::FillRule::NonZero => vector_scene::FillRule::NonZero,
                usvg::FillRule::EvenOdd => vector_scene::FillRule::EvenOdd,
            },
            opacity: f.opacity().get(),
        }
    });

    let stroke = path.stroke().map(|s| {
        let paint = convert_paint(s.paint(), scene, gradient_cache);
        vector_scene::style::Stroke {
            paint,
            style: vector_scene::StrokeStyle {
                width: s.width().get() as f64,
                cap: match s.linecap() {
                    usvg::LineCap::Butt => vector_scene::LineCap::Butt,
                    usvg::LineCap::Round => vector_scene::LineCap::Round,
                    usvg::LineCap::Square => vector_scene::LineCap::Square,
                },
                join: match s.linejoin() {
                    usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => {
                        vector_scene::LineJoin::Miter
                    }
                    usvg::LineJoin::Round => vector_scene::LineJoin::Round,
                    usvg::LineJoin::Bevel => vector_scene::LineJoin::Bevel,
                },
                miter_limit: s.miterlimit().get() as f64,
                dash: s
                    .dasharray()
                    .as_ref()
                    .map(|d| vector_scene::style::DashPattern {
                        array: d.iter().map(|v| *v as f64).collect(),
                        offset: s.dashoffset() as f64,
                    }),
            },
            opacity: s.opacity().get(),
        }
    });

    Style { fill, stroke }
}

/// Convert a usvg `Text` node into our `Node` with `TextData`.
///
/// Extracts font info from the first span of the first chunk. Concatenates
/// all chunk text into a single string. The node's transform is taken from
/// usvg's absolute transform, and the text position from chunk x/y.
fn convert_text(t: &usvg::Text) -> Node {
    // Concatenate all chunk text.
    let content: String = t.chunks().iter().map(|c| c.text().to_string()).collect();

    // Extract font info from the first span of the first chunk (fallback defaults).
    let (font_family, font_size) = t
        .chunks()
        .first()
        .and_then(|c| c.spans().first())
        .map(|span| {
            let family = span
                .font()
                .families()
                .first()
                .map(|f| match f {
                    usvg::FontFamily::Named(name) => name.to_string(),
                    usvg::FontFamily::Serif => "serif".to_string(),
                    usvg::FontFamily::SansSerif => "sans-serif".to_string(),
                    usvg::FontFamily::Cursive => "cursive".to_string(),
                    usvg::FontFamily::Fantasy => "fantasy".to_string(),
                    usvg::FontFamily::Monospace => "monospace".to_string(),
                })
                .unwrap_or_else(|| "sans-serif".to_string());
            let size = span.font_size().get() as f64;
            (family, size)
        })
        .unwrap_or_else(|| ("sans-serif".to_string(), 16.0));

    // Extract fill from the first span (text often has per-span fill).
    let fill = t
        .chunks()
        .first()
        .and_then(|c| c.spans().first())
        .and_then(|span| {
            span.fill().map(|f| {
                let paint = match f.paint() {
                    usvg::Paint::Color(c) => vector_scene::PaintRef::Solid(Color::from_srgb8(
                        c.red, c.green, c.blue, 255,
                    )),
                    _ => vector_scene::PaintRef::Solid(Color::BLACK),
                };
                vector_scene::style::Fill {
                    paint,
                    rule: vector_scene::FillRule::NonZero,
                    opacity: f.opacity().get(),
                }
            })
        })
        .unwrap_or(vector_scene::style::Fill {
            paint: vector_scene::PaintRef::Solid(Color::BLACK),
            rule: vector_scene::FillRule::NonZero,
            opacity: 1.0,
        });

    let style = Style {
        fill: Some(fill),
        stroke: None,
    };

    // Convert usvg's absolute transform to our Affine. The text position
    // (chunk x/y) is baked into the transform by usvg.
    let ut = t.abs_transform();
    let transform = Affine {
        a: ut.sx as f64,
        b: ut.kx as f64,
        c: ut.ky as f64,
        d: ut.sy as f64,
        tx: ut.tx as f64,
        ty: ut.ty as f64,
    };

    Node {
        label: t.id().to_string(),
        transform,
        data: NodeData::Text(TextData {
            content,
            font_family,
            font_size,
            style,
        }),
        children: Vec::new(),
        visible: true,
        locked: false,
    }
}

/// Convert a usvg `Paint` into our `PaintRef`, creating gradient nodes in
/// the scene's defs subtree as needed.
fn convert_paint(
    paint: &usvg::Paint,
    scene: &mut Scene,
    gradient_cache: &mut HashMap<usize, NodeId>,
) -> vector_scene::PaintRef {
    match paint {
        usvg::Paint::Color(c) => {
            vector_scene::PaintRef::Solid(Color::from_srgb8(c.red, c.green, c.blue, 255))
        }
        usvg::Paint::LinearGradient(lg) => {
            // Dedup by Arc pointer identity.
            let ptr = std::sync::Arc::as_ptr(lg) as usize;
            if let Some(&node_id) = gradient_cache.get(&ptr) {
                return vector_scene::PaintRef::Ref(node_id);
            }

            let gradient = vector_scene::Gradient {
                kind: vector_scene::GradientKind::Linear {
                    start: Point::new(lg.x1() as f64, lg.y1() as f64),
                    end: Point::new(lg.x2() as f64, lg.y2() as f64),
                },
                stops: convert_stops(lg.stops()),
                interpolation: vector_scene::InterpolationSpace::LinearRgb,
                transform: convert_transform(lg.transform()),
                spread: convert_spread(lg.spread_method()),
            };
            let label = if lg.id().is_empty() {
                format!("linearGradient_{}", gradient_cache.len())
            } else {
                lg.id().to_string()
            };
            let node = vector_scene::Node {
                label,
                transform: Affine::IDENTITY,
                data: NodeData::Paint(vector_scene::Paint::Gradient(gradient)),
                children: Vec::new(),
                visible: true,
                locked: false,
            };
            let defs = scene.defs();
            if let Some(id) = scene.insert(defs, node) {
                gradient_cache.insert(ptr, id);
                vector_scene::PaintRef::Ref(id)
            } else {
                log::warn!("Failed to insert linear gradient into defs");
                vector_scene::PaintRef::Solid(Color::BLACK)
            }
        }
        usvg::Paint::RadialGradient(rg) => {
            let ptr = std::sync::Arc::as_ptr(rg) as usize;
            if let Some(&node_id) = gradient_cache.get(&ptr) {
                return vector_scene::PaintRef::Ref(node_id);
            }

            let gradient = vector_scene::Gradient {
                kind: vector_scene::GradientKind::Radial {
                    center: Point::new(rg.cx() as f64, rg.cy() as f64),
                    radius: rg.r().get() as f64,
                    focal: Point::new(rg.fx() as f64, rg.fy() as f64),
                    focal_radius: rg.fr().get() as f64,
                },
                stops: convert_stops(rg.stops()),
                interpolation: vector_scene::InterpolationSpace::LinearRgb,
                transform: convert_transform(rg.transform()),
                spread: convert_spread(rg.spread_method()),
            };
            let label = if rg.id().is_empty() {
                format!("radialGradient_{}", gradient_cache.len())
            } else {
                rg.id().to_string()
            };
            let node = vector_scene::Node {
                label,
                transform: Affine::IDENTITY,
                data: NodeData::Paint(vector_scene::Paint::Gradient(gradient)),
                children: Vec::new(),
                visible: true,
                locked: false,
            };
            let defs = scene.defs();
            if let Some(id) = scene.insert(defs, node) {
                gradient_cache.insert(ptr, id);
                vector_scene::PaintRef::Ref(id)
            } else {
                log::warn!("Failed to insert radial gradient into defs");
                vector_scene::PaintRef::Solid(Color::BLACK)
            }
        }
        usvg::Paint::Pattern(pat) => {
            // Dedup by Arc pointer identity (same as gradients).
            let ptr = std::sync::Arc::as_ptr(pat) as usize;
            if let Some(&node_id) = gradient_cache.get(&ptr) {
                return vector_scene::PaintRef::Ref(node_id);
            }

            // Import pattern content as a group in defs.
            let content_label = if pat.id().is_empty() {
                format!("pattern_content_{}", gradient_cache.len())
            } else {
                format!("{}_content", pat.id())
            };
            let content_group = Node::group(content_label);
            let defs = scene.defs();
            let Some(content_id) = scene.insert(defs, content_group) else {
                log::warn!("Failed to insert pattern content group into defs");
                return vector_scene::PaintRef::Solid(Color::BLACK);
            };

            // Recursively import the pattern's children into the content group.
            import_group(pat.root(), scene, content_id, gradient_cache);

            // Create the Pattern paint node.
            let rect = pat.rect();
            let pattern = vector_scene::Pattern {
                rect: [
                    rect.x() as f64,
                    rect.y() as f64,
                    rect.width() as f64,
                    rect.height() as f64,
                ],
                transform: convert_transform(pat.transform()),
                content: content_id,
            };
            let label = if pat.id().is_empty() {
                format!("pattern_{}", gradient_cache.len())
            } else {
                pat.id().to_string()
            };
            let paint_node = Node {
                label,
                transform: Affine::IDENTITY,
                data: NodeData::Paint(vector_scene::Paint::Pattern(pattern)),
                children: Vec::new(),
                visible: true,
                locked: false,
            };
            let Some(paint_id) = scene.insert(defs, paint_node) else {
                log::warn!("Failed to insert pattern paint node into defs");
                return vector_scene::PaintRef::Solid(Color::BLACK);
            };

            gradient_cache.insert(ptr, paint_id);
            vector_scene::PaintRef::Ref(paint_id)
        }
    }
}

/// Convert usvg gradient stops to our `ColorStop` representation.
fn convert_stops(stops: &[usvg::Stop]) -> Vec<vector_scene::ColorStop> {
    stops
        .iter()
        .map(|s| {
            let c = s.color();
            let opacity = s.opacity().get();
            // Convert sRGB stop color to linear RGBA, pre-multiplied by stop opacity.
            let base = Color::from_srgb8(c.red, c.green, c.blue, 255);
            vector_scene::ColorStop {
                offset: s.offset().get(),
                color: Color::new(base.r, base.g, base.b, base.a * opacity),
            }
        })
        .collect()
}

/// Convert a usvg `Transform` to our `Affine`.
/// usvg: `(sx, ky, kx, sy, tx, ty)` — our Affine: `{ a, b, c, d, tx, ty }`
/// where matrix is [[a, b, tx], [c, d, ty], [0, 0, 1]].
fn convert_transform(t: usvg::Transform) -> Affine {
    Affine {
        a: t.sx as f64,
        b: t.kx as f64,
        c: t.ky as f64,
        d: t.sy as f64,
        tx: t.tx as f64,
        ty: t.ty as f64,
    }
}

fn convert_spread(s: usvg::SpreadMethod) -> vector_scene::SpreadMethod {
    match s {
        usvg::SpreadMethod::Pad => vector_scene::SpreadMethod::Pad,
        usvg::SpreadMethod::Reflect => vector_scene::SpreadMethod::Reflect,
        usvg::SpreadMethod::Repeat => vector_scene::SpreadMethod::Repeat,
    }
}

#[derive(Debug)]
pub enum ImportError {
    Parse(usvg::Error),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Parse(e) => write!(f, "SVG parse error: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_linear_gradient() {
        let svg = r#"
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 200">
                <defs>
                    <linearGradient id="lg1" x1="0" y1="0" x2="200" y2="0">
                        <stop offset="0" stop-color="red"/>
                        <stop offset="1" stop-color="blue"/>
                    </linearGradient>
                </defs>
                <rect x="0" y="0" width="200" height="200" fill="url(#lg1)"/>
            </svg>
        "#;
        let scene = import_svg(svg.as_bytes()).unwrap();

        // There should be at least one gradient node in defs.
        let defs = scene.defs();
        let defs_node = scene.get(defs).unwrap();
        let gradient_children: Vec<_> = defs_node
            .children
            .iter()
            .filter_map(|&id| {
                let node = scene.get(id)?;
                if let NodeData::Paint(vector_scene::Paint::Gradient(_)) = &node.data {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            !gradient_children.is_empty(),
            "should have gradient in defs"
        );

        // The rect path should reference the gradient via PaintRef::Ref.
        let root = scene.root();
        let root_node = scene.get(root).unwrap();
        let has_gradient_ref = root_node.children.iter().any(|&id| {
            let node = scene.get(id).unwrap();
            if let NodeData::Path { style, .. } = &node.data {
                matches!(&style.fill, Some(f) if matches!(&f.paint, vector_scene::PaintRef::Ref(_)))
            } else {
                false
            }
        });
        assert!(
            has_gradient_ref,
            "path should reference gradient via PaintRef::Ref"
        );
    }

    #[test]
    fn import_radial_gradient() {
        let svg = r#"
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 200">
                <defs>
                    <radialGradient id="rg1" cx="100" cy="100" r="100">
                        <stop offset="0" stop-color="white"/>
                        <stop offset="1" stop-color="black"/>
                    </radialGradient>
                </defs>
                <circle cx="100" cy="100" r="100" fill="url(#rg1)"/>
            </svg>
        "#;
        let scene = import_svg(svg.as_bytes()).unwrap();

        // Check gradient exists in defs.
        let defs = scene.defs();
        let defs_node = scene.get(defs).unwrap();
        let grad_id = defs_node.children.iter().find_map(|&id| {
            let node = scene.get(id)?;
            if let NodeData::Paint(vector_scene::Paint::Gradient(g)) = &node.data
                && matches!(g.kind, vector_scene::GradientKind::Radial { .. })
            {
                return Some(id);
            }
            None
        });
        assert!(grad_id.is_some(), "should have radial gradient in defs");
    }

    #[test]
    fn gradient_ref_from_two_paths() {
        // Two paths referencing the same gradient should both get PaintRef::Ref.
        let svg = r#"
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 200">
                <defs>
                    <linearGradient id="lg1" x1="0" y1="0" x2="200" y2="0">
                        <stop offset="0" stop-color="red"/>
                        <stop offset="1" stop-color="blue"/>
                    </linearGradient>
                </defs>
                <rect x="0" y="0" width="100" height="100" fill="url(#lg1)"/>
                <rect x="100" y="0" width="100" height="100" fill="url(#lg1)"/>
            </svg>
        "#;
        let scene = import_svg(svg.as_bytes()).unwrap();

        // Both paths should reference a gradient (not fall back to solid black).
        let root = scene.root();
        let root_node = scene.get(root).unwrap();
        let gradient_ref_count = root_node
            .children
            .iter()
            .filter(|&&id| {
                if let Some(node) = scene.get(id)
                    && let NodeData::Path { style, .. } = &node.data
                {
                    matches!(&style.fill, Some(f) if matches!(&f.paint, vector_scene::PaintRef::Ref(_)))
                } else {
                    false
                }
            })
            .count();
        assert_eq!(
            gradient_ref_count, 2,
            "both paths should reference gradient via PaintRef::Ref"
        );
    }

    #[test]
    fn import_pattern() {
        let svg = r#"
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 200">
                <defs>
                    <pattern id="dots" x="0" y="0" width="20" height="20" patternUnits="userSpaceOnUse">
                        <circle cx="10" cy="10" r="5" fill="red"/>
                    </pattern>
                </defs>
                <rect x="0" y="0" width="200" height="200" fill="url(#dots)"/>
            </svg>
        "#;
        let scene = import_svg(svg.as_bytes()).unwrap();

        // There should be a pattern paint node in defs.
        let defs = scene.defs();
        let defs_node = scene.get(defs).unwrap();
        let pattern_id = defs_node.children.iter().find_map(|&id| {
            let node = scene.get(id)?;
            if let NodeData::Paint(vector_scene::Paint::Pattern(p)) = &node.data {
                // Verify the tile rect is correct.
                assert!(
                    (p.rect[2] - 20.0).abs() < 1e-4,
                    "pattern width should be 20"
                );
                assert!(
                    (p.rect[3] - 20.0).abs() < 1e-4,
                    "pattern height should be 20"
                );
                // The content group should exist and have children.
                let content = scene.get(p.content)?;
                assert!(
                    !content.children.is_empty(),
                    "pattern content group should have children"
                );
                Some(id)
            } else {
                None
            }
        });
        assert!(
            pattern_id.is_some(),
            "should have pattern paint node in defs"
        );

        // The rect path should reference the pattern via PaintRef::Ref.
        let root = scene.root();
        let root_node = scene.get(root).unwrap();
        let has_pattern_ref = root_node.children.iter().any(|&id| {
            let node = scene.get(id).unwrap();
            if let NodeData::Path { style, .. } = &node.data {
                matches!(&style.fill, Some(f) if matches!(&f.paint, vector_scene::PaintRef::Ref(_)))
            } else {
                false
            }
        });
        assert!(
            has_pattern_ref,
            "path should reference pattern via PaintRef::Ref"
        );
    }

    #[test]
    fn import_pattern_dedup() {
        // Two paths using the same pattern should share a single paint node.
        let svg = r#"
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 200">
                <defs>
                    <pattern id="stripes" width="10" height="10" patternUnits="userSpaceOnUse">
                        <rect width="5" height="10" fill="blue"/>
                    </pattern>
                </defs>
                <rect x="0" y="0" width="100" height="100" fill="url(#stripes)"/>
                <rect x="100" y="0" width="100" height="100" fill="url(#stripes)"/>
            </svg>
        "#;
        let scene = import_svg(svg.as_bytes()).unwrap();

        // Count pattern paint nodes in defs.
        let defs = scene.defs();
        let defs_node = scene.get(defs).unwrap();
        let pattern_count = defs_node
            .children
            .iter()
            .filter(|&&id| {
                scene.get(id).is_some_and(|n| {
                    matches!(&n.data, NodeData::Paint(vector_scene::Paint::Pattern(_)))
                })
            })
            .count();
        assert_eq!(
            pattern_count, 1,
            "should deduplicate to a single pattern paint node"
        );

        // Both paths should reference via PaintRef::Ref.
        let root = scene.root();
        let root_node = scene.get(root).unwrap();
        let ref_count = root_node
            .children
            .iter()
            .filter(|&&id| {
                scene.get(id).is_some_and(|n| {
                    if let NodeData::Path { style, .. } = &n.data {
                        matches!(&style.fill, Some(f) if matches!(&f.paint, vector_scene::PaintRef::Ref(_)))
                    } else {
                        false
                    }
                })
            })
            .count();
        assert_eq!(ref_count, 2, "both paths should reference the pattern");
    }

    #[test]
    fn import_text_node() {
        let svg = r#"
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 200">
                <text x="10" y="50" font-family="Arial" font-size="24" fill="red">Hello World</text>
            </svg>
        "#;
        let scene = import_svg(svg.as_bytes()).unwrap();

        // Find the text node.
        let root = scene.root();
        let root_node = scene.get(root).unwrap();
        let text_node = root_node.children.iter().find_map(|&id| {
            let node = scene.get(id)?;
            if let NodeData::Text(ref text) = node.data {
                Some(text.clone())
            } else {
                None
            }
        });

        assert!(text_node.is_some(), "should have a text node");
        let text = text_node.unwrap();
        assert_eq!(text.content, "Hello World");
        assert_eq!(text.font_size, 24.0);
        // Fill should be red (or close to it).
        assert!(text.style.fill.is_some(), "text should have a fill");
    }
}
