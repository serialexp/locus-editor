use locus_geom::{Affine, Bounds, Point, Segment, VertexMode};
use locus_scene::{
    FillRule, Gradient, GradientKind, GroupKind, NodeData, NodeId, Paint, PaintRef, Scene,
};

/// Whether `id` is a (transitive) descendant of `ancestor`. Walks the
/// parent chain — bounded by tree depth, which is small in practice.
/// Returns `false` when `id == ancestor` (a node is not its own
/// descendant).
fn scene_node_is_descendant(scene: &Scene, id: NodeId, ancestor: NodeId) -> bool {
    let mut cur = id;
    while let Some(parent) = scene.parent(cur) {
        if parent == ancestor {
            return true;
        }
        cur = parent;
    }
    false
}

/// Compute the world-space bounding box for a node's visual content,
/// including the visible stroke area around the geometry.
///
/// For Boolean groups this returns the bounds of the computed result
/// path rather than `EMPTY` — so a click on the boolean's visible shape
/// can hit the group node itself rather than falling through to nothing.
fn node_bounds(scene: &Scene, id: NodeId, data: &NodeData, world: Affine) -> Bounds {
    if let NodeData::Group {
        kind: GroupKind::Boolean { .. },
        ..
    } = data
    {
        let computed = locus_bool::compute_boolean_group_path(scene, id);
        let local = locus_tess::path_visual_bounds(&computed, true, None);
        return local.transform(world);
    }
    data.visual_bounds(world)
}

/// Fill-aware single-node hit test. `target` is in world coordinates.
///
/// For Path nodes with a fill: the click must lie inside the path's
/// filled area (winding number under the path's fill rule). For Boolean
/// groups: tests against the resolved boolean result path. For other
/// node types — Text, Raster, plain Groups, paths without fill — we
/// fall back to bbox containment so the node stays selectable. (A
/// stroke-only path's interior should arguably miss; we keep it
/// pickable for now to match prior behaviour. A future stroke-distance
/// test could refine this.)
fn node_hit_at_point(
    scene: &Scene,
    id: NodeId,
    data: &NodeData,
    world: Affine,
    target: Point,
) -> bool {
    // Cheap pre-filter — if the world bbox doesn't contain the click,
    // nothing inside this node could possibly hit. This also avoids the
    // cost of a kurbo conversion for thousands of off-cursor candidates.
    let bounds = node_bounds(scene, id, data, world);
    if bounds.is_empty() || !bounds.contains_point(target) {
        return false;
    }

    match data {
        NodeData::Path { path, style } => {
            let Some(fill) = style.fill.as_ref() else {
                // No fill → bbox match is the best signal we have without
                // an explicit stroke-distance test. Keeps stroke-only
                // shapes pickable, matching prior behaviour.
                return true;
            };
            let even_odd = matches!(fill.rule, FillRule::EvenOdd);
            let local = if world.is_identity() {
                target
            } else {
                let Some(inv) = world.inverse() else {
                    // Degenerate transform — bbox is the best we can do.
                    return true;
                };
                inv.apply(target)
            };
            path.contains_point(local, even_odd)
        }
        NodeData::Group {
            kind: GroupKind::Boolean { .. },
            ..
        } => {
            // Test against the resolved boolean path. We don't have access
            // to the renderer's `bool_path_cache` from here, so this
            // recomputes — fine on click (rare) but worth noting.
            let computed = locus_bool::compute_boolean_group_path(scene, id);
            let local = if world.is_identity() {
                target
            } else {
                let Some(inv) = world.inverse() else {
                    return true;
                };
                inv.apply(target)
            };
            // Boolean output is a single contour; nonzero rule is
            // appropriate (and matches how it's tessellated for fill).
            computed.contains_point(local, false)
        }
        // Text, Raster, plain Groups: bbox is the best we have. Plain
        // groups would normally fall through to a child anyway because
        // the walk recurses into them — so this branch is mostly the
        // text/raster path.
        _ => true,
    }
}

/// Which specific point within a segment we're referring to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PointKind {
    /// The `start` field of a SubPath.
    SubpathStart,
    /// The `to` (endpoint) of a segment.
    Endpoint,
    /// The single control point of a Quad segment.
    QuadCtrl,
    /// First control point of a Cubic segment.
    CubicCtrl1,
    /// Second control point of a Cubic segment.
    CubicCtrl2,
}

/// A unique reference to a single editable point in the scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VertexRef {
    /// Which node in the scene graph.
    pub node: NodeId,
    /// Which subpath within the path.
    pub subpath: usize,
    /// Which segment within the subpath (ignored for `SubpathStart`).
    pub segment: usize,
    /// Which point within the segment.
    pub kind: PointKind,
}

impl VertexRef {
    /// Read the current position of this vertex from the scene.
    pub fn get_position(&self, scene: &Scene) -> Option<Point> {
        let node = scene.get(self.node)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.get(self.subpath)?;

        match self.kind {
            PointKind::SubpathStart => Some(subpath.start),
            PointKind::Endpoint => {
                let seg = subpath.segments.get(self.segment)?;
                Some(seg.endpoint())
            }
            PointKind::QuadCtrl => match subpath.segments.get(self.segment)? {
                Segment::Quad { ctrl, .. } => Some(*ctrl),
                _ => None,
            },
            PointKind::CubicCtrl1 => match subpath.segments.get(self.segment)? {
                Segment::Cubic { ctrl1, .. } => Some(*ctrl1),
                _ => None,
            },
            PointKind::CubicCtrl2 => match subpath.segments.get(self.segment)? {
                Segment::Cubic { ctrl2, .. } => Some(*ctrl2),
                _ => None,
            },
        }
    }

    /// Move this vertex by a delta in scene coordinates.
    pub fn translate(&self, scene: &mut Scene, dx: f64, dy: f64) {
        let subpath_idx = self.subpath;
        let segment_idx = self.segment;
        let kind = self.kind;
        scene.with_path_data_mut(self.node, |path| {
            let Some(subpath) = path.subpaths.get_mut(subpath_idx) else {
                return;
            };

            match kind {
                PointKind::SubpathStart => {
                    subpath.start.x += dx;
                    subpath.start.y += dy;
                }
                PointKind::Endpoint => {
                    if let Some(seg) = subpath.segments.get_mut(segment_idx) {
                        translate_endpoint(seg, dx, dy);
                    }
                }
                PointKind::QuadCtrl => {
                    if let Some(Segment::Quad { ctrl, .. }) = subpath.segments.get_mut(segment_idx)
                    {
                        ctrl.x += dx;
                        ctrl.y += dy;
                    }
                }
                PointKind::CubicCtrl1 => {
                    if let Some(Segment::Cubic { ctrl1, .. }) =
                        subpath.segments.get_mut(segment_idx)
                    {
                        ctrl1.x += dx;
                        ctrl1.y += dy;
                    }
                }
                PointKind::CubicCtrl2 => {
                    if let Some(Segment::Cubic { ctrl2, .. }) =
                        subpath.segments.get_mut(segment_idx)
                    {
                        ctrl2.x += dx;
                        ctrl2.y += dy;
                    }
                }
            }
        });
    }
}

/// After moving a control point, enforce the vertex mode constraint on
/// the opposite handle at the same anchor vertex.
pub fn enforce_vertex_constraint(vr: &VertexRef, scene: &mut Scene) {
    let subpath_idx = vr.subpath;
    let segment_idx = vr.segment;
    let kind = vr.kind;
    scene.with_path_data_mut(vr.node, |path| {
        let Some(subpath) = path.subpaths.get_mut(subpath_idx) else {
            return;
        };

        match kind {
            PointKind::CubicCtrl1 => {
                // ctrl1 of segment[seg] belongs to anchor at vertex_modes[seg].
                let anchor_mode = subpath
                    .vertex_modes
                    .get(segment_idx)
                    .copied()
                    .unwrap_or(VertexMode::Corner);
                if anchor_mode == VertexMode::Corner {
                    return;
                }

                // The anchor point is the endpoint of segment[seg-1], or subpath.start.
                let anchor = if segment_idx == 0 {
                    subpath.start
                } else {
                    subpath.segments[segment_idx - 1].endpoint()
                };

                let ctrl1 = match &subpath.segments[segment_idx] {
                    Segment::Cubic { ctrl1, .. } => *ctrl1,
                    _ => return,
                };

                // The opposite handle is ctrl2 of the previous segment (incoming to anchor).
                // For closed paths, segment 0's anchor wraps to the last segment.
                let prev_idx = if segment_idx > 0 {
                    Some(segment_idx - 1)
                } else if subpath.closed && !subpath.segments.is_empty() {
                    Some(subpath.segments.len() - 1)
                } else {
                    None
                };
                let Some(prev_idx) = prev_idx else {
                    return;
                };
                let prev = &mut subpath.segments[prev_idx];
                match prev {
                    Segment::Cubic { ctrl2, .. } => {
                        mirror_handle(anchor, ctrl1, ctrl2, anchor_mode);
                    }
                    Segment::Quad { ctrl, .. } => {
                        mirror_handle(anchor, ctrl1, ctrl, anchor_mode);
                    }
                    _ => {}
                }
            }
            PointKind::CubicCtrl2 => {
                // ctrl2 of segment[seg] belongs to anchor at vertex_modes[seg+1].
                let anchor_mode = subpath
                    .vertex_modes
                    .get(segment_idx + 1)
                    .copied()
                    .unwrap_or(VertexMode::Corner);
                if anchor_mode == VertexMode::Corner {
                    return;
                }

                let anchor = subpath.segments[segment_idx].endpoint();
                let ctrl2 = match &subpath.segments[segment_idx] {
                    Segment::Cubic { ctrl2, .. } => *ctrl2,
                    _ => return,
                };

                // The opposite handle is ctrl1 of the next segment (outgoing from anchor).
                // For closed paths, the last segment's anchor wraps to segment 0.
                let next_idx = segment_idx + 1;
                let wrap_idx = if next_idx < subpath.segments.len() {
                    Some(next_idx)
                } else if subpath.closed && !subpath.segments.is_empty() {
                    Some(0)
                } else {
                    None
                };
                let Some(wrap_idx) = wrap_idx else {
                    return;
                };
                let next = &mut subpath.segments[wrap_idx];
                match next {
                    Segment::Cubic { ctrl1, .. } => {
                        mirror_handle(anchor, ctrl2, ctrl1, anchor_mode);
                    }
                    Segment::Quad { ctrl, .. } => {
                        mirror_handle(anchor, ctrl2, ctrl, anchor_mode);
                    }
                    _ => {}
                }
            }
            PointKind::QuadCtrl => {
                // Quad control point affects both the "from" and "to" anchors.
                // For simplicity, constrain the "from" anchor's opposite handle
                // (incoming) and the "to" anchor's opposite handle (outgoing).
                // This is complex for quads; skip for now since quads are rare
                // in practice (most curves are cubics).
            }
            _ => {
                // Endpoints and SubpathStart don't trigger handle constraints.
            }
        }
    });
}

/// Fraction of segment length to place initial cubic handles at.
const HANDLE_FRACTION: f64 = 1.0 / 3.0;

/// Ensure that the segments adjacent to anchor `mode_idx` are cubics with
/// handles visibly offset from the anchor. Converts Lines to Cubics and
/// spreads handles that sit directly on the anchor.
///
/// Only the control point belonging to `mode_idx` is spread out; the control
/// point belonging to the neighboring vertex stays collapsed at the neighbor's
/// position so it doesn't create a spurious visible handle there.
///
/// Handles wrap-around for closed subpaths: vertex 0's incoming is the last
/// segment, and the last vertex's outgoing is segment 0.
fn ensure_cubic_handles(subpath: &mut locus_geom::SubPath, mode_idx: usize) {
    let n_segs = subpath.segments.len();
    if n_segs == 0 {
        return;
    }

    // The anchor point at this mode index.
    let anchor = if mode_idx == 0 {
        subpath.start
    } else {
        subpath.segments[mode_idx - 1].endpoint()
    };

    // Determine incoming and outgoing segment indices.
    // For open paths, first/last vertex may lack one side.
    // For closed paths, they wrap around.
    let incoming_idx: Option<usize> = if mode_idx > 0 {
        Some(mode_idx - 1)
    } else if subpath.closed && n_segs > 0 {
        Some(n_segs - 1) // wrap: last segment leads into start
    } else {
        None
    };

    let outgoing_idx: Option<usize> = if mode_idx < n_segs {
        Some(mode_idx)
    } else if subpath.closed && n_segs > 0 {
        Some(0) // wrap: segment 0 leads out from the last vertex
    } else {
        None
    };

    // --- Incoming segment: our handle is ctrl2 ---
    // ctrl1 belongs to the previous vertex → collapse it at `from`.
    if let Some(seg_idx) = incoming_idx {
        let from = if seg_idx == 0 {
            subpath.start
        } else {
            subpath.segments[seg_idx - 1].endpoint()
        };
        let seg = &mut subpath.segments[seg_idx];
        match seg {
            Segment::Line { to } => {
                let dx = to.x - from.x;
                let dy = to.y - from.y;
                *seg = Segment::Cubic {
                    ctrl1: from,
                    ctrl2: Point::new(
                        anchor.x - dx * HANDLE_FRACTION,
                        anchor.y - dy * HANDLE_FRACTION,
                    ),
                    to: *to,
                };
            }
            Segment::Quad { ctrl, to } => {
                // Degree-elevate: Quad → Cubic (exact shape preservation).
                // ctrl1 = from + 2/3*(ctrl - from)
                // ctrl2 = to   + 2/3*(ctrl - to)
                let c1 = Point::new(
                    from.x + 2.0 / 3.0 * (ctrl.x - from.x),
                    from.y + 2.0 / 3.0 * (ctrl.y - from.y),
                );
                let c2 = Point::new(
                    to.x + 2.0 / 3.0 * (ctrl.x - to.x),
                    to.y + 2.0 / 3.0 * (ctrl.y - to.y),
                );
                *seg = Segment::Cubic {
                    ctrl1: c1,
                    ctrl2: c2,
                    to: *to,
                };
            }
            Segment::Cubic { ctrl2, .. } => {
                let d = ((ctrl2.x - anchor.x).powi(2) + (ctrl2.y - anchor.y).powi(2)).sqrt();
                if d < 1e-6 {
                    let dx = anchor.x - from.x;
                    let dy = anchor.y - from.y;
                    ctrl2.x = anchor.x - dx * HANDLE_FRACTION;
                    ctrl2.y = anchor.y - dy * HANDLE_FRACTION;
                }
            }
            _ => {}
        }
    }

    // --- Outgoing segment: our handle is ctrl1 ---
    // ctrl2 belongs to the next vertex → collapse it at `to`.
    if let Some(seg_idx) = outgoing_idx {
        let from = anchor;
        let seg = &mut subpath.segments[seg_idx];
        match seg {
            Segment::Line { to } => {
                let dx = to.x - from.x;
                let dy = to.y - from.y;
                *seg = Segment::Cubic {
                    ctrl1: Point::new(from.x + dx * HANDLE_FRACTION, from.y + dy * HANDLE_FRACTION),
                    ctrl2: *to,
                    to: *to,
                };
            }
            Segment::Quad { ctrl, to } => {
                // Degree-elevate: Quad → Cubic (exact shape preservation).
                let c1 = Point::new(
                    from.x + 2.0 / 3.0 * (ctrl.x - from.x),
                    from.y + 2.0 / 3.0 * (ctrl.y - from.y),
                );
                let c2 = Point::new(
                    to.x + 2.0 / 3.0 * (ctrl.x - to.x),
                    to.y + 2.0 / 3.0 * (ctrl.y - to.y),
                );
                *seg = Segment::Cubic {
                    ctrl1: c1,
                    ctrl2: c2,
                    to: *to,
                };
            }
            Segment::Cubic { ctrl1, .. } => {
                let d = ((ctrl1.x - anchor.x).powi(2) + (ctrl1.y - anchor.y).powi(2)).sqrt();
                if d < 1e-6 {
                    let to = subpath.segments[seg_idx].endpoint();
                    let dx = to.x - anchor.x;
                    let dy = to.y - anchor.y;
                    if let Segment::Cubic { ctrl1, .. } = &mut subpath.segments[seg_idx] {
                        ctrl1.x = anchor.x + dx * HANDLE_FRACTION;
                        ctrl1.y = anchor.y + dy * HANDLE_FRACTION;
                    }
                }
            }
            _ => {}
        }
    }

    // Enforce the constraint so the two handles are consistent with the mode.
    // Use the outgoing handle as the "source" to mirror/constrain the incoming.
    let new_mode = subpath
        .vertex_modes
        .get(mode_idx)
        .copied()
        .unwrap_or(VertexMode::Corner);
    if new_mode == VertexMode::Corner {
        return;
    }

    if let (Some(out_idx), Some(in_idx)) = (outgoing_idx, incoming_idx)
        && let Segment::Cubic {
            ctrl1: out_ctrl, ..
        } = subpath.segments[out_idx]
        && let Segment::Cubic { ctrl2: in_ctrl, .. } = subpath.segments[in_idx]
    {
        // Pick the LONGER of the two handles as the source for the mirror so
        // we don't shrink a meaningful handle to match a collapsed-then-just-
        // spread one. This keeps the visible geometry stable across mode
        // switches (Symmetric especially).
        let out_d = (out_ctrl.x - anchor.x).hypot(out_ctrl.y - anchor.y);
        let in_d = (in_ctrl.x - anchor.x).hypot(in_ctrl.y - anchor.y);
        if out_d >= in_d {
            if let Segment::Cubic { ctrl2, .. } = &mut subpath.segments[in_idx] {
                mirror_handle(anchor, out_ctrl, ctrl2, new_mode);
            }
        } else if let Segment::Cubic { ctrl1, .. } = &mut subpath.segments[out_idx] {
            mirror_handle(anchor, in_ctrl, ctrl1, new_mode);
        }
    }
}

