//! Pen tool — draw paths by clicking to place anchor points.
//!
//! Interaction model (Illustrator/Inkscape style):
//!
//! - **Click** to place a corner point (line segment).
//! - **Click-drag** to place a smooth point — the drag direction and distance
//!   define mirrored cubic bezier handles.
//! - **Click on start point** to close the path (requires ≥ 2 segments).
//! - **Click on last point** to finish the path as open.
//! - **Right-click** to undo the last placed point.
//! - **Enter** or **Escape** to finish as an open path.
//! - Switching tools finishes as an open path.

use vector_geom::{Color, Path, Point, Segment, SubPath};
use vector_scene::{NodeData, NodeId, PaintRef, Scene};

/// Hit-test radius in screen pixels (divided by zoom for canvas-space radius).
const CLOSE_RADIUS_SCREEN_PX: f64 = 8.0;

/// Minimum drag distance in screen pixels to count as "defining handles"
/// (below this threshold, the point is treated as a corner).
const MIN_DRAG_SCREEN_PX: f64 = 3.0;

/// Result of a pen tool action.
pub enum PenAction {
    /// Nothing happened.
    None,
    /// Building continues — caller should mark dirty and redraw.
    Continue,
    /// Path was finished. Contains the node ID for auto-selection.
    Finished(NodeId),
    /// Building was cancelled (node removed, nothing to select).
    Cancelled,
}

/// State for the pen (path drawing) tool.
#[derive(Default)]
pub struct PenState {
    /// The node being constructed. `None` when idle.
    building_node: Option<NodeId>,
    /// Number of finalized ("committed") segments. The path's subpath may
    /// contain one additional segment at the end for the hover preview.
    committed_count: usize,
    /// Outgoing handle of the very first anchor point. Needed when closing
    /// the path — the closing segment's ctrl2 is the mirror of this.
    start_outgoing: Option<Point>,
    /// Outgoing handle of the most recently committed anchor point.
    /// Becomes ctrl1 of the next segment.
    prev_outgoing: Option<Point>,
    /// Stack of `prev_outgoing` values before each commit, for undo.
    outgoing_stack: Vec<Option<Point>>,
    /// Anchor of the current press-drag gesture. `Some` between press and
    /// release; `None` when hovering between clicks.
    drag_anchor: Option<Point>,
}

// Default is derived — all fields are `None`, `0`, or empty `Vec`.

impl PenState {
    /// Whether a path is currently being built.
    pub fn is_building(&self) -> bool {
        self.building_node.is_some()
    }

    /// The node currently under construction, if any.
    pub fn building_node(&self) -> Option<NodeId> {
        self.building_node
    }

    // ── Press ───────────────────────────────────────────────────────

    /// Handle left mouse press at `canvas_pos`.
    pub fn on_press(&mut self, scene: &mut Scene, canvas_pos: [f64; 2], zoom: f64) -> PenAction {
        let pos = Point::new(canvas_pos[0], canvas_pos[1]);
        let radius = CLOSE_RADIUS_SCREEN_PX / zoom;

        if self.building_node.is_some() {
            // Already building — check special targets first.
            let start = self.subpath_start(scene);
            let last = self.last_committed_anchor(scene);

            // Close path: click near start point (need ≥ 2 committed segments).
            if self.committed_count >= 2
                && let Some(s) = start
                && pos.distance(s) < radius
            {
                return self.close_path(scene);
            }

            // Finish open: click near the last committed anchor.
            if self.committed_count >= 1
                && let Some(l) = last
                && pos.distance(l) < radius
            {
                return self.finish(scene, false);
            }

            // Normal case — place a new anchor.
            self.trim_to_committed(scene);
            let prev_anchor = self
                .last_committed_anchor(scene)
                .or_else(|| self.subpath_start(scene))
                .unwrap_or(pos);
            let segment = self.make_segment(prev_anchor, pos);
            self.push_segment(scene, segment);
            self.outgoing_stack.push(self.prev_outgoing);
            self.committed_count += 1;
            self.prev_outgoing = None; // Set on release if dragged.
            self.drag_anchor = Some(pos);

            PenAction::Continue
        } else {
            // Start a new path.
            let mut path = Path::new();
            path.subpaths.push(SubPath::new(pos));

            let mut node = vector_scene::Node::path("Path", path);
            // Default style for pen: no fill, thin black stroke (so you can
            // see the path as you draw it).
            if let NodeData::Path { ref mut style, .. } = node.data {
                style.fill = None;
                style.stroke = Some(vector_scene::style::Stroke {
                    paint: PaintRef::Solid(Color::BLACK),
                    style: vector_scene::StrokeStyle::default(),
                    opacity: 1.0,
                });
            }

            let root = scene.root();
            self.building_node = scene.insert(root, node);
            self.committed_count = 0;
            self.start_outgoing = None;
            self.prev_outgoing = None;
            self.outgoing_stack.clear();
            self.drag_anchor = Some(pos);

            PenAction::Continue
        }
    }

