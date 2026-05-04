//! Snapping configuration and resolution. Editor preference only —
//! not stored in SVG.
//!
//! Three snap targets, in priority order:
//! 1. Vertices/anchors of other paths (`vertex_enabled`).
//! 2. Closest point on a path edge (`edge_enabled`).
//! 3. Grid (`grid_enabled`).
//!
//! All three sources use the same screen-pixel hit radius. The most
//! specific hit (vertex > edge > grid) wins. The resolved indicator
//! is returned alongside the snapped position so the renderer can
//! draw a small marker on the snap target.

use vector_geom::{Point, Segment};
use vector_scene::{NodeData, NodeId, Scene};

/// Snap-radius in screen pixels — divided by zoom to get a canvas-space
/// radius. Matches `HIT_RADIUS_SCREEN_PX` in vector-tools so snap reach
/// matches what the user can directly click.
const SNAP_RADIUS_SCREEN_PX: f64 = 8.0;

/// What kind of geometry snap fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapKind {
    /// Snapped to an anchor or control point of another path.
    Vertex,
    /// Snapped to the nearest point on a path edge.
    Edge,
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

        // 2. Edge snap.
        if self.edge_enabled
            && let Some(p) = closest_edge_point(scene, target, radius, exclude)
        {
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
        vector_geom::Affine::IDENTITY,
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

fn closest_edge_point(
    scene: &Scene,
    target: Point,
    radius: f64,
    exclude: &[NodeId],
) -> Option<Point> {
    let mut best: Option<(Point, f64)> = None;
    let root = scene.root();
    scene.walk_depth_first(
        root,
        vector_geom::Affine::IDENTITY,
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

            for subpath in &path.subpaths {
                let mut current = subpath.start;
                for seg in &subpath.segments {
                    let (t, _, _) = seg.closest_point(current, local_target);
                    let local_pt = seg.eval_at(current, t);
                    let world_pt = if world.is_identity() {
                        local_pt
                    } else {
                        world.apply(local_pt)
                    };
                    let d = target.distance(world_pt);
                    if d < radius && best.as_ref().is_none_or(|(_, bd)| d < *bd) {
                        best = Some((world_pt, d));
                    }
                    current = seg.endpoint();
                }
                // Implicit closing edge for closed subpaths.
                if subpath.closed && !subpath.segments.is_empty() && current != subpath.start {
                    let closing = Segment::Line { to: subpath.start };
                    let (t, _, _) = closing.closest_point(current, local_target);
                    let local_pt = closing.eval_at(current, t);
                    let world_pt = if world.is_identity() {
                        local_pt
                    } else {
                        world.apply(local_pt)
                    };
                    let d = target.distance(world_pt);
                    if d < radius && best.as_ref().is_none_or(|(_, bd)| d < *bd) {
                        best = Some((world_pt, d));
                    }
                }
            }
            true
        },
    );
    best.map(|(p, _)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_geom::{Path, Point as P, Segment, SubPath, VertexMode};
    use vector_scene::Node;

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
