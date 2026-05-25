//! Polygon boolean operations on `vector_geom::Path`, for non-destructive
//! boolean groups in the scene graph.
//!
//! # Pipeline
//!
//! 1. Each input `(Path, Affine)` is flattened to closed polylines in
//!    world space at a small tolerance (`FLATTEN_TOLERANCE`). Curves
//!    (Q/C/A) become short line segments.
//! 2. `i_overlay`'s `SingleFloatOverlay` is invoked with the appropriate
//!    `OverlayRule` — n-ary operations (Union/Intersect/Exclude) are
//!    chained cumulatively, Difference uses the bottom-most operand as
//!    base and subtracts everything above it in turn.
//! 3. The result (a list of shapes, each with an outer contour +
//!    possibly holes, in the even-odd fill convention) is rebuilt into
//!    a single `Path` of `Line` segments, one `SubPath` per contour.
//!
//! Curve fidelity is a known tradeoff — the resulting path is all Line
//! segments. A refit-to-cubics post-pass is a future improvement.

use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::single::SingleFloatOverlay;
use vector_geom::{Affine, Bounds, Path, Point, Segment, SubPath, VertexMode};
use vector_scene::{BoolOp, GroupKind, NodeData, NodeId, Scene};

/// Tolerance for curve flattening, in world-space units. Small enough that
/// the polyline is visually indistinguishable from the curve at typical
/// zoom levels, large enough to keep i_overlay's workload bounded.
const FLATTEN_TOLERANCE: f64 = 0.1;

/// A single contour — a closed ring of points. Holes are separate rings
/// in a `Shape`. This matches `i_overlay`'s float contour representation.
type Contour = Vec<[f64; 2]>;

/// A single shape = outer contour + zero-or-more hole contours.
type Shape = Vec<Contour>;

/// A list of shapes — the full geometry of one operand (or the result).
type Shapes = Vec<Shape>;

/// Apply a boolean op over a list of operands.
///
/// Semantics per op:
/// * `Union`, `Intersect`, `Exclude` — symmetric and n-ary; operands
///   accumulate left-to-right.
/// * `Difference` — asymmetric; the first operand is the base, every
///   subsequent operand is subtracted from the running result.
///
/// The caller is responsible for passing operands in z-order. For
/// Difference the convention is **bottom-most first** (base = bottom,
/// things stacked on top cut holes).
///
/// Returns a `Path` of `Line` segments — one closed `SubPath` per
/// resulting contour. Holes are emitted as separate subpaths; pair them
/// with an even-odd fill rule to render correctly.
pub fn apply(op: BoolOp, operands: &[(Path, Affine)]) -> Path {
    if operands.is_empty() {
        return Path::new();
    }

    let flattened: Vec<Shape> = operands.iter().map(|(p, t)| flatten_path(p, *t)).collect();

    let result_shapes = match op {
        BoolOp::Union => chain_op(&flattened, OverlayRule::Union),
        BoolOp::Intersect => chain_op(&flattened, OverlayRule::Intersect),
        BoolOp::Exclude => chain_op(&flattened, OverlayRule::Xor),
        BoolOp::Difference => {
            // First operand is the base; subtract everything else.
            let mut acc: Shapes = vec![flattened[0].clone()];
            for other in flattened.iter().skip(1) {
                acc = overlay_shapes_lists(&acc, other, OverlayRule::Difference);
            }
            acc
        }
    };

    shapes_to_path(&result_shapes)
}

/// Apply `rule` left-associatively across every operand.
///
/// `Shapes` (a full set of shapes) is the result accumulator; each
/// subsequent operand (`Shape`, the flattened polygon of one input) is
/// merged in with `overlay_shapes_lists`.
fn chain_op(operands: &[Shape], rule: OverlayRule) -> Shapes {
    let mut acc: Shapes = vec![operands[0].clone()];
    for other in operands.iter().skip(1) {
        acc = overlay_shapes_lists(&acc, other, rule);
    }
    acc
}