    // ── Drag ────────────────────────────────────────────────────────

    /// Handle mouse move while the left button is held (drag to define handles).
    /// Returns `true` if the scene changed and needs a redraw.
    pub fn on_drag(&mut self, scene: &mut Scene, canvas_pos: [f64; 2]) -> bool {
        let Some(anchor) = self.drag_anchor else {
            return false;
        };
        let drag_pos = Point::new(canvas_pos[0], canvas_pos[1]);

        if self.committed_count == 0 {
            // Dragging the first point — no segments exist yet.
            // We could render handle guides here, but for now just track it.
            // The outgoing handle will be set on release.
            return false;
        }

        // Update the last committed segment's ctrl2 (incoming handle of the
        // anchor being dragged). The mirror of the drag position around the
        // anchor gives the incoming handle.
        let mirror = Point::new(2.0 * anchor.x - drag_pos.x, 2.0 * anchor.y - drag_pos.y);

        // Read ctrl1 from the outgoing_stack before entering the closure
        // (can't borrow self inside with_subpath_mut).
        let outgoing = *self.outgoing_stack.last().unwrap_or(&None);
        let seg_idx = self.committed_count - 1;

        self.with_subpath_mut(scene, |subpath| {
            let prev_anchor = if seg_idx == 0 {
                subpath.start
            } else {
                subpath.segments[seg_idx - 1].endpoint()
            };
            let Some(seg) = subpath.segments.get_mut(seg_idx) else {
                return false;
            };
            let ctrl1 = outgoing.unwrap_or(prev_anchor);
            *seg = Segment::Cubic {
                ctrl1,
                ctrl2: mirror,
                to: anchor,
            };
            true
        })
        .unwrap_or(false)
    }

    // ── Release ─────────────────────────────────────────────────────

    /// Handle left mouse release after press (or press-drag).
    /// Returns `true` if the scene changed.
    pub fn on_release(&mut self, scene: &mut Scene, canvas_pos: [f64; 2], zoom: f64) -> bool {
        let Some(anchor) = self.drag_anchor.take() else {
            return false;
        };

        let drag_pos = Point::new(canvas_pos[0], canvas_pos[1]);
        let drag_dist_screen = anchor.distance(drag_pos) * zoom;

        if drag_dist_screen >= MIN_DRAG_SCREEN_PX {
            // Significant drag — store outgoing handle → smooth/symmetric vertex.
            if self.committed_count == 0 {
                // First point: record start outgoing, upgrade start vertex mode.
                self.start_outgoing = Some(drag_pos);
                self.prev_outgoing = Some(drag_pos);
                self.with_subpath_mut(scene, |subpath| {
                    subpath.vertex_modes[0] = vector_geom::VertexMode::Symmetric;
                });
            } else {
                self.prev_outgoing = Some(drag_pos);
                // Upgrade the last committed endpoint to Symmetric.
                let committed = self.committed_count;
                self.with_subpath_mut(scene, |subpath| {
                    // vertex_modes index: 0 = start, committed_count = last endpoint.
                    if let Some(mode) = subpath.vertex_modes.get_mut(committed) {
                        *mode = vector_geom::VertexMode::Symmetric;
                    }
                });
            }
        } else {
            // Click (no drag) — corner point, no outgoing handle.
            self.prev_outgoing = None;
            if self.committed_count == 0 {
                self.start_outgoing = None;
            }
        }

        true
    }

