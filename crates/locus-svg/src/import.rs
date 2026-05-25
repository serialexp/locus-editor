use std::collections::HashMap;

use locus_geom::{Affine, Bounds, Color, Path, Point, Segment, SubPath, VertexMode};
use locus_scene::{Node, TextData};
use locus_scene::{NodeData, NodeId, Scene, Style};

/// Import an SVG file from bytes into a scene graph.
pub fn import_svg(data: &[u8]) -> Result<Scene, ImportError> {
    let mut opts = usvg::Options::default();
    // Load the bundled fallback font so text parsing works even without
    // system fonts (e.g. on CI). Also load system fonts for better coverage.
    opts.fontdb_mut()
        .load_font_data(locus_text::DEFAULT_FONT_DATA.to_vec());
    opts.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_data(data, &opts).map_err(ImportError::Parse)?;

    let mut scene = Scene::new();
    let root = scene.root();

    // Cache for gradient deduplication. usvg shares `Arc<LinearGradient>` /
    // `Arc<RadialGradient>` across paths that reference the same SVG element.
    // We key by the Arc's raw pointer to avoid creating duplicate paint nodes.
    let mut gradient_cache: HashMap<usize, NodeId> = HashMap::new();

    // usvg normalises the SVG so that `tree.root()` always sees a viewBox
    // anchored at (0, 0). If the source file's viewBox had a non-zero origin
    // (which our own exporter produces whenever content doesn't start at
    // origin), usvg compensates by wrapping the entire document in a
    // synthetic `<g transform="translate(-vbX, -vbY)">`. That synthetic group
    // has no semantic meaning — it would just accumulate as a ghost wrapper
    // around the scene every round-trip. Strip it here, baking its translate
    // into the children we import.
    let (source, pre_transform) = unwrap_viewbox_normalizer(tree.root());

    // Reconstruct the original viewBox: the synthetic wrapper's translate is
    // (-vbX, -vbY), and the tree's size is the viewBox's width × height.
    // When no wrapper was present, the viewBox was at origin.
    let size = tree.size();
    let (vbx, vby) = if pre_transform.is_identity() {
        (0.0, 0.0)
    } else {
        (-pre_transform.tx, -pre_transform.ty)
    };
    scene.set_view_box(Bounds::new(
        Point::new(vbx, vby),
        Point::new(vbx + size.width() as f64, vby + size.height() as f64),
    ));

    import_group(source, &mut scene, root, &mut gradient_cache, pre_transform);

    Ok(scene)
}

