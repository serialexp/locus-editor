//! Snapping configuration and resolution. Editor preference only —
//! not stored in SVG.
//!
//! Three snap targets, in priority order:
//! 1. Vertices/anchors of other paths (`vertex_enabled`).
//! 2. Closest point on a path edge (`edge_enabled`).
//! 3. Grid (`grid_enabled`).
//!
//! Vertex and edge snapping use the same screen-pixel hit radius; the most
//! specific hit (vertex > edge > grid) wins.
//!
//! Edge and grid **compose**: when both are enabled and the cursor is near
//! an edge, the drag is locked to the edge but quantized *along* it to the
//! points where the edge crosses a grid line (line×grid intersections),
//! rather than sliding freely. This keeps grid discipline while tracing
//! another path. (Only if the edge's segment has no grid crossing near the
//! cursor does it fall back to the free closest-point projection.)
//!
//! The resolved indicator is returned alongside the snapped position so the
//! renderer can draw a small marker on the snap target.

use locus_geom::{Point, Segment};
use locus_scene::{NodeData, NodeId, Scene};

/// Snap-radius in screen pixels — divided by zoom to get a canvas-space
/// radius. Matches `HIT_RADIUS_SCREEN_PX` in locus-tools so snap reach
/// matches what the user can directly click.
const SNAP_RADIUS_SCREEN_PX: f64 = 8.0;

/// What kind of geometry snap fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapKind {
    /// Snapped to an anchor or control point of another path.
    Vertex,
    /// Snapped to the nearest point on a path edge.
    Edge,
    /// Snapped to a line×grid intersection — a point that is both on a
    /// path edge *and* on a grid line. Fires when edge and grid snapping
    /// are both enabled: the drag stays locked to the edge but is quantized
    /// along it to the grid crossings.
    EdgeGrid,
    /// Snapped to the canvas grid.
    Grid,
}

/// A successful snap, including the (canvas-space) target point that
/// the cursor was pulled to. Carried back to the renderer for the
/// on-canvas indicator.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SnapHit {
    pub pos: [f64; 2],
    pub kind: SnapKind,
}

/// Snapping configuration — editor preference, not stored in SVG.
#[derive(Clone)]
pub(crate) struct SnapSettings {
    /// Snap to grid intersections.
    pub(crate) grid_enabled: bool,
    /// Grid spacing in canvas units.
    pub(crate) grid_size: f64,
    /// Snap to anchor / control points of other paths.
    pub(crate) vertex_enabled: bool,
    /// Snap to the nearest point on a path edge.
    pub(crate) edge_enabled: bool,
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self {
            grid_enabled: false,
            grid_size: 1.0,
            // Geometry snap is on by default — Inkscape-style.
            vertex_enabled: true,
            edge_enabled: true,
        }
    }
}

impl SnapSettings {
    /// True if any snap source is enabled.
    pub(crate) fn any_enabled(&self) -> bool {
        self.grid_enabled || self.vertex_enabled || self.edge_enabled
    }

    /// Resolve a canvas-space position against all enabled snap sources.
    ///
    /// `exclude` lists nodes whose vertices/edges should not be snap
    /// targets — typically the nodes currently being dragged or
    /// extended (so a vertex doesn't snap to itself, and a half-built
    /// pen path doesn't pull onto its own previous point).
    ///
    /// Returns the (possibly adjusted) position and an optional
    /// indicator describing which snap fired (for the on-canvas marker).
    pub(crate) fn resolve(
        &self,
        pos: [f64; 2],
        scene: &Scene,
        zoom: f64,
        exclude: &[NodeId],
    ) -> ([f64; 2], Option<SnapHit>) {
        if !self.any_enabled() {
            return (pos, None);
        }

        let radius = SNAP_RADIUS_SCREEN_PX / zoom.max(f64::EPSILON);
        let target = Point::new(pos[0], pos[1]);

        // 1. Vertex snap: most specific, wins over edge/grid.
        if self.vertex_enabled
            && let Some(p) = closest_vertex(scene, target, radius, exclude)
        {
            return (
                [p.x, p.y],
                Some(SnapHit {
                    pos: [p.x, p.y],
                    kind: SnapKind::Vertex,
                }),
            );
        }

        // 2. Edge snap. When grid snapping is also on, quantize along the
        //    edge to the nearest line×grid intersection instead of returning
        //    the free projection — so tracing another path stays grid-sticky.
        if self.edge_enabled
            && let Some(hit) = closest_edge_point(scene, target, radius, exclude)
        {
            if self.grid_enabled
                && self.grid_size > 0.0
                && let Some(cross) =
                    nearest_crossing_on_poly(&hit.world_poly, self.grid_size, target)
            {
                return (
                    [cross.x, cross.y],
                    Some(SnapHit {
                        pos: [cross.x, cross.y],
                        kind: SnapKind::EdgeGrid,
                    }),
                );
            }
            let p = hit.point;
            return (
                [p.x, p.y],
                Some(SnapHit {
                    pos: [p.x, p.y],
                    kind: SnapKind::Edge,
                }),
            );
        }

        // 3. Grid snap.
        if self.grid_enabled {
            let g = self.grid_size;
            let snapped = [(pos[0] / g).round() * g, (pos[1] / g).round() * g];
            return (
                snapped,
                Some(SnapHit {
                    pos: snapped,
                    kind: SnapKind::Grid,
                }),
            );
        }

        (pos, None)
    }
}