    // ── Hover preview ───────────────────────────────────────────────

    /// Handle mouse move when not pressing (hover). Draws a preview segment
    /// from the last committed anchor to the cursor position.
    /// Returns `true` if the scene changed.
    pub fn on_move(&mut self, scene: &mut Scene, canvas_pos: [f64; 2]) -> bool {
        if self.building_node.is_none() || self.drag_anchor.is_some() {
            return false;
        }

        let cursor = Point::new(canvas_pos[0], canvas_pos[1]);
        let prev_anchor = self
            .last_committed_anchor(scene)
            .or_else(|| self.subpath_start(scene))
            .unwrap_or(cursor);

        // Build the preview segment.
        let preview = self.make_segment(prev_anchor, cursor);

        let committed = self.committed_count;
        self.with_subpath_mut(scene, |subpath| {
            // The subpath should have exactly committed_count segments
            // before we add/update the preview.
            if subpath.segments.len() > committed {
                // Replace existing preview.
                subpath.segments[committed] = preview;
            } else {
                // Add new preview.
                subpath.push_segment(preview, vector_geom::VertexMode::Corner);
            }
        })
        .is_some()
    }

    // ── Undo last point (right-click) ───────────────────────────────

    /// Undo the last placed point. Returns a `PenAction` indicating
    /// whether building continues or was cancelled (no points left).
    pub fn undo_last(&mut self, scene: &mut Scene) -> PenAction {
        if self.building_node.is_none() {
            return PenAction::None;
        }

        // Remove preview if present.
        self.trim_to_committed(scene);

        if self.committed_count > 0 {
            // Remove the last committed segment.
            self.with_subpath_mut(scene, |subpath| {
                if !subpath.segments.is_empty() {
                    subpath.segments.pop();
                    subpath.vertex_modes.pop();
                }
            });
            self.committed_count -= 1;
            self.prev_outgoing = self.outgoing_stack.pop().flatten();
            // Also clear drag state in case undo happens mid-drag.
            self.drag_anchor = None;

            if self.committed_count == 0 {
                // Back to just the start point — clear start outgoing too
                // since we might re-drag it.
                self.start_outgoing = None;
            }

            PenAction::Continue
        } else {
            // No segments left — cancel the whole path.
            self.cancel(scene)
        }
    }

    // ── Finish / Close / Cancel ─────────────────────────────────────

    /// Finish the path as open or closed.
    /// Removes the hover preview, optionally adds a closing segment.
    /// Returns `Finished(id)` or `Cancelled` if degenerate.
    pub fn finish(&mut self, scene: &mut Scene, close: bool) -> PenAction {
        let Some(node_id) = self.building_node.take() else {
            return PenAction::None;
        };

        // Remove preview segment.
        self.trim_to_committed_with_node(scene, node_id);

        if close {
            // Add closing segment from last anchor back to start.
            let prev_outgoing = self.prev_outgoing;
            let start_outgoing = self.start_outgoing;
            scene.with_path_data_mut(node_id, |path| {
                let Some(subpath) = path.subpaths.first_mut() else {
                    return;
                };
                let start = subpath.start;
                let last_anchor = subpath
                    .segments
                    .last()
                    .map(|s| s.endpoint())
                    .unwrap_or(start);

                let ctrl1 = prev_outgoing.unwrap_or(last_anchor);
                let ctrl2 = start_outgoing
                    .map(|h| Point::new(2.0 * start.x - h.x, 2.0 * start.y - h.y))
                    .unwrap_or(start);

                let closing_seg =
                    if ctrl1.distance(last_anchor) < 1e-9 && ctrl2.distance(start) < 1e-9 {
                        Segment::Line { to: start }
                    } else {
                        Segment::Cubic {
                            ctrl1,
                            ctrl2,
                            to: start,
                        }
                    };

                subpath.push_segment(closing_seg, vector_geom::VertexMode::Corner);
                subpath.closed = true;
            });
        }

        // Remove degenerate paths (no segments).
        let is_degenerate = scene.get(node_id).is_some_and(|n| match &n.data {
            NodeData::Path { path, .. } => path
                .subpaths
                .first()
                .is_none_or(|sp| sp.segments.is_empty()),
            _ => true,
        });

        self.reset();

        if is_degenerate {
            scene.remove(node_id);
            PenAction::Cancelled
        } else {
            PenAction::Finished(node_id)
        }
    }