/// Detect the synthetic top-level wrapper that usvg injects when an SVG's
/// viewBox doesn't start at the origin. The wrapper is the sole child of
/// `tree.root()`, has no id, no clip/mask/filter/opacity/blend, and a
/// transform that's pure translate (no rotation or scale). When found,
/// returns the wrapper's *contents* together with the translate that needs
/// to be applied to each child to preserve world position. Otherwise returns
/// `(root, IDENTITY)` and import proceeds normally.
fn unwrap_viewbox_normalizer(root: &usvg::Group) -> (&usvg::Group, Affine) {
    if root.children().len() != 1 {
        return (root, Affine::IDENTITY);
    }
    let usvg::Node::Group(g) = &root.children()[0] else {
        return (root, Affine::IDENTITY);
    };
    let only_translate = g.transform().sx == 1.0
        && g.transform().sy == 1.0
        && g.transform().kx == 0.0
        && g.transform().ky == 0.0;
    let unadorned = g.id().is_empty()
        && g.opacity() == usvg::Opacity::ONE
        && g.blend_mode() == usvg::BlendMode::Normal
        && g.clip_path().is_none()
        && g.mask().is_none()
        && g.filters().is_empty();
    if only_translate && unadorned {
        let t = Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: g.transform().tx as f64,
            ty: g.transform().ty as f64,
        };
        (g, t)
    } else {
        (root, Affine::IDENTITY)
    }
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
    pre_transform: Affine,
) {
    for child in group.children() {
        match child {
            usvg::Node::Group(g) => {
                if is_trivial_group(g) {
                    // Skip trivial wrapper groups — import children directly
                    // into the current parent. The pre-transform propagates
                    // through transparent wrappers untouched.
                    import_group(g, scene, parent, gradient_cache, pre_transform);
                } else {
                    let mut node = Node::group(g.id().to_string());
                    // Preserve the group's transform, composed with any
                    // pre-transform from an absorbed wrapper above. The
                    // pre-transform applies to *this* group's world position,
                    // so it goes on the outside: world = pre * g.transform.
                    let ut = g.transform();
                    let g_t = Affine {
                        a: ut.sx as f64,
                        b: ut.kx as f64,
                        c: ut.ky as f64,
                        d: ut.sy as f64,
                        tx: ut.tx as f64,
                        ty: ut.ty as f64,
                    };
                    node.transform = if pre_transform.is_identity() {
                        g_t
                    } else {
                        pre_transform.then(g_t)
                    };
                    if let Some(id) = scene.insert(parent, node) {
                        // Children inherit the group's transform naturally
                        // via the scene graph; no further pre-transform.
                        import_group(g, scene, id, gradient_cache, Affine::IDENTITY);
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
                if !pre_transform.is_identity() {
                    node.transform = pre_transform.then(node.transform);
                }
                scene.insert(parent, node);
            }
            usvg::Node::Image(_) => {
                log::warn!("Image nodes not yet supported, skipping");
            }
            usvg::Node::Text(t) => {
                let mut text_node = convert_text(t);
                if !pre_transform.is_identity() {
                    text_node.transform = pre_transform.then(text_node.transform);
                }
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

    // Post-process: detect smooth/symmetric junctions from control point geometry.
    for sp in &mut path.subpaths {
        detect_smooth_junctions(sp);
    }

    path
}

/// Tolerance for the cross product test: handles are considered collinear
/// when |cross| < COLLINEAR_EPS * (|a| * |b|).  This is the sine of the
/// angle between them — 1e-3 is ≈ 0.06°.
const COLLINEAR_EPS: f64 = 1e-3;

/// Tolerance for comparing handle lengths for symmetry detection.
/// Two handles are "equidistant" when their lengths differ by less than
/// this fraction of the longer one.
const SYMMETRIC_EPS: f64 = 1e-3;

/// Examine each anchor vertex in a subpath and upgrade its `VertexMode`
/// from `Corner` to `Smooth` or `Symmetric` when the incoming and outgoing
/// control-point handles are collinear through the anchor.
///
/// For cubic segments this checks `ctrl2` (incoming) and `ctrl1` (outgoing).
/// Quad segments use their single `ctrl` as both the incoming and outgoing
/// handle on their respective sides of the junction.
///
/// On a closed subpath, the start↔last junction is also checked.
fn detect_smooth_junctions(sp: &mut SubPath) {
    let n = sp.segments.len();
    if n == 0 {
        return;
    }

    // Number of anchor vertices: n+1 (start + each segment endpoint).
    // vertex_modes[0] = start, vertex_modes[i+1] = endpoint of segment i.
    //
    // For each anchor we need the incoming handle vector (from the previous
    // segment) and the outgoing handle vector (from the next segment).

    // Check interior junctions: vertex_modes[1] through vertex_modes[n-1].
    // These are endpoints of segment[i-1] and start points of segment[i].
    for i in 1..n {
        let anchor = sp.segments[i - 1].endpoint();
        let incoming = incoming_handle_vec(&sp.segments[i - 1], anchor);
        let outgoing = outgoing_handle_vec(&sp.segments[i], anchor);
        if let Some(mode) = classify_junction(incoming, outgoing) {
            sp.vertex_modes[i] = mode;
        }
    }

    // For closed paths, also check the start↔last junction (vertex_modes[0])
    // and the last-endpoint↔start junction (vertex_modes[n]).
    if sp.closed && n > 0 {
        // vertex_modes[0]: anchor = start, incoming = last segment, outgoing = first segment.
        let anchor = sp.start;
        let incoming = incoming_handle_vec(&sp.segments[n - 1], anchor);
        let outgoing = outgoing_handle_vec(&sp.segments[0], anchor);
        if let Some(mode) = classify_junction(incoming, outgoing) {
            sp.vertex_modes[0] = mode;
        }

        // vertex_modes[n]: anchor = last segment endpoint.
        // This is the same geometric point as start on a closed path,
        // but it's a separate vertex-mode entry.
        // incoming = last segment, outgoing = first segment (same pair).
        // So vertex_modes[n] gets the same classification as vertex_modes[0].
        let anchor_last = sp.segments[n - 1].endpoint();
        let incoming_last = incoming_handle_vec(&sp.segments[n - 1], anchor_last);
        let outgoing_last = outgoing_handle_vec(&sp.segments[0], anchor_last);
        if let Some(mode) = classify_junction(incoming_last, outgoing_last) {
            sp.vertex_modes[n] = mode;
        }
    }
}

/// Compute the incoming handle vector for a segment, pointing from the
/// segment's endpoint (the anchor) towards the last control point.
/// Returns `None` if the segment has no handle (Line, Arc).
fn incoming_handle_vec(seg: &Segment, anchor: Point) -> Option<[f64; 2]> {
    match seg {
        Segment::Cubic { ctrl2, .. } => {
            let dx = ctrl2.x - anchor.x;
            let dy = ctrl2.y - anchor.y;
            // Degenerate handle (sitting on anchor) — treat as no handle.
            if dx.abs() < 1e-12 && dy.abs() < 1e-12 {
                None
            } else {
                Some([dx, dy])
            }
        }
        Segment::Quad { ctrl, .. } => {
            let dx = ctrl.x - anchor.x;
            let dy = ctrl.y - anchor.y;
            if dx.abs() < 1e-12 && dy.abs() < 1e-12 {
                None
            } else {
                Some([dx, dy])
            }
        }
        _ => None,
    }
}

/// Compute the outgoing handle vector for a segment, pointing from the
/// segment's start point (the anchor) towards the first control point.
/// Returns `None` if the segment has no handle (Line, Arc).
fn outgoing_handle_vec(seg: &Segment, anchor: Point) -> Option<[f64; 2]> {
    match seg {
        Segment::Cubic { ctrl1, .. } => {
            let dx = ctrl1.x - anchor.x;
            let dy = ctrl1.y - anchor.y;
            if dx.abs() < 1e-12 && dy.abs() < 1e-12 {
                None
            } else {
                Some([dx, dy])
            }
        }
        Segment::Quad { ctrl, .. } => {
            let dx = ctrl.x - anchor.x;
            let dy = ctrl.y - anchor.y;
            if dx.abs() < 1e-12 && dy.abs() < 1e-12 {
                None
            } else {
                Some([dx, dy])
            }
        }
        _ => None,
    }
}

/// Given two handle vectors (incoming and outgoing, both relative to the
/// anchor), determine whether they form a smooth or symmetric junction.
///
/// - **Collinear + equidistant** → `Symmetric`
/// - **Collinear only** → `Smooth`
/// - Otherwise → `None` (stays `Corner`)
fn classify_junction(incoming: Option<[f64; 2]>, outgoing: Option<[f64; 2]>) -> Option<VertexMode> {
    let (inc, out) = match (incoming, outgoing) {
        (Some(a), Some(b)) => (a, b),
        _ => return None,
    };

    let len_inc = (inc[0] * inc[0] + inc[1] * inc[1]).sqrt();
    let len_out = (out[0] * out[0] + out[1] * out[1]).sqrt();

    // Cross product — zero when collinear.
    let cross = inc[0] * out[1] - inc[1] * out[0];

    // Normalize cross product by the product of lengths to get sine of angle.
    let denom = len_inc * len_out;
    if denom < 1e-12 {
        return None;
    }
    let sin_angle = (cross / denom).abs();
    if sin_angle > COLLINEAR_EPS {
        return None; // Not collinear.
    }

    // Collinear — but they must point in opposite directions (dot < 0).
    // If they point in the same direction, it's not a smooth junction (the
    // curve doubles back on itself).
    let dot = inc[0] * out[0] + inc[1] * out[1];
    if dot >= 0.0 {
        return None;
    }

    // Check equidistance for symmetric classification.
    let max_len = len_inc.max(len_out);
    if (len_inc - len_out).abs() / max_len < SYMMETRIC_EPS {
        Some(VertexMode::Symmetric)
    } else {
        Some(VertexMode::Smooth)
    }
}

fn convert_style(
    path: &usvg::Path,
    scene: &mut Scene,
    gradient_cache: &mut HashMap<usize, NodeId>,
) -> Style {
    let fill = path.fill().map(|f| {
        let paint = convert_paint(f.paint(), scene, gradient_cache);
        locus_scene::style::Fill {
            paint,
            rule: match f.rule() {
                usvg::FillRule::NonZero => locus_scene::FillRule::NonZero,
                usvg::FillRule::EvenOdd => locus_scene::FillRule::EvenOdd,
            },
            opacity: f.opacity().get(),
        }
    });

    let stroke = path.stroke().map(|s| {
        let paint = convert_paint(s.paint(), scene, gradient_cache);
        locus_scene::style::Stroke {
            paint,
            style: locus_scene::StrokeStyle {
                width: s.width().get() as f64,
                cap: match s.linecap() {
                    usvg::LineCap::Butt => locus_scene::LineCap::Butt,
                    usvg::LineCap::Round => locus_scene::LineCap::Round,
                    usvg::LineCap::Square => locus_scene::LineCap::Square,
                },
                join: match s.linejoin() {
                    usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => {
                        locus_scene::LineJoin::Miter
                    }
                    usvg::LineJoin::Round => locus_scene::LineJoin::Round,
                    usvg::LineJoin::Bevel => locus_scene::LineJoin::Bevel,
                },
                miter_limit: s.miterlimit().get() as f64,
                dash: s
                    .dasharray()
                    .as_ref()
                    .map(|d| locus_scene::style::DashPattern {
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
                    usvg::Paint::Color(c) => {
                        locus_scene::PaintRef::Solid(Color::from_srgb8(c.red, c.green, c.blue, 255))
                    }
                    _ => locus_scene::PaintRef::Solid(Color::BLACK),
                };
                locus_scene::style::Fill {
                    paint,
                    rule: locus_scene::FillRule::NonZero,
                    opacity: f.opacity().get(),
                }
            })
        })
        .unwrap_or(locus_scene::style::Fill {
            paint: locus_scene::PaintRef::Solid(Color::BLACK),
            rule: locus_scene::FillRule::NonZero,
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
) -> locus_scene::PaintRef {
    match paint {
        usvg::Paint::Color(c) => {
            locus_scene::PaintRef::Solid(Color::from_srgb8(c.red, c.green, c.blue, 255))
        }
        usvg::Paint::LinearGradient(lg) => {
            // Dedup by Arc pointer identity.
            let ptr = std::sync::Arc::as_ptr(lg) as usize;
            if let Some(&node_id) = gradient_cache.get(&ptr) {
                return locus_scene::PaintRef::Ref(node_id);
            }

            let gradient = locus_scene::Gradient {
                kind: locus_scene::GradientKind::Linear {
                    start: Point::new(lg.x1() as f64, lg.y1() as f64),
                    end: Point::new(lg.x2() as f64, lg.y2() as f64),
                },
                stops: convert_stops(lg.stops()),
                interpolation: locus_scene::InterpolationSpace::LinearRgb,
                transform: convert_transform(lg.transform()),
                spread: convert_spread(lg.spread_method()),
            };
            let label = if lg.id().is_empty() {
                format!("linearGradient_{}", gradient_cache.len())
            } else {
                lg.id().to_string()
            };
            let node = locus_scene::Node {
                label,
                transform: Affine::IDENTITY,
                data: NodeData::Paint(locus_scene::Paint::Gradient(gradient)),
                children: Vec::new(),
                visible: true,
                locked: false,
            };
            let defs = scene.defs();
            if let Some(id) = scene.insert(defs, node) {
                gradient_cache.insert(ptr, id);
                locus_scene::PaintRef::Ref(id)
            } else {
                log::warn!("Failed to insert linear gradient into defs");
                locus_scene::PaintRef::Solid(Color::BLACK)
            }
        }
        usvg::Paint::RadialGradient(rg) => {
            let ptr = std::sync::Arc::as_ptr(rg) as usize;
            if let Some(&node_id) = gradient_cache.get(&ptr) {
                return locus_scene::PaintRef::Ref(node_id);
            }

            let gradient = locus_scene::Gradient {
                kind: locus_scene::GradientKind::Radial {
                    center: Point::new(rg.cx() as f64, rg.cy() as f64),
                    radius: rg.r().get() as f64,
                    focal: Point::new(rg.fx() as f64, rg.fy() as f64),
                    focal_radius: rg.fr().get() as f64,
                },
                stops: convert_stops(rg.stops()),
                interpolation: locus_scene::InterpolationSpace::LinearRgb,
                transform: convert_transform(rg.transform()),
                spread: convert_spread(rg.spread_method()),
            };
            let label = if rg.id().is_empty() {
                format!("radialGradient_{}", gradient_cache.len())
            } else {
                rg.id().to_string()
            };
            let node = locus_scene::Node {
                label,
                transform: Affine::IDENTITY,
                data: NodeData::Paint(locus_scene::Paint::Gradient(gradient)),
                children: Vec::new(),
                visible: true,
                locked: false,
            };
            let defs = scene.defs();
            if let Some(id) = scene.insert(defs, node) {
                gradient_cache.insert(ptr, id);
                locus_scene::PaintRef::Ref(id)
            } else {
                log::warn!("Failed to insert radial gradient into defs");
                locus_scene::PaintRef::Solid(Color::BLACK)
            }
        }
        usvg::Paint::Pattern(pat) => {
            // Dedup by Arc pointer identity (same as gradients).
            let ptr = std::sync::Arc::as_ptr(pat) as usize;
            if let Some(&node_id) = gradient_cache.get(&ptr) {
                return locus_scene::PaintRef::Ref(node_id);
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
                return locus_scene::PaintRef::Solid(Color::BLACK);
            };

            // Recursively import the pattern's children into the content group.
            import_group(
                pat.root(),
                scene,
                content_id,
                gradient_cache,
                Affine::IDENTITY,
            );

            // Create the Pattern paint node.
            let rect = pat.rect();
            let pattern = locus_scene::Pattern {
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
                data: NodeData::Paint(locus_scene::Paint::Pattern(pattern)),
                children: Vec::new(),
                visible: true,
                locked: false,
            };
            let Some(paint_id) = scene.insert(defs, paint_node) else {
                log::warn!("Failed to insert pattern paint node into defs");
                return locus_scene::PaintRef::Solid(Color::BLACK);
            };

            gradient_cache.insert(ptr, paint_id);
            locus_scene::PaintRef::Ref(paint_id)
        }
    }
}

/// Convert usvg gradient stops to our `ColorStop` representation.
fn convert_stops(stops: &[usvg::Stop]) -> Vec<locus_scene::ColorStop> {
    stops
        .iter()
        .map(|s| {
            let c = s.color();
            let opacity = s.opacity().get();
            // Convert sRGB stop color to linear RGBA, pre-multiplied by stop opacity.
            let base = Color::from_srgb8(c.red, c.green, c.blue, 255);
            locus_scene::ColorStop {
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

fn convert_spread(s: usvg::SpreadMethod) -> locus_scene::SpreadMethod {
    match s {
        usvg::SpreadMethod::Pad => locus_scene::SpreadMethod::Pad,
        usvg::SpreadMethod::Reflect => locus_scene::SpreadMethod::Reflect,
        usvg::SpreadMethod::Repeat => locus_scene::SpreadMethod::Repeat,
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
                if let NodeData::Paint(locus_scene::Paint::Gradient(_)) = &node.data {
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
                matches!(&style.fill, Some(f) if matches!(&f.paint, locus_scene::PaintRef::Ref(_)))
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
            if let NodeData::Paint(locus_scene::Paint::Gradient(g)) = &node.data
                && matches!(g.kind, locus_scene::GradientKind::Radial { .. })
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
                    matches!(&style.fill, Some(f) if matches!(&f.paint, locus_scene::PaintRef::Ref(_)))
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
            if let NodeData::Paint(locus_scene::Paint::Pattern(p)) = &node.data {
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
                matches!(&style.fill, Some(f) if matches!(&f.paint, locus_scene::PaintRef::Ref(_)))
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
                    matches!(&n.data, NodeData::Paint(locus_scene::Paint::Pattern(_)))
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
                        matches!(&style.fill, Some(f) if matches!(&f.paint, locus_scene::PaintRef::Ref(_)))
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
                <text x="10" y="50" font-family="Liberation Sans" font-size="24" fill="red">Hello World</text>
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

    // ── Smooth junction detection tests ─────────────────────────────

    #[test]
    fn classify_junction_symmetric() {
        // Two handles pointing in opposite directions, same length.
        let inc = Some([-3.0, 0.0]);
        let out = Some([3.0, 0.0]);
        assert_eq!(classify_junction(inc, out), Some(VertexMode::Symmetric));
    }

    #[test]
    fn classify_junction_smooth() {
        // Collinear but different lengths.
        let inc = Some([-2.0, 0.0]);
        let out = Some([5.0, 0.0]);
        assert_eq!(classify_junction(inc, out), Some(VertexMode::Smooth));
    }

    #[test]
    fn classify_junction_corner() {
        // 90° angle — definitely a corner.
        let inc = Some([0.0, -3.0]);
        let out = Some([3.0, 0.0]);
        assert_eq!(classify_junction(inc, out), None);
    }

    #[test]
    fn classify_junction_same_direction() {
        // Both handles point the same way — not smooth (cusp / loop).
        let inc = Some([3.0, 0.0]);
        let out = Some([3.0, 0.0]);
        assert_eq!(classify_junction(inc, out), None);
    }

    #[test]
    fn classify_junction_missing_handle() {
        // One side is a line — no handle to compare.
        assert_eq!(classify_junction(None, Some([3.0, 0.0])), None);
        assert_eq!(classify_junction(Some([3.0, 0.0]), None), None);
        assert_eq!(classify_junction(None, None), None);
    }

    #[test]
    fn classify_junction_diagonal_symmetric() {
        // Handles at 45° in opposite directions, same length.
        let inc = Some([-1.0, -1.0]);
        let out = Some([1.0, 1.0]);
        assert_eq!(classify_junction(inc, out), Some(VertexMode::Symmetric));
    }

    #[test]
    fn detect_smooth_on_subpath() {
        // Three cubics forming a smooth S-curve.
        // Anchor at (10,0) has symmetric handles, anchor at (20,0) has smooth handles.
        let mut sp = SubPath::new(Point::new(0.0, 0.0));

        // Segment 0: cubic from (0,0) to (10,0) with ctrl2 at (8,2)
        sp.push_segment(
            Segment::Cubic {
                ctrl1: Point::new(2.0, -2.0),
                ctrl2: Point::new(8.0, 2.0),
                to: Point::new(10.0, 0.0),
            },
            VertexMode::Corner,
        );

        // Segment 1: cubic from (10,0) to (20,0)
        // ctrl1 at (12,-2) → symmetric with ctrl2 of segment 0 (both length ≈ 2.83)
        sp.push_segment(
            Segment::Cubic {
                ctrl1: Point::new(12.0, -2.0),
                ctrl2: Point::new(17.0, 3.0),
                to: Point::new(20.0, 0.0),
            },
            VertexMode::Corner,
        );

        // Segment 2: cubic from (20,0) to (30,0)
        // ctrl1 at (22,-2) — different length from ctrl2=(17,3),
        // but let's make them collinear: ctrl2 direction from (20,0) is (-3,3),
        // so outgoing should be in direction (1,-1). Use (22,-2) which is (2,-2).
        // Direction of incoming: (17,3)-(20,0) = (-3,3) → normalized (-1,1)
        // Direction of outgoing: (22,-2)-(20,0) = (2,-2) → normalized (1,-1)
        // These are opposite directions: dot = -1-1 = -2 < 0 ✓
        // Cross = (-1)(-1) - (1)(1) = 1-1 = 0 ✓ collinear!
        // But lengths differ: |(-3,3)| ≈ 4.24, |(2,-2)| ≈ 2.83 → Smooth
        sp.push_segment(
            Segment::Cubic {
                ctrl1: Point::new(22.0, -2.0),
                ctrl2: Point::new(28.0, 0.0),
                to: Point::new(30.0, 0.0),
            },
            VertexMode::Corner,
        );

        detect_smooth_junctions(&mut sp);

        // vertex_modes: [start, seg0_end, seg1_end, seg2_end]
        assert_eq!(
            sp.vertex_modes[0],
            VertexMode::Corner,
            "start has no incoming"
        );
        assert_eq!(
            sp.vertex_modes[1],
            VertexMode::Symmetric,
            "junction at (10,0) should be symmetric"
        );
        assert_eq!(
            sp.vertex_modes[2],
            VertexMode::Smooth,
            "junction at (20,0) should be smooth"
        );
        assert_eq!(
            sp.vertex_modes[3],
            VertexMode::Corner,
            "end has no outgoing"
        );
    }

    #[test]
    fn detect_smooth_closed_path() {
        // A closed path (like a circle approximation) where all junctions
        // are symmetric.
        let mut sp = SubPath::new(Point::new(50.0, 0.0));
        let k = 27.614; // ~= 4/3 * (sqrt(2) - 1) * 50 for quarter-circle approx

        sp.push_segment(
            Segment::Cubic {
                ctrl1: Point::new(50.0, k),
                ctrl2: Point::new(k, 50.0),
                to: Point::new(0.0, 50.0),
            },
            VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Cubic {
                ctrl1: Point::new(-k, 50.0),
                ctrl2: Point::new(-50.0, k),
                to: Point::new(-50.0, 0.0),
            },
            VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Cubic {
                ctrl1: Point::new(-50.0, -k),
                ctrl2: Point::new(-k, -50.0),
                to: Point::new(0.0, -50.0),
            },
            VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Cubic {
                ctrl1: Point::new(k, -50.0),
                ctrl2: Point::new(50.0, -k),
                to: Point::new(50.0, 0.0),
            },
            VertexMode::Corner,
        );
        sp.closed = true;

        detect_smooth_junctions(&mut sp);

        // All 4 interior junctions should be symmetric.
        for i in 1..=3 {
            assert_eq!(
                sp.vertex_modes[i],
                VertexMode::Symmetric,
                "interior junction {i} should be symmetric"
            );
        }
        // Start vertex wraps — also symmetric.
        assert_eq!(
            sp.vertex_modes[0],
            VertexMode::Symmetric,
            "start junction on closed circle should be symmetric"
        );
    }

    #[test]
    fn detect_smooth_lines_stay_corner() {
        // A path made of straight lines — all junctions stay Corner.
        let mut sp = SubPath::new(Point::new(0.0, 0.0));
        sp.push_segment(
            Segment::Line {
                to: Point::new(10.0, 0.0),
            },
            VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Line {
                to: Point::new(10.0, 10.0),
            },
            VertexMode::Corner,
        );
        sp.closed = true;

        detect_smooth_junctions(&mut sp);

        for (i, mode) in sp.vertex_modes.iter().enumerate() {
            assert_eq!(*mode, VertexMode::Corner, "vertex {i} should stay Corner");
        }
    }

    #[test]
    fn import_circle_detects_symmetric() {
        // Import an SVG circle — usvg converts it to 4 cubics.
        // All junctions should be detected as Symmetric.
        let svg = r#"
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 200">
                <circle cx="100" cy="100" r="50"/>
            </svg>
        "#;
        let scene = import_svg(svg.as_bytes()).unwrap();

        let root = scene.root();
        let root_node = scene.get(root).unwrap();
        let path_node = root_node.children.iter().find_map(|&id| {
            let node = scene.get(id)?;
            if let NodeData::Path { path, .. } = &node.data {
                Some(path.clone())
            } else {
                None
            }
        });

        let path = path_node.expect("should have a path node");
        assert!(!path.subpaths.is_empty(), "path should have subpaths");
        let sp = &path.subpaths[0];
        assert!(sp.closed, "circle should be a closed path");

        // All vertex modes (except possibly the last which duplicates the
        // start on a closed path) should be Symmetric.
        for (i, mode) in sp.vertex_modes.iter().enumerate() {
            assert_eq!(
                *mode,
                VertexMode::Symmetric,
                "vertex {i} of imported circle should be Symmetric, got {mode:?}"
            );
        }
    }
}