/// Mirror `opposite` around `anchor` to match the direction (and optionally
/// distance) of `moved`. Mutates `opposite` in place.
fn mirror_handle(anchor: Point, moved: Point, opposite: &mut Point, mode: VertexMode) {
    let dx = moved.x - anchor.x;
    let dy = moved.y - anchor.y;

    match mode {
        VertexMode::Symmetric => {
            // Mirror: same distance, opposite direction.
            opposite.x = anchor.x - dx;
            opposite.y = anchor.y - dy;
        }
        VertexMode::Smooth => {
            // Keep opposite's distance but constrain direction to be opposite.
            let moved_len = (dx * dx + dy * dy).sqrt();
            if moved_len < 1e-12 {
                return;
            }
            let opp_dx = opposite.x - anchor.x;
            let opp_dy = opposite.y - anchor.y;
            let mut opp_len = (opp_dx * opp_dx + opp_dy * opp_dy).sqrt();
            // If the opposite handle is collapsed at the anchor we'd compute
            // `anchor - 0 = anchor` and leave it invisible. Fall back to the
            // moved handle's length so the user sees a usable handle to grab.
            if opp_len < 1e-12 {
                opp_len = moved_len;
            }
            // Opposite direction, length = opp_len.
            opposite.x = anchor.x - dx / moved_len * opp_len;
            opposite.y = anchor.y - dy / moved_len * opp_len;
        }
        VertexMode::Corner => {} // No constraint.
    }
}

fn translate_endpoint(seg: &mut Segment, dx: f64, dy: f64) {
    match seg {
        Segment::Line { to } => {
            to.x += dx;
            to.y += dy;
        }
        Segment::Quad { to, .. } => {
            to.x += dx;
            to.y += dy;
        }
        Segment::Cubic { to, .. } => {
            to.x += dx;
            to.y += dy;
        }
        Segment::Arc { to, .. } => {
            to.x += dx;
            to.y += dy;
        }
    }
}

/// Which scale handle on the bounding box is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl ScaleHandle {
    /// All eight handles in order.
    pub const ALL: [Self; 8] = [
        Self::TopLeft,
        Self::Top,
        Self::TopRight,
        Self::Right,
        Self::BottomRight,
        Self::Bottom,
        Self::BottomLeft,
        Self::Left,
    ];

    /// Position of this handle on a bounding box, as (x, y).
    pub fn position(self, bounds: Bounds) -> Point {
        let cx = (bounds.min.x + bounds.max.x) * 0.5;
        let cy = (bounds.min.y + bounds.max.y) * 0.5;
        match self {
            Self::TopLeft => bounds.min,
            Self::Top => Point::new(cx, bounds.min.y),
            Self::TopRight => Point::new(bounds.max.x, bounds.min.y),
            Self::Right => Point::new(bounds.max.x, cy),
            Self::BottomRight => bounds.max,
            Self::Bottom => Point::new(cx, bounds.max.y),
            Self::BottomLeft => Point::new(bounds.min.x, bounds.max.y),
            Self::Left => Point::new(bounds.min.x, cy),
        }
    }

    /// The anchor point for scaling — the opposite corner/edge.
    pub fn anchor(self, bounds: Bounds) -> Point {
        match self {
            Self::TopLeft => bounds.max,
            Self::Top => Point::new((bounds.min.x + bounds.max.x) * 0.5, bounds.max.y),
            Self::TopRight => Point::new(bounds.min.x, bounds.max.y),
            Self::Right => Point::new(bounds.min.x, (bounds.min.y + bounds.max.y) * 0.5),
            Self::BottomRight => bounds.min,
            Self::Bottom => Point::new((bounds.min.x + bounds.max.x) * 0.5, bounds.min.y),
            Self::BottomLeft => Point::new(bounds.max.x, bounds.min.y),
            Self::Left => Point::new(bounds.max.x, (bounds.min.y + bounds.max.y) * 0.5),
        }
    }

    /// Whether this handle scales horizontally.
    pub fn scales_x(self) -> bool {
        !matches!(self, Self::Top | Self::Bottom)
    }

    /// Whether this handle scales vertically.
    pub fn scales_y(self) -> bool {
        !matches!(self, Self::Left | Self::Right)
    }
}

/// What the select tool is currently doing.
enum DragMode {
    /// Not dragging.
    Idle,
    /// Dragging selected vertices. Stores last canvas position.
    MoveVertices { prev: [f64; 2] },
    /// Dragging a gradient geometry handle (start / end / center / radius
    /// edge / focal / stop). Stores the handle being moved.
    MoveGradientHandle { handle: GradientHandleRef },
    /// Dragging entire objects by translating their transforms.
    MoveObjects { prev: [f64; 2] },
    /// Rotating selected objects around their combined center.
    RotateObjects {
        /// Center of rotation in canvas (world) coordinates.
        center: [f64; 2],
        /// The angle (radians) from center to the initial mouse press.
        /// Kept for future snap-to-angle support (e.g. 15° increments).
        #[expect(dead_code)]
        start_angle: f64,
        /// Accumulated rotation applied so far (for incremental updates).
        prev_angle: f64,
    },
    /// Scaling selected objects by dragging a bounding box handle.
    ScaleObjects {
        /// Which handle is being dragged.
        handle: ScaleHandle,
        /// The fixed anchor point in world space (opposite the dragged handle).
        anchor: [f64; 2],
        /// The original bounding box at drag start.
        orig_bounds: Bounds,
        /// Original local transforms for each selected node at drag start,
        /// so we can recompute the absolute scale each frame.
        orig_transforms: Vec<(NodeId, Affine)>,
    },
    /// Drawing a marquee rectangle. Stores the anchor corner in canvas coords
    /// and whether shift was held at the start.
    Marquee {
        anchor: [f64; 2],
        current: [f64; 2],
        shift: bool,
    },
}

/// Whether we're showing the bounding box (object level) or individual
/// vertex handles (node editing level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionMode {
    /// Show bounding box around selected objects. Dragging moves the whole object.
    Object,
    /// Show individual vertex handles. Dragging moves vertices.
    Node,
}

/// Which point on a referenced gradient is being targeted by an on-canvas
/// handle. All variants identify a single editable handle in the
/// gradient's user space; the handle's world position is `gradient.transform
/// * point`. (Per SVG `userSpaceOnUse` semantics — the path's own transform
///   does not apply.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GradientHandlePoint {
    /// Linear gradient — the `start` endpoint.
    LinearStart,
    /// Linear gradient — the `end` endpoint.
    LinearEnd,
    /// Radial gradient — the `center` point.
    RadialCenter,
    /// Radial gradient — a draggable point on the radius circle (used to
    /// adjust the radius). Anchored at `center + (radius, 0)` in gradient
    /// space.
    RadialRadiusEdge,
    /// Radial gradient — the focal point. Only displayed when the focal
    /// differs from the center.
    RadialFocal,
    /// A draggable colour stop, positioned along the gradient's parametric
    /// axis at `stops[index].offset`. Drag is constrained to the axis;
    /// the offset is clamped to neighbouring stops so they cannot cross.
    Stop(usize),
}

/// A specific gradient handle in a specific path's selection. We store
/// `owner` (the path that caused this handle to render) alongside the
/// `paint` (the gradient node in defs) so a single canvas-side handle drag
/// has a 1:1 mapping back to the panel's selection — useful when a single
/// gradient is shared by many shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GradientHandleRef {
    /// The gradient node in the defs subtree.
    pub paint: NodeId,
    /// The path / boolean group / text node whose selection brought this
    /// handle on screen.
    pub owner: NodeId,
    /// Which point on the gradient.
    pub point: GradientHandlePoint,
}

/// State for the select tool — tracks object and vertex selection.
///
/// Selection is two-level:
/// 1. **Object selection** (`selected_nodes`) — which paths/groups are active.
///    In `Object` mode, a bounding box is shown around them.
/// 2. **Vertex selection** (`selected`) — individual control points within
///    object-selected nodes, for direct manipulation. Only active in `Node` mode.
pub struct SelectState {
    /// Object-level selection: which nodes are "active".
    pub selected_nodes: Vec<NodeId>,
    /// Nodes inside the active marquee (previewed, not yet committed).
    pub marquee_preview_nodes: Vec<NodeId>,
    /// Currently selected vertices (within object-selected nodes).
    pub selected: Vec<VertexRef>,
    /// Vertex currently under the cursor (only within object-selected nodes).
    pub hovered: Option<VertexRef>,
    /// World-space position of a ghost vertex on an edge near the cursor.
    /// Shown when the cursor is near an edge but not near an existing vertex,
    /// indicating where a double-click would insert a new point.
    pub edge_hover_point: Option<Point>,
    /// The segment currently under the cursor in node mode (if any).
    /// Drives a segment-highlight overlay so the user can see which segment
    /// a right-click context menu would act on. Always `None` outside Node
    /// mode and when a vertex handle is hovered (vertex hover wins).
    pub edge_hover_hit: Option<EdgeHit>,
    /// Gradient handle currently under the cursor — when an Object-mode
    /// selected path has a gradient fill or stroke. Vertex handles still
    /// win the priority order in Node mode.
    pub gradient_hovered: Option<GradientHandleRef>,
    /// Current drag mode.
    drag_mode: DragMode,
    /// Whether we're in object mode (bounding box) or node mode (vertex handles).
    pub mode: SelectionMode,
    /// Group isolation stack — outermost-first list of Regular groups the
    /// user has "entered" via double-click. The innermost (last) is the
    /// current scope; clicks resolve to its direct children, and marquee
    /// only picks direct children. Empty = top-level scope (the scene
    /// root). Stored as a stack so that deleting an entered group can
    /// auto-fall-back to the next outer scope rather than dropping all
    /// the way to the top.
    group_scope_stack: Vec<NodeId>,
}

impl Default for SelectState {
    fn default() -> Self {
        Self {
            selected_nodes: Vec::new(),
            marquee_preview_nodes: Vec::new(),
            selected: Vec::new(),
            hovered: None,
            edge_hover_point: None,
            edge_hover_hit: None,
            gradient_hovered: None,
            drag_mode: DragMode::Idle,
            mode: SelectionMode::Object,
            group_scope_stack: Vec::new(),
        }
    }
}

/// Hit-test radius in screen pixels. Divided by zoom to get canvas-space radius.
const HIT_RADIUS_SCREEN_PX: f64 = 8.0;

/// Edge-interaction hit radius in screen pixels. Wider than the general
/// handle radius because path edges are 1-2 px thin lines, so users need a
/// generous corridor to land on them — landing on a 4 px square handle is
/// much easier than landing on a stroke. Drives the segment-hover highlight,
/// the ghost-insertion ball, and edge-based context-menu hits.
const EDGE_HIT_RADIUS_SCREEN_PX: f64 = 18.0;

/// A hit on a path edge (between vertices), used for inserting new points.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeHit {
    /// Which node.
    pub node: NodeId,
    /// Which subpath within the path.
    pub subpath: usize,
    /// Which segment within the subpath.
    pub segment: usize,
    /// Parameter t ∈ [0, 1] along the segment.
    pub t: f64,
}

impl SelectState {
    // ── Group isolation scope ────────────────────────────────────────