/// Walk every interactive path node in the scene, applying `f` to each
/// vertex (anchor + control points) in world coordinates. Excluded
/// nodes are skipped entirely.
fn for_each_world_vertex(scene: &Scene, exclude: &[NodeId], mut f: impl FnMut(Point)) {
    let root = scene.root();
    scene.walk_depth_first(
        root,
        locus_geom::Affine::IDENTITY,
        &mut |id, node, world| {
            // Hidden subtree: walk_depth_first will short-circuit on `false`.
            if !node.is_interactive() {
                return false;
            }
            if exclude.contains(&id) {
                // Skip this node's own vertices but keep descending —
                // groups in `exclude` may have unrelated children.
                return true;
            }
            if let NodeData::Path { ref path, .. } = node.data {
                let xform = |p: Point| {
                    if world.is_identity() {
                        p
                    } else {
                        world.apply(p)
                    }
                };
                for subpath in &path.subpaths {
                    f(xform(subpath.start));
                    for seg in &subpath.segments {
                        f(xform(seg.endpoint()));
                        match seg {
                            Segment::Quad { ctrl, .. } => f(xform(*ctrl)),
                            Segment::Cubic { ctrl1, ctrl2, .. } => {
                                f(xform(*ctrl1));
                                f(xform(*ctrl2));
                            }
                            _ => {}
                        }
                    }
                }
            }
            true
        },
    );
}

fn closest_vertex(scene: &Scene, target: Point, radius: f64, exclude: &[NodeId]) -> Option<Point> {
    let mut best: Option<(Point, f64)> = None;
    for_each_world_vertex(scene, exclude, |p| {
        let d = target.distance(p);
        if d < radius && best.as_ref().is_none_or(|(_, bd)| d < *bd) {
            best = Some((p, d));
        }
    });
    best.map(|(p, _)| p)
}

/// The winning edge snap: the free closest-point projection plus the
/// world-space geometry of the segment it landed on, so the caller can
/// quantize along that segment to grid crossings.
struct EdgeHit {
    /// Free closest-point projection onto the edge (world coords).
    point: Point,
    /// The winning segment, flattened to world-space points (two points
    /// for a straight line; a sampled polyline for a curve). Used to find
    /// line×grid intersections without re-walking the scene.
    world_poly: Vec<Point>,
}

fn closest_edge_point(
    scene: &Scene,
    target: Point,
    radius: f64,
    exclude: &[NodeId],
) -> Option<EdgeHit> {
    let mut best: Option<(EdgeHit, f64)> = None;
    let root = scene.root();
    scene.walk_depth_first(
        root,
        locus_geom::Affine::IDENTITY,
        &mut |id, node, world| {
            if !node.is_interactive() {
                return false;
            }
            if exclude.contains(&id) {
                return true;
            }
            let NodeData::Path { ref path, .. } = node.data else {
                return true;
            };

            // Closest-point math is cheaper in local space; transform the
            // target into local coords once per node, transform the hit back
            // to world for the final distance comparison.
            let local_target = if world.is_identity() {
                target
            } else if let Some(inv) = world.inverse() {
                inv.apply(target)
            } else {
                return true;
            };

            let consider = |seg: &Segment, from: Point, best: &mut Option<(EdgeHit, f64)>| {
                let (t, _, _) = seg.closest_point(from, local_target);
                let local_pt = seg.eval_at(from, t);
                let world_pt = if world.is_identity() {
                    local_pt
                } else {
                    world.apply(local_pt)
                };
                let d = target.distance(world_pt);
                if d < radius && best.as_ref().is_none_or(|(_, bd)| d < *bd) {
                    *best = Some((
                        EdgeHit {
                            point: world_pt,
                            world_poly: flatten_seg_world(seg, from, &world),
                        },
                        d,
                    ));
                }
            };

            for subpath in &path.subpaths {
                let mut current = subpath.start;
                for seg in &subpath.segments {
                    consider(seg, current, &mut best);
                    current = seg.endpoint();
                }
                // Implicit closing edge for closed subpaths.
                if subpath.closed && !subpath.segments.is_empty() && current != subpath.start {
                    let closing = Segment::Line { to: subpath.start };
                    consider(&closing, current, &mut best);
                }
            }
            true
        },
    );
    best.map(|(h, _)| h)
}

