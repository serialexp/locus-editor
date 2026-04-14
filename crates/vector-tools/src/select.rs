use vector_geom::{Point, Segment};
use vector_scene::{NodeData, NodeId, Scene};

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
    /// Drawing a marquee rectangle. Stores the anchor corner in canvas coords
    /// and whether shift was held at the start.
    Marquee {
        anchor: [f64; 2],
        current: [f64; 2],
        shift: bool,
    },
}

/// State for the select tool — tracks object and vertex selection.
///
/// Selection is two-level:
/// 1. **Object selection** (`selected_nodes`) — which paths/groups are active.
///    Handles are shown for object-selected nodes.
/// 2. **Vertex selection** (`selected`) — individual control points within
///    object-selected nodes, for direct manipulation.
pub struct SelectState {
    /// Object-level selection: which nodes are "active".
    pub selected_nodes: Vec<NodeId>,
    /// Nodes inside the active marquee (previewed, not yet committed).
    pub marquee_preview_nodes: Vec<NodeId>,
    /// Currently selected vertices (within object-selected nodes).
    pub selected: Vec<VertexRef>,
    /// Vertex currently under the cursor (only within object-selected nodes).
    pub hovered: Option<VertexRef>,
    /// Current drag mode.
    drag_mode: DragMode,
}

impl Default for SelectState {
    fn default() -> Self {
        Self {
            selected_nodes: Vec::new(),
            marquee_preview_nodes: Vec::new(),
            selected: Vec::new(),
            hovered: None,
            drag_mode: DragMode::Idle,
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
            for (sp_idx, subpath) in path.subpaths.iter().enumerate() {
                f(
                    VertexRef {
                        node: node_id,
                        subpath: sp_idx,
                        segment: 0,
                        kind: PointKind::SubpathStart,
                    },
                    subpath.start,
                );

                for (seg_idx, seg) in subpath.segments.iter().enumerate() {
                    f(
                        VertexRef {
                            node: node_id,
                            subpath: sp_idx,
                            segment: seg_idx,
                            kind: PointKind::Endpoint,
                        },
                        seg.endpoint(),
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
                                *ctrl,
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
                                *ctrl1,
                            );
                            f(
                                VertexRef {
                                    node: node_id,
                                    subpath: sp_idx,
                                    segment: seg_idx,
                                    kind: PointKind::CubicCtrl2,
                                },
                                *ctrl2,
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
            &mut |id, node, _world| {
                if !node.visible {
                    return;
                }
                if let NodeData::Path { ref path, .. } = node.data {
                    let bounds = path.bounding_box();
                    if !bounds.is_empty() && bounds.contains_point(target) {
                        best = Some(id);
                    }
                }
            },
        );

        best
    }

    /// Find all visible path nodes whose bounding boxes intersect the given rect.
    pub fn objects_in_rect(scene: &Scene, min: [f64; 2], max: [f64; 2]) -> Vec<NodeId> {
        let query =
            vector_geom::Bounds::new(Point::new(min[0], min[1]), Point::new(max[0], max[1]));
        let mut result = Vec::new();

        let root = scene.root();
        scene.walk_depth_first(
            root,
            vector_geom::Affine::IDENTITY,
            &mut |id, node, _world| {
                if !node.visible {
                    return;
                }
                if let NodeData::Path { ref path, .. } = node.data {
                    let bounds = path.bounding_box();
                    if !bounds.is_empty() && bounds.intersects(query) {
                        result.push(id);
                    }
                }
            },
        );

        result
    }

    // ── Hover ────────────────────────────────────────────────────────

    /// Update the hover state for vertices within object-selected nodes.
    /// Returns `true` if the hovered vertex changed (caller should request redraw).
    pub fn update_hover(&mut self, scene: &Scene, canvas_pos: [f64; 2], zoom: f64) -> bool {
        let new_hover = if self.selected_nodes.is_empty() {
            None
        } else {
            Self::hit_test_in_nodes(scene, canvas_pos, zoom, &self.selected_nodes)
        };
        if new_hover != self.hovered {
            self.hovered = new_hover;
            true
        } else {
            false
        }
    }

    // ── Press / drag / release ───────────────────────────────────────

    /// Handle a mouse press at `canvas_pos`.
    ///
    /// Priority:
    /// 1. Vertex hit on an already-object-selected node → vertex select + drag.
    /// 2. Object hit on any visible path → object select (clears vertex selection).
    /// 3. Empty space → clear all, start marquee for object selection.
    pub fn on_press(&mut self, scene: &Scene, canvas_pos: [f64; 2], shift: bool, zoom: f64) {
        // 1. Try vertex hit within object-selected nodes.
        let vertex_hit = if !self.selected_nodes.is_empty() {
            Self::hit_test_in_nodes(scene, canvas_pos, zoom, &self.selected_nodes)
        } else {
            None
        };

        if let Some(vr) = vertex_hit {
            // Vertex hit — do vertex-level selection.
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

        // 2. Try object hit.
        let object_hit = Self::object_hit_test(scene, canvas_pos);

        if let Some(node_id) = object_hit {
            // Clear vertex selection when changing object selection.
            self.selected.clear();

            if shift {
                // Toggle node in object selection.
                if let Some(idx) = self.selected_nodes.iter().position(|n| *n == node_id) {
                    self.selected_nodes.remove(idx);
                } else {
                    self.selected_nodes.push(node_id);
                }
            } else if !self.selected_nodes.contains(&node_id) {
                self.selected_nodes.clear();
                self.selected_nodes.push(node_id);
            }
            // Don't start a drag — object selection is instant.
            return;
        }

        // 3. Empty space — clear and start marquee.
        if !shift {
            self.selected_nodes.clear();
            self.selected.clear();
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
                    vr.translate(scene, dx, dy);
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
    fn delete_selected_vertices(&mut self, scene: &mut Scene) -> bool {
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
                    }
                }
                PointKind::Endpoint => {
                    if vr.segment < subpath.segments.len() {
                        subpath.segments.remove(vr.segment);
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
        let target = Point::new(canvas_pos[0], canvas_pos[1]);
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
