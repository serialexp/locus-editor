//! Editor state — all non-GPU mutable state for the editor. Kept separate
//! from `GpuState` so disjoint borrows work cleanly when passed to free
//! functions that build UI panels or respond to events.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;

use winit::window::Window;

use vector_geom::{Affine, Path};
use vector_ops::{Command, History};
use vector_render::Renderer;
use vector_scene::{Gradient, NodeData, NodeId, RasterImage, Scene};
use vector_tools::{
    EdgeHit, PenAction, PenState, SelectState, ShapeDrawState, TextAction, TextToolState, ToolType,
    VertexRef,
};

use crate::camera::Camera;
use crate::hud::PerfStats;
use crate::snap::SnapSettings;

/// What kind of canvas element was right-clicked, driving the context menu.
pub(crate) enum CanvasContextTarget {
    /// Right-clicked on a vertex (anchor or control point).
    Vertex(VertexRef),
    /// Right-clicked on a segment edge.
    Segment(EdgeHit),
}

/// Persistent state for a canvas right-click context menu.
pub(crate) struct CanvasContextMenu {
    /// What was right-clicked.
    pub(crate) target: CanvasContextTarget,
    /// Screen-space position where the menu should appear.
    pub(crate) screen_pos: egui::Pos2,
    /// Whether this is the first frame (used to spawn the Area).
    pub(crate) first_frame: bool,
}

/// All editor state except the GPU resources. Extracted as a struct so it
/// can be passed as a single `&mut EditorState` to free functions that need
/// disjoint borrows from `GpuState`.
pub(crate) struct EditorState {
    pub(crate) scene: Scene,
    pub(crate) history: History,
    pub(crate) active_tool: ToolType,
    pub(crate) select_state: SelectState,
    pub(crate) shape_draw: ShapeDrawState,
    pub(crate) pen_state: PenState,
    pub(crate) text_tool: TextToolState,
    pub(crate) camera: Camera,
    pub(crate) snap: SnapSettings,
    /// When true, the checkerboard background uses a fixed screen-pixel size
    /// instead of scaling with zoom.
    pub(crate) checker_fixed_size: bool,
    /// The canvas area in screen pixels (excluding egui panels), updated each frame.
    pub(crate) canvas_rect: [f32; 4],
    /// Current cursor position in screen pixels.
    pub(crate) cursor_pos: Option<[f32; 2]>,
    /// Whether middle mouse (or space+left) is held for panning.
    pub(crate) is_panning: bool,
    /// Whether left mouse is held (for tool dragging).
    pub(crate) is_left_down: bool,
    /// Whether to zoom-to-fit the scene on the next frame.
    pub(crate) pending_zoom_to_fit: bool,
    /// For double-click detection: time + screen position of the last left press.
    pub(crate) last_left_press: Option<(Instant, [f32; 2])>,
    /// Snapshots of path data captured at the start of a vertex drag,
    /// so we can record undo commands when the drag finishes.
    pub(crate) drag_path_snapshots: Vec<(NodeId, Path)>,
    /// Snapshots of transforms captured at the start of an object drag,
    /// so we can record undo commands when the drag finishes.
    pub(crate) drag_transform_snapshots: Vec<(NodeId, Affine)>,
    /// Snapshot of the gradient captured at the start of an on-canvas
    /// gradient handle drag, so we can record an undo command when the
    /// drag finishes. `(paint_node_id, gradient_before)`.
    pub(crate) drag_gradient_snapshot: Option<(NodeId, Gradient)>,
    /// Active canvas context menu (right-click on vertex/segment).
    pub(crate) canvas_context_menu: Option<CanvasContextMenu>,
    /// Performance stats (FPS + RSS), updated once per rendered frame.
    pub(crate) perf: PerfStats,
    /// Whether the title-bar performance HUD is visible.
    pub(crate) show_perf_hud: bool,
    /// Structure-panel collapse state. Absence = expanded; `true` = collapsed.
    /// We own this rather than relying on `CollapsingHeader`'s widget-owned
    /// state because the tree is virtualized — a given group's widget may
    /// not exist on every frame, so egui can't persist collapse state for it.
    pub(crate) structure_collapse: HashMap<NodeId, bool>,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            scene: Scene::new(),
            history: History::new(),
            active_tool: ToolType::default(),
            select_state: SelectState::default(),
            shape_draw: ShapeDrawState::default(),
            pen_state: PenState::default(),
            text_tool: TextToolState::default(),
            camera: Camera::default(),
            snap: SnapSettings::default(),
            checker_fixed_size: true,
            canvas_rect: [0.0, 0.0, 1280.0, 800.0],
            cursor_pos: None,
            is_panning: false,
            is_left_down: false,
            pending_zoom_to_fit: false,
            last_left_press: None,
            drag_path_snapshots: Vec::new(),
            drag_transform_snapshots: Vec::new(),
            drag_gradient_snapshot: None,
            canvas_context_menu: None,
            perf: PerfStats::new(),
            show_perf_hud: true,
            structure_collapse: HashMap::new(),
        }
    }
}