    /// The currently entered Regular group, or `None` for top-level
    /// scope. When set, clicks resolve to direct children of this group
    /// rather than to top-level groups, and marquee only picks direct
    /// children. Inkscape calls this "isolation mode".
    pub fn group_scope(&self) -> Option<NodeId> {
        self.group_scope_stack.last().copied()
    }

    /// Returns the node ID whose direct children are the current scope's
    /// pick targets — the innermost entered group, or the scene root if
    /// no group is entered.
    fn scope_root(&self, scene: &Scene) -> NodeId {
        self.group_scope_stack
            .last()
            .copied()
            .unwrap_or_else(|| scene.root())
    }

    /// Walk up from `leaf` toward the current scope root and return the
    /// ancestor that is a *direct* child of the scope root — i.e. the
    /// outermost group whose contents the leaf belongs to. If the leaf
    /// is itself a direct child of the scope root, returns `leaf`. Used
    /// to implement "click selects the topmost group, not the leaf".
    pub fn resolve_pick_in_scope(&self, scene: &Scene, leaf: NodeId) -> NodeId {
        let scope_root = self.scope_root(scene);
        if leaf == scope_root {
            return leaf;
        }
        let mut current = leaf;
        while let Some(parent) = scene.parent(current) {
            if parent == scope_root {
                return current;
            }
            current = parent;
        }
        // The leaf isn't a descendant of the current scope — possible if
        // the scope was stale. Return the leaf and let validate_group_scope
        // clean up.
        leaf
    }

    /// Enter a Regular group: subsequent picks resolve relative to it
    /// and marquee selection picks only its direct children. Pushes the
    /// group onto the scope stack and clears any object/vertex selection
    /// from the outer scope. No-op if `id` is not a Regular group, isn't
    /// in the scene, or is already at the top of the stack.
    pub fn enter_group(&mut self, scene: &Scene, id: NodeId) -> bool {
        let Some(node) = scene.get(id) else {
            return false;
        };
        if !matches!(
            node.data,
            NodeData::Group {
                kind: GroupKind::Regular,
                ..
            }
        ) {
            return false;
        }
        if self.group_scope_stack.last() == Some(&id) {
            return false;
        }
        self.group_scope_stack.push(id);
        self.selected_nodes.clear();
        self.selected.clear();
        self.mode = SelectionMode::Object;
        true
    }

    /// Pop one level off the group scope stack. Returns true if the
    /// scope changed. Bound to Esc when nothing else needs it.
    pub fn exit_group_scope(&mut self) -> bool {
        if self.group_scope_stack.pop().is_some() {
            self.selected_nodes.clear();
            self.selected.clear();
            true
        } else {
            false
        }
    }

    /// Truncate the scope stack to `depth` entries. Used by the
    /// breadcrumb to jump to a specific level in one click — `depth = 0`
    /// returns to top-level scope; `depth = N` keeps the first N entered
    /// groups. No-op if `depth` is already the current depth or larger.
    /// Returns true if anything was popped.
    pub fn truncate_group_scope(&mut self, depth: usize) -> bool {
        if self.group_scope_stack.len() <= depth {
            return false;
        }
        self.group_scope_stack.truncate(depth);
        self.selected_nodes.clear();
        self.selected.clear();
        true
    }

    /// Read access to the entered-group stack, outermost-first. Used by
    /// the breadcrumb UI to render one segment per level.
    pub fn group_scope_path(&self) -> &[NodeId] {
        &self.group_scope_stack
    }

    /// Drop scope-stack entries whose group nodes no longer exist in the
    /// scene. Call after structural changes (delete, undo/redo) so a
    /// dangling scope falls back to the nearest still-living ancestor —
    /// or to top-level if none survives.
    pub fn validate_group_scope(&mut self, scene: &Scene) {
        let mut changed = false;
        while let Some(&id) = self.group_scope_stack.last() {
            let still_valid = scene.get(id).is_some_and(|n| {
                matches!(
                    n.data,
                    NodeData::Group {
                        kind: GroupKind::Regular,
                        ..
                    }
                )
            });
            if still_valid {
                break;
            }
            self.group_scope_stack.pop();
            changed = true;
        }
        if changed {
            self.selected_nodes.clear();
            self.selected.clear();
        }
    }

    // ── Vertex-level hit testing ─────────────────────────────────────

    /// Find the closest vertex to `canvas_pos` within a screen-space radius,
    /// but **only** among the given set of nodes.
    pub fn hit_test_in_nodes(
        scene: &Scene,
        canvas_pos: [f64; 2],
        zoom: f64,
        nodes: &[NodeId],
    ) -> Option<VertexRef> {
        let radius = HIT_RADIUS_SCREEN_PX / zoom;
        let target = Point::new(canvas_pos[0], canvas_pos[1]);
        let mut best: Option<(VertexRef, f64)> = None;

        Self::for_each_vertex_in_nodes(scene, nodes, &mut |vr, pt| {
            let d = target.distance(pt);
            if d < radius && best.as_ref().is_none_or(|(_, bd)| d < *bd) {
                best = Some((vr, d));
            }
        });

        best.map(|(vr, _)| vr)
    }