/// Run one overlay op: `subject` (a list of shapes) against `clip` (one
/// shape, consisting of outer + holes). `i_overlay`'s `SingleFloatOverlay`
/// operates shape-against-shape, so we flatten the subject list of shapes
/// into a single flat list of contours and rely on the fill rule to
/// reconcile overlaps.
fn overlay_shapes_lists(subject: &Shapes, clip: &Shape, rule: OverlayRule) -> Shapes {
    // Flatten shapes into a single list of contours for subject.
    let subj_flat: Vec<Contour> = subject.iter().flat_map(|s| s.iter().cloned()).collect();
    subj_flat.overlay(clip, rule, FillRule::EvenOdd)
}

/// Flatten a `Path` to polygon contours under `world_transform`. Curves
/// are approximated by line segments at `FLATTEN_TOLERANCE`. Open subpaths
/// are implicitly closed (polygon booleans require closed contours).
///
/// Returns one `Shape` — i.e. the result is treated as one polygon (with
/// its subpaths as contours). The caller picks the fill rule when feeding
/// into i_overlay, so hole-vs-outer is resolved there.
fn flatten_path(path: &Path, world_transform: Affine) -> Shape {
    let mut shape: Shape = Vec::new();
    for sub in &path.subpaths {
        let contour = flatten_subpath(sub, world_transform);
        if contour.len() >= 3 {
            shape.push(contour);
        }
    }
    shape
}

fn flatten_subpath(subpath: &SubPath, world: Affine) -> Contour {
    let mut out: Contour = Vec::new();

    let start = world.apply(subpath.start);
    out.push([start.x, start.y]);

    let mut current = subpath.start;
    for seg in &subpath.segments {
        match seg {
            Segment::Line { to } => {
                let p = world.apply(*to);
                out.push([p.x, p.y]);
            }
            Segment::Quad { ctrl, to } => {
                let q = lyon_geom::QuadraticBezierSegment {
                    from: point_f(current),
                    ctrl: point_f(*ctrl),
                    to: point_f(*to),
                };
                // for_each_flattened emits the endpoint of each flat
                // segment (not the start), which is exactly what we want.
                q.for_each_flattened(FLATTEN_TOLERANCE as f32, &mut |seg| {
                    let p = world.apply(Point::new(seg.to.x as f64, seg.to.y as f64));
                    out.push([p.x, p.y]);
                });
            }
            Segment::Cubic { ctrl1, ctrl2, to } => {
                let c = lyon_geom::CubicBezierSegment {
                    from: point_f(current),
                    ctrl1: point_f(*ctrl1),
                    ctrl2: point_f(*ctrl2),
                    to: point_f(*to),
                };
                c.for_each_flattened(FLATTEN_TOLERANCE as f32, &mut |seg| {
                    let p = world.apply(Point::new(seg.to.x as f64, seg.to.y as f64));
                    out.push([p.x, p.y]);
                });
            }
            Segment::Arc {
                radii,
                x_rotation,
                large_arc,
                sweep,
                to,
            } => {
                let svg_arc = lyon_geom::SvgArc {
                    from: point_f(current),
                    to: point_f(*to),
                    radii: lyon_geom::vector(radii.x as f32, radii.y as f32),
                    x_rotation: lyon_geom::Angle::radians(*x_rotation as f32),
                    flags: lyon_geom::ArcFlags {
                        large_arc: *large_arc,
                        sweep: *sweep,
                    },
                };
                // Arc -> cubics -> flatten each cubic.
                svg_arc.for_each_cubic_bezier(&mut |cb| {
                    cb.for_each_flattened(FLATTEN_TOLERANCE as f32, &mut |seg| {
                        let p = world.apply(Point::new(seg.to.x as f64, seg.to.y as f64));
                        out.push([p.x, p.y]);
                    });
                });
            }
        }
        current = seg.endpoint();
    }

    // i_overlay auto-closes contours, so we don't append the start point
    // again — but we may have accidentally duplicated it if the last
    // segment endpoint lands exactly on `start` (closed subpath). Strip
    // one duplicate if so.
    if out.len() >= 2 {
        let first = out[0];
        let last = out[out.len() - 1];
        if (first[0] - last[0]).abs() < 1e-9 && (first[1] - last[1]).abs() < 1e-9 {
            out.pop();
        }
    }

    out
}

