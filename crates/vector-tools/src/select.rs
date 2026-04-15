use vector_geom::{Affine, Bounds, Point, Segment, VertexMode};
use vector_scene::{NodeData, NodeId, Scene};

/// Compute the world-space bounding box for a node's visual content.
fn node_bounds(data: &NodeData, world: Affine) -> Bounds {
    match data {
        NodeData::Path { path, .. } => path.bounding_box().transform(world),
        NodeData::Text(text) => {
            vector_text::text_bounds(&text.content, &text.font_family, text.font_size)
                .transform(world)
        }
        _ => Bounds::EMPTY,
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
        let Some(node) = scene.get_mut(self.node) else {
            return;
        };
        let NodeData::Path { ref mut path, .. } = node.data else {
            return;
        };
        let Some(subpath) = path.subpaths.get_mut(self.subpath) else {
            return;
        };

        match self.kind {
            PointKind::SubpathStart => {
                subpath.start.x += dx;
                subpath.start.y += dy;
            }
            PointKind::Endpoint => {
                if let Some(seg) = subpath.segments.get_mut(self.segment) {
                    translate_endpoint(seg, dx, dy);
                }
            }
            PointKind::QuadCtrl => {
                if let Some(Segment::Quad { ctrl, .. }) = subpath.segments.get_mut(self.segment) {
                    ctrl.x += dx;
                    ctrl.y += dy;
                }
            }
            PointKind::CubicCtrl1 => {
                if let Some(Segment::Cubic { ctrl1, .. }) = subpath.segments.get_mut(self.segment) {
                    ctrl1.x += dx;
                    ctrl1.y += dy;
                }
            }
            PointKind::CubicCtrl2 => {
                if let Some(Segment::Cubic { ctrl2, .. }) = subpath.segments.get_mut(self.segment) {
                    ctrl2.x += dx;
                    ctrl2.y += dy;
                }
            }
        }
    }
}