impl EditorState {
    /// Convert screen coordinates to canvas coordinates, applying snap if enabled.
    pub(crate) fn screen_to_canvas_snapped(&self, screen_x: f32, screen_y: f32) -> [f64; 2] {
        let c = self.camera.screen_to_canvas(screen_x, screen_y);
        self.snap.snap([c[0] as f64, c[1] as f64])
    }

    /// Decode `bytes` as a raster image (PNG/JPEG/GIF/BMP/WEBP/TIFF) and
    /// insert it as a new `Raster` node centred on the current viewport.
    /// The image's local box is `(0, 0)..(width, height)` in pixels (so 1 px
    /// = 1 canvas unit at zoom 1.0); the node's transform places the centre
    /// of that box on the centre of the visible canvas.
    ///
    /// On success the new node is selected and an undo command is recorded.
    /// `label` is used for the structure-panel display name.
    ///
    /// Used by both drag-drop (`WindowEvent::DroppedFile` for raster files)
    /// and the File → Insert Image… menu — the decode path is identical, so
    /// it lives here.
    pub(crate) fn insert_raster_from_bytes(
        &mut self,
        bytes: &[u8],
        label: impl Into<String>,
        renderer: &mut Renderer,
    ) -> Result<NodeId, String> {
        let decoded = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| format!("guess image format: {e}"))?
            .decode()
            .map_err(|e| format!("decode image: {e}"))?
            .to_rgba8();
        let pixel_width = decoded.width();
        let pixel_height = decoded.height();
        if pixel_width == 0 || pixel_height == 0 {
            return Err("image has zero width or height".into());
        }

        let image = Arc::new(RasterImage::new(
            decoded.into_raw(),
            pixel_width,
            pixel_height,
        ));

        // Centre on the visible viewport. canvas_rect is screen-pixel
        // (min_x, min_y, w, h); convert its centre to canvas coords.
        let [cx, cy, cw, ch] = self.canvas_rect;
        let centre = self.camera.screen_to_canvas(cx + cw * 0.5, cy + ch * 0.5);
        let w = pixel_width as f64;
        let h = pixel_height as f64;
        let transform = Affine::translate(centre[0] as f64 - w * 0.5, centre[1] as f64 - h * 0.5);

        let mut node = vector_scene::Node::raster(label, image, w, h);
        node.transform = transform;

        // Insert directly so we can grab the new NodeId, then record the
        // matching undo (Delete). Mirrors the pen tool's PenAction::Finished
        // pattern in `handle_pen_action` — same reason: `History::execute`
        // doesn't return the inserted id.
        let parent = self.scene.root();
        let id = self
            .scene
            .insert(parent, node)
            .ok_or_else(|| "scene insert failed".to_string())?;
        self.history.record_undo(Command::Delete { id });

        // Select the new node so it shows handles and is obvious in the UI.
        self.select_state.selected_nodes.clear();
        self.select_state.selected_nodes.push(id);
        self.select_state.selected.clear();
        self.select_state.hovered = None;