fn point_f(p: Point) -> lyon_geom::Point<f32> {
    lyon_geom::point(p.x as f32, p.y as f32)
}

/// Rebuild a `Path` from i_overlay's result. Each contour becomes a
/// closed `SubPath` of `Line` segments. The outer and hole contours are
/// emitted side-by-side (flat list of subpaths) — pair with EvenOdd fill
/// rule to render holes correctly.
fn shapes_to_path(shapes: &Shapes) -> Path {
    let mut path = Path::new();
    for shape in shapes {
        for contour in shape {
            if contour.len() < 3 {
                continue;
            }
            let start = Point::new(contour[0][0], contour[0][1]);
            let mut sp = SubPath::new(start);
            for pt in contour.iter().skip(1) {
                sp.push_segment(
                    Segment::Line {
                        to: Point::new(pt[0], pt[1]),
                    },
                    VertexMode::Corner,
                );
            }
            sp.closed = true;
            path.subpaths.push(sp);
        }
    }
    path
}

/// Compute the baked path for a Boolean group node.
///
/// Walks the group's subtree, collects every visible `Path` node (with
/// its resolved world transform *relative to the boolean group*), then
/// applies the group's `BoolOp`. Returns the computed `Path` in the
/// boolean group's local space.
///
/// For `Difference`, descendants are fed **bottom-of-z-order first**
/// (the scene stores children front-to-back with last-drawn-on-top, so
/// our `children[0]` is actually the bottom). Matches the visual
/// convention where things stacked on top cut holes.
///
/// If `group_id` is not a Boolean group, or has no path descendants,
/// returns an empty `Path`.
pub fn compute_boolean_group_path(scene: &Scene, group_id: NodeId) -> Path {
    let Some(group_node) = scene.get(group_id) else {
        return Path::new();
    };
    let NodeData::Group {
        kind: GroupKind::Boolean { op, .. },
        ..
    } = group_node.data
    else {
        return Path::new();
    };

    // Gather descendants in depth-first order, relative transform from
    // the group's local space (i.e. skipping the group's own transform
    // when computing "world" for descendants).
    let mut operands: Vec<(Path, Affine)> = Vec::new();
    scene.walk_depth_first(group_id, Affine::IDENTITY, &mut |id, node, world| {
        if id == group_id {
            // Top of walk — skip, but recurse.
            return true;
        }
        if !node.visible {
            return false;
        }
        // If a descendant is itself a Boolean group, use its computed
        // path instead of recursing into its children.
        if let NodeData::Group {
            kind: GroupKind::Boolean { .. },
            ..
        } = node.data
        {
            let nested = compute_boolean_group_path(scene, id);
            if !nested.subpaths.is_empty() {
                operands.push((nested, world));
            }
            return false; // do not recurse into nested boolean group
        }
        if let NodeData::Path { path, .. } = &node.data {
            operands.push((path.clone(), world));
        }
        // Text nodes are not supported as operands in v1.
        true
    });

    apply(op, &operands)
}

/// Visual bounds for a Boolean group in world space, including the group's
/// own stroke expansion. The baked path lives in the group's local space,
/// so we compute its tight local AABB (with stroke) and transform by the
/// group's world transform.
///
/// Returns `Bounds::EMPTY` if `group_id` is not a Boolean group or has no
/// path descendants.
pub fn boolean_group_visual_bounds(scene: &Scene, group_id: NodeId, world: Affine) -> Bounds {
    let Some(group_node) = scene.get(group_id) else {
        return Bounds::EMPTY;
    };
    let NodeData::Group {
        kind: GroupKind::Boolean { style, .. },
        ..
    } = &group_node.data
    else {
        return Bounds::EMPTY;
    };
    let computed = compute_boolean_group_path(scene, group_id);
    if computed.subpaths.is_empty() {
        return Bounds::EMPTY;
    }
    let has_fill = style.fill.is_some();
    let stroke_params = style.stroke.as_ref().map(|s| s.style.bounds_params());
    let local = vector_tess::path_visual_bounds(&computed, has_fill, stroke_params.as_ref());
    local.transform(world)
}