/// Flatten a segment to world-space points: two points for a straight
/// line, a sampled polyline for a curve. Used to intersect the winning
/// edge with grid lines.
fn flatten_seg_world(seg: &Segment, from: Point, world: &locus_geom::Affine) -> Vec<Point> {
    let to_world = |p: Point| {
        if world.is_identity() {
            p
        } else {
            world.apply(p)
        }
    };
    match seg {
        Segment::Line { .. } => vec![to_world(from), to_world(seg.endpoint())],
        _ => {
            // Curves: sample uniformly. 24 chords track the grid crossings
            // closely enough at the snap radius while staying cheap (built
            // only for the single winning segment).
            const SAMPLES: usize = 24;
            (0..=SAMPLES)
                .map(|i| to_world(seg.eval_at(from, i as f64 / SAMPLES as f64)))
                .collect()
        }
    }
}

/// Nearest point on a world-space polyline where it crosses a grid line
/// (`x = k·g` or `y = k·g`), measured to `target`. Returns `None` if no
/// crossing lies on the polyline near the target.
fn nearest_crossing_on_poly(poly: &[Point], g: f64, target: Point) -> Option<Point> {
    let mut best: Option<(Point, f64)> = None;
    for seg in poly.windows(2) {
        accumulate_seg_crossings(seg[0], seg[1], g, target, &mut best);
    }
    best.map(|(p, _)| p)
}