    /// Cancel building entirely — remove the node from the scene.
    pub fn cancel(&mut self, scene: &mut Scene) -> PenAction {
        if let Some(id) = self.building_node.take() {
            scene.remove(id);
        }
        self.reset();
        PenAction::Cancelled
    }

    // ── Internal helpers ────────────────────────────────────────────

    /// Reset all transient state.
    fn reset(&mut self) {
        self.building_node = None;
        self.committed_count = 0;
        self.start_outgoing = None;
        self.prev_outgoing = None;
        self.outgoing_stack.clear();
        self.drag_anchor = None;
    }

    /// Close the path by adding a closing segment and finishing.
    fn close_path(&mut self, scene: &mut Scene) -> PenAction {
        // Remove preview first.
        self.trim_to_committed(scene);
        self.finish(scene, true)
    }

    /// Get the start point of the building subpath.
    fn subpath_start(&self, scene: &Scene) -> Option<Point> {
        let node = scene.get(self.building_node?)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        path.subpaths.first().map(|sp| sp.start)
    }

    /// Get the endpoint of the last committed segment, or `None` if no
    /// segments have been committed yet.
    fn last_committed_anchor(&self, scene: &Scene) -> Option<Point> {
        if self.committed_count == 0 {
            return None;
        }
        let node = scene.get(self.building_node?)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        let subpath = path.subpaths.first()?;
        subpath
            .segments
            .get(self.committed_count - 1)
            .map(|s| s.endpoint())
    }

    /// Remove any preview segment beyond the committed count.
    fn trim_to_committed(&mut self, scene: &mut Scene) {
        let Some(node_id) = self.building_node else {
            return;
        };
        self.trim_to_committed_with_node(scene, node_id);
    }

    fn trim_to_committed_with_node(&self, scene: &mut Scene, node_id: NodeId) {
        let committed = self.committed_count;
        scene.with_path_data_mut(node_id, |path| {
            if let Some(subpath) = path.subpaths.first_mut() {
                subpath.segments.truncate(committed);
                // +1 because vertex_modes[0] is the start point.
                subpath.vertex_modes.truncate(committed + 1);
            }
        });
    }

    /// Run `f` with mutable access to the building subpath, returning
    /// `f`'s result. Returns `None` if we're not building, the node was
    /// lost, the node isn't a path, or the path has no subpath yet.
    /// Auto-bumps `geometry_rev` via `with_path_data_mut` if the call
    /// reached the closure.
    fn with_subpath_mut<F, R>(&self, scene: &mut Scene, f: F) -> Option<R>
    where
        F: FnOnce(&mut SubPath) -> R,
    {
        let node_id = self.building_node?;
        scene
            .with_path_data_mut(node_id, |path| path.subpaths.first_mut().map(f))
            .flatten()
    }

    /// Push a segment onto the building subpath.
    /// The vertex mode for the new endpoint defaults to Corner;
    /// it gets upgraded to Symmetric on drag release.
    fn push_segment(&self, scene: &mut Scene, segment: Segment) {
        self.with_subpath_mut(scene, |subpath| {
            subpath.push_segment(segment, vector_geom::VertexMode::Corner);
        });
    }

    /// Build the appropriate segment type from `prev_anchor` to `to`,
    /// using the current `prev_outgoing` handle.
    fn make_segment(&self, _prev_anchor: Point, to: Point) -> Segment {
        match self.prev_outgoing {
            Some(ctrl1) => {
                // Previous anchor had an outgoing handle → cubic.
                // ctrl2 defaults to the target point (corner-like arrival).
                // During drag this gets updated to the mirror of the drag.
                Segment::Cubic {
                    ctrl1,
                    ctrl2: to,
                    to,
                }
            }
            None => {
                // Previous anchor was a corner → straight line.
                Segment::Line { to }
            }
        }
    }
}
