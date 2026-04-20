use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use vector_geom::{Affine, Path};
use vector_ops::{Command, History};
use vector_render::Renderer;
use vector_scene::{NodeData, NodeId, Scene};
use vector_tools::{
    EdgeHit, PenAction, PenState, PointKind, SelectState, ShapeDrawState, TextAction,
    TextToolState, ToolType, VertexRef,
};

/// Camera state for canvas pan/zoom.
struct Camera {
    /// Offset in screen pixels (how much the canvas origin has been dragged).
    pan: [f32; 2],
    /// Zoom level (1.0 = 100%).
    zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            pan: [0.0, 0.0],
            zoom: 1.0,
        }
    }
}

impl Camera {
    /// Convert screen pixel coordinates to canvas (scene) coordinates.
    pub fn screen_to_canvas(&self, screen_x: f32, screen_y: f32) -> [f32; 2] {
        [
            (screen_x - self.pan[0]) / self.zoom,
            (screen_y - self.pan[1]) / self.zoom,
        ]
    }

    /// Set pan and zoom so that `bounds` (in canvas coordinates) fits inside
    /// the screen-space rectangle described by `viewport` (min_x, min_y, width,
    /// height in screen pixels), with some padding.
    pub fn zoom_to_fit(&mut self, bounds: vector_geom::Bounds, viewport: [f32; 4]) {
        if bounds.is_empty() {
            return;
        }
        let [vx, vy, vw, vh] = viewport;
        let content_w = bounds.width() as f32;
        let content_h = bounds.height() as f32;
        if content_w <= 0.0 || content_h <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return;
        }

        // Leave 5% padding on each side
        let padding_frac = 0.05;
        let usable_w = vw * (1.0 - 2.0 * padding_frac);
        let usable_h = vh * (1.0 - 2.0 * padding_frac);

        // Zoom to fit the smaller axis
        self.zoom = (usable_w / content_w).min(usable_h / content_h);
        self.zoom = self.zoom.clamp(0.05, 100.0);

        // Pan so that the center of the content maps to the center of the viewport.
        // screen = canvas * zoom + pan  =>  pan = screen_center - canvas_center * zoom
        let center = bounds.center();
        let viewport_center_x = vx + vw * 0.5;
        let viewport_center_y = vy + vh * 0.5;
        self.pan[0] = viewport_center_x - center.x as f32 * self.zoom;
        self.pan[1] = viewport_center_y - center.y as f32 * self.zoom;
    }

    /// Zoom by a factor, keeping the given screen point fixed.
    pub fn zoom_at(&mut self, factor: f32, screen_x: f32, screen_y: f32) {
        // Point in canvas coords before zoom
        let before = self.screen_to_canvas(screen_x, screen_y);
        self.zoom *= factor;
        self.zoom = self.zoom.clamp(0.05, 100.0);
        // Adjust pan so that `before` maps back to (screen_x, screen_y)
        self.pan[0] = screen_x - before[0] * self.zoom;
        self.pan[1] = screen_y - before[1] * self.zoom;
    }
}

/// Initialized GPU + window state. Created on first resume.
struct GpuState {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    renderer: Renderer,
}

/// Snapping configuration — editor preference, not stored in SVG.
#[derive(Clone)]
struct SnapSettings {
    /// Whether grid snapping is enabled.
    grid_enabled: bool,
    /// Grid spacing in canvas units.
    grid_size: f64,
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self {
            grid_enabled: false,
            grid_size: 1.0,
        }
    }
}

impl SnapSettings {
    /// Snap a canvas-space coordinate according to current settings.
    fn snap(&self, pos: [f64; 2]) -> [f64; 2] {
        if self.grid_enabled {
            let g = self.grid_size;
            [(pos[0] / g).round() * g, (pos[1] / g).round() * g]
        } else {
            pos
        }
    }
}

/// What kind of canvas element was right-clicked, driving the context menu.
enum CanvasContextTarget {
    /// Right-clicked on a vertex (anchor or control point).
    Vertex(VertexRef),
    /// Right-clicked on a segment edge.
    Segment(EdgeHit),
}

/// Persistent state for a canvas right-click context menu.
struct CanvasContextMenu {
    /// What was right-clicked.
    target: CanvasContextTarget,
    /// Screen-space position where the menu should appear.
    screen_pos: egui::Pos2,
    /// Whether this is the first frame (used to spawn the Area).
    first_frame: bool,
}

/// All editor state except the GPU resources. Extracted as a struct so it
/// can be passed as a single `&mut EditorState` to free functions that need
/// disjoint borrows from `GpuState`.
struct EditorState {
    scene: Scene,
    history: History,
    active_tool: ToolType,
    select_state: SelectState,
    shape_draw: ShapeDrawState,
    pen_state: PenState,
    text_tool: TextToolState,
    camera: Camera,
    snap: SnapSettings,
    /// When true, the checkerboard background uses a fixed screen-pixel size
    /// instead of scaling with zoom.
    checker_fixed_size: bool,
    /// The canvas area in screen pixels (excluding egui panels), updated each frame.
    canvas_rect: [f32; 4],
    /// Current cursor position in screen pixels.
    cursor_pos: Option<[f32; 2]>,
    /// Whether middle mouse (or space+left) is held for panning.
    is_panning: bool,
    /// Whether left mouse is held (for tool dragging).
    is_left_down: bool,
    /// Whether to zoom-to-fit the scene on the next frame.
    pending_zoom_to_fit: bool,
    /// For double-click detection: time + screen position of the last left press.
    last_left_press: Option<(Instant, [f32; 2])>,
    /// Snapshots of path data captured at the start of a vertex drag,
    /// so we can record undo commands when the drag finishes.
    drag_path_snapshots: Vec<(NodeId, Path)>,
    /// Snapshots of transforms captured at the start of an object drag,
    /// so we can record undo commands when the drag finishes.
    drag_transform_snapshots: Vec<(NodeId, Affine)>,
    /// Active canvas context menu (right-click on vertex/segment).
    canvas_context_menu: Option<CanvasContextMenu>,
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
            canvas_context_menu: None,
        }
    }
}

impl EditorState {
    /// Convert screen coordinates to canvas coordinates, applying snap if enabled.
    fn screen_to_canvas_snapped(&self, screen_x: f32, screen_y: f32) -> [f64; 2] {
        let c = self.camera.screen_to_canvas(screen_x, screen_y);
        self.snap.snap([c[0] as f64, c[1] as f64])
    }

    /// Process a PenAction result — auto-select, switch tools, mark dirty.
    fn handle_pen_action(&mut self, action: PenAction, renderer: &mut Renderer) {
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
    fn handle_text_action(&mut self, action: TextAction, renderer: &mut Renderer) {
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
    fn snapshot_selected_vertex_paths(&mut self) {
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
    fn record_vertex_drag_undo(&mut self) {
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

    /// Snapshot transforms of all selected objects for undo.
    fn snapshot_selected_transforms(&mut self) {
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
    fn record_object_drag_undo(&mut self) {
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
    fn snapshot_path(&self, node_id: NodeId) -> Option<Path> {
        let node = self.scene.get(node_id)?;
        let NodeData::Path { ref path, .. } = node.data else {
            return None;
        };
        Some(path.clone())
    }

    /// Delete the current selection with proper undo recording.
    /// Returns `true` if anything was deleted.
    fn delete_with_undo(&mut self) -> bool {
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
    fn undo(&mut self) {
        self.history.undo(&mut self.scene);
        // Clear selection — node IDs may have changed.
        self.select_state.selected_nodes.clear();
        self.select_state.selected.clear();
        self.select_state.hovered = None;
    }

    /// Perform redo: apply the top redo command to the scene.
    fn redo(&mut self) {
        self.history.redo(&mut self.scene);
        // Clear selection — node IDs may have changed.
        self.select_state.selected_nodes.clear();
        self.select_state.selected.clear();
        self.select_state.hovered = None;
    }

    /// Update the mouse cursor icon based on tool, hover state, and panning.
    fn update_cursor(&mut self, gpu: &GpuState) {
        // Use is_pointer_over_egui() instead of egui_wants_pointer_input() —
        // the latter only returns true when a widget actively wants input,
        // but we need to suppress canvas hover whenever the pointer is over
        // any egui panel (structure panel, properties, toolbar, etc.).
        if gpu.egui_ctx.is_pointer_over_egui() {
            gpu.window.set_cursor(winit::window::CursorIcon::Default);
            // Clear hover when over egui
            if self.select_state.hovered.is_some() {
                self.select_state.hovered = None;
                gpu.window.request_redraw();
            }
            return;
        }

        if self.is_panning || self.select_state.is_dragging_vertices() {
            gpu.window.set_cursor(winit::window::CursorIcon::Grabbing);
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
                    gpu.window.request_redraw();
                }

                // Cursor: grab for selected vertex, resize for scale handles,
                // rotation for corner zone, pointer for clickable object,
                // default otherwise.
                if self.select_state.is_rotating() || self.select_state.is_scaling() {
                    gpu.window.set_cursor(winit::window::CursorIcon::Grabbing);
                } else if self
                    .select_state
                    .hovered
                    .is_some_and(|vr| self.select_state.selected.contains(&vr))
                {
                    gpu.window.set_cursor(winit::window::CursorIcon::Grab);
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
                    gpu.window.set_cursor(cursor);
                } else if self.select_state.hit_rotation_zone(
                    &self.scene,
                    canvas_f64,
                    self.camera.zoom as f64,
                ) {
                    // Crosshair when hovering the rotation zone outside bbox corners.
                    gpu.window.set_cursor(winit::window::CursorIcon::Crosshair);
                } else if self.select_state.hovered.is_some()
                    || SelectState::object_hit_test(&self.scene, canvas_f64).is_some()
                {
                    gpu.window.set_cursor(winit::window::CursorIcon::Pointer);
                } else {
                    gpu.window.set_cursor(winit::window::CursorIcon::Default);
                }
            } else {
                if self.select_state.hovered.is_some() {
                    self.select_state.hovered = None;
                    gpu.window.request_redraw();
                }
                gpu.window.set_cursor(winit::window::CursorIcon::Default);
            }
        } else if self.active_tool == ToolType::Pen
            || self.active_tool == ToolType::Rectangle
            || self.active_tool == ToolType::Ellipse
        {
            gpu.window.set_cursor(winit::window::CursorIcon::Crosshair);
        } else if self.active_tool == ToolType::Text {
            gpu.window.set_cursor(winit::window::CursorIcon::Text);
        } else {
            gpu.window.set_cursor(winit::window::CursorIcon::Default);
        }
    }
}

#[derive(Default)]
pub struct App {
    gpu: Option<GpuState>,
    state: EditorState,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }

        let window_attrs = Window::default_attributes()
            .with_title("Vector Editor")
            .with_inner_size(winit::dpi::LogicalSize::new(1280, 800));
        let window = Arc::new(
            event_loop
                .create_window(window_attrs)
                .expect("create window"),
        );

        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(instance_desc);

        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");

        let (adapter, device, queue) = pollster::block_on(async {
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: Some(&surface),
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    ..Default::default()
                })
                .await
                .expect("no suitable GPU adapter");

            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("vector editor device"),
                    ..Default::default()
                })
                .await
                .expect("request device");

            (adapter, device, queue)
        });