/// Accumulate the grid-line crossings of a single straight segment `a→b`
/// into `best` (nearest to `target` wins). Only the grid multiples adjacent
/// to `target` on each axis are tested — on a straight segment a coordinate
/// is monotonic in the parameter, so the crossing nearest the cursor sits at
/// `round(target/g)` ± 1. This is O(1) per segment regardless of grid size.
fn accumulate_seg_crossings(
    a: Point,
    b: Point,
    g: f64,
    target: Point,
    best: &mut Option<(Point, f64)>,
) {
    // Parameter (0..=1) is required for the point to lie on the segment.
    const AXIS_EPS: f64 = 1e-9;
    let dx = b.x - a.x;
    let dy = b.y - a.y;

    let mut ts: [f64; 6] = [f64::NAN; 6];
    let mut n = 0;
    if dx.abs() > AXIS_EPS {
        let k0 = (target.x / g).round() as i64;
        for k in (k0 - 1)..=(k0 + 1) {
            ts[n] = (k as f64 * g - a.x) / dx;
            n += 1;
        }
    }
    if dy.abs() > AXIS_EPS {
        let k0 = (target.y / g).round() as i64;
        for k in (k0 - 1)..=(k0 + 1) {
            ts[n] = (k as f64 * g - a.y) / dy;
            n += 1;
        }
    }

    for &t in &ts[..n] {
        if (0.0..=1.0).contains(&t) {
            let p = Point::new(a.x + t * dx, a.y + t * dy);
            let d = target.distance(p);
            if best.as_ref().is_none_or(|(_, bd)| d < *bd) {
                *best = Some((p, d));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use locus_geom::{Path, Point as P, Segment, SubPath, VertexMode};
    use locus_scene::Node;

    fn node_with_line(from: [f64; 2], to: [f64; 2]) -> Node {
        let mut sp = SubPath::new(P::new(from[0], from[1]));
        sp.push_segment(
            Segment::Line {
                to: P::new(to[0], to[1]),
            },
            VertexMode::Corner,
        );
        let path = Path { subpaths: vec![sp] };
        Node::path("line", path)
    }

    #[test]
    fn vertex_snap_pulls_to_endpoint_within_radius() {
        let mut scene = Scene::new();
        let parent = scene.root();
        scene
            .insert(parent, node_with_line([0.0, 0.0], [10.0, 0.0]))
            .unwrap();

        let snap = SnapSettings {
            grid_enabled: false,
            grid_size: 1.0,
            vertex_enabled: true,
            edge_enabled: false,
        };

        // Cursor near (10, 0) at zoom 1.0 → snap radius 8 px.
        let (snapped, hit) = snap.resolve([10.5, 0.5], &scene, 1.0, &[]);
        assert!(hit.is_some(), "expected vertex hit");
        assert_eq!(hit.unwrap().kind, SnapKind::Vertex);
        assert!((snapped[0] - 10.0).abs() < 1e-9);
        assert!((snapped[1]).abs() < 1e-9);
    }

    #[test]
    fn vertex_snap_skips_excluded_node() {
        let mut scene = Scene::new();
        let parent = scene.root();
        let id = scene
            .insert(parent, node_with_line([0.0, 0.0], [10.0, 0.0]))
            .unwrap();

        let snap = SnapSettings {
            grid_enabled: false,
            grid_size: 1.0,
            vertex_enabled: true,
            edge_enabled: false,
        };

        // The only path is excluded → no vertex snap, no fallback.
        let (snapped, hit) = snap.resolve([10.5, 0.5], &scene, 1.0, &[id]);
        assert!(hit.is_none());
        assert_eq!(snapped, [10.5, 0.5]);
    }

    #[test]
    fn edge_snap_pulls_to_nearest_point_on_segment() {
        let mut scene = Scene::new();
        let parent = scene.root();
        // Horizontal line from (0,0) to (100,0).
        scene
            .insert(parent, node_with_line([0.0, 0.0], [100.0, 0.0]))
            .unwrap();

        let snap = SnapSettings {
            grid_enabled: false,
            grid_size: 1.0,
            // Vertex disabled so the midpoint isn't pulled to an endpoint.
            vertex_enabled: false,
            edge_enabled: true,
        };

        let (snapped, hit) = snap.resolve([50.0, 2.0], &scene, 1.0, &[]);
        assert!(hit.is_some(), "expected edge hit");
        assert_eq!(hit.unwrap().kind, SnapKind::Edge);
        assert!((snapped[0] - 50.0).abs() < 1e-9);
        assert!(snapped[1].abs() < 1e-9);
    }

    #[test]
    fn vertex_snap_wins_over_edge() {
        let mut scene = Scene::new();
        let parent = scene.root();
        scene
            .insert(parent, node_with_line([0.0, 0.0], [10.0, 0.0]))
            .unwrap();

        let snap = SnapSettings {
            grid_enabled: false,
            grid_size: 1.0,
            vertex_enabled: true,
            edge_enabled: true,
        };

        // Within reach of both the (10,0) endpoint and the segment.
        // Vertex snap should fire first.
        let (snapped, hit) = snap.resolve([9.5, 0.5], &scene, 1.0, &[]);
        assert_eq!(hit.unwrap().kind, SnapKind::Vertex);
        assert!((snapped[0] - 10.0).abs() < 1e-9);
    }

    #[test]
    fn edge_and_grid_compose_to_line_grid_crossing() {
        let mut scene = Scene::new();
        let parent = scene.root();
        // Horizontal line at y = 3, well away from any grid point on y.
        scene
            .insert(parent, node_with_line([0.0, 3.0], [100.0, 3.0]))
            .unwrap();

        let snap = SnapSettings {
            grid_enabled: true,
            grid_size: 10.0,
            // Vertex off so endpoints don't interfere; edge + grid compose.
            vertex_enabled: false,
            edge_enabled: true,
        };

        // Cursor at (23, 4): within radius of the line (projection (23,3)).
        // With grid 10, the line crosses vertical grid lines at x=20 and
        // x=30. Nearest to the cursor is (20, 3). The line is constant in y,
        // so there are no horizontal-grid crossings.
        let (snapped, hit) = snap.resolve([23.0, 4.0], &scene, 1.0, &[]);
        assert_eq!(hit.unwrap().kind, SnapKind::EdgeGrid);
        assert!((snapped[0] - 20.0).abs() < 1e-9, "x={}", snapped[0]);
        assert!((snapped[1] - 3.0).abs() < 1e-9, "y={}", snapped[1]);
    }

    #[test]
    fn edge_only_slides_freely_when_grid_disabled() {
        let mut scene = Scene::new();
        let parent = scene.root();
        scene
            .insert(parent, node_with_line([0.0, 3.0], [100.0, 3.0]))
            .unwrap();

        // Same geometry, grid OFF: edge snap returns the free projection,
        // i.e. the cursor slides smoothly along the line.
        let snap = SnapSettings {
            grid_enabled: false,
            grid_size: 10.0,
            vertex_enabled: false,
            edge_enabled: true,
        };
        let (snapped, hit) = snap.resolve([23.0, 4.0], &scene, 1.0, &[]);
        assert_eq!(hit.unwrap().kind, SnapKind::Edge);
        assert!((snapped[0] - 23.0).abs() < 1e-9, "x={}", snapped[0]);
        assert!((snapped[1] - 3.0).abs() < 1e-9, "y={}", snapped[1]);
    }

    #[test]
    fn edge_grid_snaps_to_diagonal_crossing() {
        let mut scene = Scene::new();
        let parent = scene.root();
        // Diagonal line y = x from (0,0) to (100,100).
        scene
            .insert(parent, node_with_line([0.0, 0.0], [100.0, 100.0]))
            .unwrap();

        let snap = SnapSettings {
            grid_enabled: true,
            grid_size: 10.0,
            vertex_enabled: false,
            edge_enabled: true,
        };

        // Cursor near (47, 53): projection onto y=x is (50, 50), which is a
        // grid crossing on both axes (x=50 and y=50). Expect exactly (50,50).
        let (snapped, hit) = snap.resolve([47.0, 53.0], &scene, 1.0, &[]);
        assert_eq!(hit.unwrap().kind, SnapKind::EdgeGrid);
        assert!((snapped[0] - 50.0).abs() < 1e-9, "x={}", snapped[0]);
        assert!((snapped[1] - 50.0).abs() < 1e-9, "y={}", snapped[1]);
    }

    #[test]
    fn edge_grid_falls_back_to_free_point_without_crossing() {
        let mut scene = Scene::new();
        let parent = scene.root();
        // Short horizontal line at y = 3 spanning x ∈ [1, 9]: with grid 10
        // it crosses no vertical grid line, and y is constant (no horizontal
        // crossing either), so there is no line×grid intersection on it.
        scene
            .insert(parent, node_with_line([1.0, 3.0], [9.0, 3.0]))
            .unwrap();

        let snap = SnapSettings {
            grid_enabled: true,
            grid_size: 10.0,
            vertex_enabled: false,
            edge_enabled: true,
        };

        // Projection is (5, 3); no crossing exists → fall back to the free
        // edge point rather than yanking off the line.
        let (snapped, hit) = snap.resolve([5.0, 4.0], &scene, 1.0, &[]);
        assert_eq!(hit.unwrap().kind, SnapKind::Edge);
        assert!((snapped[0] - 5.0).abs() < 1e-9, "x={}", snapped[0]);
        assert!((snapped[1] - 3.0).abs() < 1e-9, "y={}", snapped[1]);
    }

    #[test]
    fn grid_only_falls_back_when_geometry_snap_disabled() {
        let scene = Scene::new();
        let snap = SnapSettings {
            grid_enabled: true,
            grid_size: 5.0,
            vertex_enabled: false,
            edge_enabled: false,
        };
        let (snapped, hit) = snap.resolve([7.4, 12.6], &scene, 1.0, &[]);
        assert_eq!(snapped, [5.0, 15.0]);
        assert_eq!(hit.unwrap().kind, SnapKind::Grid);
    }

    #[test]
    fn nothing_enabled_returns_input_unchanged() {
        let scene = Scene::new();
        let snap = SnapSettings {
            grid_enabled: false,
            grid_size: 1.0,
            vertex_enabled: false,
            edge_enabled: false,
        };
        let (snapped, hit) = snap.resolve([3.7, 4.2], &scene, 1.0, &[]);
        assert_eq!(snapped, [3.7, 4.2]);
        assert!(hit.is_none());
    }

    #[test]
    fn radius_scales_inversely_with_zoom() {
        let mut scene = Scene::new();
        let parent = scene.root();
        scene
            .insert(parent, node_with_line([0.0, 0.0], [10.0, 0.0]))
            .unwrap();

        let snap = SnapSettings {
            grid_enabled: false,
            grid_size: 1.0,
            vertex_enabled: true,
            edge_enabled: false,
        };

        // At zoom 1.0, radius is 8 px → cursor 4 units away snaps.
        let (_, hit_in) = snap.resolve([14.0, 0.0], &scene, 1.0, &[]);
        assert!(hit_in.is_some());

        // At zoom 4.0, radius is 2 units → 4 units away should NOT snap.
        let (snapped_out, hit_out) = snap.resolve([14.0, 0.0], &scene, 4.0, &[]);
        assert!(hit_out.is_none());
        assert_eq!(snapped_out, [14.0, 0.0]);
    }
}