/// After moving a control point, enforce the vertex mode constraint on
/// the opposite handle at the same anchor vertex.
fn enforce_vertex_constraint(vr: &VertexRef, scene: &mut Scene) {
    let Some(node) = scene.get_mut(vr.node) else {
        return;
    };
    let NodeData::Path { ref mut path, .. } = node.data else {
        return;
    };
    let Some(subpath) = path.subpaths.get_mut(vr.subpath) else {
        return;
    };

    match vr.kind {
        PointKind::CubicCtrl1 => {
            // ctrl1 of segment[seg] belongs to anchor at vertex_modes[seg].
            let anchor_mode = subpath
                .vertex_modes
                .get(vr.segment)
                .copied()
                .unwrap_or(VertexMode::Corner);
            if anchor_mode == VertexMode::Corner {
                return;
            }

            // The anchor point is the endpoint of segment[seg-1], or subpath.start.
            let anchor = if vr.segment == 0 {
                subpath.start
            } else {
                subpath.segments[vr.segment - 1].endpoint()
            };

            let ctrl1 = match &subpath.segments[vr.segment] {
                Segment::Cubic { ctrl1, .. } => *ctrl1,
                _ => return,
            };

            // The opposite handle is ctrl2 of segment[seg-1] (incoming to anchor).
            if vr.segment == 0 {
                return; // No previous segment to constrain.
            }
            let prev = &mut subpath.segments[vr.segment - 1];
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
                .get(vr.segment + 1)
                .copied()
                .unwrap_or(VertexMode::Corner);
            if anchor_mode == VertexMode::Corner {
                return;
            }

            let anchor = subpath.segments[vr.segment].endpoint();
            let ctrl2 = match &subpath.segments[vr.segment] {
                Segment::Cubic { ctrl2, .. } => *ctrl2,
                _ => return,
            };

            // The opposite handle is ctrl1 of segment[seg+1] (outgoing from anchor).
            let next_idx = vr.segment + 1;
            if next_idx >= subpath.segments.len() {
                return; // No next segment to constrain.
            }
            let next = &mut subpath.segments[next_idx];
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
        let seg = &mut subpath.segments[seg_idx];
        match seg {
            Segment::Line { to } => {
                let dx = to.x - anchor.x;
                let dy = to.y - anchor.y;
                *seg = Segment::Cubic {
                    ctrl1: Point::new(
                        anchor.x + dx * HANDLE_FRACTION,
                        anchor.y + dy * HANDLE_FRACTION,
                    ),
                    ctrl2: *to,
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
        && let Segment::Cubic { ctrl1, .. } = subpath.segments[out_idx]
        && let Segment::Cubic { ctrl2, .. } = &mut subpath.segments[in_idx]
    {
        mirror_handle(anchor, ctrl1, ctrl2, new_mode);
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
            let opp_len = (opp_dx * opp_dx + opp_dy * opp_dy).sqrt();
            // Opposite direction, same length as before.
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

/// What the select tool is currently doing.
enum DragMode {
    /// Not dragging.
    Idle,
    /// Dragging selected vertices. Stores last canvas position.
    MoveVertices { prev: [f64; 2] },
    /// Dragging entire objects by translating their transforms.
    MoveObjects { prev: [f64; 2] },
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
                    return;
                }
                let bounds = node_bounds(&node.data, world);
                if !bounds.is_empty() && bounds.contains_point(target) {
                    best = Some(id);
                }
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
                    return;
                }
                let bounds = node_bounds(&node.data, world);
                if !bounds.is_empty() && bounds.intersects(query) {
                    result.push(id);
                }
            },
        );

        result
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

    /// Cycle the vertex mode of the anchor vertex at `vr` through
    /// Corner → Smooth → Symmetric → Corner.
    /// Only applies to anchor points (SubpathStart or Endpoint).
    /// Returns the new mode, or None if inapplicable.
    pub fn cycle_vertex_mode(scene: &mut Scene, vr: &VertexRef) -> Option<VertexMode> {
        let node = scene.get_mut(vr.node)?;
        let NodeData::Path { ref mut path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.get_mut(vr.subpath)?;

        // Determine which vertex_modes index this anchor maps to.
        let mode_idx = match vr.kind {
            PointKind::SubpathStart => 0,
            PointKind::Endpoint => vr.segment + 1,
            // Control points aren't anchors — cycle the anchor they belong to.
            PointKind::CubicCtrl1 | PointKind::QuadCtrl => vr.segment,
            PointKind::CubicCtrl2 => vr.segment + 1,
        };

        let mode = subpath.vertex_modes.get_mut(mode_idx)?;
        let old_mode = *mode;
        *mode = match *mode {
            VertexMode::Corner => VertexMode::Smooth,
            VertexMode::Smooth => VertexMode::Symmetric,
            VertexMode::Symmetric => VertexMode::Corner,
        };
        let new_mode = *mode;

        // When switching FROM Corner TO Smooth/Symmetric, ensure adjacent
        // segments are cubics with handles spread out from the anchor.
        // Otherwise the control points sit on top of the vertex and are
        // invisible/unselectable.
        if old_mode == VertexMode::Corner && new_mode != VertexMode::Corner {
            ensure_cubic_handles(subpath, mode_idx);
        }

        Some(new_mode)
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

            // Clicked outside vertices — exit node mode, fall through to
            // object-level handling below.
            self.exit_node_mode();
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

                    if let Some(node) = scene.get_mut(node_id) {
                        node.transform.tx += local_dx;
                        node.transform.ty += local_dy;
                    }
                }

                *prev = canvas_pos;
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
            let Some(node) = scene.get_mut(vr.node) else {
                continue;
            };
            let NodeData::Path { ref mut path, .. } = node.data else {
                continue;
            };
            let Some(subpath) = path.subpaths.get_mut(vr.subpath) else {
                continue;
            };

            match vr.kind {
                PointKind::SubpathStart => {
                    // Move start to the first segment's endpoint and remove
                    // that segment.
                    if !subpath.segments.is_empty() {
                        subpath.start = subpath.segments[0].endpoint();
                        subpath.segments.remove(0);
                        // Remove the start point's mode, shift the next one
                        // into position 0 (it becomes the new start).
                        if subpath.vertex_modes.len() > 1 {
                            subpath.vertex_modes.remove(0);
                        }
                    }
                }
                PointKind::Endpoint => {
                    if vr.segment < subpath.segments.len() {
                        subpath.segments.remove(vr.segment);
                        // vertex_modes index for this endpoint is vr.segment + 1.
                        let mode_idx = vr.segment + 1;
                        if mode_idx < subpath.vertex_modes.len() {
                            subpath.vertex_modes.remove(mode_idx);
                        }
                    }
                }
                _ => {}
            }
        }

        // Clean up: remove empty subpaths and empty paths.
        for vr in &anchor_vertices {
            let Some(node) = scene.get_mut(vr.node) else {
                continue;
            };
            let NodeData::Path { ref mut path, .. } = node.data else {
                continue;
            };
            // Remove subpaths with no segments (just a lone point).
            path.subpaths.retain(|sp| !sp.segments.is_empty());
            // Mark empty paths for removal.
            if path.subpaths.is_empty() {
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
            }
        }

        best.map(|(pt, _)| pt)
    }

    /// Insert a new anchor point on the edge described by `hit`, splitting
    /// the segment in two. Returns a `VertexRef` to the newly created point.
    pub fn insert_point_on_edge(scene: &mut Scene, hit: &EdgeHit) -> Option<VertexRef> {
        let node = scene.get_mut(hit.node)?;
        let NodeData::Path { ref mut path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.get_mut(hit.subpath)?;
        let seg_idx = hit.segment;
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
        let (first, second) = subpath.segments[seg_idx].split_at(from, hit.t);

        // Replace the original segment with the two halves.
        subpath.segments[seg_idx] = first;
        subpath.segments.insert(seg_idx + 1, second);
        // Insert a vertex mode for the new split point (Corner by default).
        subpath.vertex_modes.insert(seg_idx + 1, VertexMode::Corner);

        // The new point is the endpoint of `first` (at seg_idx).
        Some(VertexRef {
            node: hit.node,
            subpath: hit.subpath,
            segment: seg_idx,
            kind: PointKind::Endpoint,
        })
    }
}

/// Normalize two corners into (min, max).
fn marquee_rect(a: [f64; 2], b: [f64; 2]) -> ([f64; 2], [f64; 2]) {
    (
        [a[0].min(b[0]), a[1].min(b[1])],
        [a[0].max(b[0]), a[1].max(b[1])],
    )
}