        let size = window.inner_size();
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // egui setup
        let egui_ctx = egui::Context::default();
        {
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            egui_ctx.set_fonts(fonts);
        }
        // Disable text selection on labels — we're a graphics editor, not a text
        // document. Selectable labels cause confusing drag-to-select behavior
        // when hovering over the structure/properties panels.
        egui_ctx.global_style_mut(|s| {
            s.interaction.selectable_labels = false;
        });
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            None,
            None,
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            surface_format,
            egui_wgpu::RendererOptions::default(),
        );

        let renderer = Renderer::new(&device, surface_format);

        self.gpu = Some(GpuState {
            window,
            device,
            queue,
            surface,
            surface_config,
            egui_ctx,
            egui_state,
            egui_renderer,
            renderer,
        });

        // Create a demo triangle path so we see something on screen
        create_demo_content(&mut self.state.scene);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gpu) = &mut self.gpu else { return };

        // Always handle RedrawRequested and CloseRequested regardless of egui
        match &event {
            WindowEvent::RedrawRequested => {
                // Let the gpu borrow end before calling self.draw()
                let _ = gpu;
                self.draw();
                return;
            }
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            _ => {}
        }

        // Let egui handle the event first
        let response = gpu.egui_state.on_window_event(&gpu.window, &event);

        // Request a redraw so egui can update visually (hover, focus, clicks)
        if response.repaint {
            gpu.window.request_redraw();
        }

        // Only let egui consume mouse/keyboard input events, never cursor movement
        // — we always need cursor_pos updated for canvas interactions (raycasting, hover, etc.).
        let consumed = response.consumed
            && !matches!(
                event,
                WindowEvent::CursorMoved { .. } | WindowEvent::CursorLeft { .. }
            );

        if consumed {
            return;
        }

        match event {
            // Resize is never consumed by egui, but always relevant
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                gpu.surface_config.width = size.width;
                gpu.surface_config.height = size.height;
                gpu.surface.configure(&gpu.device, &gpu.surface_config);
                gpu.renderer.resize(size.width as f32, size.height as f32);
                gpu.window.request_redraw();
            }
            WindowEvent::DroppedFile(path) if path.extension().is_some_and(|e| e == "svg") => {
                match std::fs::read(&path) {
                    Ok(data) => match vector_svg::import_svg(&data) {
                        Ok(scene) => {
                            self.state.scene = scene;
                            self.state.history = History::new();
                            self.state.select_state = SelectState::default();
                            self.state.pending_zoom_to_fit = true;
                            if let Some(gpu) = &mut self.gpu {
                                gpu.renderer.mark_dirty();
                                gpu.window.request_redraw();
                            }
                            log::info!("Loaded SVG: {}", path.display());
                        }
                        Err(e) => log::error!("Failed to import SVG: {e}"),
                    },
                    Err(e) => log::error!("Failed to read file: {e}"),
                }
            }
            // --- Canvas input events: guard with wants_pointer/wants_keyboard ---
            WindowEvent::MouseInput { button, state, .. } => {
                // Always track button-up so is_left_down doesn't get stuck when the
                // release happens over an egui panel (pointer moved from canvas to sidebar).
                if button == MouseButton::Left && state == ElementState::Released {
                    self.state.is_left_down = false;
                }
                if button == MouseButton::Middle && state == ElementState::Released {
                    self.state.is_panning = false;
                }
                if !gpu.egui_ctx.egui_wants_pointer_input() {
                    match button {
                        MouseButton::Middle => {
                            self.state.is_panning = state == ElementState::Pressed;
                        }
                        MouseButton::Left => {
                            if state == ElementState::Pressed {
                                self.state.is_left_down = true;
                                // Close any open canvas context menu.
                                self.state.canvas_context_menu = None;
                                // Double-click detection.
                                let now = Instant::now();
                                let is_double_click = if let Some((prev_time, prev_pos)) =
                                    self.state.last_left_press
                                {
                                    let dt = now.duration_since(prev_time);
                                    let dx = self
                                        .state
                                        .cursor_pos
                                        .map_or(f32::INFINITY, |c| (c[0] - prev_pos[0]).abs());
                                    let dy = self
                                        .state
                                        .cursor_pos
                                        .map_or(f32::INFINITY, |c| (c[1] - prev_pos[1]).abs());
                                    dt.as_millis() < 400 && dx < 5.0 && dy < 5.0
                                } else {
                                    false
                                };
                                self.state.last_left_press =
                                    self.state.cursor_pos.map(|c| (now, c));

                                if let Some(cursor) = self.state.cursor_pos {
                                    let canvas_f64 =
                                        self.state.screen_to_canvas_snapped(cursor[0], cursor[1]);
                                    match self.state.active_tool {
                                        ToolType::Select => {
                                            let mut handled = false;

                                            if is_double_click {
                                                use vector_tools::SelectionMode;
                                                match self.state.select_state.mode {
                                                    SelectionMode::Object => {
                                                        // Double-click in object mode on a
                                                        // selected object → enter node mode.
                                                        if !self
                                                            .state
                                                            .select_state
                                                            .selected_nodes
                                                            .is_empty()
                                                        {
                                                            let hit = SelectState::object_hit_test(
                                                                &self.state.scene,
                                                                canvas_f64,
                                                            );
                                                            if let Some(node_id) = hit
                                                                && self
                                                                    .state
                                                                    .select_state
                                                                    .is_node_selected(node_id)
                                                            {
                                                                // Text nodes: switch to text tool
                                                                // and start editing instead of
                                                                // entering vertex mode.
                                                                let is_text = self
                                                                    .state
                                                                    .scene
                                                                    .get(node_id)
                                                                    .is_some_and(|n| {
                                                                        matches!(
                                                                            n.data,
                                                                            NodeData::Text(_)
                                                                        )
                                                                    });
                                                                if is_text {
                                                                    self.state.active_tool =
                                                                        ToolType::Text;
                                                                    let action = self
                                                                        .state
                                                                        .text_tool
                                                                        .on_press(
                                                                            &mut self.state.scene,
                                                                            canvas_f64,
                                                                            self.state.camera.zoom
                                                                                as f64,
                                                                        );
                                                                    self.state.handle_text_action(
                                                                        action,
                                                                        &mut gpu.renderer,
                                                                    );
                                                                } else {
                                                                    self.state
                                                                        .select_state
                                                                        .enter_node_mode();
                                                                }
                                                                handled = true;
                                                            }
                                                        }
                                                    }
                                                    SelectionMode::Node => {
                                                        // Double-click in node mode:
                                                        // If on a vertex → cycle its mode.
                                                        // If on an edge → insert point.
                                                        if let Some(vr) =
                                                            SelectState::hit_test_in_nodes(
                                                                &self.state.scene,
                                                                canvas_f64,
                                                                self.state.camera.zoom as f64,
                                                                &self
                                                                    .state
                                                                    .select_state
                                                                    .selected_nodes,
                                                            )
                                                        {
                                                            // Cycle vertex mode.
                                                            let old_path =
                                                                self.state.snapshot_path(vr.node);
                                                            if let Some(_new_mode) =
                                                                SelectState::cycle_vertex_mode(
                                                                    &mut self.state.scene,
                                                                    &vr,
                                                                )
                                                            {
                                                                if let Some(path) = old_path {
                                                                    self.state.history.record_undo(
                                                                        Command::SetPathData {
                                                                            id: vr.node,
                                                                            path,
                                                                        },
                                                                    );
                                                                }
                                                                gpu.renderer.mark_dirty();
                                                                handled = true;
                                                            }
                                                        } else if let Some(hit) =
                                                            SelectState::edge_hit_test(
                                                                &self.state.scene,
                                                                canvas_f64,
                                                                self.state.camera.zoom as f64,
                                                                &self
                                                                    .state
                                                                    .select_state
                                                                    .selected_nodes,
                                                            )
                                                        {
                                                            let old_path =
                                                                self.state.snapshot_path(hit.node);
                                                            if let Some(vr) =
                                                                SelectState::insert_point_on_edge(
                                                                    &mut self.state.scene,
                                                                    &hit,
                                                                )
                                                            {
                                                                if let Some(path) = old_path {
                                                                    self.state.history.record_undo(
                                                                        Command::SetPathData {
                                                                            id: hit.node,
                                                                            path,
                                                                        },
                                                                    );
                                                                }
                                                                self.state
                                                                    .select_state
                                                                    .selected
                                                                    .clear();
                                                                self.state
                                                                    .select_state
                                                                    .selected
                                                                    .push(vr);
                                                                gpu.renderer.mark_dirty();
                                                                handled = true;
                                                            }
                                                        }
                                                    }
                                                }
                                            }

                                            if !handled {
                                                let shift =
                                                    gpu.egui_ctx.input(|i| i.modifiers.shift);
                                                self.state.select_state.on_press(
                                                    &self.state.scene,
                                                    canvas_f64,
                                                    shift,
                                                    self.state.camera.zoom as f64,
                                                );
                                                // If on_press started a drag,
                                                // snapshot for undo.
                                                if self.state.select_state.is_dragging_vertices() {
                                                    self.state.snapshot_selected_vertex_paths();
                                                } else if self
                                                    .state
                                                    .select_state
                                                    .is_dragging_objects()
                                                    || self.state.select_state.is_rotating()
                                                    || self.state.select_state.is_scaling()
                                                {
                                                    self.state.snapshot_selected_transforms();
                                                }
                                            }
                                        }
                                        ToolType::Pen => {
                                            let action = self.state.pen_state.on_press(
                                                &mut self.state.scene,
                                                canvas_f64,
                                                self.state.camera.zoom as f64,
                                            );
                                            self.state.handle_pen_action(action, &mut gpu.renderer);
                                        }
                                        ToolType::Rectangle | ToolType::Ellipse => {
                                            self.state.shape_draw.on_press(
                                                &mut self.state.scene,
                                                canvas_f64,
                                                self.state.active_tool,
                                            );
                                            gpu.renderer.mark_dirty();
                                        }
                                        ToolType::Text => {
                                            let action = self.state.text_tool.on_press(
                                                &mut self.state.scene,
                                                canvas_f64,
                                                self.state.camera.zoom as f64,
                                            );
                                            self.state
                                                .handle_text_action(action, &mut gpu.renderer);
                                        }
                                    }
                                    gpu.window.request_redraw();
                                }
                            } else {
                                self.state.is_left_down = false;
                                match self.state.active_tool {
                                    ToolType::Select => {
                                        // If we were dragging, record the undo
                                        // command before ending the drag.
                                        if self.state.select_state.is_dragging_vertices() {
                                            self.state.record_vertex_drag_undo();
                                        } else if self.state.select_state.is_dragging_objects()
                                            || self.state.select_state.is_rotating()
                                            || self.state.select_state.is_scaling()
                                        {
                                            self.state.record_object_drag_undo();
                                        }
                                        self.state.select_state.on_release();
                                    }
                                    ToolType::Pen => {
                                        if let Some(cursor) = self.state.cursor_pos {
                                            let canvas_f64 = self
                                                .state
                                                .screen_to_canvas_snapped(cursor[0], cursor[1]);
                                            self.state.pen_state.on_release(
                                                &mut self.state.scene,
                                                canvas_f64,
                                                self.state.camera.zoom as f64,
                                            );
                                            gpu.renderer.mark_dirty();
                                            gpu.window.request_redraw();
                                        }
                                    }
                                    ToolType::Rectangle | ToolType::Ellipse => {
                                        if let Some(node_id) =
                                            self.state.shape_draw.on_release(&mut self.state.scene)
                                        {
                                            // Record undo: the shape was just created,
                                            // so undoing means deleting it.
                                            self.state
                                                .history
                                                .record_undo(Command::Delete { id: node_id });
                                            // Auto-select the new shape and switch to Select
                                            self.state.select_state.selected_nodes.clear();
                                            self.state.select_state.selected_nodes.push(node_id);
                                            self.state.active_tool = ToolType::Select;
                                        }
                                        gpu.renderer.mark_dirty();
                                        gpu.window.request_redraw();
                                    }
                                    _ => {}
                                }
                            }
                        }
                        MouseButton::Right if state == ElementState::Pressed => {
                            if self.state.active_tool == ToolType::Pen {
                                // Pen tool: undo last pen point.
                                let action = self.state.pen_state.undo_last(&mut self.state.scene);
                                self.state.handle_pen_action(action, &mut gpu.renderer);
                                gpu.window.request_redraw();
                            } else if self.state.active_tool == ToolType::Select
                                && let Some(cursor) = self.state.cursor_pos
                            {
                                // Select tool: open context menu on vertex or segment.
                                let canvas =
                                    self.state.camera.screen_to_canvas(cursor[0], cursor[1]);
                                let canvas_f64 = [canvas[0] as f64, canvas[1] as f64];
                                let zoom = self.state.camera.zoom as f64;
                                let screen_pos = egui::Pos2::new(cursor[0], cursor[1]);
                                let nodes = &self.state.select_state.selected_nodes;

                                if self.state.select_state.mode == vector_tools::SelectionMode::Node
                                    && !nodes.is_empty()
                                {
                                    // Node mode: check vertex first, then edge.
                                    if let Some(vr) = SelectState::hit_test_in_nodes(
                                        &self.state.scene,
                                        canvas_f64,
                                        zoom,
                                        nodes,
                                    ) {
                                        self.state.canvas_context_menu = Some(CanvasContextMenu {
                                            target: CanvasContextTarget::Vertex(vr),
                                            screen_pos,
                                            first_frame: true,
                                        });
                                    } else if let Some(hit) = SelectState::edge_hit_test(
                                        &self.state.scene,
                                        canvas_f64,
                                        zoom,
                                        nodes,
                                    ) {
                                        self.state.canvas_context_menu = Some(CanvasContextMenu {
                                            target: CanvasContextTarget::Segment(hit),
                                            screen_pos,
                                            first_frame: true,
                                        });
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    self.state.update_cursor(gpu);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let new_pos = [position.x as f32, position.y as f32];

                if self.state.is_panning {
                    if let Some(prev) = self.state.cursor_pos {
                        let dx = new_pos[0] - prev[0];
                        let dy = new_pos[1] - prev[1];
                        self.state.camera.pan[0] += dx;
                        self.state.camera.pan[1] += dy;
                        gpu.window.request_redraw();
                    }
                } else if self.state.is_left_down && !gpu.egui_ctx.egui_wants_pointer_input() {
                    let canvas_f64 = self.state.screen_to_canvas_snapped(new_pos[0], new_pos[1]);
                    let changed = match self.state.active_tool {
                        ToolType::Select => self
                            .state
                            .select_state
                            .on_drag(&mut self.state.scene, canvas_f64),
                        ToolType::Pen => self
                            .state
                            .pen_state
                            .on_drag(&mut self.state.scene, canvas_f64),
                        ToolType::Rectangle | ToolType::Ellipse => self.state.shape_draw.on_drag(
                            &mut self.state.scene,
                            canvas_f64,
                            self.state.active_tool,
                        ),
                        _ => false,
                    };
                    if changed {
                        gpu.renderer.mark_dirty();
                        gpu.window.request_redraw();
                    }
                } else if !self.state.is_left_down
                    && !gpu.egui_ctx.egui_wants_pointer_input()
                    && self.state.active_tool == ToolType::Pen
                    && self.state.pen_state.is_building()
                {
                    // Pen hover preview: show tentative segment to cursor.
                    let canvas_f64 = self.state.screen_to_canvas_snapped(new_pos[0], new_pos[1]);
                    if self
                        .state
                        .pen_state
                        .on_move(&mut self.state.scene, canvas_f64)
                    {
                        gpu.renderer.mark_dirty();
                        gpu.window.request_redraw();
                    }
                }

                self.state.cursor_pos = Some(new_pos);
                self.state.update_cursor(gpu);
            }
            WindowEvent::CursorLeft { .. } => {
                self.state.cursor_pos = None;
            }
            WindowEvent::MouseWheel { delta, .. } if !gpu.egui_ctx.egui_wants_pointer_input() => {
                let scroll_y = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y as f32 / 40.0,
                };

                if let Some(cursor) = self.state.cursor_pos {
                    let factor = 1.0 + scroll_y * 0.1;
                    self.state.camera.zoom_at(factor, cursor[0], cursor[1]);
                    gpu.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput {
                event: ref key_event,
                ..
            } if !gpu.egui_ctx.egui_wants_keyboard_input()
                && key_event.state == ElementState::Pressed =>
            {
                use winit::keyboard::{Key, NamedKey};
                let modifiers = gpu.egui_ctx.input(|i| i.modifiers);
                let ctrl = modifiers.ctrl || modifiers.mac_cmd;

                // ── Text tool editing intercept ─────────────────────
                // When the text tool is actively editing, route all
                // non-Ctrl keys to it before any other handler.
                let mut text_handled = false;
                if self.state.text_tool.is_editing() {
                    let action = match &key_event.logical_key {
                        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Escape) => {
                            self.state.text_tool.commit(&mut self.state.scene)
                        }
                        Key::Named(NamedKey::Backspace) if !ctrl => {
                            self.state.text_tool.on_backspace(&mut self.state.scene)
                        }
                        Key::Named(NamedKey::Delete) if !ctrl => {
                            self.state.text_tool.on_delete(&mut self.state.scene)
                        }
                        Key::Named(NamedKey::ArrowLeft) => self.state.text_tool.move_left(),
                        Key::Named(NamedKey::ArrowRight) => {
                            self.state.text_tool.move_right(&self.state.scene)
                        }
                        Key::Named(NamedKey::Home) => self.state.text_tool.move_home(),
                        Key::Named(NamedKey::End) => {
                            self.state.text_tool.move_end(&self.state.scene)
                        }
                        Key::Named(NamedKey::Space) if !ctrl => {
                            self.state.text_tool.on_char(&mut self.state.scene, " ")
                        }
                        Key::Character(c) if !ctrl => self
                            .state
                            .text_tool
                            .on_char(&mut self.state.scene, c.as_str()),
                        _ => TextAction::None,
                    };
                    text_handled = !matches!(action, TextAction::None);
                    self.state.handle_text_action(action, &mut gpu.renderer);
                    if text_handled {
                        gpu.window.request_redraw();
                    }
                }

                if !text_handled {
                    match &key_event.logical_key {
                        // Undo: Ctrl+Z (no shift)
                        Key::Character(c)
                            if (c.as_str() == "z" || c.as_str() == "Z")
                                    && ctrl
                                    && !modifiers.shift
                                    // Don't undo while a tool is actively building.
                                    && !self.state.pen_state.is_building()
                                    && !self.state.shape_draw.is_drawing()
                                    && !self.state.text_tool.is_editing()
                                    && self.state.history.can_undo() =>
                        {
                            self.state.undo();
                            gpu.renderer.mark_dirty();
                            gpu.window.request_redraw();
                        }
                        // Redo: Ctrl+Shift+Z or Ctrl+Y
                        Key::Character(c)
                            if ((c.as_str() == "z" || c.as_str() == "Z") && modifiers.shift
                                || (c.as_str() == "y" || c.as_str() == "Y")
                                    && !modifiers.shift)
                                && ctrl
                                && !self.state.pen_state.is_building()
                                && !self.state.shape_draw.is_drawing()
                                && !self.state.text_tool.is_editing()
                                && self.state.history.can_redo() =>
                        {
                            self.state.redo();
                            gpu.renderer.mark_dirty();
                            gpu.window.request_redraw();
                        }
                        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Escape) => {
                            if self.state.active_tool == ToolType::Pen
                                && self.state.pen_state.is_building()
                            {
                                let action =
                                    self.state.pen_state.finish(&mut self.state.scene, false);
                                self.state.handle_pen_action(action, &mut gpu.renderer);
                                gpu.window.request_redraw();
                            } else if self.state.active_tool == ToolType::Select
                                && self.state.select_state.mode == vector_tools::SelectionMode::Node
                            {
                                self.state.select_state.exit_node_mode();
                                gpu.window.request_redraw();
                            }
                        }
                        Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace)
                            if self.state.active_tool == ToolType::Select =>
                        {
                            let deleted = self.state.delete_with_undo();
                            if deleted {
                                gpu.renderer.mark_dirty();
                                gpu.window.request_redraw();
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

impl App {
    fn draw(&mut self) {
        let Some(gpu) = &mut self.gpu else { return };
        draw_frame(gpu, &mut self.state);
    }
}

fn draw_frame(gpu: &mut GpuState, state: &mut EditorState) {
    let output = match gpu.surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) => t,
        wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
            gpu.surface.configure(&gpu.device, &gpu.surface_config);
            return;
        }
        other => {
            log::error!("Surface error: {other:?}");
            return;
        }
    };

    let view = output.texture.create_view(&Default::default());

    // Run egui — use begin_pass/end_pass so the canvas area is NOT part of
    // any egui Ui, which lets is_pointer_over_egui() return false for it.
    let raw_input = gpu.egui_state.take_egui_input(&gpu.window);
    gpu.egui_ctx.begin_pass(raw_input);
    let (structure_cmds, ui_scene_dirty) = run_ui(&gpu.egui_ctx, state, &mut gpu.renderer);

    // Canvas context menu (right-click on vertex/segment in node mode).
    show_canvas_context_menu(&gpu.egui_ctx, state, &mut gpu.renderer);

    // Capture the canvas rect (area not covered by egui panels) before ending the pass.
    #[expect(deprecated)] // content_rect may not exist in this egui version yet
    let avail = gpu.egui_ctx.available_rect();
    state.canvas_rect = [avail.min.x, avail.min.y, avail.width(), avail.height()];

    let full_output = gpu.egui_ctx.end_pass();

    // Mark dirty if the UI changed the scene (e.g. visibility toggle).
    if ui_scene_dirty {
        gpu.renderer.mark_dirty();
    }

    // Apply structure commands from the UI (after egui pass, before renderer.prepare).
    for action in structure_cmds {
        apply_structure_action(action, state);
        gpu.renderer.mark_dirty();
    }

    // Handle pending zoom-to-fit (e.g. after loading an SVG).
    if state.pending_zoom_to_fit {
        state.pending_zoom_to_fit = false;
        let bounds = state.scene.content_bounds();
        state.camera.zoom_to_fit(bounds, state.canvas_rect);
    }

    gpu.egui_state
        .handle_platform_output(&gpu.window, full_output.platform_output);

    let paint_jobs = gpu
        .egui_ctx
        .tessellate(full_output.shapes, full_output.pixels_per_point);

    let screen_descriptor = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [gpu.surface_config.width, gpu.surface_config.height],
        pixels_per_point: gpu.window.scale_factor() as f32,
    };

    for (id, delta) in &full_output.textures_delta.set {
        gpu.egui_renderer
            .update_texture(&gpu.device, &gpu.queue, *id, delta);
    }

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });

    gpu.egui_renderer.update_buffers(
        &gpu.device,
        &gpu.queue,
        &mut encoder,
        &paint_jobs,
        &screen_descriptor,
    );

    // Update camera and prepare vector scene
    gpu.renderer.set_camera(
        gpu.surface_config.width as f32,
        gpu.surface_config.height as f32,
        state.camera.pan,
        state.camera.zoom,
    );
    // Sync grid spacing from snap settings to the renderer.
    gpu.renderer.grid_minor_spacing = state.snap.grid_size as f32;
    gpu.renderer.grid_major_spacing = state.snap.grid_size as f32 * 10.0;
    gpu.renderer.checker_fixed_size = state.checker_fixed_size;

    // Update text cursor info for rendering.
    gpu.renderer.text_cursor = if state.text_tool.is_editing() {
        state
            .text_tool
            .cursor_local_position(&state.scene)
            .and_then(|(x, top, bottom)| {
                state
                    .text_tool
                    .editing_world_transform(&state.scene)
                    .map(|world| vector_render::TextCursorInfo {
                        local_x: x as f32,
                        local_top: top as f32,
                        local_bottom: bottom as f32,
                        world_transform: [
                            world.a as f32,
                            world.c as f32,
                            world.b as f32,
                            world.d as f32,
                            world.tx as f32,
                            world.ty as f32,
                        ],
                    })
            })
    } else {
        None
    };
    gpu.renderer.text_editing_node = state.text_tool.editing_node();

    gpu.renderer
        .prepare(&gpu.device, &gpu.queue, &state.scene, &state.select_state);

    // Create render pass — forget_lifetime() decouples the pass from the encoder
    // borrow, which is required by egui-wgpu 0.31's render() expecting RenderPass<'static>.
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.15,
                            g: 0.15,
                            b: 0.15,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            })
            .forget_lifetime();

        // Draw vector scene
        gpu.renderer.render(&mut pass);

        // Draw egui on top
        gpu.egui_renderer
            .render(&mut pass, &paint_jobs, &screen_descriptor);
    }

    gpu.queue.submit(Some(encoder.finish()));
    output.present();

    for id in &full_output.textures_delta.free {
        gpu.egui_renderer.free_texture(id);
    }

    // If egui wants a repaint (e.g. animation, menu opening), request one
    if let Some(viewport_output) = full_output.viewport_output.get(&egui::ViewportId::ROOT)
        && viewport_output.repaint_delay.is_zero()
    {
        gpu.window.request_redraw();
    }
}

