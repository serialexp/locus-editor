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
use vector_trace::TraceParams;

use crate::camera::Camera;
use crate::hud::PerfStats;
use crate::llm_dialog::LlmDialogState;
use crate::recent_files::RecentFiles;
use crate::snap::{SnapHit, SnapSettings};
use crate::svg_drop_dialog::SvgDropDialogState;
use crate::trace_dialog::TraceDialogState;

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
    /// Active raster-trace dialog (None when closed). While `Some`,
    /// the canvas hosts a live preview group that re-runs the tracer
    /// in the background as parameters change. The "Trace raster
    /// image…" menu item is greyed out while this is `Some` so the
    /// user can't open a second one on top.
    pub(crate) trace_dialog: Option<TraceDialogState>,
    /// Last-used trace parameters, persisted across re-imports for
    /// the lifetime of the session. Initialised to vtracer's `Poster`
    /// preset (matches the previous one-shot import default), then
    /// updated whenever the trace dialog closes (Apply or Cancel).
    pub(crate) last_trace_params: TraceParams,
    /// Performance stats (FPS + RSS), updated once per rendered frame.
    pub(crate) perf: PerfStats,
    /// Whether the title-bar performance HUD is visible.
    pub(crate) show_perf_hud: bool,
    /// Structure-panel collapse state. Absence = expanded; `true` = collapsed.
    /// We own this rather than relying on `CollapsingHeader`'s widget-owned
    /// state because the tree is virtualized — a given group's widget may
    /// not exist on every frame, so egui can't persist collapse state for it.
    pub(crate) structure_collapse: HashMap<NodeId, bool>,
    /// Monotonic counter bumped whenever `structure_collapse` is mutated
    /// from the structure panel. Combined with `Scene::ui_revision()`, this
    /// is the cache key for `cached_flatten` — re-flattening only happens
    /// when one of the two changes.
    pub(crate) structure_collapse_rev: u64,
    /// Cached output of `flatten_tree(scene, structure_collapse)`. Kept on
    /// the editor state so per-frame re-flattening is avoided when
    /// nothing the structure panel cares about has changed.
    pub(crate) cached_flatten: Option<crate::structure_panel::FlattenCache>,
    /// Inline rename in the structure panel. `Some((node, buffer))` while the
    /// user is editing a row's label; `None` otherwise. Set by double-clicking
    /// the label area; cleared on commit (Enter / focus loss) or cancel (Esc).
    pub(crate) structure_rename: Option<(NodeId, String)>,
    /// Currently focused gradient stop in the properties-panel gradient
    /// editor: `(paint_node_id, stop_index)`. Set when the user clicks a
    /// stop marker; consumed by the canvas Delete handler so pressing
    /// Delete while a stop is focused removes that stop instead of the
    /// owning object. Cleared when the focused paint node disappears,
    /// when its stop index goes out of bounds, or when Delete consumes it.
    pub(crate) gradient_stop_focus: Option<(NodeId, usize)>,
    /// Most recent geometry-snap target, set by `screen_to_canvas_snapped`.
    /// Cleared when the cursor moves without snapping. Consumed by the
    /// renderer to draw an on-canvas indicator at the snap point.
    pub(crate) last_snap: Option<SnapHit>,
    /// Recently-opened files, persisted across sessions in the platform
    /// per-app config dir. Loaded once on startup; resaved whenever a file
    /// is opened (via menu or drag-drop) or the list is cleared.
    pub(crate) recent_files: RecentFiles,
    /// Active LLM-driven SVG generation dialog (None when closed).
    /// Like `trace_dialog`, hosts a live preview group and re-renders
    /// the preview each time the user sends another chat turn. The
    /// "Generate SVG from prompt…" menu item is greyed out while this
    /// is `Some` so two dialogs can't fight over the same preview slot.
    pub(crate) llm_dialog: Option<LlmDialogState>,
    /// Pending drop of an SVG file awaiting the user's "replace document
    /// vs add as group" choice. `Some` only while the choice dialog is
    /// open. The dropped file bytes are held here so the choice handler
    /// doesn't have to re-read the source file (which may have changed
    /// or been deleted while the dialog sat open).
    pub(crate) svg_drop_dialog: Option<SvgDropDialogState>,
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
            trace_dialog: None,
            last_trace_params: TraceParams::default(),
            perf: PerfStats::new(),
            show_perf_hud: true,
            structure_collapse: HashMap::new(),
            structure_collapse_rev: 0,
            cached_flatten: None,
            structure_rename: None,
            gradient_stop_focus: None,
            last_snap: None,
            recent_files: RecentFiles::load(),
            llm_dialog: None,
            svg_drop_dialog: None,
        }
    }
}