/// World-space visual bounds for the union of a set of nodes. Boolean groups
/// contribute their baked tight bounds; regular groups recursively union
/// their visible descendants (also short-circuiting at any nested boolean
/// groups). Hidden nodes are skipped, but the hidden flag of the supplied
/// roots themselves is honored — a hidden root contributes nothing.
///
/// Useful for zoom-to-selection / fit-to-selection.
pub fn selection_visual_bounds(scene: &Scene, ids: &[NodeId]) -> Bounds {
    let mut bounds = Bounds::EMPTY;
    for &id in ids {
        let Some(node) = scene.get(id) else { continue };
        if !node.visible {
            continue;
        }
        let world = scene.world_transform(id);
        let nb = match &node.data {
            NodeData::Group {
                kind: GroupKind::Boolean { .. },
                ..
            } => boolean_group_visual_bounds(scene, id, world),
            NodeData::Group { is_defs: true, .. } => Bounds::EMPTY,
            NodeData::Group { .. } => {
                // `walk_depth_first`'s second arg is the *parent* transform —
                // the visitor receives `parent.then(visited.transform)` as
                // world. Passing `world` (= id's own world, parent.then(id.t))
                // would compose id.t a second time, doubling every descendant's
                // motion when the group is dragged. Use the group's parent
                // world here so the walk lands at id with the correct world.
                let parent_world = scene.parent_world_transform(id);
                let mut gb = Bounds::EMPTY;
                scene.walk_depth_first(id, parent_world, &mut |_cid, cnode, cworld| {
                    if !cnode.visible {
                        return false;
                    }
                    if let NodeData::Group { is_defs: true, .. } = cnode.data {
                        return false;
                    }
                    if let NodeData::Group {
                        kind: GroupKind::Boolean { .. },
                        ..
                    } = &cnode.data
                    {
                        let cb = boolean_group_visual_bounds(scene, _cid, cworld);
                        if !cb.is_empty() {
                            gb = gb.union(cb);
                        }
                        return false;
                    }
                    let cb = cnode.data.visual_bounds(cworld);
                    if !cb.is_empty() {
                        gb = gb.union(cb);
                    }
                    true
                });
                gb
            }
            _ => node.data.visual_bounds(world),
        };
        if !nb.is_empty() {
            bounds = bounds.union(nb);
        }
    }
    bounds
}

