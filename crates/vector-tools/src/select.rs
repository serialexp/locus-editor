use vector_geom::{Affine, Bounds, Point, Segment, VertexMode};
use vector_scene::{GroupKind, NodeData, NodeId, Scene};

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
        let computed = vector_bool::compute_boolean_group_path(scene, id);
        let local = vector_tess::path_visual_bounds(&computed, true, None);
        return local.transform(world);
    }
    data.visual_bounds(world)
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
fn enforce_vertex_constraint(vr: &VertexRef, scene: &mut Scene) {
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
fn ensure_cubic_handles(subpath: &mut vector_geom::SubPath, mode_idx: usize) {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Show bounding box around selected objects. Dragging moves the whole object.
    Object,
    /// Show individual vertex handles. Dragging moves vertices.
    Node,
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
    /// Current drag mode.
    drag_mode: DragMode,
    /// Whether we're in object mode (bounding box) or node mode (vertex handles).
    pub mode: SelectionMode,
}

impl Default for SelectState {
    fn default() -> Self {
        Self {
            selected_nodes: Vec::new(),
            marquee_preview_nodes: Vec::new(),
            selected: Vec::new(),
            hovered: None,
            edge_hover_point: None,
            drag_mode: DragMode::Idle,
            mode: SelectionMode::Object,
        }
    }
}

/// Hit-test radius in screen pixels. Divided by zoom to get canvas-space radius.
const HIT_RADIUS_SCREEN_PX: f64 = 8.0;

/// A hit on a path edge (between vertices), used for inserting new points.
#[derive(Debug, Clone)]
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
            if !node.visible {
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

    /// Find the front-most visible path node whose bounding box contains
    /// `canvas_pos`. Returns `None` if no path is hit.
    ///
    /// Walk order is depth-first (last child = front-most), so we keep
    /// overwriting — the last match is the topmost drawn object.
    pub fn object_hit_test(scene: &Scene, canvas_pos: [f64; 2]) -> Option<NodeId> {
        let target = Point::new(canvas_pos[0], canvas_pos[1]);
        let mut best: Option<NodeId> = None;

        let root = scene.root();
        scene.walk_depth_first(
            root,
            vector_geom::Affine::IDENTITY,
            &mut |id, node, world| {
                if !node.visible {
                    return false;
                }
                let bounds = node_bounds(scene, id, &node.data, world);
                if !bounds.is_empty() && bounds.contains_point(target) {
                    best = Some(id);
                }
                // Boolean groups behave as a single hittable shape — do
                // not descend into their operand children for hit testing.
                !matches!(
                    node.data,
                    NodeData::Group {
                        kind: GroupKind::Boolean { .. },
                        ..
                    }
                )
            },
        );

        best
    }

    /// Find all visible nodes whose bounding boxes intersect the given rect.
    pub fn objects_in_rect(scene: &Scene, min: [f64; 2], max: [f64; 2]) -> Vec<NodeId> {
        let query =
            vector_geom::Bounds::new(Point::new(min[0], min[1]), Point::new(max[0], max[1]));
        let mut result = Vec::new();

        let root = scene.root();
        scene.walk_depth_first(
            root,
            vector_geom::Affine::IDENTITY,
            &mut |id, node, world| {
                if !node.visible {
                    return false;
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
    pub fn selection_bounds(&self, scene: &Scene) -> Bounds {
        let mut combined = Bounds::EMPTY;
        for &id in &self.selected_nodes {
            if let Some(node) = scene.get(id) {
                let world = scene.world_transform(id);
                let b = node_bounds(scene, id, &node.data, world);
                if !b.is_empty() {
                    combined = combined.union(b);
                }
            }
        }
        combined
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

        // When no vertex is hovered, check for an edge nearby to show
        // a ghost insertion point.
        let new_edge_pt = if self.mode == SelectionMode::Node
            && self.hovered.is_none()
            && !self.selected_nodes.is_empty()
        {
            Self::edge_hover_position(scene, canvas_pos, zoom, &self.selected_nodes)
        } else {
            None
        };
        if new_edge_pt != self.edge_hover_point {
            self.edge_hover_point = new_edge_pt;
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
    pub fn on_press(&mut self, scene: &Scene, canvas_pos: [f64; 2], shift: bool, zoom: f64) {
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

        // Object mode: try object hit.
        let object_hit = Self::object_hit_test(scene, canvas_pos);

        if let Some(node_id) = object_hit {
            self.selected.clear();

            if shift {
                if let Some(idx) = self.selected_nodes.iter().position(|n| *n == node_id) {
                    self.selected_nodes.remove(idx);
                } else {
                    self.selected_nodes.push(node_id);
                }
            } else {
                if !self.selected_nodes.contains(&node_id) {
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
    pub fn on_drag(&mut self, scene: &mut Scene, canvas_pos: [f64; 2]) -> bool {
        match &mut self.drag_mode {
            DragMode::Idle => false,
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
                let sx = if sx.abs() < 0.01 {
                    0.01_f64.copysign(sx)
                } else {
                    sx
                };
                let sy = if sy.abs() < 0.01 {
                    0.01_f64.copysign(sy)
                } else {
                    sy
                };

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
                self.marquee_preview_nodes = Self::objects_in_rect(scene, min, max);
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
        let radius = HIT_RADIUS_SCREEN_PX / zoom;
        let canvas_target = Point::new(canvas_pos[0], canvas_pos[1]);
        let mut best: Option<(EdgeHit, f64)> = None;

        for &node_id in nodes {
            let Some(node) = scene.get(node_id) else {
                continue;
            };
            if !node.visible {
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

    /// Find the world-space point on the nearest edge to `canvas_pos`,
    /// if one is within the hit radius. Used for the ghost insertion preview.
    fn edge_hover_position(
        scene: &Scene,
        canvas_pos: [f64; 2],
        zoom: f64,
        nodes: &[NodeId],
    ) -> Option<Point> {
        let radius = HIT_RADIUS_SCREEN_PX / zoom;
        let canvas_target = Point::new(canvas_pos[0], canvas_pos[1]);
        let mut best: Option<(Point, f64)> = None;

        for &node_id in nodes {
            let Some(node) = scene.get(node_id) else {
                continue;
            };
            if !node.visible {
                continue;
            }
            let NodeData::Path { ref path, .. } = node.data else {
                continue;
            };

            let world = scene.world_transform(node_id);
            let target = if world.is_identity() {
                canvas_target
            } else if let Some(inv) = world.inverse() {
                inv.apply(canvas_target)
            } else {
                continue;
            };

            for subpath in &path.subpaths {
                let mut current = subpath.start;
                for seg in &subpath.segments {
                    let (t, _pt, dist) = seg.closest_point(current, target);
                    if dist < radius && best.as_ref().is_none_or(|(_, bd)| dist < *bd) {
                        // Compute world-space position of the point on the edge.
                        let local_pt = seg.eval_at(current, t);
                        let world_pt = if world.is_identity() {
                            local_pt
                        } else {
                            world.apply(local_pt)
                        };
                        best = Some((world_pt, dist));
                    }
                    current = seg.endpoint();
                }

                // Implicit closing edge for closed paths (see `edge_hit_test`
                // for the rationale).
                if subpath.closed && !subpath.segments.is_empty() && current != subpath.start {
                    let closing = Segment::Line { to: subpath.start };
                    let (t, _pt, dist) = closing.closest_point(current, target);
                    if dist < radius && best.as_ref().is_none_or(|(_, bd)| dist < *bd) {
                        let local_pt = closing.eval_at(current, t);
                        let world_pt = if world.is_identity() {
                            local_pt
                        } else {
                            world.apply(local_pt)
                        };
                        best = Some((world_pt, dist));
                    }
                }
            }
        }

        best.map(|(pt, _)| pt)
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

#[cfg(test)]
mod tests {
    use super::*;
    use vector_geom::{Path, SubPath};
    use vector_scene::Node;

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
}