/// A pending reorder: (node to move, new parent, index within new parent).
type ReorderCommand = Option<(vector_scene::NodeId, vector_scene::NodeId, usize)>;

/// Deferred structural commands from the structure panel context menu.
/// Applied after the egui pass to avoid mutating the tree while iterating it.
type StructureCommands = Vec<StructureAction>;

/// An action from the structure panel that modifies the scene tree.
enum StructureAction {
    /// Reorder/reparent a node (from drag-and-drop).
    Reparent {
        id: NodeId,
        new_parent: NodeId,
        index: usize,
    },
    /// Create a new empty group as a child of the given parent.
    NewGroup { parent: NodeId },
    /// Wrap the selected nodes in a new group.
    GroupSelection { nodes: Vec<NodeId> },
    /// Dissolve a group, moving its children to the group's parent.
    Ungroup { group: NodeId },
    /// Delete a node.
    Delete { id: NodeId },
}

/// Render the canvas context menu (right-click on vertex/segment).
fn show_canvas_context_menu(ctx: &egui::Context, state: &mut EditorState, renderer: &mut Renderer) {
    let Some(menu) = &mut state.canvas_context_menu else {
        return;
    };

    // We need an Area so the menu floats over the canvas. On the first
    // frame we position it; on subsequent frames egui remembers.
    let area_id = egui::Id::new("canvas_context_menu");
    let pos = menu.screen_pos;
    if menu.first_frame {
        menu.first_frame = false;
    }

    let mut close = false;
    let mut action: Option<CanvasContextAction> = None;

    let area_resp = egui::Area::new(area_id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            let frame = egui::Frame::popup(ui.style());
            frame.show(ui, |ui| {
                match &state.canvas_context_menu {
                    Some(CanvasContextMenu {
                        target: CanvasContextTarget::Vertex(vr),
                        ..
                    }) => {
                        let vr = *vr;
                        let is_anchor =
                            matches!(vr.kind, PointKind::SubpathStart | PointKind::Endpoint);

                        ui.label(
                            egui::RichText::new(if is_anchor { "Vertex" } else { "Control Point" })
                                .strong(),
                        );
                        ui.separator();

                        if is_anchor {
                            // Vertex mode buttons
                            use vector_geom::VertexMode;
                            let current_mode = state.scene.get(vr.node).and_then(|n| {
                                if let NodeData::Path { ref path, .. } = n.data {
                                    let sp = path.subpaths.get(vr.subpath)?;
                                    let idx = match vr.kind {
                                        PointKind::SubpathStart => 0,
                                        PointKind::Endpoint => vr.segment + 1,
                                        _ => return None,
                                    };
                                    sp.vertex_modes.get(idx).copied()
                                } else {
                                    None
                                }
                            });

                            if let Some(current) = current_mode {
                                for (mode, label) in [
                                    (VertexMode::Corner, "Corner"),
                                    (VertexMode::Smooth, "Smooth"),
                                    (VertexMode::Symmetric, "Symmetric"),
                                ] {
                                    let is_active = current == mode;
                                    if ui.selectable_label(is_active, label).clicked() && !is_active
                                    {
                                        action = Some(CanvasContextAction::SetVertexMode(vr, mode));
                                        close = true;
                                    }
                                }
                                ui.separator();
                            }

                            if ui.button("Delete Vertex").clicked() {
                                action = Some(CanvasContextAction::DeleteVertex(vr));
                                close = true;
                            }
                        }
                    }
                    Some(CanvasContextMenu {
                        target: CanvasContextTarget::Segment(hit),
                        ..
                    }) => {
                        let hit = hit.clone();
                        let seg_kind = SelectState::segment_type(&state.scene, &hit);

                        ui.label(egui::RichText::new("Segment").strong());
                        ui.separator();

                        if let Some(kind) = seg_kind {
                            use vector_tools::SegmentKind;
                            for (target, label) in [
                                (SegmentKind::Line, "Line"),
                                (SegmentKind::Quad, "Quadratic"),
                                (SegmentKind::Cubic, "Cubic"),
                            ] {
                                let is_active = kind == target;
                                if ui.selectable_label(is_active, label).clicked() && !is_active {
                                    action = Some(CanvasContextAction::ConvertSegment(
                                        hit.clone(),
                                        target,
                                    ));
                                    close = true;
                                }
                            }
                            ui.separator();
                        }

                        if ui.button("Insert Vertex").clicked() {
                            action = Some(CanvasContextAction::InsertVertex(hit));
                            close = true;
                        }
                    }
                    None => {}
                }
            });
        });

    // Close the menu if the user clicks anywhere outside it.
    let menu_rect = area_resp.response.rect;
    if ctx.input(|i| i.pointer.any_pressed()) {
        let pointer_over_menu = ctx
            .input(|i| i.pointer.hover_pos())
            .is_some_and(|p| menu_rect.contains(p));
        if !pointer_over_menu {
            close = true;
        }
    }

    // Dismiss on Escape.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        close = true;
    }

    // Apply the action after borrowing is released.
    if let Some(action) = action {
        apply_canvas_context_action(state, renderer, action);
    }

    if close {
        state.canvas_context_menu = None;
    }
}