impl EditorState {
    /// Nodes that should be excluded as snap targets given the current
    /// tool / interaction state. Pen and shape tools exclude their
    /// in-progress node so its own vertices don't snap to themselves.
    /// Select-tool drags exclude the nodes whose vertices/transforms
    /// are currently moving.
    pub(crate) fn snap_exclude(&self) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        if let Some(id) = self.pen_state.building_node() {
            out.push(id);
        }
        if let Some(id) = self.shape_draw.preview_node() {
            out.push(id);
        }
        // Select tool: while dragging vertices or moving objects, exclude
        // the affected nodes so geometry snap finds *other* shapes.
        if self.select_state.is_dragging_vertices()
            || self.select_state.is_dragging_objects()
            || self.select_state.is_scaling()
            || self.select_state.is_rotating()
        {
            for &id in &self.select_state.selected_nodes {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    /// Convert screen coordinates to canvas coordinates, applying snap if enabled.
    ///
    /// `exclude` lists nodes whose own geometry should not be snap targets —
    /// typically the node currently being dragged or the partial node being
    /// extended by the pen tool. Pass `&[]` when no exclusion is appropriate
    /// (e.g. a fresh shape draw).
    ///
    /// Side-effect: stores the resolved snap indicator (or `None`) on
    /// `self.last_snap`, so the renderer can draw a marker at the target.
    pub(crate) fn screen_to_canvas_snapped(
        &mut self,
        screen_x: f32,
        screen_y: f32,
        exclude: &[NodeId],
    ) -> [f64; 2] {
        let c = self.camera.screen_to_canvas(screen_x, screen_y);
        let (snapped, hit) = self.snap.resolve(
            [c[0] as f64, c[1] as f64],
            &self.scene,
            self.camera.zoom as f64,
            exclude,
        );
        self.last_snap = hit;
        snapped
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

    /// Replace the current document with the SVG decoded from `bytes`.
    /// Clears undo history and selection, re-zooms to fit on the next
    /// frame, and records the file in the recent-files list. This is
    /// the behaviour you get from File → Open SVG… and from drag-drop
    /// onto an empty document.
    pub(crate) fn load_svg_replace(
        &mut self,
        bytes: &[u8],
        path: &std::path::Path,
        renderer: &mut Renderer,
    ) -> Result<(), String> {
        let scene = vector_svg::import_svg(bytes).map_err(|e| format!("{e}"))?;
        self.scene = scene;
        self.history = History::new();
        self.select_state = SelectState::default();
        self.pending_zoom_to_fit = true;
        self.recent_files.add(path);
        self.recent_files.save();
        renderer.mark_dirty();
        Ok(())
    }

    /// Add the SVG decoded from `bytes` as a new top-level group on
    /// top of the current document. The group is labelled with the
    /// file stem. Recorded as a single undoable insert
    /// (`Command::Delete` is the inverse — undo removes the whole
    /// imported subtree in one step).
    ///
    /// Returns the new group's id on success. The file is added to
    /// the recent-files list either way — opening a file via drop
    /// counts as "opening" even when it's being layered in.
    pub(crate) fn load_svg_as_group(
        &mut self,
        bytes: &[u8],
        path: &std::path::Path,
        renderer: &mut Renderer,
    ) -> Result<NodeId, String> {
        let source = vector_svg::import_svg(bytes).map_err(|e| format!("{e}"))?;
        let label = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Imported SVG")
            .to_string();
        let dest_parent = self.scene.root();
        let new_group = vector_scene::merge_as_group(&mut self.scene, dest_parent, &source, label)
            .ok_or_else(|| "merge_as_group failed (root parent vanished?)".to_string())?;

        // Undo = remove the freshly-inserted group (and its merged
        // defs descendants are cleaned up automatically because
        // they're parented under the new group's defs copies, which
        // Delete walks). Same shape as insert_raster_from_bytes.
        self.history.record_undo(Command::Delete { id: new_group });

        // Select the new group so it's obvious in the UI where the
        // import landed and so further actions (e.g. moving it) act on
        // it by default.
        self.select_state.selected_nodes.clear();
        self.select_state.selected_nodes.push(new_group);
        self.select_state.selected.clear();
        self.select_state.hovered = None;

        self.recent_files.add(path);
        self.recent_files.save();
        renderer.mark_dirty();
        Ok(new_group)
    }

    /// True iff the scene has any user-authored content — i.e. the
    /// root has at least one child that isn't the defs subtree.
    /// Used by the drop handler to decide whether to prompt the user
    /// for a replace-vs-add-as-group choice (skipped on empty docs,
    /// where the question only has one sensible answer).
    pub(crate) fn scene_has_user_content(&self) -> bool {
        let defs = self.scene.defs();
        self.scene
            .get(self.scene.root())
            .map(|n| n.children.iter().any(|&c| c != defs))
            .unwrap_or(false)
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

    /// Nudge the current selection by a world-space delta `(dx, dy)`.
    /// Vertex selection wins over object selection (matches drag-tool
    /// precedence). Records a single undo command per call. Returns `true`
    /// if anything moved.
    ///
    /// Caller is responsible for picking `dx`/`dy` — typically derived from
    /// camera zoom so that the on-screen step is constant (e.g. 1 screen
    /// pixel per tap → `1.0 / zoom` canvas units).
    pub(crate) fn nudge_selection(&mut self, dx: f64, dy: f64) -> bool {
        if dx == 0.0 && dy == 0.0 {
            return false;
        }

        // Vertex nudge: translate every selected vertex, snapshot+undo
        // path data for affected nodes. Mirrors `record_vertex_drag_undo`.
        if !self.select_state.selected.is_empty() {
            self.snapshot_selected_vertex_paths();
            let mut moved = false;
            for vr in self.select_state.selected.clone() {
                let world = self.scene.world_transform(vr.node);
                let (lx, ly) = if world.is_identity() {
                    (dx, dy)
                } else if let Some(inv) = world.inverse() {
                    (inv.a * dx + inv.b * dy, inv.c * dx + inv.d * dy)
                } else {
                    continue;
                };
                vr.translate(&mut self.scene, lx, ly);
                if matches!(
                    vr.kind,
                    vector_tools::PointKind::CubicCtrl1
                        | vector_tools::PointKind::CubicCtrl2
                        | vector_tools::PointKind::QuadCtrl
                ) {
                    vector_tools::enforce_vertex_constraint(&vr, &mut self.scene);
                }
                moved = true;
            }
            if moved {
                self.record_vertex_drag_undo();
            } else {
                // Discard the snapshot we took above.
                self.drag_path_snapshots.clear();
            }
            return moved;
        }

        // Object nudge: translate every selected node's transform.
        if !self.select_state.selected_nodes.is_empty() {
            self.snapshot_selected_transforms();
            let mut moved = false;
            for &node_id in &self.select_state.selected_nodes.clone() {
                let parent_world = self.scene.parent_world_transform(node_id);
                let (local_dx, local_dy) = if parent_world.is_identity() {
                    (dx, dy)
                } else if let Some(inv) = parent_world.inverse() {
                    (inv.a * dx + inv.b * dy, inv.c * dx + inv.d * dy)
                } else {
                    continue;
                };
                if let Some(node) = self.scene.get(node_id) {
                    let mut t = node.transform;
                    t.tx += local_dx;
                    t.ty += local_dy;
                    self.scene.set_transform(node_id, t);
                    moved = true;
                }
            }
            if moved {
                self.record_object_drag_undo();
            } else {
                self.drag_transform_snapshots.clear();
            }
            return moved;
        }

        false
    }

    /// If a gradient stop is currently focused (via the properties-panel
    /// gradient editor) AND the focused gradient is being used by one of
    /// the selected nodes' fill or stroke, remove that stop with undo
    /// support and return `true`. Otherwise return `false`, letting the
    /// caller fall through to normal selection delete.
    ///
    /// Refuses to delete if the gradient would drop below 2 stops, since
    /// `show_gradient_editor` maintains the same invariant.
    pub(crate) fn try_delete_focused_gradient_stop(&mut self) -> bool {
        let Some((paint_id, idx)) = self.gradient_stop_focus else {
            return false;
        };
        // Only consume the focus when the focused paint is referenced by
        // the current selection. Otherwise the user has clicked away to a
        // different shape and Delete should target that shape.
        let used_by_selection = self.select_state.selected_nodes.iter().any(|&nid| {
            let Some(node) = self.scene.get(nid) else {
                return false;
            };
            let mut found = false;
            node.for_each_paint_ref(|p| {
                if matches!(p, vector_scene::PaintRef::Ref(id) if *id == paint_id) {
                    found = true;
                }
            });
            found
        });
        if !used_by_selection {
            return false;
        }
        let Some(node) = self.scene.get(paint_id) else {
            self.gradient_stop_focus = None;
            return false;
        };
        let vector_scene::NodeData::Paint(vector_scene::Paint::Gradient(g)) = &node.data else {
            self.gradient_stop_focus = None;
            return false;
        };
        if idx >= g.stops.len() {
            self.gradient_stop_focus = None;
            return false;
        }
        if g.stops.len() <= 2 {
            // Gradients must keep ≥2 stops; fall through silently rather
            // than fall back to deleting the owning object — the user
            // didn't ask for that.
            return false;
        }
        let mut new_g = g.clone();
        new_g.stops.remove(idx);
        let new_focus_idx = idx.min(new_g.stops.len() - 1);
        self.history.execute(
            Command::SetGradient {
                id: paint_id,
                gradient: new_g,
            },
            &mut self.scene,
        );
        self.gradient_stop_focus = Some((paint_id, new_focus_idx));
        true
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
            // If the user just deleted the group they were inside (or one
            // of its ancestors), pop the stack back to the nearest still-
            // living entered group — or top level if all are gone.
            self.select_state.validate_group_scope(&self.scene);
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
        // Undo can remove nodes that the scope stack still references.
        self.select_state.validate_group_scope(&self.scene);
    }

    /// Perform redo: apply the top redo command to the scene.
    pub(crate) fn redo(&mut self) {
        self.history.redo(&mut self.scene);
        // Clear selection — node IDs may have changed.
        self.select_state.selected_nodes.clear();
        self.select_state.selected.clear();
        self.select_state.hovered = None;
        // Redo can re-delete a group the scope stack still references.
        self.select_state.validate_group_scope(&self.scene);
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
                    // Winit's CursorIcon enum has no dedicated "rotate" cursor,
                    // so `Grab` is the closest semantic match — it suggests the
                    // user is about to grab and drag. `Crosshair` (the previous
                    // choice) reads as a precision pointer and was misleading.
                    window.set_cursor(winit::window::CursorIcon::Grab);
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