    /// Iterate over every editable vertex of the given nodes.
    /// Vertex positions are reported in world (canvas) coordinates,
    /// transformed by each node's accumulated world transform.
    fn for_each_vertex_in_nodes(
        scene: &Scene,
        nodes: &[NodeId],
        f: &mut impl FnMut(VertexRef, Point),
    ) {
        for &node_id in nodes {
            let Some(node) = scene.get(node_id) else {
                continue;
            };
            // Locked nodes are non-interactive: no vertex iteration / edge
            // hits / object hits land on them. They're still rendered.
            if !node.is_interactive() {
                continue;
            }
            let NodeData::Path { ref path, .. } = node.data else {
                continue;
            };

            let world = scene.world_transform(node_id);
            let xform = |p: Point| -> Point {
                if world.is_identity() {
                    p
                } else {
                    world.apply(p)
                }
            };

            for (sp_idx, subpath) in path.subpaths.iter().enumerate() {
                f(
                    VertexRef {
                        node: node_id,
                        subpath: sp_idx,
                        segment: 0,
                        kind: PointKind::SubpathStart,
                    },
                    xform(subpath.start),
                );

                for (seg_idx, seg) in subpath.segments.iter().enumerate() {
                    f(
                        VertexRef {
                            node: node_id,
                            subpath: sp_idx,
                            segment: seg_idx,
                            kind: PointKind::Endpoint,
                        },
                        xform(seg.endpoint()),
                    );

                    match seg {
                        Segment::Quad { ctrl, .. } => {
                            f(
                                VertexRef {
                                    node: node_id,
                                    subpath: sp_idx,
                                    segment: seg_idx,
                                    kind: PointKind::QuadCtrl,
                                },
                                xform(*ctrl),
                            );
                        }
                        Segment::Cubic { ctrl1, ctrl2, .. } => {
                            f(
                                VertexRef {
                                    node: node_id,
                                    subpath: sp_idx,
                                    segment: seg_idx,
                                    kind: PointKind::CubicCtrl1,
                                },
                                xform(*ctrl1),
                            );
                            f(
                                VertexRef {
                                    node: node_id,
                                    subpath: sp_idx,
                                    segment: seg_idx,
                                    kind: PointKind::CubicCtrl2,
                                },
                                xform(*ctrl2),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // ── Object-level hit testing ─────────────────────────────────────

    /// Find the front-most visible node whose **visible filled area**
    /// contains `canvas_pos`. Returns `None` if nothing is hit.
    ///
    /// Hit-testing is fill-aware: a path is only picked if the click lies
    /// inside the actual filled region (computed by winding number, with
    /// the path's own fill rule), not merely inside its bounding box. This
    /// makes overlapping shapes whose bboxes overlap but whose fills don't
    /// pickable in the obvious way — click on what you can see.
    ///
    /// Stroke-only paths and node types we don't have an exact fill test
    /// for (Text, Raster, plain Groups) fall back to bbox containment so
    /// they stay clickable.
    ///
    /// Walk order is depth-first (last child = front-most); the last
    /// matching node wins, giving topmost-first semantics.
    pub fn object_hit_test(scene: &Scene, canvas_pos: [f64; 2]) -> Option<NodeId> {
        Self::objects_at_point(scene, canvas_pos).into_iter().next()
    }

    /// All visible nodes whose visible filled area contains `canvas_pos`,
    /// returned **front-to-back** (topmost first). The same fill-aware
    /// rules as [`object_hit_test`] apply.
    ///
    /// Used by Alt-click cycling: callers iterate the list to find the
    /// candidate after the currently-selected one.
    pub fn objects_at_point(scene: &Scene, canvas_pos: [f64; 2]) -> Vec<NodeId> {
        let target = Point::new(canvas_pos[0], canvas_pos[1]);
        // Depth-first walk pushes nodes in back-to-front draw order; we
        // reverse at the end to expose front-to-back to the caller.
        let mut hits: Vec<NodeId> = Vec::new();

        let root = scene.root();
        scene.walk_depth_first(
            root,
            locus_geom::Affine::IDENTITY,
            &mut |id, node, world| {
                // Visibility hides the entire subtree; locking only blocks
                // hits on this node — its children stay interactive (matches
                // Inkscape's "lock object" semantics).
                if !node.visible {
                    return false;
                }
                let is_boolean = matches!(
                    node.data,
                    NodeData::Group {
                        kind: GroupKind::Boolean { .. },
                        ..
                    }
                );
                if !node.locked && node_hit_at_point(scene, id, &node.data, world, target) {
                    hits.push(id);
                }
                // Boolean groups behave as a single hittable shape — do
                // not descend into their operand children for hit testing.
                !is_boolean
            },
        );

        hits.reverse();
        hits
    }

    /// Find all visible nodes whose bounding boxes intersect the given rect.
    pub fn objects_in_rect(scene: &Scene, min: [f64; 2], max: [f64; 2]) -> Vec<NodeId> {
        let query = locus_geom::Bounds::new(Point::new(min[0], min[1]), Point::new(max[0], max[1]));
        let mut result = Vec::new();

        let root = scene.root();
        scene.walk_depth_first(
            root,
            locus_geom::Affine::IDENTITY,
            &mut |id, node, world| {
                // Visibility hides the entire subtree; locking only blocks
                // hits on this node — its children stay interactive (matches
                // Inkscape's "lock object" semantics).
                if !node.visible {
                    return false;
                }
                if node.locked {
                    // Don't pick this node, but DO control recursion the
                    // same way the un-locked path does (so Boolean groups
                    // remain non-recursable when locked).
                    return !matches!(
                        node.data,
                        NodeData::Group {
                            kind: GroupKind::Boolean { .. },
                            ..
                        }
                    );
                }
                let bounds = node_bounds(scene, id, &node.data, world);
                if !bounds.is_empty() && bounds.intersects(query) {
                    result.push(id);
                }
                !matches!(
                    node.data,
                    NodeData::Group {
                        kind: GroupKind::Boolean { .. },
                        ..
                    }
                )
            },
        );

        result
    }

    // ── Rotation zone hit-testing ─────────────────────────────────────

    /// Compute the combined world-space bounding box of all object-selected nodes.
    ///
    /// Delegates to `locus_bool::selection_visual_bounds`, which is group-aware:
    /// for a regular Group it recursively unions the visible descendants' bounds
    /// rather than relying on `NodeData::visual_bounds`, which is intentionally
    /// `Bounds::EMPTY` for groups (groups have no intrinsic geometry of their
    /// own). Without this, selecting a group produces an empty selection bbox,
    /// which makes the scale handles and rotation zone untestable.
    pub fn selection_bounds(&self, scene: &Scene) -> Bounds {
        locus_bool::selection_visual_bounds(scene, &self.selected_nodes)
    }

    /// Test whether `canvas_pos` is in the rotation zone: near a corner of
    /// the selection bounding box but outside the box itself. Returns true if
    /// the cursor should show a rotation indicator and a press should start
    /// rotating.
    ///
    /// `zone_radius` is in canvas units (typically `HIT_RADIUS_SCREEN_PX * 2 / zoom`).
    pub fn hit_rotation_zone(&self, scene: &Scene, canvas_pos: [f64; 2], zoom: f64) -> bool {
        if self.mode != SelectionMode::Object || self.selected_nodes.is_empty() {
            return false;
        }
        let bounds = self.selection_bounds(scene);
        if bounds.is_empty() {
            return false;
        }

        let zone_radius = HIT_RADIUS_SCREEN_PX * 2.0 / zoom;
        let p = Point::new(canvas_pos[0], canvas_pos[1]);

        // The four corners of the bounding box.
        let corners = [
            Point::new(bounds.min.x, bounds.min.y),
            Point::new(bounds.max.x, bounds.min.y),
            Point::new(bounds.min.x, bounds.max.y),
            Point::new(bounds.max.x, bounds.max.y),
        ];

        // Must be outside the box (with a small tolerance).
        let tolerance = 1.0 / zoom;
        let inside = p.x > bounds.min.x + tolerance
            && p.x < bounds.max.x - tolerance
            && p.y > bounds.min.y + tolerance
            && p.y < bounds.max.y - tolerance;
        if inside {
            return false;
        }

        // Must be within zone_radius of at least one corner.
        corners.iter().any(|c| p.distance(*c) < zone_radius)
    }

    /// Test whether `canvas_pos` is near one of the 8 scale handles on the
    /// selection bounding box. Returns the handle if hit.
    pub fn hit_scale_handle(
        &self,
        scene: &Scene,
        canvas_pos: [f64; 2],
        zoom: f64,
    ) -> Option<ScaleHandle> {
        if self.mode != SelectionMode::Object || self.selected_nodes.is_empty() {
            return None;
        }
        let bounds = self.selection_bounds(scene);
        if bounds.is_empty() {
            return None;
        }

        let radius = HIT_RADIUS_SCREEN_PX / zoom;
        let p = Point::new(canvas_pos[0], canvas_pos[1]);

        ScaleHandle::ALL
            .iter()
            .find(|h| p.distance(h.position(bounds)) < radius)
            .copied()
    }

    // ── Hover ────────────────────────────────────────────────────────

    /// Update the hover state for vertices within object-selected nodes.
    /// Returns `true` if the hovered vertex changed (caller should request redraw).
    /// Only active in `Node` mode — in `Object` mode, hover is always `None`.
    pub fn update_hover(&mut self, scene: &Scene, canvas_pos: [f64; 2], zoom: f64) -> bool {
        let mut changed = false;

        let new_hover = if self.mode != SelectionMode::Node || self.selected_nodes.is_empty() {
            None
        } else {
            Self::hit_test_in_nodes(scene, canvas_pos, zoom, &self.selected_nodes)
        };
        if new_hover != self.hovered {
            self.hovered = new_hover;
            changed = true;
        }

        // When no vertex is hovered, check for an edge nearby. We populate
        // both `edge_hover_point` (the ghost insertion ball) and
        // `edge_hover_hit` (the full segment for the highlight overlay)
        // from the same hit test so they're never out of sync.
        let (new_edge_pt, new_edge_hit) = if self.mode == SelectionMode::Node
            && self.hovered.is_none()
            && !self.selected_nodes.is_empty()
        {
            match Self::edge_hit_test(scene, canvas_pos, zoom, &self.selected_nodes) {
                Some(hit) => {
                    // Reconstruct the world-space point on the segment at
                    // parameter `t` for the ghost-insertion marker.
                    let pt = Self::edge_hover_point_from_hit(scene, &hit);
                    (pt, Some(hit))
                }
                None => (None, None),
            }
        } else {
            (None, None)
        };
        if new_edge_pt != self.edge_hover_point {
            self.edge_hover_point = new_edge_pt;
            changed = true;
        }
        if new_edge_hit != self.edge_hover_hit {
            self.edge_hover_hit = new_edge_hit;
            changed = true;
        }

        // Gradient handles: hovered in both Object and Node mode for any
        // selected path that references a gradient via fill or stroke.
        // Vertex hover wins in Node mode, so only check gradient hover
        // when no vertex is hovered.
        let new_grad_hover = if self.hovered.is_none() && !self.selected_nodes.is_empty() {
            gradient_handle_hit_test(scene, canvas_pos, zoom, &self.selected_nodes)
        } else {
            None
        };
        if new_grad_hover != self.gradient_hovered {
            self.gradient_hovered = new_grad_hover;
            changed = true;
        }

        changed
    }

    // ── Press / drag / release ───────────────────────────────────────

    /// Enter node editing mode for the currently selected objects.
    /// Shows vertex handles instead of the bounding box.
    pub fn enter_node_mode(&mut self) {
        if !self.selected_nodes.is_empty() {
            self.mode = SelectionMode::Node;
        }
    }

    /// Exit node editing mode, returning to object mode.
    /// Clears vertex selection but keeps object selection.
    pub fn exit_node_mode(&mut self) {
        self.mode = SelectionMode::Object;
        self.selected.clear();
        self.hovered = None;
    }

    /// Whether `canvas_pos` falls inside the world-space visual bounds of
    /// any currently object-selected node. Used to decide whether an
    /// empty-space click in node mode should drop back to object mode.
    fn click_inside_selection_bounds(&self, scene: &Scene, canvas_pos: [f64; 2]) -> bool {
        let target = Point::new(canvas_pos[0], canvas_pos[1]);
        self.selected_nodes.iter().any(|&id| {
            let Some(node) = scene.get(id) else {
                return false;
            };
            let world = scene.world_transform(id);
            let bounds = node_bounds(scene, id, &node.data, world);
            !bounds.is_empty() && bounds.contains_point(target)
        })
    }

    /// Cycle the vertex mode of the anchor vertex at `vr` through
    /// Corner → Smooth → Symmetric → Corner.
    /// Only applies to anchor points (SubpathStart or Endpoint).
    /// Returns the new mode, or None if inapplicable.
    /// Set a vertex's mode to a specific value. Returns the new mode if
    /// successful, or `None` if the vertex reference is invalid.
    pub fn set_vertex_mode(
        scene: &mut Scene,
        vr: &VertexRef,
        new_mode: VertexMode,
    ) -> Option<VertexMode> {
        // Determine which vertex_modes index this anchor maps to.
        let mode_idx = match vr.kind {
            PointKind::SubpathStart => 0,
            PointKind::Endpoint => vr.segment + 1,
            // Control points aren't anchors — set the anchor they belong to.
            PointKind::CubicCtrl1 | PointKind::QuadCtrl => vr.segment,
            PointKind::CubicCtrl2 => vr.segment + 1,
        };

        let subpath_idx = vr.subpath;
        scene.with_path_data_mut(vr.node, |path| {
            let subpath = path.subpaths.get_mut(subpath_idx)?;
            let mode = subpath.vertex_modes.get_mut(mode_idx)?;
            let old_mode = *mode;
            if old_mode == new_mode {
                return Some(new_mode);
            }
            *mode = new_mode;

            // Whenever we land in Smooth/Symmetric, make sure the adjacent
            // segments are cubics with both handles visibly spread from the
            // anchor and that the new mode's constraint is satisfied.
            //
            // We have to do this on EVERY transition into a non-Corner mode —
            // not just from Corner — because Smooth permits handles with length
            // 0 (one side collapsed at the anchor). Switching Smooth → Symmetric
            // without re-spreading would leave the collapsed handle invisible,
            // even though Symmetric implies it should mirror the other side.
            if new_mode != VertexMode::Corner {
                ensure_cubic_handles(subpath, mode_idx);
            }

            Some(new_mode)
        })?
    }

    /// Cycle a vertex's mode: Corner → Smooth → Symmetric → Corner.
    pub fn cycle_vertex_mode(scene: &mut Scene, vr: &VertexRef) -> Option<VertexMode> {
        // Read current mode first.
        let node = scene.get(vr.node)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.get(vr.subpath)?;
        let mode_idx = match vr.kind {
            PointKind::SubpathStart => 0,
            PointKind::Endpoint => vr.segment + 1,
            PointKind::CubicCtrl1 | PointKind::QuadCtrl => vr.segment,
            PointKind::CubicCtrl2 => vr.segment + 1,
        };
        let current = *subpath.vertex_modes.get(mode_idx)?;
        let next = match current {
            VertexMode::Corner => VertexMode::Smooth,
            VertexMode::Smooth => VertexMode::Symmetric,
            VertexMode::Symmetric => VertexMode::Corner,
        };
        Self::set_vertex_mode(scene, vr, next)
    }

    /// Handle a mouse press at `canvas_pos`.
    ///
    /// In **Node** mode:
    /// 1. Vertex hit on an object-selected node → vertex select + drag.
    /// 2. Click outside all selected objects → exit to object mode.
    ///
    /// In **Object** mode:
    /// 1. Object hit → object select.
    /// 2. Empty space → clear all, start marquee.
    ///
    /// Group / pierce / cycle modifiers:
    /// - **default** (no `alt`, no `ctrl`): the click resolves to the
    ///   *outermost* group ancestor that lives in the current isolation
    ///   scope — i.e. you select the whole group, not a leaf inside it.
    ///   Matches Inkscape / Illustrator default click behaviour.
    /// - **`alt`** alone: pierce — pick the leaf-most shape under the
    ///   cursor, ignoring its enclosing groups.
    /// - **`alt` + `ctrl`**: pierce + cycle stacked overlapping shapes
    ///   (the candidate just *behind* the currently-selected top one,
    ///   wrapping). This was the old plain-Alt cycle behaviour.
    /// - **`shift`** is independent and toggles the resolved hit in/out
    ///   of the existing selection.
    pub fn on_press(
        &mut self,
        scene: &Scene,
        canvas_pos: [f64; 2],
        shift: bool,
        alt: bool,
        ctrl: bool,
        zoom: f64,
    ) {
        if self.mode == SelectionMode::Node {
            // In node mode, try vertex hit first.
            let vertex_hit = if !self.selected_nodes.is_empty() {
                Self::hit_test_in_nodes(scene, canvas_pos, zoom, &self.selected_nodes)
            } else {
                None
            };

            if let Some(vr) = vertex_hit {
                if shift {
                    if let Some(idx) = self.selected.iter().position(|v| v == &vr) {
                        self.selected.remove(idx);
                    } else {
                        self.selected.push(vr);
                    }
                } else if !self.selected.contains(&vr) {
                    self.selected.clear();
                    self.selected.push(vr);
                }
                self.drag_mode = DragMode::MoveVertices { prev: canvas_pos };
                return;
            }

            // Vertex missed — see if we're on a gradient handle (allows
            // gradient editing while node-mode is active without forcing
            // a mode switch).
            if !self.selected_nodes.is_empty()
                && let Some(handle) =
                    gradient_handle_hit_test(scene, canvas_pos, zoom, &self.selected_nodes)
            {
                self.drag_mode = DragMode::MoveGradientHandle { handle };
                return;
            }

            // Clicked in empty space while in node mode. Only drop back to
            // object mode if the click is outside every selected object's
            // visual bounds — clicks inside the bounding box stay in node
            // mode (they just deselect vertices), so accidentally clicking
            // the interior of a path doesn't kick you out of editing.
            if self.click_inside_selection_bounds(scene, canvas_pos) {
                if !shift {
                    self.selected.clear();
                }
                self.drag_mode = DragMode::Idle;
                return;
            }

            self.exit_node_mode();
        }

        // Object-mode entry path: gradient handles take precedence over
        // marquee / object hit, so the user can grab a handle that sits
        // outside the visible shape (e.g. a Linear gradient end placed
        // far away from the path's bbox).
        if !self.selected_nodes.is_empty()
            && let Some(handle) =
                gradient_handle_hit_test(scene, canvas_pos, zoom, &self.selected_nodes)
        {
            self.drag_mode = DragMode::MoveGradientHandle { handle };
            return;
        }

        // Object mode: check scale handles, then rotation zone, then object hit.
        if let Some(handle) = self.hit_scale_handle(scene, canvas_pos, zoom) {
            let bounds = self.selection_bounds(scene);
            let anchor_pt = handle.anchor(bounds);
            let orig_transforms: Vec<(NodeId, Affine)> = self
                .selected_nodes
                .iter()
                .filter_map(|&id| scene.get(id).map(|n| (id, n.transform)))
                .collect();
            self.drag_mode = DragMode::ScaleObjects {
                handle,
                anchor: [anchor_pt.x, anchor_pt.y],
                orig_bounds: bounds,
                orig_transforms,
            };
            return;
        }

        if self.hit_rotation_zone(scene, canvas_pos, zoom) {
            let bounds = self.selection_bounds(scene);
            let center = [
                (bounds.min.x + bounds.max.x) * 0.5,
                (bounds.min.y + bounds.max.y) * 0.5,
            ];
            let start_angle = (canvas_pos[1] - center[1]).atan2(canvas_pos[0] - center[0]);
            self.drag_mode = DragMode::RotateObjects {
                center,
                start_angle,
                prev_angle: start_angle,
            };
            return;
        }

        // Object mode: pick from the stack of leaf-level fill hits, then
        // resolve up to the current isolation scope unless the user
        // pierced with Alt. Ctrl+Alt cycles through stacked shapes (the
        // pre-group-aware Alt-cycle behaviour, rebound to free Alt for
        // pierce-only).
        //
        // Filter the leaf candidates to only those that are descendants
        // of the current scope root — clicks on shapes outside an
        // entered group shouldn't grab them. (Effectively: the marquee
        // and click both honour isolation.)
        let scope_root = self.scope_root(scene);
        let all_candidates = Self::objects_at_point(scene, canvas_pos);
        let candidates: Vec<NodeId> = all_candidates
            .into_iter()
            .filter(|&id| id == scope_root || scene_node_is_descendant(scene, id, scope_root))
            .collect();

        let cycle = alt && ctrl;
        let pierce = alt; // pierce-to-leaf for either Alt or Ctrl+Alt

        let leaf_hit = if cycle {
            // Ctrl+Alt: pick the candidate just behind the currently-
            // selected top one (wraps).
            let current_top = self.selected_nodes.last().copied();
            match current_top
                .and_then(|id| candidates.iter().position(|c| *c == id))
                .map(|idx| (idx + 1) % candidates.len().max(1))
            {
                Some(next_idx) if !candidates.is_empty() => Some(candidates[next_idx]),
                _ => candidates.first().copied(),
            }
        } else {
            candidates.first().copied()
        };

        let object_hit = leaf_hit.map(|leaf| {
            if pierce {
                leaf
            } else {
                // Default click: walk up to the topmost group ancestor
                // that's a direct child of the current scope.
                self.resolve_pick_in_scope(scene, leaf)
            }
        });

        if let Some(node_id) = object_hit {
            self.selected.clear();

            if shift {
                if let Some(idx) = self.selected_nodes.iter().position(|n| *n == node_id) {
                    self.selected_nodes.remove(idx);
                } else {
                    self.selected_nodes.push(node_id);
                }
            } else {
                // Cycle (Ctrl+Alt) always replaces the selection — that's
                // how the cycle exposes one shape at a time. Otherwise
                // keep the "clicking inside an existing multi-selection
                // preserves the selection so a drag can start" behaviour.
                if cycle || !self.selected_nodes.contains(&node_id) {
                    self.selected_nodes.clear();
                    self.selected_nodes.push(node_id);
                }
                // Start dragging all selected objects.
                self.drag_mode = DragMode::MoveObjects { prev: canvas_pos };
            }
            return;
        }

        // Empty space — clear and start marquee.
        if !shift {
            self.selected_nodes.clear();
            self.selected.clear();
            self.mode = SelectionMode::Object;
        }
        self.drag_mode = DragMode::Marquee {
            anchor: canvas_pos,
            current: canvas_pos,
            shift,
        };
        self.marquee_preview_nodes.clear();
    }

    /// Handle mouse move during a drag. Returns true if a redraw is needed.
    ///
    /// `constrain_aspect` (Shift): only meaningful for `ScaleObjects` drags —
    /// forces uniform scale (the larger of |sx|, |sy|) so corner handles
    /// preserve the original aspect ratio. Side-handle drags are unaffected
    /// because there's only one free axis.
    pub fn on_drag(
        &mut self,
        scene: &mut Scene,
        canvas_pos: [f64; 2],
        constrain_aspect: bool,
    ) -> bool {
        match &mut self.drag_mode {
            DragMode::Idle => false,
            DragMode::MoveGradientHandle { handle } => {
                drag_gradient_handle(scene, *handle, canvas_pos)
            }
            DragMode::MoveVertices { prev } => {
                let dx = canvas_pos[0] - prev[0];
                let dy = canvas_pos[1] - prev[1];

                if dx == 0.0 && dy == 0.0 {
                    return false;
                }

                for vr in &self.selected {
                    // Convert the world-space delta to local-space for this
                    // node, accounting for any group transforms above it.
                    let world = scene.world_transform(vr.node);
                    if world.is_identity() {
                        vr.translate(scene, dx, dy);
                    } else if let Some(inv) = world.inverse() {
                        // Apply only the linear part (no translation) to the
                        // delta vector: local_delta = inv_linear * world_delta.
                        let local_dx = inv.a * dx + inv.b * dy;
                        let local_dy = inv.c * dx + inv.d * dy;
                        vr.translate(scene, local_dx, local_dy);
                    }

                    // Enforce smooth/symmetric constraints on the opposite handle.
                    if matches!(
                        vr.kind,
                        PointKind::CubicCtrl1 | PointKind::CubicCtrl2 | PointKind::QuadCtrl
                    ) {
                        enforce_vertex_constraint(vr, scene);
                    }
                }

                *prev = canvas_pos;
                true
            }
            DragMode::MoveObjects { prev } => {
                let dx = canvas_pos[0] - prev[0];
                let dy = canvas_pos[1] - prev[1];

                if dx == 0.0 && dy == 0.0 {
                    return false;
                }

                // Translate each selected node's transform by the delta.
                // Convert world-space delta to local-space, accounting for
                // any parent group transforms.
                for &node_id in &self.selected_nodes {
                    let parent_world = scene.parent_world_transform(node_id);
                    let (local_dx, local_dy) = if parent_world.is_identity() {
                        (dx, dy)
                    } else if let Some(inv) = parent_world.inverse() {
                        (inv.a * dx + inv.b * dy, inv.c * dx + inv.d * dy)
                    } else {
                        continue;
                    };

                    if let Some(node) = scene.get(node_id) {
                        let mut t = node.transform;
                        t.tx += local_dx;
                        t.ty += local_dy;
                        scene.set_transform(node_id, t);
                    }
                }

                *prev = canvas_pos;
                true
            }
            DragMode::RotateObjects {
                center, prev_angle, ..
            } => {
                let current_angle = (canvas_pos[1] - center[1]).atan2(canvas_pos[0] - center[0]);
                let delta = current_angle - *prev_angle;

                if delta.abs() < 1e-10 {
                    return false;
                }

                let center_pt = Point::new(center[0], center[1]);

                // Apply incremental rotation around the combined center.
                for &node_id in &self.selected_nodes {
                    // Get the node's world-space position and compute where
                    // it should move after rotating around center_pt.
                    let parent_world = scene.parent_world_transform(node_id);
                    let Some(node) = scene.get(node_id) else {
                        continue;
                    };

                    // The node's world position comes from parent_world * node.transform.
                    // We want to apply a rotation delta around center_pt in world space.
                    // New world transform = rotate_around(delta, center) * old_world.
                    // So: parent * new_local = rotate_around * parent * old_local
                    //     new_local = parent^-1 * rotate_around * parent * old_local
                    let rot = Affine::rotate_around(delta, center_pt);
                    let old_local = node.transform;
                    let world = parent_world.then(old_local);
                    let new_world = rot.then(world);
                    if let Some(inv_parent) = parent_world.inverse() {
                        scene.set_transform(node_id, inv_parent.then(new_world));
                    }
                }

                *prev_angle = current_angle;
                true
            }
            DragMode::ScaleObjects {
                handle,
                anchor,
                orig_bounds,
                orig_transforms,
            } => {
                let handle = *handle;
                let anchor_pt = Point::new(anchor[0], anchor[1]);
                let orig = *orig_bounds;

                // Scale factor = distance from anchor to mouse / distance
                // from anchor to original handle position.
                let orig_handle = handle.position(orig);
                let sx = if handle.scales_x() && (orig_handle.x - anchor_pt.x).abs() > 1e-10 {
                    (canvas_pos[0] - anchor_pt.x) / (orig_handle.x - anchor_pt.x)
                } else {
                    1.0
                };

                let sy = if handle.scales_y() && (orig_handle.y - anchor_pt.y).abs() > 1e-10 {
                    (canvas_pos[1] - anchor_pt.y) / (orig_handle.y - anchor_pt.y)
                } else {
                    1.0
                };

                // Clamp scale to avoid collapsing to zero.
                let mut sx = if sx.abs() < 0.01 {
                    0.01_f64.copysign(sx)
                } else {
                    sx
                };
                let mut sy = if sy.abs() < 0.01 {
                    0.01_f64.copysign(sy)
                } else {
                    sy
                };

                // Shift-drag: uniform scale on corner handles. We unify the
                // two axes by picking the larger absolute factor and copying
                // its sign — this is what Figma / Illustrator / Inkscape all
                // do for corner-handle aspect lock. Side handles only have
                // one free axis (the other was forced to 1.0 above), so this
                // branch is a no-op for them.
                if constrain_aspect && handle.scales_x() && handle.scales_y() {
                    let f = sx.abs().max(sy.abs());
                    sx = f.copysign(sx);
                    sy = f.copysign(sy);
                }

                // Build a world-space transform: translate anchor to origin,
                // scale, translate back.
                let scale_world = Affine::translate(anchor_pt.x, anchor_pt.y)
                    .then(Affine::scale(sx, sy))
                    .then(Affine::translate(-anchor_pt.x, -anchor_pt.y));

                // Reapply from the original transforms each frame (absolute,
                // not incremental) so the total scale is always correct.
                for &(node_id, orig_local) in orig_transforms.iter() {
                    let parent_world = scene.parent_world_transform(node_id);
                    let orig_world = parent_world.then(orig_local);
                    let new_world = scale_world.then(orig_world);
                    if let Some(inv_parent) = parent_world.inverse() {
                        scene.set_transform(node_id, inv_parent.then(new_world));
                    }
                }

                true
            }
            DragMode::Marquee {
                anchor, current, ..
            } => {
                *current = canvas_pos;
                let (min, max) = marquee_rect(*anchor, *current);
                // Marquee respects group isolation: only direct children
                // of the current scope are picked. At top level this also
                // avoids the surprise of grabbing both a group and its
                // children at the same time.
                let scope_root = self.scope_root(scene);
                self.marquee_preview_nodes = Self::objects_in_rect(scene, min, max)
                    .into_iter()
                    .filter(|&id| scene.parent(id) == Some(scope_root))
                    .collect();
                true
            }
        }
    }

    /// Handle mouse release — finalize marquee or end vertex drag.
    pub fn on_release(&mut self) {
        if let DragMode::Marquee { shift, .. } = &self.drag_mode {
            let shift = *shift;
            for node_id in self.marquee_preview_nodes.drain(..) {
                if shift {
                    // Toggle: remove if already selected, else add.
                    if let Some(idx) = self.selected_nodes.iter().position(|n| *n == node_id) {
                        self.selected_nodes.remove(idx);
                    } else {
                        self.selected_nodes.push(node_id);
                    }
                } else if !self.selected_nodes.contains(&node_id) {
                    self.selected_nodes.push(node_id);
                }
            }
        }
        self.marquee_preview_nodes.clear();
        self.drag_mode = DragMode::Idle;
    }

    // ── Queries ──────────────────────────────────────────────────────

    /// Whether we're currently dragging vertices (not marquee).
    pub fn is_dragging_vertices(&self) -> bool {
        matches!(self.drag_mode, DragMode::MoveVertices { .. })
    }

    /// Whether we're currently dragging an on-canvas gradient handle.
    pub fn is_dragging_gradient_handle(&self) -> bool {
        matches!(self.drag_mode, DragMode::MoveGradientHandle { .. })
    }

    /// If currently dragging a gradient handle, return which one.
    pub fn dragging_gradient_handle(&self) -> Option<GradientHandleRef> {
        if let DragMode::MoveGradientHandle { handle } = self.drag_mode {
            Some(handle)
        } else {
            None
        }
    }

    /// Whether we're currently dragging whole objects.
    pub fn is_dragging_objects(&self) -> bool {
        matches!(self.drag_mode, DragMode::MoveObjects { .. })
    }

    /// Whether we're currently rotating objects.
    pub fn is_rotating(&self) -> bool {
        matches!(self.drag_mode, DragMode::RotateObjects { .. })
    }

    /// Whether we're currently scaling objects via a handle.
    pub fn is_scaling(&self) -> bool {
        matches!(self.drag_mode, DragMode::ScaleObjects { .. })
    }

    /// Whether we're currently in any drag operation.
    pub fn is_dragging(&self) -> bool {
        !matches!(self.drag_mode, DragMode::Idle)
    }

    /// Whether the current drag mode places a point that should snap to
    /// other geometry — i.e. the dragged position is committed somewhere
    /// (moved object's anchor, scale-handle target, vertex, gradient stop).
    /// Marquee selection and rotation don't use a positional snap target,
    /// so showing a snap marker during them would be visual noise.
    pub fn wants_position_snap(&self) -> bool {
        matches!(
            self.drag_mode,
            DragMode::MoveVertices { .. }
                | DragMode::MoveObjects { .. }
                | DragMode::ScaleObjects { .. }
                | DragMode::MoveGradientHandle { .. }
        )
    }

    /// Get the current marquee rectangle in canvas coordinates, if active.
    pub fn marquee(&self) -> Option<([f64; 2], [f64; 2])> {
        match &self.drag_mode {
            DragMode::Marquee {
                anchor, current, ..
            } => Some(marquee_rect(*anchor, *current)),
            _ => None,
        }
    }

    /// Whether a node is object-selected (or in the marquee preview).
    pub fn is_node_selected(&self, id: NodeId) -> bool {
        self.selected_nodes.contains(&id) || self.marquee_preview_nodes.contains(&id)
    }

    /// Check if a vertex is highlighted (selected).
    pub fn is_highlighted(&self, vr: &VertexRef) -> bool {
        self.selected.contains(vr)
    }

    /// Check if a vertex is the one currently under the cursor.
    pub fn is_hovered(&self, vr: &VertexRef) -> bool {
        self.hovered.as_ref() == Some(vr)
    }

    // ── Delete ──────────────────────────────────────────────────────

    /// Delete the current selection. If individual vertices are selected,
    /// delete those vertices (removing segments from paths). Otherwise,
    /// delete the selected objects entirely.
    /// Returns `true` if anything was deleted.
    pub fn delete_selection(&mut self, scene: &mut Scene) -> bool {
        if !self.selected.is_empty() {
            self.delete_selected_vertices(scene)
        } else {
            self.delete_selected_objects(scene)
        }
    }

    /// Delete the selected objects from the scene.
    fn delete_selected_objects(&mut self, scene: &mut Scene) -> bool {
        if self.selected_nodes.is_empty() {
            return false;
        }
        for node_id in self.selected_nodes.drain(..) {
            scene.remove(node_id);
        }
        self.selected.clear();
        self.hovered = None;
        true
    }

    /// Delete the selected vertices from their paths.
    ///
    /// For each selected endpoint, the segment ending at that vertex is
    /// removed. For a selected SubpathStart, the first segment is removed
    /// and the start point moves to its endpoint. Subpaths left with no
    /// segments are removed; paths left with no subpaths are removed.
    pub fn delete_selected_vertices(&mut self, scene: &mut Scene) -> bool {
        if self.selected.is_empty() {
            return false;
        }

        // Group vertices by node, then process each node.
        // Sort vertex refs in reverse (subpath desc, segment desc) so that
        // removing earlier indices doesn't invalidate later ones.
        let mut vertices = self.selected.clone();
        vertices.sort_by(|a, b| {
            a.node
                .cmp(&b.node)
                .then(b.subpath.cmp(&a.subpath))
                .then(b.segment.cmp(&a.segment))
                .then(b.kind.cmp(&a.kind))
        });

        // Only delete actual anchor points (Endpoint and SubpathStart),
        // not control-point handles.
        let anchor_vertices: Vec<_> = vertices
            .iter()
            .filter(|v| matches!(v.kind, PointKind::Endpoint | PointKind::SubpathStart))
            .collect();

        if anchor_vertices.is_empty() {
            return false;
        }

        let mut nodes_to_remove = Vec::new();

        for vr in &anchor_vertices {
            let subpath_idx = vr.subpath;
            let segment_idx = vr.segment;
            let kind = vr.kind;
            scene.with_path_data_mut(vr.node, |path| {
                let Some(subpath) = path.subpaths.get_mut(subpath_idx) else {
                    return;
                };

                match kind {
                    PointKind::SubpathStart if !subpath.segments.is_empty() => {
                        // Move start to the first segment's endpoint and remove
                        // that segment.
                        subpath.start = subpath.segments[0].endpoint();
                        subpath.segments.remove(0);
                        // Remove the start point's mode, shift the next one
                        // into position 0 (it becomes the new start).
                        if subpath.vertex_modes.len() > 1 {
                            subpath.vertex_modes.remove(0);
                        }
                    }
                    PointKind::Endpoint if segment_idx < subpath.segments.len() => {
                        subpath.segments.remove(segment_idx);
                        // vertex_modes index for this endpoint is segment_idx + 1.
                        let mode_idx = segment_idx + 1;
                        if mode_idx < subpath.vertex_modes.len() {
                            subpath.vertex_modes.remove(mode_idx);
                        }
                    }
                    _ => {}
                }
            });
        }

        // Clean up: remove empty subpaths and empty paths.
        for vr in &anchor_vertices {
            let is_empty = scene
                .with_path_data_mut(vr.node, |path| {
                    // Remove subpaths with no segments (just a lone point).
                    path.subpaths.retain(|sp| !sp.segments.is_empty());
                    path.subpaths.is_empty()
                })
                .unwrap_or(false);
            if is_empty {
                nodes_to_remove.push(vr.node);
            }
        }

        for node_id in &nodes_to_remove {
            scene.remove(*node_id);
            self.selected_nodes.retain(|n| n != node_id);
        }

        self.selected.clear();
        self.hovered = None;
        true
    }

    // ── Insert point on edge ────────────────────────────────────────

    /// Find the closest point on any edge (segment) of the given nodes to
    /// `canvas_pos`. Returns `None` if nothing is within the screen-space
    /// hit radius.
    pub fn edge_hit_test(
        scene: &Scene,
        canvas_pos: [f64; 2],
        zoom: f64,
        nodes: &[NodeId],
    ) -> Option<EdgeHit> {
        let radius = EDGE_HIT_RADIUS_SCREEN_PX / zoom;
        let canvas_target = Point::new(canvas_pos[0], canvas_pos[1]);
        let mut best: Option<(EdgeHit, f64)> = None;

        for &node_id in nodes {
            let Some(node) = scene.get(node_id) else {
                continue;
            };
            // Locked nodes are non-interactive: no vertex iteration / edge
            // hits / object hits land on them. They're still rendered.
            if !node.is_interactive() {
                continue;
            }
            let NodeData::Path { ref path, .. } = node.data else {
                continue;
            };

            // Transform the canvas-space target into local space for this node.
            let world = scene.world_transform(node_id);
            let target = if world.is_identity() {
                canvas_target
            } else if let Some(inv) = world.inverse() {
                inv.apply(canvas_target)
            } else {
                continue; // degenerate transform, skip
            };

            for (sp_idx, subpath) in path.subpaths.iter().enumerate() {
                let mut current = subpath.start;
                for (seg_idx, seg) in subpath.segments.iter().enumerate() {
                    let (_t, _pt, dist) = seg.closest_point(current, target);
                    if dist < radius && best.as_ref().is_none_or(|(_, bd)| dist < *bd) {
                        best = Some((
                            EdgeHit {
                                node: node_id,
                                subpath: sp_idx,
                                segment: seg_idx,
                                t: _t,
                            },
                            dist,
                        ));
                    }
                    current = seg.endpoint();
                }

                // Closed paths have an implicit straight closing edge from
                // the last segment's endpoint back to `subpath.start`. It
                // isn't stored in `segments`, so we have to test it
                // separately. We encode a hit on it as
                // `segment: subpath.segments.len()` (one past the last
                // real segment), which `insert_point_on_edge` recognizes.
                if subpath.closed && !subpath.segments.is_empty() && current != subpath.start {
                    let closing = Segment::Line { to: subpath.start };
                    let (t, _pt, dist) = closing.closest_point(current, target);
                    if dist < radius && best.as_ref().is_none_or(|(_, bd)| dist < *bd) {
                        best = Some((
                            EdgeHit {
                                node: node_id,
                                subpath: sp_idx,
                                segment: subpath.segments.len(),
                                t,
                            },
                            dist,
                        ));
                    }
                }
            }
        }

        best.map(|(hit, _)| hit)
    }

    /// World-space point on the segment described by `hit` at its `t`.
    /// Returns `None` if the segment can't be resolved (missing node, wrong
    /// node kind, out-of-range indices). Handles the implicit closing-edge
    /// encoding (`segment == subpath.segments.len()`).
    pub fn edge_hover_point_from_hit(scene: &Scene, hit: &EdgeHit) -> Option<Point> {
        let node = scene.get(hit.node)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.get(hit.subpath)?;

        // Compute `from` (the segment's implicit start point) and the segment.
        let (from, seg) = if hit.segment < subpath.segments.len() {
            let from = if hit.segment == 0 {
                subpath.start
            } else {
                subpath.segments[hit.segment - 1].endpoint()
            };
            (from, subpath.segments[hit.segment])
        } else if hit.segment == subpath.segments.len()
            && subpath.closed
            && !subpath.segments.is_empty()
        {
            let from = subpath.segments.last().unwrap().endpoint();
            (from, Segment::Line { to: subpath.start })
        } else {
            return None;
        };

        let local = seg.eval_at(from, hit.t);
        let world = scene.world_transform(hit.node);
        Some(if world.is_identity() {
            local
        } else {
            world.apply(local)
        })
    }

    /// Build an `EdgeHit` for the segment immediately *before* `vref`'s
    /// anchor (i.e. the segment whose endpoint is this vertex). Returns
    /// `None` if `vref` is a control point, or if the anchor is the start
    /// of an open subpath (no incoming segment exists).
    pub fn incoming_edge_for_vertex(scene: &Scene, vref: &VertexRef) -> Option<EdgeHit> {
        let node = scene.get(vref.node)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.get(vref.subpath)?;
        match vref.kind {
            PointKind::Endpoint => Some(EdgeHit {
                node: vref.node,
                subpath: vref.subpath,
                segment: vref.segment,
                t: 0.5,
            }),
            PointKind::SubpathStart if subpath.closed && !subpath.segments.is_empty() => {
                // The implicit (or last explicit) segment that lands on the
                // start. We use the closing-edge encoding so callers that
                // care (e.g. convert helpers) gracefully no-op rather than
                // touching some unrelated segment.
                Some(EdgeHit {
                    node: vref.node,
                    subpath: vref.subpath,
                    segment: subpath.segments.len(),
                    t: 0.5,
                })
            }
            _ => None,
        }
    }

    /// Build an `EdgeHit` for the segment immediately *after* `vref`'s anchor
    /// (i.e. the segment whose implicit start is this vertex). Returns
    /// `None` if `vref` is a control point, or if no outgoing segment exists
    /// (last endpoint of an open subpath).
    pub fn outgoing_edge_for_vertex(scene: &Scene, vref: &VertexRef) -> Option<EdgeHit> {
        let node = scene.get(vref.node)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.get(vref.subpath)?;
        let seg_idx = match vref.kind {
            PointKind::SubpathStart => 0,
            PointKind::Endpoint => vref.segment + 1,
            _ => return None,
        };
        if seg_idx < subpath.segments.len() {
            Some(EdgeHit {
                node: vref.node,
                subpath: vref.subpath,
                segment: seg_idx,
                t: 0.5,
            })
        } else {
            None
        }
    }

    /// Insert a new anchor point on the edge described by `hit`, splitting
    /// the segment in two. Returns a `VertexRef` to the newly created point.
    ///
    /// Special case: a `hit.segment == subpath.segments.len()` indicates a
    /// hit on the implicit closing edge of a closed subpath (the line from
    /// the last segment's endpoint back to `subpath.start`). We materialize
    /// it as a new explicit `Line` segment appended to the subpath; the new
    /// closing line then runs from the inserted vertex back to start.
    pub fn insert_point_on_edge(scene: &mut Scene, hit: &EdgeHit) -> Option<VertexRef> {
        let subpath_idx = hit.subpath;
        let seg_idx = hit.segment;
        let t = hit.t;
        let node_id = hit.node;
        scene
            .with_path_data_mut(node_id, |path| {
                let subpath = path.subpaths.get_mut(subpath_idx)?;

                // Closing-edge case: hit is on the virtual line from the last
                // endpoint back to subpath.start. Append a Line segment ending at
                // the split point so the implicit closing line still terminates at
                // subpath.start, just from the new vertex.
                if seg_idx == subpath.segments.len() {
                    if !subpath.closed || subpath.segments.is_empty() {
                        return None;
                    }
                    let from = subpath.segments[seg_idx - 1].endpoint();
                    let closing = Segment::Line { to: subpath.start };
                    let (first, _second) = closing.split_at(from, t);
                    subpath.segments.push(first);
                    subpath.vertex_modes.insert(seg_idx + 1, VertexMode::Corner);
                    return Some(VertexRef {
                        node: node_id,
                        subpath: subpath_idx,
                        segment: seg_idx,
                        kind: PointKind::Endpoint,
                    });
                }

                if seg_idx >= subpath.segments.len() {
                    return None;
                }

                // Determine the "from" point (implicit start of this segment).
                let from = if seg_idx == 0 {
                    subpath.start
                } else {
                    subpath.segments[seg_idx - 1].endpoint()
                };

                // Split the segment at t.
                let (first, second) = subpath.segments[seg_idx].split_at(from, t);

                // Replace the original segment with the two halves.
                subpath.segments[seg_idx] = first;
                subpath.segments.insert(seg_idx + 1, second);
                // Insert a vertex mode for the new split point (Corner by default).
                subpath.vertex_modes.insert(seg_idx + 1, VertexMode::Corner);

                // The new point is the endpoint of `first` (at seg_idx).
                Some(VertexRef {
                    node: node_id,
                    subpath: subpath_idx,
                    segment: seg_idx,
                    kind: PointKind::Endpoint,
                })
            })
            .flatten()
    }

    /// Convert a segment to a Line (straight between its endpoints).
    /// Returns true if the segment was changed.
    pub fn convert_segment_to_line(scene: &mut Scene, hit: &EdgeHit) -> bool {
        let subpath_idx = hit.subpath;
        let seg_idx = hit.segment;
        scene
            .with_path_data_mut(hit.node, |path| {
                let Some(subpath) = path.subpaths.get_mut(subpath_idx) else {
                    return false;
                };
                let Some(seg) = subpath.segments.get_mut(seg_idx) else {
                    return false;
                };
                if matches!(seg, Segment::Line { .. }) {
                    return false; // already a line
                }
                let to = seg.endpoint();
                *seg = Segment::Line { to };
                true
            })
            .unwrap_or(false)
    }

    /// Convert a segment to a Quad. For Cubics, the control point is the
    /// average of ctrl1 and ctrl2. For Lines, the control point is the
    /// midpoint of the segment. Returns true if the segment was changed.
    pub fn convert_segment_to_quad(scene: &mut Scene, hit: &EdgeHit) -> bool {
        let subpath_idx = hit.subpath;
        let seg_idx = hit.segment;
        scene
            .with_path_data_mut(hit.node, |path| {
                let Some(subpath) = path.subpaths.get_mut(subpath_idx) else {
                    return false;
                };
                // Determine the "from" point for this segment.
                let from = if seg_idx == 0 {
                    subpath.start
                } else {
                    subpath.segments[seg_idx - 1].endpoint()
                };
                let Some(seg) = subpath.segments.get_mut(seg_idx) else {
                    return false;
                };
                if matches!(seg, Segment::Quad { .. }) {
                    return false; // already a quad
                }
                match seg {
                    Segment::Line { to } => {
                        let mid = Point::new((from.x + to.x) * 0.5, (from.y + to.y) * 0.5);
                        *seg = Segment::Quad { ctrl: mid, to: *to };
                    }
                    Segment::Cubic { ctrl1, ctrl2, to } => {
                        let ctrl = Point::new((ctrl1.x + ctrl2.x) * 0.5, (ctrl1.y + ctrl2.y) * 0.5);
                        *seg = Segment::Quad { ctrl, to: *to };
                    }
                    Segment::Arc { to, .. } => {
                        let mid = Point::new((from.x + to.x) * 0.5, (from.y + to.y) * 0.5);
                        *seg = Segment::Quad { ctrl: mid, to: *to };
                    }
                    _ => return false,
                }
                true
            })
            .unwrap_or(false)
    }

    /// Convert a segment to a Cubic. Quads are degree-elevated (exact shape
    /// preservation). Lines get handles at 1/3 of the segment length.
    /// Returns true if the segment was changed.
    pub fn convert_segment_to_cubic(scene: &mut Scene, hit: &EdgeHit) -> bool {
        let subpath_idx = hit.subpath;
        let seg_idx = hit.segment;
        scene
            .with_path_data_mut(hit.node, |path| {
                let Some(subpath) = path.subpaths.get_mut(subpath_idx) else {
                    return false;
                };
                let from = if seg_idx == 0 {
                    subpath.start
                } else {
                    subpath.segments[seg_idx - 1].endpoint()
                };
                let Some(seg) = subpath.segments.get_mut(seg_idx) else {
                    return false;
                };
                if matches!(seg, Segment::Cubic { .. }) {
                    return false; // already a cubic
                }
                match seg {
                    Segment::Line { to } => {
                        let dx = to.x - from.x;
                        let dy = to.y - from.y;
                        *seg = Segment::Cubic {
                            ctrl1: Point::new(from.x + dx / 3.0, from.y + dy / 3.0),
                            ctrl2: Point::new(from.x + 2.0 * dx / 3.0, from.y + 2.0 * dy / 3.0),
                            to: *to,
                        };
                    }
                    Segment::Quad { ctrl, to } => {
                        // Degree elevation: exact shape preservation.
                        let c1 = Point::new(
                            from.x + 2.0 / 3.0 * (ctrl.x - from.x),
                            from.y + 2.0 / 3.0 * (ctrl.y - from.y),
                        );
                        let c2 = Point::new(
                            to.x + 2.0 / 3.0 * (ctrl.x - to.x),
                            to.y + 2.0 / 3.0 * (ctrl.y - to.y),
                        );
                        *seg = Segment::Cubic {
                            ctrl1: c1,
                            ctrl2: c2,
                            to: *to,
                        };
                    }
                    Segment::Arc { to, .. } => {
                        let dx = to.x - from.x;
                        let dy = to.y - from.y;
                        *seg = Segment::Cubic {
                            ctrl1: Point::new(from.x + dx / 3.0, from.y + dy / 3.0),
                            ctrl2: Point::new(from.x + 2.0 * dx / 3.0, from.y + 2.0 * dy / 3.0),
                            to: *to,
                        };
                    }
                    _ => return false,
                }
                true
            })
            .unwrap_or(false)
    }

    /// Return the current type of a segment identified by an `EdgeHit`.
    pub fn segment_type(scene: &Scene, hit: &EdgeHit) -> Option<SegmentKind> {
        let node = scene.get(hit.node)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.get(hit.subpath)?;
        let seg = subpath.segments.get(hit.segment)?;
        Some(match seg {
            Segment::Line { .. } => SegmentKind::Line,
            Segment::Quad { .. } => SegmentKind::Quad,
            Segment::Cubic { .. } => SegmentKind::Cubic,
            Segment::Arc { .. } => SegmentKind::Arc,
        })
    }

    /// Retract a control point onto its anchor, downgrading the segment.
    ///
    /// * `CubicCtrl1` / `CubicCtrl2` → segment becomes a Quad, using the
    ///   surviving cubic control as the quad's ctrl.
    /// * `QuadCtrl` → segment becomes a Line.
    /// * Anchor-kind `VertexRef`s are rejected (no-op).
    ///
    /// Returns `true` if the path was changed.
    pub fn retract_control(scene: &mut Scene, vref: &VertexRef) -> bool {
        let subpath_idx = vref.subpath;
        let seg_idx = vref.segment;
        let kind = vref.kind;
        scene
            .with_path_data_mut(vref.node, |path| {
                let Some(subpath) = path.subpaths.get_mut(subpath_idx) else {
                    return false;
                };
                let Some(seg) = subpath.segments.get_mut(seg_idx) else {
                    return false;
                };
                match (kind, *seg) {
                    (PointKind::QuadCtrl, Segment::Quad { to, .. }) => {
                        *seg = Segment::Line { to };
                        true
                    }
                    (PointKind::CubicCtrl1, Segment::Cubic { ctrl2, to, .. }) => {
                        *seg = Segment::Quad { ctrl: ctrl2, to };
                        true
                    }
                    (PointKind::CubicCtrl2, Segment::Cubic { ctrl1, to, .. }) => {
                        *seg = Segment::Quad { ctrl: ctrl1, to };
                        true
                    }
                    _ => false,
                }
            })
            .unwrap_or(false)
    }

    /// Mirror this control point's position across its anchor onto the
    /// control on the *other* side of that anchor, producing a smooth
    /// (symmetric) curve transition. Also sets the anchor's `VertexMode`
    /// to `Symmetric`.
    ///
    /// Requires:
    /// * `vref` is a cubic control (mirrors are well-defined; quad has only
    ///   one control which isn't anchor-attached in the same sense).
    /// * The segment on the other side of the anchor exists and is a Cubic
    ///   (so it has a control point to write to). Quad on the other side is
    ///   rejected — a future enhancement could degree-elevate it first.
    ///
    /// Returns `true` if the path was changed.
    pub fn mirror_control_across_anchor(scene: &mut Scene, vref: &VertexRef) -> bool {
        let subpath_idx = vref.subpath;
        let seg_idx = vref.segment;
        let kind = vref.kind;
        scene
            .with_path_data_mut(vref.node, |path| {
                let Some(subpath) = path.subpaths.get_mut(subpath_idx) else {
                    return false;
                };

                // Figure out (anchor_point, this_control, anchor_mode_index, other_seg_idx).
                // "other_seg" is the segment on the opposite side of the anchor.
                let (this_ctrl, anchor, anchor_mode_idx, other_seg_idx) = match kind {
                    PointKind::CubicCtrl1 => {
                        // ctrl1 lives on the START side of segment seg_idx.
                        // The anchor is the segment's `from` point — i.e. the
                        // endpoint of the previous segment (or subpath.start /
                        // for closed paths, the last segment's endpoint).
                        let anchor = if seg_idx == 0 {
                            subpath.start
                        } else {
                            subpath.segments[seg_idx - 1].endpoint()
                        };
                        let other = if seg_idx == 0 {
                            if subpath.closed && !subpath.segments.is_empty() {
                                subpath.segments.len() - 1
                            } else {
                                return false;
                            }
                        } else {
                            seg_idx - 1
                        };
                        let Segment::Cubic { ctrl1, .. } = subpath.segments[seg_idx] else {
                            return false;
                        };
                        (ctrl1, anchor, seg_idx, other)
                    }
                    PointKind::CubicCtrl2 => {
                        // ctrl2 lives on the END side of segment seg_idx — its
                        // anchor is the segment's endpoint, and the "other"
                        // segment is the next one (or first segment if closed).
                        let Segment::Cubic { ctrl2, to, .. } = subpath.segments[seg_idx] else {
                            return false;
                        };
                        let other = if seg_idx + 1 < subpath.segments.len() {
                            seg_idx + 1
                        } else if subpath.closed && !subpath.segments.is_empty() {
                            0
                        } else {
                            return false;
                        };
                        (ctrl2, to, seg_idx + 1, other)
                    }
                    _ => return false,
                };

                // Compute the mirrored control position: reflect this_ctrl
                // through the anchor — i.e. 2*anchor - this_ctrl.
                let mirrored =
                    Point::new(2.0 * anchor.x - this_ctrl.x, 2.0 * anchor.y - this_ctrl.y);

                // Write the mirrored point into the OTHER segment's
                // anchor-adjacent control. Which control depends on whether
                // the other segment leaves or arrives at the anchor:
                // * `this_ctrl == ctrl1 of seg_idx` (we leave anchor through seg_idx)
                //   → other segment arrives at anchor → write its ctrl2.
                // * `this_ctrl == ctrl2 of seg_idx` (we arrive at anchor through seg_idx)
                //   → other segment leaves anchor → write its ctrl1.
                let write_ctrl2_on_other = matches!(kind, PointKind::CubicCtrl1);
                let Some(other_seg) = subpath.segments.get_mut(other_seg_idx) else {
                    return false;
                };
                match (other_seg, write_ctrl2_on_other) {
                    (Segment::Cubic { ctrl2, .. }, true) => *ctrl2 = mirrored,
                    (Segment::Cubic { ctrl1, .. }, false) => *ctrl1 = mirrored,
                    // The other side isn't a Cubic — bail rather than guess.
                    // The caller already gates this case at the menu level.
                    _ => return false,
                }

                // Promote the anchor's vertex mode to Symmetric so future
                // drags of either handle keep the constraint live.
                if let Some(mode) = subpath.vertex_modes.get_mut(anchor_mode_idx) {
                    *mode = VertexMode::Symmetric;
                }
                true
            })
            .unwrap_or(false)
    }

    /// True iff `mirror_control_across_anchor` would succeed for this vref.
    /// Used by the right-click menu to grey-out / hide the option when it
    /// can't be applied.
    pub fn can_mirror_control(scene: &Scene, vref: &VertexRef) -> bool {
        let Some(node) = scene.get(vref.node) else {
            return false;
        };
        let NodeData::Path { ref path, .. } = node.data else {
            return false;
        };
        let Some(subpath) = path.subpaths.get(vref.subpath) else {
            return false;
        };
        let seg_idx = vref.segment;
        let other_seg_idx = match vref.kind {
            PointKind::CubicCtrl1 => {
                if !matches!(subpath.segments.get(seg_idx), Some(Segment::Cubic { .. })) {
                    return false;
                }
                if seg_idx == 0 {
                    if subpath.closed && !subpath.segments.is_empty() {
                        subpath.segments.len() - 1
                    } else {
                        return false;
                    }
                } else {
                    seg_idx - 1
                }
            }
            PointKind::CubicCtrl2 => {
                if !matches!(subpath.segments.get(seg_idx), Some(Segment::Cubic { .. })) {
                    return false;
                }
                if seg_idx + 1 < subpath.segments.len() {
                    seg_idx + 1
                } else if subpath.closed && !subpath.segments.is_empty() {
                    0
                } else {
                    return false;
                }
            }
            _ => return false,
        };
        matches!(
            subpath.segments.get(other_seg_idx),
            Some(Segment::Cubic { .. })
        )
    }
}

/// Identifies the type of a segment without carrying its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Line,
    Quad,
    Cubic,
    Arc,
}

/// Normalize two corners into (min, max).
fn marquee_rect(a: [f64; 2], b: [f64; 2]) -> ([f64; 2], [f64; 2]) {
    (
        [a[0].min(b[0]), a[1].min(b[1])],
        [a[0].max(b[0]), a[1].max(b[1])],
    )
}

// ── Gradient handle helpers ─────────────────────────────────────────────
//
// Gradient handles live in the gradient's user space (SVG `userSpaceOnUse`
// semantics — what the renderer assumes). The world position of a handle
// is `gradient.transform * point_in_gradient_space`. The path's transform
// does NOT apply.
//
// These helpers walk a set of selected paths, look at their fill and
// stroke `PaintRef::Ref(_)`, resolve the gradient node, and produce
// handle-by-handle world positions for hit-testing and rendering.

/// Resolve a path/boolean-group/text node's fill+stroke gradients.
///
/// Returns the unique set of gradient `NodeId`s referenced by `node` —
/// each at most once even if both fill and stroke point at the same
/// gradient (the panel's banner already informs the user about that
/// case; on canvas, drawing the same handle twice would just look like
/// flickering).
fn gradients_referenced_by(node: &locus_scene::Node) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = Vec::new();
    node.for_each_paint_ref(|paint| {
        if let PaintRef::Ref(id) = paint
            && !out.contains(id)
        {
            out.push(*id);
        }
    });
    out
}

/// Compute the world-space positions of every editable handle for
/// `gradient`, identified by `(paint, owner, point)` keys, and call `f`
/// for each.
///
/// For radial gradients, the focal handle is omitted when the focal
/// coincides with the centre (the common case) — that handle would just
/// land on top of the centre handle.
pub fn for_each_handle_of_gradient(
    paint: NodeId,
    owner: NodeId,
    gradient: &Gradient,
    mut f: impl FnMut(GradientHandleRef, Point),
) {
    let xf = gradient.transform;
    match gradient.kind {
        GradientKind::Linear { start, end } => {
            f(
                GradientHandleRef {
                    paint,
                    owner,
                    point: GradientHandlePoint::LinearStart,
                },
                xf.apply(start),
            );
            f(
                GradientHandleRef {
                    paint,
                    owner,
                    point: GradientHandlePoint::LinearEnd,
                },
                xf.apply(end),
            );
            // Stops along the start→end axis.
            for (i, stop) in gradient.stops.iter().enumerate() {
                let t = stop.offset as f64;
                let local = Point::new(
                    start.x + t * (end.x - start.x),
                    start.y + t * (end.y - start.y),
                );
                f(
                    GradientHandleRef {
                        paint,
                        owner,
                        point: GradientHandlePoint::Stop(i),
                    },
                    xf.apply(local),
                );
            }
        }
        GradientKind::Radial {
            center,
            radius,
            focal,
            focal_radius: _,
        } => {
            f(
                GradientHandleRef {
                    paint,
                    owner,
                    point: GradientHandlePoint::RadialCenter,
                },
                xf.apply(center),
            );
            f(
                GradientHandleRef {
                    paint,
                    owner,
                    point: GradientHandlePoint::RadialRadiusEdge,
                },
                xf.apply(Point::new(center.x + radius, center.y)),
            );
            // Focal only when it differs meaningfully from the centre.
            let dx = focal.x - center.x;
            let dy = focal.y - center.y;
            if dx.hypot(dy) > 1e-6 {
                f(
                    GradientHandleRef {
                        paint,
                        owner,
                        point: GradientHandlePoint::RadialFocal,
                    },
                    xf.apply(focal),
                );
            }
            // Stops along the centre→radius-edge axis.
            for (i, stop) in gradient.stops.iter().enumerate() {
                let t = stop.offset as f64;
                let local = Point::new(center.x + t * radius, center.y);
                f(
                    GradientHandleRef {
                        paint,
                        owner,
                        point: GradientHandlePoint::Stop(i),
                    },
                    xf.apply(local),
                );
            }
        }
    }
}

/// Walk the gradients referenced by every node in `selected_nodes` and
/// invoke `f(handle_ref, world_position)` for each handle. Handles are
/// emitted in *enumeration* order — the renderer relies on this to order
/// painters' draw calls (stops on top of axis lines).
pub fn for_each_gradient_handle(
    scene: &Scene,
    selected_nodes: &[NodeId],
    mut f: impl FnMut(GradientHandleRef, Point),
) {
    for &owner in selected_nodes {
        let Some(node) = scene.get(owner) else {
            continue;
        };
        for grad_id in gradients_referenced_by(node) {
            let Some(grad_node) = scene.get(grad_id) else {
                continue;
            };
            let NodeData::Paint(Paint::Gradient(g)) = &grad_node.data else {
                continue;
            };
            for_each_handle_of_gradient(grad_id, owner, g, &mut f);
        }
    }
}

/// Hit-test against gradient handles for the current selection. Returns
/// the closest handle within `HIT_RADIUS_SCREEN_PX / zoom` canvas units,
/// or `None` if nothing is in range.
pub fn gradient_handle_hit_test(
    scene: &Scene,
    canvas_pos: [f64; 2],
    zoom: f64,
    selected_nodes: &[NodeId],
) -> Option<GradientHandleRef> {
    let r = HIT_RADIUS_SCREEN_PX / zoom;
    let r2 = r * r;
    let mut best: Option<(f64, GradientHandleRef)> = None;
    for_each_gradient_handle(scene, selected_nodes, |handle, world| {
        let dx = world.x - canvas_pos[0];
        let dy = world.y - canvas_pos[1];
        let d2 = dx * dx + dy * dy;
        if d2 <= r2 {
            match best {
                Some((b, _)) if b <= d2 => {}
                _ => best = Some((d2, handle)),
            }
        }
    });
    best.map(|(_, h)| h)
}

/// Apply a gradient handle drag — moving handle `handle.point` to
/// `canvas_pos` (or, for stops, projecting onto the gradient axis to find
/// the new t). Reads the current gradient via `scene`, builds a new
/// `Gradient`, and writes it back via `Scene::set_gradient`.
///
/// Returns true if the gradient was modified.
///
/// Stops are clamped to the current neighbours' offsets so dragging one
/// stop past another isn't possible (matches the panel editor's
/// semantics — keeps the stops vector sorted with no need to re-find the
/// "selected stop" after a swap).
pub fn drag_gradient_handle(
    scene: &mut Scene,
    handle: GradientHandleRef,
    canvas_pos: [f64; 2],
) -> bool {
    let Some(grad_node) = scene.get(handle.paint) else {
        return false;
    };
    let NodeData::Paint(Paint::Gradient(g)) = &grad_node.data else {
        return false;
    };

    // Convert the world-space cursor into gradient space.
    let Some(inv) = g.transform.inverse() else {
        return false;
    };
    let local_x = inv.a * canvas_pos[0] + inv.b * canvas_pos[1] + inv.tx;
    let local_y = inv.c * canvas_pos[0] + inv.d * canvas_pos[1] + inv.ty;
    let local = Point::new(local_x, local_y);

    let mut new_g = g.clone();
    let mut changed = false;

    match handle.point {
        GradientHandlePoint::LinearStart => {
            if let GradientKind::Linear { start, .. } = &mut new_g.kind
                && (local.x != start.x || local.y != start.y)
            {
                *start = local;
                changed = true;
            }
        }
        GradientHandlePoint::LinearEnd => {
            if let GradientKind::Linear { end, .. } = &mut new_g.kind
                && (local.x != end.x || local.y != end.y)
            {
                *end = local;
                changed = true;
            }
        }
        GradientHandlePoint::RadialCenter => {
            if let GradientKind::Radial { center, focal, .. } = &mut new_g.kind {
                let dx = local.x - center.x;
                let dy = local.y - center.y;
                if dx != 0.0 || dy != 0.0 {
                    // Move centre, drag focal along with it (keep relative offset).
                    *center = local;
                    focal.x += dx;
                    focal.y += dy;
                    changed = true;
                }
            }
        }
        GradientHandlePoint::RadialRadiusEdge => {
            if let GradientKind::Radial { center, radius, .. } = &mut new_g.kind {
                let dx = local.x - center.x;
                let dy = local.y - center.y;
                let new_r = dx.hypot(dy).max(1e-3);
                if (new_r - *radius).abs() > 1e-9 {
                    *radius = new_r;
                    changed = true;
                }
            }
        }
        GradientHandlePoint::RadialFocal => {
            if let GradientKind::Radial { focal, .. } = &mut new_g.kind
                && (local.x != focal.x || local.y != focal.y)
            {
                *focal = local;
                changed = true;
            }
        }
        GradientHandlePoint::Stop(i) => {
            if i < new_g.stops.len() {
                // Project the cursor onto the gradient axis to compute t.
                let (axis_a, axis_b) = match new_g.kind {
                    GradientKind::Linear { start, end } => (start, end),
                    GradientKind::Radial { center, radius, .. } => {
                        (center, Point::new(center.x + radius, center.y))
                    }
                };
                let ax = axis_b.x - axis_a.x;
                let ay = axis_b.y - axis_a.y;
                let denom = ax * ax + ay * ay;
                if denom > f64::EPSILON {
                    let bx = local.x - axis_a.x;
                    let by = local.y - axis_a.y;
                    let raw_t = ((ax * bx + ay * by) / denom) as f32;
                    // Clamp to [neighbour_below + ε, neighbour_above - ε].
                    let lo = if i == 0 {
                        0.0
                    } else {
                        new_g.stops[i - 1].offset + 1e-4
                    };
                    let hi = if i + 1 >= new_g.stops.len() {
                        1.0
                    } else {
                        new_g.stops[i + 1].offset - 1e-4
                    };
                    let clamped = raw_t.clamp(lo, hi);
                    if (clamped - new_g.stops[i].offset).abs() > f32::EPSILON {
                        new_g.stops[i].offset = clamped;
                        changed = true;
                    }
                }
            }
        }
    }

    if changed {
        scene.set_gradient(handle.paint, new_g);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use locus_geom::{Path, SubPath};
    use locus_scene::Node;

    /// Build a 10x10 closed square path:
    ///   start (0,0) → (10,0) → (10,10) → (0,10) → [implicit close back to (0,0)]
    fn closed_square_node(scene: &mut Scene) -> NodeId {
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
        sp.push_segment(
            Segment::Line {
                to: Point::new(0.0, 10.0),
            },
            VertexMode::Corner,
        );
        sp.closed = true;
        let mut path = Path::new();
        path.subpaths.push(sp);
        let root = scene.root();
        scene.insert(root, Node::path("square", path)).unwrap()
    }

    #[test]
    fn edge_hit_test_finds_closing_edge_of_closed_path() {
        let mut scene = Scene::new();
        let id = closed_square_node(&mut scene);

        // The closing edge runs from (0,10) → (0,0). A click near (0, 5)
        // should land on it. Use a high zoom so the screen-pixel hit
        // radius shrinks to ~2 world units — comfortably catching the
        // closing edge but missing every other side of the square.
        let hit = SelectState::edge_hit_test(&scene, [0.5, 5.0], 4.0, &[id])
            .expect("should hit the closing edge");

        assert_eq!(hit.subpath, 0);
        // The closing edge is encoded as one past the last real segment.
        assert_eq!(hit.segment, 3);
        assert!((hit.t - 0.5).abs() < 1e-9, "hit.t {} should be ~0.5", hit.t);
    }

    #[test]
    fn edge_hit_test_skips_closing_edge_on_open_path() {
        let mut scene = Scene::new();
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
        sp.push_segment(
            Segment::Line {
                to: Point::new(0.0, 10.0),
            },
            VertexMode::Corner,
        );
        // Note: NOT closed.
        let mut path = Path::new();
        path.subpaths.push(sp);
        let root = scene.root();
        let id = scene.insert(root, Node::path("open", path)).unwrap();

        // Same click location and zoom as the closed-path test — should
        // miss because there's no implicit closing edge to hit.
        let hit = SelectState::edge_hit_test(&scene, [0.5, 5.0], 4.0, &[id]);
        assert!(hit.is_none(), "open path shouldn't have a closing edge");
    }

    #[test]
    fn insert_point_on_closing_edge_appends_segment() {
        let mut scene = Scene::new();
        let id = closed_square_node(&mut scene);

        let hit = EdgeHit {
            node: id,
            subpath: 0,
            segment: 3, // closing edge
            t: 0.5,
        };

        let vr = SelectState::insert_point_on_edge(&mut scene, &hit)
            .expect("insert on closing edge should succeed");

        assert_eq!(vr.segment, 3);
        assert_eq!(vr.kind, PointKind::Endpoint);

        // Verify the subpath now has 4 segments and the new endpoint is at (0, 5).
        let node = scene.get(id).unwrap();
        let NodeData::Path { ref path, .. } = node.data else {
            panic!("expected path");
        };
        let sp = &path.subpaths[0];
        assert_eq!(sp.segments.len(), 4);
        assert_eq!(sp.vertex_modes.len(), 5);
        assert!(sp.closed);
        assert_eq!(sp.segments[3].endpoint(), Point::new(0.0, 5.0));
        // The implicit closing line still terminates at subpath.start.
        assert_eq!(sp.start, Point::new(0.0, 0.0));
    }

    #[test]
    fn insert_point_on_closing_edge_of_open_path_returns_none() {
        let mut scene = Scene::new();
        let mut sp = SubPath::new(Point::new(0.0, 0.0));
        sp.push_segment(
            Segment::Line {
                to: Point::new(10.0, 0.0),
            },
            VertexMode::Corner,
        );
        let mut path = Path::new();
        path.subpaths.push(sp);
        let root = scene.root();
        let id = scene.insert(root, Node::path("open", path)).unwrap();

        let hit = EdgeHit {
            node: id,
            subpath: 0,
            segment: 1, // would be the closing edge if the path were closed
            t: 0.5,
        };

        assert!(
            SelectState::insert_point_on_edge(&mut scene, &hit).is_none(),
            "open path has no closing edge to insert into"
        );
    }

    // ── Gradient handle tests ───────────────────────────────────────

    use locus_geom::Color;
    use locus_scene::style::Fill;
    use locus_scene::{ColorStop, InterpolationSpace, SpreadMethod, Style};

    /// Build a scene with one path filled by a linear gradient placed in
    /// a defs node off the root. Returns `(path_id, gradient_id)`.
    fn linear_gradient_fill_scene() -> (Scene, NodeId, NodeId) {
        let mut scene = Scene::new();
        let path_id = closed_square_node(&mut scene);

        // Insert a gradient node directly off the root (we don't strictly
        // need the SVG `defs` indirection for these tests).
        let root = scene.root();
        let gradient = Gradient {
            kind: GradientKind::Linear {
                start: Point::new(0.0, 0.0),
                end: Point::new(10.0, 0.0),
            },
            stops: vec![
                ColorStop {
                    offset: 0.0,
                    color: Color::new(0.0, 0.0, 0.0, 1.0),
                },
                ColorStop {
                    offset: 1.0,
                    color: Color::new(1.0, 1.0, 1.0, 1.0),
                },
            ],
            interpolation: InterpolationSpace::LinearRgb,
            transform: Affine::IDENTITY,
            spread: SpreadMethod::Pad,
        };
        let grad_id = scene
            .insert(
                root,
                locus_scene::Node {
                    label: "g".into(),
                    transform: Affine::IDENTITY,
                    data: NodeData::Paint(Paint::Gradient(gradient)),
                    children: Vec::new(),
                    visible: true,
                    locked: false,
                },
            )
            .unwrap();

        // Re-target the path's fill to the gradient.
        let style = Style {
            fill: Some(Fill {
                paint: PaintRef::Ref(grad_id),
                opacity: 1.0,
                rule: locus_scene::FillRule::NonZero,
            }),
            stroke: None,
        };
        scene.set_style(path_id, style).unwrap();

        (scene, path_id, grad_id)
    }

    fn current_linear(scene: &Scene, grad_id: NodeId) -> (Point, Point) {
        let node = scene.get(grad_id).unwrap();
        let NodeData::Paint(Paint::Gradient(g)) = &node.data else {
            panic!("not a gradient");
        };
        let GradientKind::Linear { start, end } = g.kind else {
            panic!("not linear");
        };
        (start, end)
    }

    #[test]
    fn linear_gradient_emits_endpoint_and_stop_handles() {
        let (scene, path_id, grad_id) = linear_gradient_fill_scene();
        let mut handles: Vec<(GradientHandleRef, Point)> = Vec::new();
        for_each_gradient_handle(&scene, &[path_id], |h, p| handles.push((h, p)));

        // Two endpoints + two stops.
        assert_eq!(handles.len(), 4);
        assert!(
            handles
                .iter()
                .any(|(h, p)| matches!(h.point, GradientHandlePoint::LinearStart)
                    && p == &Point::new(0.0, 0.0))
        );
        assert!(
            handles
                .iter()
                .any(|(h, p)| matches!(h.point, GradientHandlePoint::LinearEnd)
                    && p == &Point::new(10.0, 0.0))
        );

        // Stops at offsets 0.0 and 1.0 land on the endpoints.
        let stop_positions: Vec<Point> = handles
            .iter()
            .filter(|(h, _)| matches!(h.point, GradientHandlePoint::Stop(_)))
            .map(|(_, p)| *p)
            .collect();
        assert_eq!(stop_positions.len(), 2);
        assert!(stop_positions.contains(&Point::new(0.0, 0.0)));
        assert!(stop_positions.contains(&Point::new(10.0, 0.0)));

        // Sanity: every emitted handle ties back to the source gradient.
        for (h, _) in &handles {
            assert_eq!(h.paint, grad_id);
            assert_eq!(h.owner, path_id);
        }
    }

    #[test]
    fn drag_gradient_start_endpoint_updates_kind() {
        let (mut scene, path_id, grad_id) = linear_gradient_fill_scene();
        let handle = GradientHandleRef {
            paint: grad_id,
            owner: path_id,
            point: GradientHandlePoint::LinearStart,
        };
        // Drag the start to (5, 5) in canvas / world coordinates. With an
        // identity gradient transform this should land directly in the
        // gradient's local space.
        assert!(drag_gradient_handle(&mut scene, handle, [5.0, 5.0]));
        let (start, end) = current_linear(&scene, grad_id);
        assert!((start.x - 5.0).abs() < 1e-9 && (start.y - 5.0).abs() < 1e-9);
        // End untouched.
        assert!((end.x - 10.0).abs() < 1e-9 && end.y.abs() < 1e-9);
    }

    #[test]
    fn drag_gradient_end_endpoint_updates_kind() {
        let (mut scene, path_id, grad_id) = linear_gradient_fill_scene();
        let handle = GradientHandleRef {
            paint: grad_id,
            owner: path_id,
            point: GradientHandlePoint::LinearEnd,
        };
        assert!(drag_gradient_handle(&mut scene, handle, [3.0, -2.0]));
        let (start, end) = current_linear(&scene, grad_id);
        // Start untouched.
        assert!(start.x.abs() < 1e-9 && start.y.abs() < 1e-9);
        assert!((end.x - 3.0).abs() < 1e-9 && (end.y + 2.0).abs() < 1e-9);
    }

    /// Insert an additional middle stop into the gradient at `grad_id`,
    /// returning the new (3-stop) gradient. Stops in the helper start as
    /// just `[0.0, 1.0]`; this lets a test introduce a middle stop to
    /// drag without crossing a neighbour.
    fn add_middle_stop(scene: &mut Scene, grad_id: NodeId, offset: f32) {
        let mut g = match &scene.get(grad_id).unwrap().data {
            NodeData::Paint(Paint::Gradient(g)) => g.clone(),
            _ => unreachable!(),
        };
        g.stops.insert(
            1,
            ColorStop {
                offset,
                color: Color::new(0.5, 0.5, 0.5, 1.0),
            },
        );
        scene.set_gradient(grad_id, g).unwrap();
    }

    #[test]
    fn drag_gradient_stop_projects_onto_axis_and_updates_offset() {
        // Start with stops at 0.0 and 1.0; we'll add a middle stop at 0.5
        // so we have somewhere to drag without crossing a neighbour.
        let (mut scene, path_id, grad_id) = linear_gradient_fill_scene();
        add_middle_stop(&mut scene, grad_id, 0.5);
        let handle = GradientHandleRef {
            paint: grad_id,
            owner: path_id,
            point: GradientHandlePoint::Stop(1),
        };

        // Drag to (8, 4): along the start→end axis (which runs along
        // y=0 from x=0 to x=10), the projected t is 0.8. The y component
        // is dropped because it's perpendicular to the axis.
        assert!(drag_gradient_handle(&mut scene, handle, [8.0, 4.0]));
        let node = scene.get(grad_id).unwrap();
        let NodeData::Paint(Paint::Gradient(g)) = &node.data else {
            unreachable!();
        };
        assert!(
            (g.stops[1].offset - 0.8).abs() < 1e-4,
            "stop offset {} ≠ 0.8",
            g.stops[1].offset
        );
        // Neighbours unchanged.
        assert_eq!(g.stops[0].offset, 0.0);
        assert_eq!(g.stops[2].offset, 1.0);
    }

    #[test]
    fn dragging_stop_clamps_to_neighbours() {
        // With three stops at 0.0, 0.5, 1.0, dragging the middle stop
        // *past* a neighbour (e.g. far past x=10) must clamp it strictly
        // less than the next stop's offset, not swap order.
        let (mut scene, path_id, grad_id) = linear_gradient_fill_scene();
        add_middle_stop(&mut scene, grad_id, 0.5);
        let handle = GradientHandleRef {
            paint: grad_id,
            owner: path_id,
            point: GradientHandlePoint::Stop(1),
        };
        // Try to drag well past the end (x=100) — should clamp under 1.0.
        assert!(drag_gradient_handle(&mut scene, handle, [100.0, 0.0]));
        let node = scene.get(grad_id).unwrap();
        let NodeData::Paint(Paint::Gradient(g)) = &node.data else {
            unreachable!();
        };
        assert!(
            g.stops[1].offset < 1.0,
            "stop offset {} should clamp below 1.0",
            g.stops[1].offset
        );
        assert!(g.stops[1].offset > 0.0); // Still above the lower neighbour.
        // Order preserved.
        assert!(g.stops[0].offset < g.stops[1].offset);
        assert!(g.stops[1].offset < g.stops[2].offset);
    }

    #[test]
    fn gradient_hit_test_returns_closest_handle() {
        // At zoom=10, HIT_RADIUS_SCREEN_PX (8 px) divided by zoom is 0.8
        // canvas units. The handle endpoints sit at (0, 0) and (10, 0),
        // so a click within ~0.8 of either endpoint should connect.
        let (scene, path_id, grad_id) = linear_gradient_fill_scene();

        // Click very close to the start endpoint at (0,0).
        let hit =
            gradient_handle_hit_test(&scene, [0.1, 0.0], 10.0, &[path_id]).expect("should hit");
        assert_eq!(hit.paint, grad_id);
        assert!(matches!(
            hit.point,
            GradientHandlePoint::LinearStart | GradientHandlePoint::Stop(0)
        ));

        // Click near the end.
        let hit =
            gradient_handle_hit_test(&scene, [9.9, 0.0], 10.0, &[path_id]).expect("should hit end");
        assert!(matches!(
            hit.point,
            GradientHandlePoint::LinearEnd | GradientHandlePoint::Stop(1)
        ));

        // Click far away returns None.
        assert!(gradient_handle_hit_test(&scene, [500.0, 500.0], 10.0, &[path_id]).is_none());
    }
}