/// Deferred action from a canvas context menu click.
enum CanvasContextAction {
    SetVertexMode(VertexRef, vector_geom::VertexMode),
    DeleteVertex(VertexRef),
    ConvertSegment(EdgeHit, vector_tools::SegmentKind),
    InsertVertex(EdgeHit),
}

fn apply_canvas_context_action(
    state: &mut EditorState,
    renderer: &mut Renderer,
    action: CanvasContextAction,
) {
    match action {
        CanvasContextAction::SetVertexMode(vr, mode) => {
            // Snapshot for undo.
            if let Some(node) = state.scene.get(vr.node)
                && let NodeData::Path { ref path, .. } = node.data
            {
                state.history.record_undo(Command::SetPathData {
                    id: vr.node,
                    path: path.clone(),
                });
            }
            SelectState::set_vertex_mode(&mut state.scene, &vr, mode);
            renderer.mark_dirty();
        }
        CanvasContextAction::DeleteVertex(vr) => {
            // Select the vertex and delete via the existing method.
            state.select_state.selected.clear();
            state.select_state.selected.push(vr);
            // Snapshot for undo.
            if let Some(node) = state.scene.get(vr.node)
                && let NodeData::Path { ref path, .. } = node.data
            {
                state.history.record_undo(Command::SetPathData {
                    id: vr.node,
                    path: path.clone(),
                });
            }
            state
                .select_state
                .delete_selected_vertices(&mut state.scene);
            renderer.mark_dirty();
        }
        CanvasContextAction::ConvertSegment(hit, kind) => {
            // Snapshot for undo.
            if let Some(node) = state.scene.get(hit.node)
                && let NodeData::Path { ref path, .. } = node.data
            {
                state.history.record_undo(Command::SetPathData {
                    id: hit.node,
                    path: path.clone(),
                });
            }
            let changed = match kind {
                vector_tools::SegmentKind::Line => {
                    SelectState::convert_segment_to_line(&mut state.scene, &hit)
                }
                vector_tools::SegmentKind::Quad => {
                    SelectState::convert_segment_to_quad(&mut state.scene, &hit)
                }
                vector_tools::SegmentKind::Cubic => {
                    SelectState::convert_segment_to_cubic(&mut state.scene, &hit)
                }
                vector_tools::SegmentKind::Arc => false, // no conversion to arc yet
            };
            if changed {
                renderer.mark_dirty();
            }
        }
        CanvasContextAction::InsertVertex(hit) => {
            // Snapshot for undo.
            if let Some(node) = state.scene.get(hit.node)
                && let NodeData::Path { ref path, .. } = node.data
            {
                state.history.record_undo(Command::SetPathData {
                    id: hit.node,
                    path: path.clone(),
                });
            }
            if let Some(vr) = SelectState::insert_point_on_edge(&mut state.scene, &hit) {
                state.select_state.selected.clear();
                state.select_state.selected.push(vr);
                renderer.mark_dirty();
            }
        }
    }
}