/// Bounding box over all visible rendered content in a scene, short-circuiting
/// at Boolean groups (so they contribute the tight bounds of their baked
/// computed path, not the naive union of their operands).
///
/// This is the boolean-group-aware analogue of `Scene::content_bounds` —
/// prefer it wherever accurate fit-to-content matters (SVG viewBox export,
/// zoom-to-fit), since the naive descendant union can be significantly
/// looser than the real result for Intersect / Difference groups.
pub fn scene_content_bounds(scene: &Scene) -> Bounds {
    let mut bounds = Bounds::EMPTY;
    scene.walk_depth_first(scene.root(), Affine::IDENTITY, &mut |id, node, world| {
        if !node.visible {
            return false;
        }
        // Skip defs subtree entirely — not rendered.
        if let NodeData::Group { is_defs: true, .. } = node.data {
            return false;
        }
        // Boolean group: use the baked path's visual bounds and do not
        // recurse into the (individually-invisible) operand children.
        if let NodeData::Group {
            kind: GroupKind::Boolean { .. },
            ..
        } = &node.data
        {
            let nb = boolean_group_visual_bounds(scene, id, world);
            if !nb.is_empty() {
                bounds = bounds.union(nb);
            }
            return false;
        }
        let nb = node.data.visual_bounds(world);
        if !nb.is_empty() {
            bounds = bounds.union(nb);
        }
        true
    });
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, size: f64) -> Path {
        let mut sp = SubPath::new(Point::new(x, y));
        sp.push_segment(
            Segment::Line {
                to: Point::new(x + size, y),
            },
            VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Line {
                to: Point::new(x + size, y + size),
            },
            VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Line {
                to: Point::new(x, y + size),
            },
            VertexMode::Corner,
        );
        sp.closed = true;
        let mut p = Path::new();
        p.subpaths.push(sp);
        p
    }

    fn path_bounds(p: &Path) -> (Point, Point) {
        let mut min = Point::new(f64::INFINITY, f64::INFINITY);
        let mut max = Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
        for sp in &p.subpaths {
            for pt in std::iter::once(sp.start).chain(sp.segments.iter().map(|s| s.endpoint())) {
                min = Point::new(min.x.min(pt.x), min.y.min(pt.y));
                max = Point::new(max.x.max(pt.x), max.y.max(pt.y));
            }
        }
        (min, max)
    }

    #[test]
    fn union_of_two_overlapping_squares_is_single_shape() {
        // Two 10x10 squares overlapping by 5 on the x axis.
        let a = square(0.0, 0.0, 10.0);
        let b = square(5.0, 0.0, 10.0);
        let result = apply(
            BoolOp::Union,
            &[(a, Affine::IDENTITY), (b, Affine::IDENTITY)],
        );
        assert_eq!(
            result.subpaths.len(),
            1,
            "union should produce a single contour"
        );
        let (min, max) = path_bounds(&result);
        assert!((min.x - 0.0).abs() < 1e-6);
        assert!((min.y - 0.0).abs() < 1e-6);
        assert!((max.x - 15.0).abs() < 1e-6);
        assert!((max.y - 10.0).abs() < 1e-6);
    }

    #[test]
    fn intersection_of_two_overlapping_squares() {
        let a = square(0.0, 0.0, 10.0);
        let b = square(5.0, 0.0, 10.0);
        let result = apply(
            BoolOp::Intersect,
            &[(a, Affine::IDENTITY), (b, Affine::IDENTITY)],
        );
        assert_eq!(result.subpaths.len(), 1);
        let (min, max) = path_bounds(&result);
        assert!((min.x - 5.0).abs() < 1e-6);
        assert!((min.y - 0.0).abs() < 1e-6);
        assert!((max.x - 10.0).abs() < 1e-6);
        assert!((max.y - 10.0).abs() < 1e-6);
    }

    #[test]
    fn difference_subtracts_second_from_first() {
        // 20x20 square with a 5x5 square punched out of its center-ish.
        let a = square(0.0, 0.0, 20.0);
        let b = square(5.0, 5.0, 5.0);
        let result = apply(
            BoolOp::Difference,
            &[(a, Affine::IDENTITY), (b, Affine::IDENTITY)],
        );
        // Result is the 20x20 outer with a 5x5 hole — two contours.
        assert_eq!(result.subpaths.len(), 2);
        let (_min, max) = path_bounds(&result);
        assert!((max.x - 20.0).abs() < 1e-6);
    }

    #[test]
    fn exclude_of_two_overlapping_squares_is_two_crescents() {
        let a = square(0.0, 0.0, 10.0);
        let b = square(5.0, 0.0, 10.0);
        let result = apply(
            BoolOp::Exclude,
            &[(a, Affine::IDENTITY), (b, Affine::IDENTITY)],
        );
        // XOR of two overlapping squares: two disjoint regions.
        assert_eq!(result.subpaths.len(), 2);
    }

    #[test]
    fn scene_content_bounds_uses_tight_intersect_bounds() {
        // Two 10x10 squares overlapping by 5 on x, wrapped in a Boolean
        // Intersect group. The naive descendant-union would span 0..15;
        // the baked intersect result is only 5..10 on x. The
        // boolean-aware `scene_content_bounds` must return the tight
        // result — otherwise SVG export and zoom-to-fit waste space.
        use vector_scene::Node;

        let mut scene = Scene::new();
        let root = scene.root();

        let group_id = scene
            .insert(root, Node::group("bool"))
            .expect("insert group");
        // Convert to a Boolean::Intersect group.
        scene.set_group_kind(
            group_id,
            GroupKind::Boolean {
                op: BoolOp::Intersect,
                style: vector_scene::Style::default(),
            },
        );

        scene
            .insert(group_id, Node::path("a", square(0.0, 0.0, 10.0)))
            .expect("insert a");
        scene
            .insert(group_id, Node::path("b", square(5.0, 0.0, 10.0)))
            .expect("insert b");

        let bounds = scene_content_bounds(&scene);
        assert!(!bounds.is_empty());
        // Tight intersect: x ∈ [5, 10], y ∈ [0, 10]. Allow tiny slack for
        // flattening + stroke-expansion (the default path style has no
        // stroke but a solid fill, so the tessellator's AABB matches the
        // geometry to within float noise).
        assert!(
            (bounds.min.x - 5.0).abs() < 1e-3,
            "min.x = {}",
            bounds.min.x
        );
        assert!(
            (bounds.max.x - 10.0).abs() < 1e-3,
            "max.x = {}",
            bounds.max.x
        );
        assert!(
            (bounds.min.y - 0.0).abs() < 1e-3,
            "min.y = {}",
            bounds.min.y
        );
        assert!(
            (bounds.max.y - 10.0).abs() < 1e-3,
            "max.y = {}",
            bounds.max.y
        );
    }

    /// Regression: `selection_visual_bounds` of a Group with a non-identity
    /// transform must not apply the group's transform twice. The bug
    /// manifested as the on-screen selection bbox drifting at 2× the rate
    /// of the group's contents when the group was dragged.
    ///
    /// Setup: a 10×10 square at local origin, wrapped in a Group whose
    /// transform translates by (100, 50). Correct world bounds are
    /// (100,50)..(110,60). The doubled-transform bug would produce
    /// (200,100)..(210,110).
    #[test]
    fn selection_bounds_of_translated_group_applies_transform_once() {
        use vector_scene::Node;
        let mut scene = Scene::new();
        let root = scene.root();

        let group_id = scene.insert(root, Node::group("g")).unwrap();
        scene.set_transform(group_id, Affine::translate(100.0, 50.0));

        let mut path_node = Node::path("sq", square(0.0, 0.0, 10.0));
        // Give the path a fill so visual_bounds includes its area.
        if let NodeData::Path { ref mut style, .. } = path_node.data {
            style.fill = Some(vector_scene::Fill {
                paint: vector_scene::PaintRef::Solid(vector_geom::Color::from_srgb8(
                    255, 0, 0, 255,
                )),
                rule: vector_scene::FillRule::NonZero,
                opacity: 1.0,
            });
        }
        scene.insert(group_id, path_node).unwrap();

        let bounds = selection_visual_bounds(&scene, &[group_id]);
        assert!(!bounds.is_empty(), "expected non-empty bounds");
        assert!(
            (bounds.min.x - 100.0).abs() < 1e-6
                && (bounds.min.y - 50.0).abs() < 1e-6
                && (bounds.max.x - 110.0).abs() < 1e-6
                && (bounds.max.y - 60.0).abs() < 1e-6,
            "group transform applied twice — bounds = {bounds:?}"
        );
    }

    #[test]
    fn empty_operands_returns_empty_path() {
        let result = apply(BoolOp::Union, &[]);
        assert!(result.subpaths.is_empty());
    }

    #[test]
    fn transforms_are_honored() {
        // Two 10x10 squares, but the second is translated by +5 via
        // Affine, so the result should match the "both translated"
        // union test above.
        let a = square(0.0, 0.0, 10.0);
        let b = square(0.0, 0.0, 10.0);
        let translate_x5 = Affine {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: 5.0,
            ty: 0.0,
        };
        let result = apply(BoolOp::Union, &[(a, Affine::IDENTITY), (b, translate_x5)]);
        assert_eq!(result.subpaths.len(), 1);
        let (_min, max) = path_bounds(&result);
        assert!((max.x - 15.0).abs() < 1e-6);
    }
}