        renderer.mark_dirty();
        Ok(id)
    }

    /// Process a PenAction result — auto-select, switch tools, mark dirty.
    pub(crate) fn handle_pen_action(&mut self, action: PenAction, renderer: &mut Renderer) {
        match action {
            PenAction::None => {}
            PenAction::Continue => {
                if let Some(node_id) = self.pen_state.building_node()
                    && !self.select_state.selected_nodes.contains(&node_id)
                {
                    self.select_state.selected_nodes.clear();
                    self.select_state.selected_nodes.push(node_id);
                    self.select_state.enter_node_mode();
                }
                renderer.mark_dirty();
            }
            PenAction::Finished(node_id) => {
                // Record undo: the pen tool created this node,
                // so undoing means deleting it.
                self.history.record_undo(Command::Delete { id: node_id });
                self.select_state.selected_nodes.clear();
                self.select_state.selected_nodes.push(node_id);
                self.select_state.enter_node_mode();
                self.active_tool = ToolType::Select;
                renderer.mark_dirty();
            }
            PenAction::Cancelled => {
                self.select_state.selected_nodes.clear();
                renderer.mark_dirty();
            }
        }
    }

    /// Process a TextAction result — record undo, update selection, mark dirty.
    pub(crate) fn handle_text_action(&mut self, action: TextAction, renderer: &mut Renderer) {
        match action {
            TextAction::None => {}
            TextAction::Continue => {
                if let Some(node_id) = self.text_tool.editing_node()
                    && !self.select_state.selected_nodes.contains(&node_id)
                {
                    self.select_state.selected_nodes.clear();
                    self.select_state.selected_nodes.push(node_id);
                }
                renderer.mark_dirty();
            }
            TextAction::Committed(node_id, original) => {
                self.history.record_undo(Command::SetTextData {
                    id: node_id,
                    text: original,
                });
                self.select_state.selected_nodes.clear();
                self.select_state.selected_nodes.push(node_id);
                renderer.mark_dirty();
            }
            TextAction::Created(node_id) => {
                // Undo = delete the newly created node.
                self.history.record_undo(Command::Delete { id: node_id });
                self.select_state.selected_nodes.clear();
                self.select_state.selected_nodes.push(node_id);
                renderer.mark_dirty();
            }
            TextAction::Cancelled => {
                self.select_state.selected_nodes.clear();
                renderer.mark_dirty();
            }
        }
    }

    /// Snapshot the path data of all nodes that have selected vertices,
    /// storing them in `drag_path_snapshots` for undo when the drag ends.
    pub(crate) fn snapshot_selected_vertex_paths(&mut self) {
        self.drag_path_snapshots.clear();
        // Collect unique node IDs from the selected vertices.
        let mut seen = Vec::new();
        for vr in &self.select_state.selected {
            if !seen.contains(&vr.node) {
                seen.push(vr.node);
            }
        }
        for node_id in seen {
            if let Some(node) = self.scene.get(node_id)
                && let NodeData::Path { ref path, .. } = node.data
            {
                self.drag_path_snapshots.push((node_id, path.clone()));
            }
        }
    }

    /// Record undo commands for any path data that changed since the last
    /// snapshot. Called when a vertex drag ends.
    pub(crate) fn record_vertex_drag_undo(&mut self) {
        if self.drag_path_snapshots.is_empty() {
            return;
        }
        let snapshots = std::mem::take(&mut self.drag_path_snapshots);
        let cmds: Vec<Command> = snapshots
            .into_iter()
            .map(|(id, path)| Command::SetPathData { id, path })
            .collect();
        if cmds.len() == 1 {
            self.history.record_undo(cmds.into_iter().next().unwrap());
        } else if !cmds.is_empty() {
            self.history.record_undo(Command::Batch(cmds));
        }
    }

    /// Snapshot the gradient currently being dragged on-canvas, so we can
    /// record an undo command when the drag finishes. The handle being
    /// dragged determines which paint node to capture.
    pub(crate) fn snapshot_gradient_for_drag(&mut self) {
        self.drag_gradient_snapshot = None;
        let Some(handle) = self.select_state.dragging_gradient_handle() else {
            return;
        };
        if let Some(node) = self.scene.get(handle.paint)
            && let NodeData::Paint(vector_scene::Paint::Gradient(ref g)) = node.data
        {
            self.drag_gradient_snapshot = Some((handle.paint, g.clone()));
        }
    }

    /// Record an undo command for the gradient handle drag that just
    /// finished. The undo restores the gradient captured at drag start.
    /// We always record when a snapshot was taken: the only path that
    /// reaches `record_gradient_drag_undo` is a completed handle drag,
    /// and a no-op drag (e.g. mouse-down then mouse-up without movement)
    /// is rare enough that an extra undo entry is acceptable.
    pub(crate) fn record_gradient_drag_undo(&mut self) {
        let Some((id, gradient)) = self.drag_gradient_snapshot.take() else {
            return;
        };
        if !matches!(
            self.scene.get(id).map(|n| &n.data),
            Some(NodeData::Paint(vector_scene::Paint::Gradient(_)))
        ) {
            return;
        }
        self.history
            .record_undo(Command::SetGradient { id, gradient });
    }

    /// Snapshot transforms of all selected objects for undo.
    pub(crate) fn snapshot_selected_transforms(&mut self) {
        self.drag_transform_snapshots.clear();
        for &node_id in &self.select_state.selected_nodes {
            if let Some(node) = self.scene.get(node_id) {
                self.drag_transform_snapshots
                    .push((node_id, node.transform));
            }
        }
    }

    /// Record undo commands for any transforms that changed since the last
    /// snapshot. Called when an object drag ends.
    pub(crate) fn record_object_drag_undo(&mut self) {
        if self.drag_transform_snapshots.is_empty() {
            return;
        }
        let snapshots = std::mem::take(&mut self.drag_transform_snapshots);
        let cmds: Vec<Command> = snapshots
            .into_iter()
            .map(|(id, transform)| Command::SetTransform { id, transform })
            .collect();
        if cmds.len() == 1 {
            self.history.record_undo(cmds.into_iter().next().unwrap());
        } else if !cmds.is_empty() {
            self.history.record_undo(Command::Batch(cmds));
        }
    }

    /// Snapshot the path data of a single node, for undo of edge insert or
    /// vertex deletion.
    pub(crate) fn snapshot_path(&self, node_id: NodeId) -> Option<Path> {
        let node = self.scene.get(node_id)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        Some(path.clone())
    }

    /// Delete the current selection with proper undo recording.
    /// Returns `true` if anything was deleted.
    pub(crate) fn delete_with_undo(&mut self) -> bool {
        if !self.select_state.selected.is_empty() {
            // Vertex deletion: snapshot affected nodes before modifying.
            let mut affected: Vec<NodeId> = Vec::new();
            for vr in &self.select_state.selected {
                if !affected.contains(&vr.node) {
                    affected.push(vr.node);
                }
            }

            // Snapshot each affected node: its path data, parent, and child
            // index — in case delete_selected_vertices fully removes it.
            struct NodeInfo {
                id: NodeId,
                path: Path,
                parent: NodeId,
                index: usize,
                snapshot: vector_scene::NodeSnapshot,
            }
            let infos: Vec<NodeInfo> = affected
                .iter()
                .filter_map(|&id| {
                    let path = self.snapshot_path(id)?;
                    let parent = self.scene.parent(id)?;
                    let index = self.scene.child_index(id).unwrap_or(0);
                    let snapshot = self.scene.snapshot_subtree(id)?;
                    Some(NodeInfo {
                        id,
                        path,
                        parent,
                        index,
                        snapshot,
                    })
                })
                .collect();

            let result = self.select_state.delete_selected_vertices(&mut self.scene);

            if result {
                let mut cmds = Vec::new();
                for info in infos {
                    if self.scene.get(info.id).is_some() {
                        // Node still exists — undo restores old path data.
                        cmds.push(Command::SetPathData {
                            id: info.id,
                            path: info.path,
                        });
                    } else {
                        // Node was fully removed — undo re-inserts the subtree.
                        cmds.push(Command::InsertSubtree {
                            parent: info.parent,
                            index: info.index,
                            snapshot: Box::new(info.snapshot),
                        });
                    }
                }
                if cmds.len() == 1 {
                    self.history.record_undo(cmds.into_iter().next().unwrap());
                } else if !cmds.is_empty() {
                    self.history.record_undo(Command::Batch(cmds));
                }
            }
            result
        } else if !self.select_state.selected_nodes.is_empty() {
            // Object deletion: use Command::Delete through history.execute().
            let nodes: Vec<NodeId> = self.select_state.selected_nodes.drain(..).collect();
            let cmds: Vec<Command> = nodes.into_iter().map(|id| Command::Delete { id }).collect();
            let cmd = if cmds.len() == 1 {
                cmds.into_iter().next().unwrap()
            } else {
                Command::Batch(cmds)
            };
            self.history.execute(cmd, &mut self.scene);
            self.select_state.selected.clear();
            self.select_state.hovered = None;
            true
        } else {
            false
        }
    }

    /// Perform undo: apply the top undo command to the scene.
    pub(crate) fn undo(&mut self) {
        self.history.undo(&mut self.scene);
        // Clear selection — node IDs may have changed.
        self.select_state.selected_nodes.clear();
        self.select_state.selected.clear();
        self.select_state.hovered = None;
    }

    /// Perform redo: apply the top redo command to the scene.
    pub(crate) fn redo(&mut self) {
        self.history.redo(&mut self.scene);
        // Clear selection — node IDs may have changed.
        self.select_state.selected_nodes.clear();
        self.select_state.selected.clear();
        self.select_state.hovered = None;
    }

    /// Update the mouse cursor icon based on tool, hover state, and panning.
    pub(crate) fn update_cursor(&mut self, window: &Window, egui_ctx: &egui::Context) {
        // Use is_pointer_over_egui() instead of egui_wants_pointer_input() —
        // the latter only returns true when a widget actively wants input,
        // but we need to suppress canvas hover whenever the pointer is over
        // any egui panel (structure panel, properties, toolbar, etc.).
        if egui_ctx.is_pointer_over_egui() {
            window.set_cursor(winit::window::CursorIcon::Default);
            // Clear hover when over egui
            if self.select_state.hovered.is_some() {
                self.select_state.hovered = None;
                window.request_redraw();
            }
            return;
        }

        if self.is_panning || self.select_state.is_dragging_vertices() {
            window.set_cursor(winit::window::CursorIcon::Grabbing);
        } else if self.active_tool == ToolType::Select {
            if let Some(pos) = self.cursor_pos {
                let canvas = self.camera.screen_to_canvas(pos[0], pos[1]);
                let canvas_f64 = [canvas[0] as f64, canvas[1] as f64];

                // Update vertex hover (only within object-selected nodes)
                let hover_changed = self.select_state.update_hover(
                    &self.scene,
                    canvas_f64,
                    self.camera.zoom as f64,
                );
                if hover_changed {
                    window.request_redraw();
                }

                // Cursor: grab for selected vertex, resize for scale handles,
                // rotation for corner zone, pointer for clickable object,
                // default otherwise.
                if self.select_state.is_rotating() || self.select_state.is_scaling() {
                    window.set_cursor(winit::window::CursorIcon::Grabbing);
                } else if self
                    .select_state
                    .hovered
                    .is_some_and(|vr| self.select_state.selected.contains(&vr))
                {
                    window.set_cursor(winit::window::CursorIcon::Grab);
                } else if let Some(handle) = self.select_state.hit_scale_handle(
                    &self.scene,
                    canvas_f64,
                    self.camera.zoom as f64,
                ) {
                    use vector_tools::ScaleHandle;
                    let cursor = match handle {
                        ScaleHandle::TopLeft | ScaleHandle::BottomRight => {
                            winit::window::CursorIcon::NwseResize
                        }
                        ScaleHandle::TopRight | ScaleHandle::BottomLeft => {
                            winit::window::CursorIcon::NeswResize
                        }
                        ScaleHandle::Top | ScaleHandle::Bottom => {
                            winit::window::CursorIcon::NsResize
                        }
                        ScaleHandle::Left | ScaleHandle::Right => {
                            winit::window::CursorIcon::EwResize
                        }
                    };
                    window.set_cursor(cursor);
                } else if self.select_state.hit_rotation_zone(
                    &self.scene,
                    canvas_f64,
                    self.camera.zoom as f64,
                ) {
                    // Crosshair when hovering the rotation zone outside bbox corners.
                    window.set_cursor(winit::window::CursorIcon::Crosshair);
                } else if self.select_state.hovered.is_some()
                    || SelectState::object_hit_test(&self.scene, canvas_f64).is_some()
                {
                    window.set_cursor(winit::window::CursorIcon::Pointer);
                } else {
                    window.set_cursor(winit::window::CursorIcon::Default);
                }
            } else {
                if self.select_state.hovered.is_some() {
                    self.select_state.hovered = None;
                    window.request_redraw();
                }
                window.set_cursor(winit::window::CursorIcon::Default);
            }
        } else if self.active_tool == ToolType::Pen
            || self.active_tool == ToolType::Rectangle
            || self.active_tool == ToolType::Ellipse
        {
            window.set_cursor(winit::window::CursorIcon::Crosshair);
        } else if self.active_tool == ToolType::Text {
            window.set_cursor(winit::window::CursorIcon::Text);
        } else {
            window.set_cursor(winit::window::CursorIcon::Default);
        }
    }
}