/// UI layout — uses begin_pass/end_pass with Panel::show on the Context directly,
/// so the canvas area is NOT part of any egui Ui. This lets
/// `is_pointer_over_egui()` return false for the canvas.
///
/// Returns (structure_commands, scene_dirty) from the structure panel.
#[expect(deprecated)] // Panel::show is deprecated in 0.34 but needed for top-level panels
fn run_ui(
    ctx: &egui::Context,
    state: &mut EditorState,
    renderer: &mut Renderer,
) -> (StructureCommands, bool) {
    let scene = &mut state.scene;
    let history = &mut state.history;
    let active_tool = &mut state.active_tool;
    let selection = &mut state.select_state;
    let pen_state = &mut state.pen_state;
    let text_tool = &mut state.text_tool;
    let pending_zoom_to_fit = &mut state.pending_zoom_to_fit;
    let snap = &mut state.snap;
    let checker_fixed_size = &mut state.checker_fixed_size;
    let mut dump_requested = false;
    let mut reorder_cmd: ReorderCommand = None;
    let mut structure_cmds: StructureCommands = Vec::new();
    let mut scene_dirty = false;
    let mut open_requested = false;
    let mut save_requested = false;
    let mut undo_requested = false;
    let mut redo_requested = false;

    let menu_resp = egui::Panel::top("menu_bar").show(ctx, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open SVG...").clicked() {
                    open_requested = true;
                    ui.close();
                }
                if ui.button("Save SVG...").clicked() {
                    save_requested = true;
                    ui.close();
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(history.can_undo(), egui::Button::new("Undo  Ctrl+Z"))
                    .clicked()
                {
                    undo_requested = true;
                    ui.close();
                }
                if ui
                    .add_enabled(history.can_redo(), egui::Button::new("Redo  Ctrl+Shift+Z"))
                    .clicked()
                {
                    redo_requested = true;
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut snap.grid_enabled, "Snap to grid");
                ui.horizontal(|ui| {
                    ui.label("Grid size:");
                    ui.add(
                        egui::DragValue::new(&mut snap.grid_size)
                            .range(0.1..=100.0)
                            .speed(0.1)
                            .suffix(" px"),
                    );
                });
                ui.separator();
                ui.checkbox(checker_fixed_size, "Fixed-size checkerboard");
            });
            ui.menu_button("Debug", |ui| {
                if ui.button("Dump layout").clicked() {
                    dump_requested = true;
                    ui.close();
                }
            });
        });
    });

    let tools_resp = egui::Panel::left("tools_panel")
        .default_size(120.0)
        .resizable(false)
        .show(ctx, |ui| {
            ui.vertical(|ui| {
                for &tool in ToolType::ALL {
                    let label = format!("{} {}", tool.icon(), tool.name());
                    if ui.selectable_label(*active_tool == tool, label).clicked() {
                        // Finish pen path if switching away from pen tool.
                        if *active_tool == ToolType::Pen
                            && tool != ToolType::Pen
                            && pen_state.is_building()
                        {
                            let action = pen_state.finish(scene, false);
                            match action {
                                PenAction::Finished(node_id) => {
                                    history.record_undo(Command::Delete { id: node_id });
                                    selection.selected_nodes.clear();
                                    selection.selected_nodes.push(node_id);
                                    renderer.mark_dirty();
                                }
                                PenAction::Cancelled => {
                                    selection.selected_nodes.clear();
                                    renderer.mark_dirty();
                                }
                                _ => {}
                            }
                        }
                        // Commit text edit if switching away from text tool.
                        if *active_tool == ToolType::Text
                            && tool != ToolType::Text
                            && text_tool.is_editing()
                        {
                            let action = text_tool.commit(scene);
                            match action {
                                TextAction::Committed(node_id, original) => {
                                    history.record_undo(Command::SetTextData {
                                        id: node_id,
                                        text: original,
                                    });
                                    selection.selected_nodes.clear();
                                    selection.selected_nodes.push(node_id);
                                    renderer.mark_dirty();
                                }
                                TextAction::Cancelled => {
                                    selection.selected_nodes.clear();
                                    renderer.mark_dirty();
                                }
                                _ => {}
                            }
                        }
                        *active_tool = tool;
                    }
                }
            });
        });

    let mut properties_rect = egui::Rect::NOTHING;
    let mut structure_rect = egui::Rect::NOTHING;

    let inspector_resp = egui::Panel::right("inspector_panel")
        .default_size(220.0)
        .show(ctx, |ui| {
            let half = ui.available_height() * 0.5;

            // Properties — top half
            let props_resp = egui::Panel::top("properties_section")
                .default_size(half)
                .min_size(half)
                .show_inside(ui, |ui| {
                    ui.heading("Properties");
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("properties_scroll")
                        .show(ui, |ui| {
                            show_properties(ui, scene, history, selection, renderer);
                        });
                });
            properties_rect = props_resp.response.rect;

            // Structure — bottom half (scene graph tree)
            let structure_resp = egui::CentralPanel::default().show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Structure");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button(egui_phosphor::regular::FOLDER_PLUS)
                            .on_hover_text("New Group")
                            .clicked()
                        {
                            structure_cmds.push(StructureAction::NewGroup {
                                parent: scene.root(),
                            });
                        }
                        // "Group Selection" button — enabled when ≥1 node selected
                        let has_selection = !selection.selected_nodes.is_empty();
                        if ui
                            .add_enabled(
                                has_selection,
                                egui::Button::new(egui_phosphor::regular::SELECTION_ALL).small(),
                            )
                            .on_hover_text("Group Selection")
                            .clicked()
                        {
                            structure_cmds.push(StructureAction::GroupSelection {
                                nodes: selection.selected_nodes.clone(),
                            });
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("structure_scroll")
                    .show(ui, |ui| {
                        let root = scene.root();
                        if let Some(root_node) = scene.get(root) {
                            // Don't show root itself, just its children
                            let children: Vec<_> = root_node.children.clone();
                            show_children(
                                ui,
                                scene,
                                root,
                                &children,
                                selection,
                                &mut reorder_cmd,
                                &mut structure_cmds,
                                &mut scene_dirty,
                            );
                        }
                    });
            });
            structure_rect = structure_resp.response.rect;
        });

    if dump_requested {
        fn fmt_rect(name: &str, r: egui::Rect) -> String {
            format!(
                "  {name:20} pos=({:.0}, {:.0})  size=({:.0} x {:.0})",
                r.min.x,
                r.min.y,
                r.width(),
                r.height()
            )
        }
        #[expect(deprecated)]
        let available = ctx.available_rect();
        log::info!("=== egui layout dump ===");
        log::info!("{}", fmt_rect("canvas (available)", available));
        log::info!("{}", fmt_rect("menu_bar", menu_resp.response.rect));
        log::info!("{}", fmt_rect("tools_panel", tools_resp.response.rect));
        log::info!(
            "{}",
            fmt_rect("inspector_panel", inspector_resp.response.rect)
        );
        log::info!("{}", fmt_rect("  properties", properties_rect));
        log::info!("{}", fmt_rect("  structure", structure_rect));
        log::info!("========================");
    }

    // ── Undo/redo from menu ──

    if undo_requested && !pen_state.is_building() && history.can_undo() {
        history.undo(scene);
        selection.selected_nodes.clear();
        selection.selected.clear();
        selection.hovered = None;
        renderer.mark_dirty();
    }

    if redo_requested && !pen_state.is_building() && history.can_redo() {
        history.redo(scene);
        selection.selected_nodes.clear();
        selection.selected.clear();
        selection.hovered = None;
        renderer.mark_dirty();
    }

    // ── File dialogs (blocking — runs after egui pass) ──

    if open_requested {
        // Finish any in-progress pen path before loading.
        if pen_state.is_building() {
            let action = pen_state.finish(scene, false);
            match action {
                PenAction::Finished(node_id) => {
                    selection.selected_nodes.clear();
                    selection.selected_nodes.push(node_id);
                    renderer.mark_dirty();
                }
                PenAction::Cancelled => {
                    selection.selected_nodes.clear();
                    renderer.mark_dirty();
                }
                _ => {}
            }
        }

        if let Some(path) = rfd::FileDialog::new()
            .add_filter("SVG files", &["svg"])
            .pick_file()
        {
            match std::fs::read(&path) {
                Ok(data) => match vector_svg::import_svg(&data) {
                    Ok(new_scene) => {
                        *scene = new_scene;
                        *history = History::new(); // clear undo/redo for old scene
                        *selection = SelectState::default();
                        *pending_zoom_to_fit = true;
                        renderer.mark_dirty();
                        log::info!("Opened SVG: {}", path.display());
                    }
                    Err(e) => log::error!("Failed to import SVG: {e}"),
                },
                Err(e) => log::error!("Failed to read file: {e}"),
            }
        }
    }

    if save_requested
        && let Some(path) = rfd::FileDialog::new()
            .add_filter("SVG files", &["svg"])
            .set_file_name("untitled.svg")
            .save_file()
    {
        let svg = vector_svg::export_svg(scene);
        match std::fs::write(&path, &svg) {
            Ok(()) => log::info!("Saved SVG: {}", path.display()),
            Err(e) => log::error!("Failed to save file: {e}"),
        }
    }

    // Convert drag-and-drop reorder to a structure action.
    if let Some((node_id, new_parent, index)) = reorder_cmd {
        structure_cmds.push(StructureAction::Reparent {
            id: node_id,
            new_parent,
            index,
        });
    }

    (structure_cmds, scene_dirty)
}

/// Apply a structural action from the structure panel to the scene,
/// recording undo history as appropriate.
fn apply_structure_action(action: StructureAction, state: &mut EditorState) {
    use vector_scene::Node;

    match action {
        StructureAction::Reparent {
            id,
            new_parent,
            index,
        } => {
            let cmd = Command::Reparent {
                id,
                new_parent,
                index,
            };
            state.history.execute(cmd, &mut state.scene);
        }
        StructureAction::NewGroup { parent } => {
            let cmd = Command::Insert {
                parent,
                index: None,
                node: Box::new(Node::group("Group")),
            };
            state.history.execute(cmd, &mut state.scene);
        }
        StructureAction::GroupSelection { nodes } => {
            if nodes.is_empty() {
                return;
            }
            // Find the common parent of all selected nodes.
            // Use the first node's parent as the group target.
            let Some(parent) = state.scene.parent(nodes[0]) else {
                return;
            };

            // Find the lowest index among selected nodes so the group
            // appears at the position of the first selected node.
            let mut min_index = usize::MAX;
            for &nid in &nodes {
                if state.scene.parent(nid) != Some(parent) {
                    // For simplicity, only group siblings. If they have
                    // different parents, just use the first node's parent.
                    continue;
                }
                if let Some(idx) = state.scene.child_index(nid) {
                    min_index = min_index.min(idx);
                }
            }
            if min_index == usize::MAX {
                min_index = 0;
            }

            // Insert the group first, then reparent each node into it.
            // We need stepwise execution because Batch doesn't expose
            // the inserted NodeId.
            let group_cmd = Command::Insert {
                parent,
                index: Some(min_index),
                node: Box::new(Node::group("Group")),
            };
            state.history.execute(group_cmd, &mut state.scene);

            // The new group is at `min_index` in `parent`'s children.
            let Some(parent_node) = state.scene.get(parent) else {
                return;
            };
            let Some(&group_id) = parent_node.children.get(min_index) else {
                return;
            };

            // Now reparent each selected node into the group.
            // Collect reparent commands into a batch so undo is atomic.
            let mut reparent_cmds = Vec::new();
            for (i, &nid) in nodes.iter().enumerate() {
                // Skip the defs node or root.
                if nid == state.scene.root() || nid == state.scene.defs() {
                    continue;
                }
                reparent_cmds.push(Command::Reparent {
                    id: nid,
                    new_parent: group_id,
                    index: i,
                });
            }
            if !reparent_cmds.is_empty() {
                state
                    .history
                    .execute(Command::Batch(reparent_cmds), &mut state.scene);
            }

            // Select the new group.
            state.select_state.selected_nodes.clear();
            state.select_state.selected_nodes.push(group_id);
        }
        StructureAction::Ungroup { group } => {
            let Some(group_node) = state.scene.get(group) else {
                return;
            };
            // Only ungroup actual groups (not defs).
            if !matches!(group_node.data, NodeData::Group { is_defs: false }) {
                return;
            }
            let children: Vec<NodeId> = group_node.children.clone();
            if children.is_empty() {
                // Just delete the empty group.
                state
                    .history
                    .execute(Command::Delete { id: group }, &mut state.scene);
                return;
            }

            let Some(parent) = state.scene.parent(group) else {
                return;
            };
            let group_index = state.scene.child_index(group).unwrap_or(0);

            // Reparent each child to the group's parent, at sequential indices
            // starting from the group's current position.
            let mut reparent_cmds = Vec::new();
            for (i, &child) in children.iter().enumerate() {
                reparent_cmds.push(Command::Reparent {
                    id: child,
                    new_parent: parent,
                    index: group_index + i,
                });
            }
            reparent_cmds.push(Command::Delete { id: group });

            state
                .history
                .execute(Command::Batch(reparent_cmds), &mut state.scene);

            // Select the ungrouped children.
            state.select_state.selected_nodes = children;
        }
        StructureAction::Delete { id } => {
            if id == state.scene.root() || id == state.scene.defs() {
                return;
            }
            state
                .history
                .execute(Command::Delete { id }, &mut state.scene);
            state.select_state.selected_nodes.retain(|&n| n != id);
        }
    }
}

