use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use vector_geom::Path;
use vector_ops::{Command, History};
use vector_render::Renderer;
use vector_scene::{NodeData, NodeId, Scene};
use vector_tools::{PenAction, PenState, SelectState, ShapeDrawState, ToolType};

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
    camera: Camera,
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
            camera: Camera::default(),
            canvas_rect: [0.0, 0.0, 1280.0, 800.0],
            cursor_pos: None,
            is_panning: false,
            is_left_down: false,
            pending_zoom_to_fit: false,
            last_left_press: None,
            drag_path_snapshots: Vec::new(),
        }
    }
}

impl EditorState {
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
                }
                renderer.mark_dirty();
            }
            PenAction::Finished(node_id) => {
                // Record undo: the pen tool created this node,
                // so undoing means deleting it.
                self.history.record_undo(Command::Delete { id: node_id });
                self.select_state.selected_nodes.clear();
                self.select_state.selected_nodes.push(node_id);
                self.active_tool = ToolType::Select;
                renderer.mark_dirty();
            }
            PenAction::Cancelled => {
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
        if gpu.egui_ctx.egui_wants_pointer_input() {
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

                // Cursor: grab for selected vertex, pointer for clickable object, default otherwise
                if self
                    .select_state
                    .hovered
                    .is_some_and(|vr| self.select_state.selected.contains(&vr))
                {
                    gpu.window.set_cursor(winit::window::CursorIcon::Grab);
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
            WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    gpu.surface_config.width = size.width;
                    gpu.surface_config.height = size.height;
                    gpu.surface.configure(&gpu.device, &gpu.surface_config);
                    gpu.renderer.resize(size.width as f32, size.height as f32);
                    gpu.window.request_redraw();
                }
            }
            WindowEvent::DroppedFile(path) => {
                if path.extension().is_some_and(|e| e == "svg") {
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
            }
            // --- Canvas input events: guard with wants_pointer/wants_keyboard ---
            WindowEvent::MouseInput { button, state, .. } => {
                if !gpu.egui_ctx.egui_wants_pointer_input() {
                    match button {
                        MouseButton::Middle => {
                            self.state.is_panning = state == ElementState::Pressed;
                        }
                        MouseButton::Left => {
                            if state == ElementState::Pressed {
                                self.state.is_left_down = true;
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
                                    let canvas =
                                        self.state.camera.screen_to_canvas(cursor[0], cursor[1]);
                                    let canvas_f64 = [canvas[0] as f64, canvas[1] as f64];
                                    match self.state.active_tool {
                                        ToolType::Select => {
                                            // Double-click: insert point on edge.
                                            let mut handled = false;
                                            if is_double_click
                                                && !self
                                                    .state
                                                    .select_state
                                                    .selected_nodes
                                                    .is_empty()
                                                && let Some(hit) = SelectState::edge_hit_test(
                                                    &self.state.scene,
                                                    canvas_f64,
                                                    self.state.camera.zoom as f64,
                                                    &self.state.select_state.selected_nodes,
                                                )
                                            {
                                                // Snapshot path before splitting for undo.
                                                let old_path = self.state.snapshot_path(hit.node);
                                                if let Some(vr) = SelectState::insert_point_on_edge(
                                                    &mut self.state.scene,
                                                    &hit,
                                                ) {
                                                    if let Some(path) = old_path {
                                                        self.state.history.record_undo(
                                                            Command::SetPathData {
                                                                id: hit.node,
                                                                path,
                                                            },
                                                        );
                                                    }
                                                    self.state.select_state.selected.clear();
                                                    self.state.select_state.selected.push(vr);
                                                    gpu.renderer.mark_dirty();
                                                    handled = true;
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
                                                // If on_press started a vertex drag,
                                                // snapshot the paths for undo.
                                                if self.state.select_state.is_dragging_vertices() {
                                                    self.state.snapshot_selected_vertex_paths();
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
                                        _ => {}
                                    }
                                    gpu.window.request_redraw();
                                }
                            } else {
                                self.state.is_left_down = false;
                                match self.state.active_tool {
                                    ToolType::Select => {
                                        // If we were dragging vertices, record
                                        // the undo command before ending the drag.
                                        if self.state.select_state.is_dragging_vertices() {
                                            self.state.record_vertex_drag_undo();
                                        }
                                        self.state.select_state.on_release();
                                    }
                                    ToolType::Pen => {
                                        if let Some(cursor) = self.state.cursor_pos {
                                            let canvas = self
                                                .state
                                                .camera
                                                .screen_to_canvas(cursor[0], cursor[1]);
                                            let canvas_f64 = [canvas[0] as f64, canvas[1] as f64];
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
                        MouseButton::Right => {
                            // Right-click: undo last pen point
                            if state == ElementState::Pressed
                                && self.state.active_tool == ToolType::Pen
                            {
                                let action = self.state.pen_state.undo_last(&mut self.state.scene);
                                self.state.handle_pen_action(action, &mut gpu.renderer);
                                gpu.window.request_redraw();
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
                    let canvas = self.state.camera.screen_to_canvas(new_pos[0], new_pos[1]);
                    let canvas_f64 = [canvas[0] as f64, canvas[1] as f64];
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
                    let canvas = self.state.camera.screen_to_canvas(new_pos[0], new_pos[1]);
                    let canvas_f64 = [canvas[0] as f64, canvas[1] as f64];
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
            WindowEvent::MouseWheel { delta, .. } => {
                if !gpu.egui_ctx.egui_wants_pointer_input() {
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
            }
            WindowEvent::KeyboardInput {
                event: ref key_event,
                ..
            } => {
                if !gpu.egui_ctx.egui_wants_keyboard_input()
                    && key_event.state == ElementState::Pressed
                {
                    use winit::keyboard::{Key, NamedKey};
                    let modifiers = gpu.egui_ctx.input(|i| i.modifiers);
                    let ctrl = modifiers.ctrl || modifiers.mac_cmd;

                    match &key_event.logical_key {
                        // Undo: Ctrl+Z (no shift)
                        Key::Character(c)
                            if (c.as_str() == "z" || c.as_str() == "Z")
                                && ctrl
                                && !modifiers.shift =>
                        {
                            // Don't undo while a tool is actively building.
                            if !self.state.pen_state.is_building()
                                && !self.state.shape_draw.is_drawing()
                                && self.state.history.can_undo()
                            {
                                self.state.undo();
                                gpu.renderer.mark_dirty();
                                gpu.window.request_redraw();
                            }
                        }
                        // Redo: Ctrl+Shift+Z or Ctrl+Y
                        Key::Character(c)
                            if ((c.as_str() == "z" || c.as_str() == "Z") && modifiers.shift
                                || (c.as_str() == "y" || c.as_str() == "Y")
                                    && !modifiers.shift)
                                && ctrl =>
                        {
                            if !self.state.pen_state.is_building()
                                && !self.state.shape_draw.is_drawing()
                                && self.state.history.can_redo()
                            {
                                self.state.redo();
                                gpu.renderer.mark_dirty();
                                gpu.window.request_redraw();
                            }
                        }
                        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Escape) => {
                            if self.state.active_tool == ToolType::Pen
                                && self.state.pen_state.is_building()
                            {
                                let action =
                                    self.state.pen_state.finish(&mut self.state.scene, false);
                                self.state.handle_pen_action(action, &mut gpu.renderer);
                                gpu.window.request_redraw();
                            }
                        }
                        Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) => {
                            if self.state.active_tool == ToolType::Select {
                                let deleted = self.state.delete_with_undo();
                                if deleted {
                                    gpu.renderer.mark_dirty();
                                    gpu.window.request_redraw();
                                }
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
    let reorder = run_ui(&gpu.egui_ctx, state, &mut gpu.renderer);

    // Capture the canvas rect (area not covered by egui panels) before ending the pass.
    #[expect(deprecated)] // content_rect may not exist in this egui version yet
    let avail = gpu.egui_ctx.available_rect();
    state.canvas_rect = [avail.min.x, avail.min.y, avail.width(), avail.height()];

    let full_output = gpu.egui_ctx.end_pass();

    // Apply any reorder from the structure panel (before renderer.prepare).
    if let Some((node_id, new_parent, index)) = reorder
        && state.scene.reparent(node_id, new_parent, index)
    {
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

/// UI layout — uses begin_pass/end_pass with Panel::show on the Context directly,
/// so the canvas area is NOT part of any egui Ui. This lets
/// `is_pointer_over_egui()` return false for the canvas.
///
/// Returns an optional reorder command from the structure panel drag-and-drop.
#[expect(deprecated)] // Panel::show is deprecated in 0.34 but needed for top-level panels
fn run_ui(ctx: &egui::Context, state: &mut EditorState, renderer: &mut Renderer) -> ReorderCommand {
    let scene = &mut state.scene;
    let history = &mut state.history;
    let active_tool = &mut state.active_tool;
    let selection = &mut state.select_state;
    let pen_state = &mut state.pen_state;
    let pending_zoom_to_fit = &mut state.pending_zoom_to_fit;
    let mut dump_requested = false;
    let mut reorder_cmd: ReorderCommand = None;
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
                ui.heading("Structure");
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("structure_scroll")
                    .show(ui, |ui| {
                        let root = scene.root();
                        if let Some(root_node) = scene.get(root) {
                            // Don't show root itself, just its children
                            let children: Vec<_> = root_node.children.clone();
                            show_children(ui, scene, root, &children, selection, &mut reorder_cmd);
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

    reorder_cmd
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

    if selection.selected_nodes.len() > 1 {
        ui.label(format!(
            "{} objects selected",
            selection.selected_nodes.len()
        ));
        ui.separator();
        // For multi-selection, apply changes to all selected nodes.
        // Read from the first node as the "reference".
    }

    // Collect the node IDs so we don't borrow selection during scene mutation.
    let node_ids: Vec<_> = selection.selected_nodes.clone();

    // Read current style from the first selected path node.
    let reference_style = node_ids.iter().find_map(|&id| {
        let node = scene.get(id)?;
        match &node.data {
            vector_scene::NodeData::Path { style, .. } => Some(style.clone()),
            _ => None,
        }
    });

    let Some(mut style) = reference_style else {
        ui.label("Selected node has no style");
        return;
    };

    let mut changed = false;

    // ── Fill section ──
    ui.label(egui::RichText::new("Fill").strong());
    let mut has_fill = style.fill.is_some();
    if ui.checkbox(&mut has_fill, "Enabled").changed() {
        if has_fill && style.fill.is_none() {
            // Add default fill
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
        if let vector_scene::PaintRef::Solid(ref mut color) = fill.paint {
            let mut egui_color = color_to_egui(*color);
            if ui.color_edit_button_srgba(&mut egui_color).changed() {
                *color = egui_to_color(egui_color);
                changed = true;
            }
        }
        if ui
            .add(egui::Slider::new(&mut fill.opacity, 0.0..=1.0).text("Opacity"))
            .changed()
        {
            changed = true;
        }
    }

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);

    // ── Stroke section ──
    ui.label(egui::RichText::new("Stroke").strong());
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
        if let vector_scene::PaintRef::Solid(ref mut color) = stroke.paint {
            let mut egui_color = color_to_egui(*color);
            if ui.color_edit_button_srgba(&mut egui_color).changed() {
                *color = egui_to_color(egui_color);
                changed = true;
            }
        }

        let mut width = stroke.style.width as f32;
        if ui
            .add(egui::Slider::new(&mut width, 0.0..=50.0).text("Width"))
            .changed()
        {
            stroke.style.width = width as f64;
            changed = true;
        }

        if ui
            .add(egui::Slider::new(&mut stroke.opacity, 0.0..=1.0).text("Opacity"))
            .changed()
        {
            changed = true;
        }
    }

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
fn show_children(
    ui: &mut egui::Ui,
    scene: &Scene,
    parent_id: vector_scene::NodeId,
    children: &[vector_scene::NodeId],
    selection: &SelectState,
    reorder_cmd: &mut ReorderCommand,
) {
    for (i, &child_id) in children.iter().enumerate() {
        // Drop slot before this child (insert at index i)
        drop_slot(ui, parent_id, i, reorder_cmd);
        // The node itself
        show_scene_node(ui, scene, child_id, selection, reorder_cmd);
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

/// Recursively render a scene node and its children in the structure panel.
/// Each node is a drag source for reordering.
fn show_scene_node(
    ui: &mut egui::Ui,
    scene: &Scene,
    node_id: vector_scene::NodeId,
    selection: &SelectState,
    reorder_cmd: &mut ReorderCommand,
) {
    use vector_scene::NodeData;

    let Some(node) = scene.get(node_id) else {
        return;
    };

    let is_defs = matches!(&node.data, NodeData::Group { is_defs: true });
    let is_group = matches!(&node.data, NodeData::Group { .. });
    let has_children = !node.children.is_empty() && !is_defs;

    let (icon, label) = node_display(node);
    let children: Vec<_> = node.children.clone();

    // Check if this node is object-selected
    let node_selected = selection.is_node_selected(node_id);

    // Text color: dim invisible/defs, highlight selected
    let text_color = if !node.visible || is_defs {
        ui.visuals().weak_text_color()
    } else if node_selected {
        egui::Color32::from_rgb(80, 160, 255) // selection blue
    } else {
        ui.visuals().text_color()
    };

    // Visibility icon
    let vis_icon = if node.visible {
        egui_phosphor::regular::EYE
    } else {
        egui_phosphor::regular::EYE_SLASH
    };

    // Don't allow dragging the defs group
    let draggable = !is_defs;

    if has_children && is_group {
        // Collapsible group with drag support
        let header_text = format!("{vis_icon}  {icon} {label}");

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
                            ui.label(egui::RichText::new(&header_text).color(text_color));
                        });
                    });

                // Show a placeholder in the original position
                ui.label(egui::RichText::new(&header_text).color(ui.visuals().weak_text_color()));
            } else {
                // Normal rendering — collapsing header with drag sense
                let header = egui::CollapsingHeader::new(
                    egui::RichText::new(&header_text).color(text_color),
                )
                .id_salt(node_id)
                .default_open(true);

                let resp = header.show(ui, |ui| {
                    show_children(ui, scene, node_id, &children, selection, reorder_cmd);
                });

                // Make the header row a drag source
                let header_resp = resp.header_response;
                if header_resp.drag_started() {
                    egui::DragAndDrop::set_payload(ui.ctx(), node_id);
                    ui.ctx().set_dragged_id(drag_id);
                }
            }
        } else {
            // Non-draggable (defs) — just show the header
            let header =
                egui::CollapsingHeader::new(egui::RichText::new(header_text).color(text_color))
                    .id_salt(node_id)
                    .default_open(true);

            header.show(ui, |ui| {
                show_children(ui, scene, node_id, &children, selection, reorder_cmd);
            });
        }
    } else {
        // Leaf node — draggable row
        let row_text = format!("{vis_icon}  {icon} {label}");

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
                            ui.label(egui::RichText::new(&row_text).color(text_color));
                        });
                    });

                // Placeholder in original position
                ui.label(egui::RichText::new(&row_text).color(ui.visuals().weak_text_color()));
            } else {
                // Normal row with drag sensing
                let desired_size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
                let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::drag());

                // Selection tint
                if node_selected {
                    ui.painter().rect_filled(
                        rect,
                        2.0,
                        egui::Color32::from_rgba_premultiplied(40, 90, 160, 60),
                    );
                }

                ui.put(
                    rect,
                    egui::Label::new(egui::RichText::new(&row_text).color(text_color)),
                );

                if response.drag_started() {
                    egui::DragAndDrop::set_payload(ui.ctx(), node_id);
                    ui.ctx().set_dragged_id(drag_id);
                }
            }
        } else {
            // Non-draggable leaf (shouldn't happen in practice, but handle gracefully)
            ui.label(egui::RichText::new(row_text).color(text_color));
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