// ── Color conversion helpers ─────────────────────────────────────────

/// Convert our linear RGBA `Color` to egui's sRGB `Color32`.
fn color_to_egui(c: vector_geom::Color) -> egui::Color32 {
    let [r, g, b, a] = c.to_srgb8();
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Convert egui's sRGB `Color32` to our linear RGBA `Color`.
fn egui_to_color(c: egui::Color32) -> vector_geom::Color {
    vector_geom::Color::from_srgb8(c.r(), c.g(), c.b(), c.a())
}

// ── Properties panel ─────────────────────────────────────────────────

/// Compute the combined world-space bounding box for a set of node IDs.
fn combined_bounds(scene: &Scene, node_ids: &[NodeId]) -> vector_geom::Bounds {
    let mut combined = vector_geom::Bounds::EMPTY;
    for &id in node_ids {
        if let Some(node) = scene.get(id) {
            let world = scene.world_transform(id);
            let b = node.data.visual_bounds(world);
            if !b.is_empty() {
                combined = combined.union(b);
            }
        }
    }
    combined
}

/// Show fill/stroke properties for the current selection.
fn show_properties(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    history: &mut History,
    selection: &SelectState,
    renderer: &mut Renderer,
) {
    if selection.selected_nodes.is_empty() {
        ui.label("No selection");
        return;
    }

    // Collect the node IDs so we don't borrow selection during scene mutation.
    let node_ids: Vec<_> = selection.selected_nodes.clone();

    if node_ids.len() > 1 {
        ui.small(format!("{} objects selected", node_ids.len()));
        ui.add_space(2.0);
    }

    // ── Name field (single selection) ──
    if node_ids.len() == 1
        && let Some(node) = scene.get(node_ids[0])
    {
        let mut label = node.label.clone();
        let (icon, default_name) = node_display(node);
        let hint = default_name.clone();
        ui.horizontal(|ui| {
            ui.label(format!("{icon} Name"));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut label)
                    .hint_text(hint)
                    .desired_width(ui.available_width()),
            );
            if resp.changed() {
                if let Some(n) = scene.get_mut(node_ids[0]) {
                    n.label = label;
                }
                renderer.mark_dirty();
            }
        });
        ui.add_space(2.0);
    }

    // ── Transform / Geometry section ──
    {
        use vector_tools::SelectionMode;
        let header = match selection.mode {
            SelectionMode::Object => "Transform",
            SelectionMode::Node if selection.selected.len() == 1 => "Vertex",
            SelectionMode::Node => "Selection",
        };

        egui::CollapsingHeader::new(egui::RichText::new(header).strong())
            .default_open(true)
            .show(ui, |ui| {
                match selection.mode {
                    SelectionMode::Object => {
                        let bounds = combined_bounds(scene, &node_ids);
                        // Position from the first selected node's transform.
                        let (orig_tx, orig_ty) = scene
                            .get(node_ids[0])
                            .map(|n| (n.transform.tx, n.transform.ty))
                            .unwrap_or((0.0, 0.0));
                        let mut new_tx = orig_tx as f32;
                        let mut new_ty = orig_ty as f32;
                        let orig_w = bounds.width() as f32;
                        let orig_h = bounds.height() as f32;
                        let mut new_w = orig_w;
                        let mut new_h = orig_h;

                        egui::Grid::new("transform_grid")
                            .num_columns(4)
                            .spacing([4.0, 4.0])
                            .show(ui, |ui| {
                                ui.label("X");
                                ui.add(
                                    egui::DragValue::new(&mut new_tx)
                                        .speed(0.5)
                                        .fixed_decimals(1),
                                );
                                ui.label("Y");
                                ui.add(
                                    egui::DragValue::new(&mut new_ty)
                                        .speed(0.5)
                                        .fixed_decimals(1),
                                );
                                ui.end_row();

                                if !bounds.is_empty() {
                                    ui.label("W");
                                    ui.add(
                                        egui::DragValue::new(&mut new_w)
                                            .speed(0.5)
                                            .fixed_decimals(1)
                                            .range(0.001..=f32::INFINITY),
                                    );
                                    ui.label("H");
                                    ui.add(
                                        egui::DragValue::new(&mut new_h)
                                            .speed(0.5)
                                            .fixed_decimals(1)
                                            .range(0.001..=f32::INFINITY),
                                    );
                                    ui.end_row();
                                }
                            });

                        // Rotation field (single selection only — multi-select
                        // rotation from properties is ambiguous).
                        if node_ids.len() == 1 {
                            let orig_deg = scene
                                .get(node_ids[0])
                                .map(|n| n.transform.rotation_deg())
                                .unwrap_or(0.0);
                            let mut new_deg = orig_deg as f32;

                            ui.horizontal(|ui| {
                                ui.label(format!(
                                    "{} Rotation",
                                    egui_phosphor::regular::ARROW_CLOCKWISE
                                ));
                                ui.add(
                                    egui::DragValue::new(&mut new_deg)
                                        .speed(0.5)
                                        .fixed_decimals(1)
                                        .suffix("°"),
                                );
                            });

                            if (new_deg - orig_deg as f32).abs() > 1e-4 {
                                let node_id = node_ids[0];
                                if let Some(node) = scene.get(node_id) {
                                    let old_transform = node.transform;
                                    // Decompose → recompose with new angle.
                                    // We rotate around the node's bounding box center.
                                    let world = scene.world_transform(node_id);
                                    let node_bounds = node.data.visual_bounds(world);
                                    let center = if !node_bounds.is_empty() {
                                        vector_geom::Point::new(
                                            (node_bounds.min.x + node_bounds.max.x) * 0.5,
                                            (node_bounds.min.y + node_bounds.max.y) * 0.5,
                                        )
                                    } else {
                                        vector_geom::Point::new(old_transform.tx, old_transform.ty)
                                    };

                                    let delta_rad = (new_deg as f64 - orig_deg).to_radians();
                                    let rot = Affine::rotate_around(delta_rad, center);
                                    let parent_world = scene.parent_world_transform(node_id);
                                    let new_world = rot.then(parent_world.then(old_transform));
                                    if let Some(inv_parent) = parent_world.inverse() {
                                        let new_local = inv_parent.then(new_world);
                                        history.record_undo(Command::SetTransform {
                                            id: node_id,
                                            transform: old_transform,
                                        });
                                        if let Some(n) = scene.get_mut(node_id) {
                                            n.transform = new_local;
                                        }
                                        renderer.mark_dirty();
                                    }
                                }
                            }
                        }

                        // Apply position changes.
                        let pos_changed = (new_tx - orig_tx as f32).abs() > 1e-6
                            || (new_ty - orig_ty as f32).abs() > 1e-6;
                        if pos_changed {
                            let dx = new_tx as f64 - orig_tx;
                            let dy = new_ty as f64 - orig_ty;
                            let mut undo_cmds = Vec::new();
                            for &node_id in &node_ids {
                                if let Some(node) = scene.get(node_id) {
                                    undo_cmds.push(Command::SetTransform {
                                        id: node_id,
                                        transform: node.transform,
                                    });
                                }
                            }
                            for &node_id in &node_ids {
                                if let Some(node) = scene.get_mut(node_id) {
                                    node.transform.tx += dx;
                                    node.transform.ty += dy;
                                }
                            }
                            if undo_cmds.len() == 1 {
                                history.record_undo(undo_cmds.into_iter().next().unwrap());
                            } else if !undo_cmds.is_empty() {
                                history.record_undo(Command::Batch(undo_cmds));
                            }
                            renderer.mark_dirty();
                        }

                        // Apply size changes (scale around object's bounds center).
                        let size_changed = !bounds.is_empty()
                            && ((new_w - orig_w).abs() > 1e-6 || (new_h - orig_h).abs() > 1e-6);
                        if size_changed && node_ids.len() == 1 {
                            let sx = if orig_w.abs() > 1e-6 {
                                new_w as f64 / orig_w as f64
                            } else {
                                1.0
                            };
                            let sy = if orig_h.abs() > 1e-6 {
                                new_h as f64 / orig_h as f64
                            } else {
                                1.0
                            };
                            let node_id = node_ids[0];
                            if let Some(node) = scene.get(node_id) {
                                let old_transform = node.transform;
                                // Scale the transform: multiply a/b by sx, c/d by sy.
                                // This scales the node's local coordinate system.
                                let mut new_transform = old_transform;
                                new_transform.a *= sx;
                                new_transform.b *= sx;
                                new_transform.c *= sy;
                                new_transform.d *= sy;
                                history.record_undo(Command::SetTransform {
                                    id: node_id,
                                    transform: old_transform,
                                });
                                if let Some(node) = scene.get_mut(node_id) {
                                    node.transform = new_transform;
                                }
                                renderer.mark_dirty();
                            }
                        }
                    }
                    SelectionMode::Node => {
                        if selection.selected.len() == 1 {
                            // Single vertex — show its world-space position.
                            let vref = selection.selected[0];
                            if let Some(local_pos) = vref.get_position(scene) {
                                let world = scene.world_transform(vref.node);
                                let wp = world.apply(local_pos);

                                // Show vertex mode toggle buttons for the
                                // anchor this point belongs to.
                                if let Some(node) = scene.get(vref.node)
                                    && let NodeData::Path { ref path, .. } = node.data
                                    && let Some(subpath) = path.subpaths.get(vref.subpath)
                                {
                                    use vector_geom::VertexMode;
                                    use vector_tools::PointKind;
                                    let mode_idx = match vref.kind {
                                        PointKind::SubpathStart => 0,
                                        PointKind::Endpoint => vref.segment + 1,
                                        PointKind::CubicCtrl1 | PointKind::QuadCtrl => vref.segment,
                                        PointKind::CubicCtrl2 => vref.segment + 1,
                                    };
                                    if let Some(current_mode) =
                                        subpath.vertex_modes.get(mode_idx).copied()
                                    {
                                        let mut mode_changed = None;
                                        ui.horizontal(|ui| {
                                            for (mode, label) in [
                                                (VertexMode::Corner, "Corner"),
                                                (VertexMode::Smooth, "Smooth"),
                                                (VertexMode::Symmetric, "Symmetric"),
                                            ] {
                                                let is_active = current_mode == mode;
                                                if ui.selectable_label(is_active, label).clicked()
                                                    && !is_active
                                                {
                                                    mode_changed = Some(mode);
                                                }
                                            }
                                        });
                                        if let Some(new_mode) = mode_changed {
                                            // Snapshot path for undo before
                                            // changing the mode.
                                            if let Some(node) = scene.get(vref.node)
                                                && let NodeData::Path { ref path, .. } = node.data
                                            {
                                                history.record_undo(Command::SetPathData {
                                                    id: vref.node,
                                                    path: path.clone(),
                                                });
                                            }
                                            if SelectState::set_vertex_mode(scene, &vref, new_mode)
                                                .is_some()
                                            {
                                                renderer.mark_dirty();
                                            }
                                        }
                                    }
                                }

                                let mut vx = wp.x as f32;
                                let mut vy = wp.y as f32;

                                egui::Grid::new("vertex_grid")
                                    .num_columns(4)
                                    .spacing([4.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.label("X");
                                        ui.add(
                                            egui::DragValue::new(&mut vx)
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.label("Y");
                                        ui.add(
                                            egui::DragValue::new(&mut vy)
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.end_row();
                                    });

                                // Apply vertex position change.
                                let vtx_changed = (vx - wp.x as f32).abs() > 1e-6
                                    || (vy - wp.y as f32).abs() > 1e-6;
                                if vtx_changed {
                                    // Convert new world position to local delta.
                                    let new_world = vector_geom::Point::new(vx as f64, vy as f64);
                                    if let Some(inv_world) = world.inverse() {
                                        let new_local = inv_world.apply(new_world);
                                        let dx = new_local.x - local_pos.x;
                                        let dy = new_local.y - local_pos.y;

                                        // Snapshot path for undo.
                                        if let Some(node) = scene.get(vref.node)
                                            && let NodeData::Path { ref path, .. } = node.data
                                        {
                                            history.record_undo(Command::SetPathData {
                                                id: vref.node,
                                                path: path.clone(),
                                            });
                                        }

                                        vref.translate(scene, dx, dy);
                                        renderer.mark_dirty();
                                    }
                                }
                            }
                        } else if selection.selected.len() > 1 {
                            // Multiple vertices — show the bounding box of the selection (read-only).
                            let mut sel_bounds = vector_geom::Bounds::EMPTY;
                            for vref in &selection.selected {
                                if let Some(local_pos) = vref.get_position(scene) {
                                    let world = scene.world_transform(vref.node);
                                    let wp = world.apply(local_pos);
                                    sel_bounds = sel_bounds.include_point(wp);
                                }
                            }
                            if !sel_bounds.is_empty() {
                                ui.small(format!("{} vertices selected", selection.selected.len()));
                                egui::Grid::new("vertex_sel_grid")
                                    .num_columns(4)
                                    .spacing([4.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.label("X");
                                        ui.add(
                                            egui::DragValue::new(&mut (sel_bounds.min.x as f32))
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.label("Y");
                                        ui.add(
                                            egui::DragValue::new(&mut (sel_bounds.min.y as f32))
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.end_row();

                                        ui.label("W");
                                        ui.add(
                                            egui::DragValue::new(&mut (sel_bounds.width() as f32))
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.label("H");
                                        ui.add(
                                            egui::DragValue::new(&mut (sel_bounds.height() as f32))
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.end_row();
                                    });
                            }
                        } else {
                            ui.small("No vertices selected");
                        }
                    }
                }
            });
    }

    // Read current style from the first selected path node.
    let reference_style = node_ids.iter().find_map(|&id| {
        let node = scene.get(id)?;
        match &node.data {
            vector_scene::NodeData::Path { style, .. } => Some(style.clone()),
            _ => None,
        }
    });

    let Some(mut style) = reference_style else {
        return;
    };

    let mut changed = false;

    // ── Fill section ──
    egui::CollapsingHeader::new(egui::RichText::new("Fill").strong())
        .default_open(true)
        .show(ui, |ui| {
            let mut has_fill = style.fill.is_some();
            if ui.checkbox(&mut has_fill, "Enabled").changed() {
                if has_fill && style.fill.is_none() {
                    style.fill = Some(vector_scene::style::Fill {
                        paint: vector_scene::PaintRef::Solid(vector_geom::Color::BLACK),
                        rule: vector_scene::FillRule::NonZero,
                        opacity: 1.0,
                    });
                } else if !has_fill {
                    style.fill = None;
                }
                changed = true;
            }

            if let Some(ref mut fill) = style.fill {
                egui::Grid::new("fill_grid")
                    .num_columns(2)
                    .spacing([4.0, 4.0])
                    .show(ui, |ui| {
                        if let vector_scene::PaintRef::Solid(ref mut color) = fill.paint {
                            ui.label("Color");
                            let mut egui_color = color_to_egui(*color);
                            if ui.color_edit_button_srgba(&mut egui_color).changed() {
                                *color = egui_to_color(egui_color);
                                changed = true;
                            }
                            ui.end_row();
                        }

                        ui.label("Opacity");
                        if ui
                            .add(
                                egui::DragValue::new(&mut fill.opacity)
                                    .range(0.0..=1.0)
                                    .speed(0.01)
                                    .fixed_decimals(2),
                            )
                            .changed()
                        {
                            changed = true;
                        }
                        ui.end_row();
                    });
            }
        });

    // ── Stroke section ──
    egui::CollapsingHeader::new(egui::RichText::new("Stroke").strong())
        .default_open(true)
        .show(ui, |ui| {
            let mut has_stroke = style.stroke.is_some();
            if ui.checkbox(&mut has_stroke, "Enabled").changed() {
                if has_stroke && style.stroke.is_none() {
                    style.stroke = Some(vector_scene::style::Stroke {
                        paint: vector_scene::PaintRef::Solid(vector_geom::Color::BLACK),
                        style: vector_scene::StrokeStyle::default(),
                        opacity: 1.0,
                    });
                } else if !has_stroke {
                    style.stroke = None;
                }
                changed = true;
            }

            if let Some(ref mut stroke) = style.stroke {
                egui::Grid::new("stroke_grid")
                    .num_columns(2)
                    .spacing([4.0, 4.0])
                    .show(ui, |ui| {
                        if let vector_scene::PaintRef::Solid(ref mut color) = stroke.paint {
                            ui.label("Color");
                            let mut egui_color = color_to_egui(*color);
                            if ui.color_edit_button_srgba(&mut egui_color).changed() {
                                *color = egui_to_color(egui_color);
                                changed = true;
                            }
                            ui.end_row();
                        }

                        ui.label("Width");
                        let mut width = stroke.style.width as f32;
                        if ui
                            .add(
                                egui::DragValue::new(&mut width)
                                    .range(0.0..=50.0)
                                    .speed(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            stroke.style.width = width as f64;
                            changed = true;
                        }
                        ui.end_row();

                        ui.label("Opacity");
                        if ui
                            .add(
                                egui::DragValue::new(&mut stroke.opacity)
                                    .range(0.0..=1.0)
                                    .speed(0.01)
                                    .fixed_decimals(2),
                            )
                            .changed()
                        {
                            changed = true;
                        }
                        ui.end_row();
                    });
            }
        });

    // Apply changes to all selected path nodes.
    if changed {
        // Snapshot old styles for undo.
        let mut undo_cmds = Vec::new();
        for &node_id in &node_ids {
            if let Some(node) = scene.get(node_id)
                && let vector_scene::NodeData::Path {
                    style: ref old_style,
                    ..
                } = node.data
            {
                undo_cmds.push(Command::SetStyle {
                    id: node_id,
                    style: old_style.clone(),
                });
            }
        }

        // Apply new style.
        for &node_id in &node_ids {
            if let Some(node) = scene.get_mut(node_id)
                && let vector_scene::NodeData::Path {
                    style: ref mut node_style,
                    ..
                } = node.data
            {
                *node_style = style.clone();
            }
        }

        // Record undo.
        if undo_cmds.len() == 1 {
            history.record_undo(undo_cmds.into_iter().next().unwrap());
        } else if !undo_cmds.is_empty() {
            history.record_undo(Command::Batch(undo_cmds));
        }

        renderer.mark_dirty();
    }
}

/// Build the display label and icon for a scene node.
fn node_display(node: &vector_scene::Node) -> (&'static str, String) {
    use vector_scene::NodeData;

    let icon = match &node.data {
        NodeData::Group { is_defs: true } => egui_phosphor::regular::LOCK_KEY,
        NodeData::Group { is_defs: false } => egui_phosphor::regular::FOLDER,
        NodeData::Path { .. } => egui_phosphor::regular::PATH,
        NodeData::Paint(_) => egui_phosphor::regular::PALETTE,
        NodeData::Text(_) => egui_phosphor::regular::TEXT_AA,
    };

    let label = if node.label.is_empty() {
        match &node.data {
            NodeData::Group { is_defs: true } => "defs".to_string(),
            NodeData::Group { .. } => "Group".to_string(),
            NodeData::Path { .. } => "Path".to_string(),
            NodeData::Paint(_) => "Paint".to_string(),
            NodeData::Text(t) => {
                let preview: String = t.content.chars().take(20).collect();
                if t.content.len() > 20 {
                    format!("{preview}...")
                } else {
                    preview
                }
            }
        }
    } else {
        node.label.clone()
    };

    (icon, label)
}

/// Render a list of sibling nodes with drop-slots between them for reordering.
///
/// `parent_id` is the parent whose children we're rendering. Each child gets a
/// drop slot before it, plus one final slot after the last child.
#[expect(clippy::too_many_arguments)]
fn show_children(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    parent_id: vector_scene::NodeId,
    children: &[vector_scene::NodeId],
    selection: &mut SelectState,
    reorder_cmd: &mut ReorderCommand,
    structure_cmds: &mut StructureCommands,
    scene_dirty: &mut bool,
) {
    for (i, &child_id) in children.iter().enumerate() {
        // Drop slot before this child (insert at index i)
        drop_slot(ui, parent_id, i, reorder_cmd);
        // The node itself
        show_scene_node(
            ui,
            scene,
            child_id,
            selection,
            reorder_cmd,
            structure_cmds,
            scene_dirty,
        );
    }
    // Final drop slot after last child
    drop_slot(ui, parent_id, children.len(), reorder_cmd);
}

/// A thin drop target between sibling rows. When a node is dragged over it,
/// it highlights; when released, it emits a reorder command.
fn drop_slot(
    ui: &mut egui::Ui,
    parent_id: vector_scene::NodeId,
    index: usize,
    reorder_cmd: &mut ReorderCommand,
) {
    // Only show/interact when a drag is active
    if !egui::DragAndDrop::has_any_payload(ui.ctx()) {
        return;
    }

    let id = egui::Id::new(("drop_slot", parent_id, index));
    let is_being_dragged_here = ui.ctx().is_being_dragged(id);
    _ = is_being_dragged_here;

    // Allocate a thin horizontal strip
    let desired = egui::vec2(ui.available_width(), 4.0);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::hover());

    // Check if a NodeId payload is hovering over this slot
    let hovering = response.contains_pointer()
        && egui::DragAndDrop::has_payload_of_type::<vector_scene::NodeId>(ui.ctx());

    if hovering {
        // Visual feedback: draw a bright line
        let stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 160, 255));
        ui.painter().hline(rect.x_range(), rect.center().y, stroke);
    }

    // Check for drop
    if hovering
        && ui.input(|i| i.pointer.any_released())
        && let Some(dragged_id) = egui::DragAndDrop::take_payload::<vector_scene::NodeId>(ui.ctx())
    {
        *reorder_cmd = Some((*dragged_id, parent_id, index));
    }
}

/// Render a small clickable eye button that toggles node visibility.
/// Returns true if visibility was toggled.
fn visibility_button(ui: &mut egui::Ui, visible: bool) -> bool {
    let icon = if visible {
        egui_phosphor::regular::EYE
    } else {
        egui_phosphor::regular::EYE_SLASH
    };
    let color = if visible {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let btn = ui.add(
        egui::Button::new(egui::RichText::new(icon).color(color))
            .frame(false)
            .small(),
    );
    btn.on_hover_text(if visible { "Hide" } else { "Show" })
        .clicked()
}

/// Context menu for a scene node in the structure panel.
fn node_context_menu(
    ui: &mut egui::Ui,
    node_id: NodeId,
    is_group: bool,
    is_defs: bool,
    selection: &SelectState,
    structure_cmds: &mut StructureCommands,
) {
    if !is_defs {
        // "Add Group Inside" — only for groups
        if is_group
            && ui
                .button(format!(
                    "{} Add Group Inside",
                    egui_phosphor::regular::FOLDER_PLUS
                ))
                .clicked()
        {
            structure_cmds.push(StructureAction::NewGroup { parent: node_id });
            ui.close();
        }

        // "Group Selection" — when multiple nodes are selected
        if selection.selected_nodes.len() > 1
            && ui
                .button(format!(
                    "{} Group Selection",
                    egui_phosphor::regular::SELECTION_ALL
                ))
                .clicked()
        {
            structure_cmds.push(StructureAction::GroupSelection {
                nodes: selection.selected_nodes.clone(),
            });
            ui.close();
        }

        // "Ungroup" — only for non-defs groups
        if is_group
            && ui
                .button(format!("{} Ungroup", egui_phosphor::regular::FOLDER_MINUS))
                .clicked()
        {
            structure_cmds.push(StructureAction::Ungroup { group: node_id });
            ui.close();
        }

        ui.separator();

        // "Delete"
        if ui
            .button(format!("{} Delete", egui_phosphor::regular::TRASH))
            .clicked()
        {
            structure_cmds.push(StructureAction::Delete { id: node_id });
            ui.close();
        }
    }
}

/// Handle a click on a node in the structure panel.
///
/// With `additive` (Shift or Ctrl/Cmd held), the node is toggled in the
/// selection — added if not present, removed if already selected.
/// Without a modifier the selection is replaced with just this node.
fn structure_panel_click(
    selection: &mut SelectState,
    node_id: vector_scene::NodeId,
    additive: bool,
) {
    selection.selected.clear();
    selection.mode = vector_tools::SelectionMode::Object;

    if additive {
        // Toggle: remove if already selected, otherwise add
        if let Some(idx) = selection.selected_nodes.iter().position(|n| *n == node_id) {
            selection.selected_nodes.remove(idx);
        } else {
            selection.selected_nodes.push(node_id);
        }
    } else {
        selection.selected_nodes.clear();
        selection.selected_nodes.push(node_id);
    }
}

/// Recursively render a scene node and its children in the structure panel.
/// Each node is a drag source for reordering; clicking selects the node.
fn show_scene_node(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    node_id: vector_scene::NodeId,
    selection: &mut SelectState,
    reorder_cmd: &mut ReorderCommand,
    structure_cmds: &mut StructureCommands,
    scene_dirty: &mut bool,
) {
    use vector_scene::NodeData;

    // Extract all data we need from the node upfront so we can release the
    // immutable borrow and later mutably borrow for visibility toggle.
    let Some(node) = scene.get(node_id) else {
        return;
    };

    let is_defs = matches!(&node.data, NodeData::Group { is_defs: true });
    let is_group = matches!(&node.data, NodeData::Group { .. });
    let has_children = !node.children.is_empty() && !is_defs;
    let visible = node.visible;

    let (icon, label) = node_display(node);
    let children: Vec<_> = node.children.clone();

    // Check if this node is object-selected
    let node_selected = selection.is_node_selected(node_id);

    // Text color: dim invisible/defs, highlight selected
    let text_color = if !visible || is_defs {
        ui.visuals().weak_text_color()
    } else if node_selected {
        egui::Color32::from_rgb(80, 160, 255) // selection blue
    } else {
        ui.visuals().text_color()
    };

    // Don't allow dragging the defs group
    let draggable = !is_defs;

    if has_children && is_group {
        // Collapsible group with drag support
        let node_text = format!("{icon} {label}");

        if draggable {
            let drag_id = egui::Id::new(("struct_drag", node_id));

            // Check if THIS node is currently being dragged
            if ui.ctx().is_being_dragged(drag_id) {
                // Render a ghost at the cursor
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                egui::Area::new(egui::Id::new(("drag_ghost", node_id)))
                    .interactable(false)
                    .pivot(egui::Align2::LEFT_CENTER)
                    .current_pos(ui.ctx().pointer_hover_pos().unwrap_or_default())
                    .show(ui.ctx(), |ui| {
                        let frame = egui::Frame::popup(ui.style())
                            .fill(egui::Color32::from_rgba_premultiplied(50, 50, 50, 200));
                        frame.show(ui, |ui| {
                            ui.label(egui::RichText::new(&node_text).color(text_color));
                        });
                    });

                // Show a placeholder in the original position
                ui.label(egui::RichText::new(&node_text).color(ui.visuals().weak_text_color()));
            } else {
                // Eye button + collapsing header on the same line.
                // We use a horizontal layout for the eye button, then let the
                // CollapsingHeader take the rest.
                ui.horizontal(|ui| {
                    if visibility_button(ui, visible)
                        && let Some(n) = scene.get_mut(node_id)
                    {
                        n.visible = !n.visible;
                        *scene_dirty = true;
                    }

                    let header = egui::CollapsingHeader::new(
                        egui::RichText::new(&node_text).color(text_color),
                    )
                    .id_salt(node_id)
                    .default_open(true);

                    let resp = header.show(ui, |ui| {
                        show_children(
                            ui,
                            scene,
                            node_id,
                            &children,
                            selection,
                            reorder_cmd,
                            structure_cmds,
                            scene_dirty,
                        );
                    });

                    // Make the header row a drag source; click to select
                    let header_resp = resp.header_response;
                    if header_resp.clicked() {
                        let shift = ui.input(|i| i.modifiers.shift);
                        let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                        structure_panel_click(selection, node_id, shift || ctrl);
                    } else if header_resp.drag_started() {
                        egui::DragAndDrop::set_payload(ui.ctx(), node_id);
                        ui.ctx().set_dragged_id(drag_id);
                    }

                    // Context menu on group header
                    header_resp.context_menu(|ui| {
                        node_context_menu(
                            ui,
                            node_id,
                            is_group,
                            is_defs,
                            selection,
                            structure_cmds,
                        );
                    });
                });
            }
        } else {
            // Non-draggable (defs) — just show the header
            let header = egui::CollapsingHeader::new(
                egui::RichText::new(format!("{icon} {label}")).color(text_color),
            )
            .id_salt(node_id)
            .default_open(true);

            header.show(ui, |ui| {
                show_children(
                    ui,
                    scene,
                    node_id,
                    &children,
                    selection,
                    reorder_cmd,
                    structure_cmds,
                    scene_dirty,
                );
            });
        }
    } else {
        // Leaf node — draggable row
        let node_text = format!("{icon} {label}");

        if draggable {
            let drag_id = egui::Id::new(("struct_drag", node_id));

            if ui.ctx().is_being_dragged(drag_id) {
                // Ghost at cursor
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                egui::Area::new(egui::Id::new(("drag_ghost", node_id)))
                    .interactable(false)
                    .pivot(egui::Align2::LEFT_CENTER)
                    .current_pos(ui.ctx().pointer_hover_pos().unwrap_or_default())
                    .show(ui.ctx(), |ui| {
                        let frame = egui::Frame::popup(ui.style())
                            .fill(egui::Color32::from_rgba_premultiplied(50, 50, 50, 200));
                        frame.show(ui, |ui| {
                            ui.label(egui::RichText::new(&node_text).color(text_color));
                        });
                    });

                // Placeholder in original position
                ui.label(egui::RichText::new(&node_text).color(ui.visuals().weak_text_color()));
            } else {
                // Eye button + label row
                ui.horizontal(|ui| {
                    if visibility_button(ui, visible)
                        && let Some(n) = scene.get_mut(node_id)
                    {
                        n.visible = !n.visible;
                        *scene_dirty = true;
                    }

                    let remaining = ui.available_width();
                    let desired_size = egui::vec2(remaining, ui.spacing().interact_size.y);
                    let (rect, response) =
                        ui.allocate_exact_size(desired_size, egui::Sense::click_and_drag());

                    // Selection tint
                    if node_selected {
                        ui.painter().rect_filled(
                            rect,
                            2.0,
                            egui::Color32::from_rgba_premultiplied(40, 90, 160, 60),
                        );
                    }

                    // Left-aligned label inside the allocated rect.
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |ui| {
                            ui.label(egui::RichText::new(&node_text).color(text_color));
                        },
                    );

                    if response.clicked() {
                        let shift = ui.input(|i| i.modifiers.shift);
                        let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
                        structure_panel_click(selection, node_id, shift || ctrl);
                    } else if response.drag_started() {
                        egui::DragAndDrop::set_payload(ui.ctx(), node_id);
                        ui.ctx().set_dragged_id(drag_id);
                    }

                    // Context menu on leaf node
                    response.context_menu(|ui| {
                        node_context_menu(
                            ui,
                            node_id,
                            is_group,
                            is_defs,
                            selection,
                            structure_cmds,
                        );
                    });
                });
            }
        } else {
            // Non-draggable leaf (shouldn't happen in practice, but handle gracefully)
            ui.label(egui::RichText::new(node_text).color(text_color));
        }
    }
}

fn create_demo_content(scene: &mut Scene) {
    use vector_geom::*;

    // A simple triangle path
    let mut path = Path::new();
    path.subpaths.push(SubPath {
        start: Point::new(400.0, 100.0),
        segments: vec![
            Segment::Line {
                to: Point::new(600.0, 400.0),
            },
            Segment::Line {
                to: Point::new(200.0, 400.0),
            },
        ],
        closed: true,
        vertex_modes: vec![VertexMode::Corner; 3],
    });

    let mut node = vector_scene::Node::path("demo triangle", path);
    if let vector_scene::NodeData::Path { style, .. } = &mut node.data {
        style.fill = Some(vector_scene::style::Fill {
            paint: vector_scene::PaintRef::Solid(Color::from_srgb8(70, 130, 180, 255)),
            rule: vector_scene::FillRule::NonZero,
            opacity: 1.0,
        });
    }

    let root = scene.root();
    scene.insert(root, node);
}
