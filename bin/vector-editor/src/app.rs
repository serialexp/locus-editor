use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use vector_ops::History;
use vector_render::Renderer;
use vector_scene::Scene;
use vector_tools::ToolType;

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

pub struct App {
    gpu: Option<GpuState>,
    scene: Scene,
    history: History,
    active_tool: ToolType,
}

impl Default for App {
    fn default() -> Self {
        Self {
            gpu: None,
            scene: Scene::new(),
            history: History::new(),
            active_tool: ToolType::default(),
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }

        let window_attrs = Window::default_attributes()
            .with_title("Vector Editor")
            .with_inner_size(winit::dpi::LogicalSize::new(1280, 800));
        let window = Arc::new(event_loop.create_window(window_attrs).expect("create window"));

        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(instance_desc);

        let surface = instance.create_surface(window.clone()).expect("create surface");

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
        create_demo_content(&mut self.scene);
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
                                self.scene = scene;
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
                    // TODO: forward to active tool (select, pen, etc.)
                    let _ = (button, state);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if !gpu.egui_ctx.egui_wants_pointer_input() {
                    // TODO: forward to active tool / update hover hit-test
                    let _ = position;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if !gpu.egui_ctx.egui_wants_pointer_input() {
                    // TODO: canvas pan/zoom
                    let _ = delta;
                }
            }
            WindowEvent::KeyboardInput {
                event: ref key_event,
                ..
            } => {
                if !gpu.egui_ctx.egui_wants_keyboard_input() {
                    // TODO: keyboard shortcuts, tool switching
                    let _ = key_event;
                }
            }
            _ => {}
        }
    }
}

impl App {
    fn draw(&mut self) {
        // Destructure to get disjoint borrows on gpu, scene, and active_tool
        let Some(gpu) = &mut self.gpu else { return };
        let scene = &self.scene;
        let active_tool = &mut self.active_tool;

        draw_frame(gpu, scene, active_tool);
    }
}

fn draw_frame(gpu: &mut GpuState, scene: &Scene, active_tool: &mut ToolType) {
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

    // Run egui
    let raw_input = gpu.egui_state.take_egui_input(&gpu.window);
    let full_output = gpu.egui_ctx.run_ui(raw_input, |ui| {
        run_ui(ui, active_tool);
    });

    gpu.egui_state
        .handle_platform_output(&gpu.window, full_output.platform_output);

    let paint_jobs = gpu.egui_ctx.tessellate(
        full_output.shapes,
        full_output.pixels_per_point,
    );

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

    // Prepare vector scene
    gpu.renderer.prepare(&gpu.device, &gpu.queue, scene);

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
    if let Some(viewport_output) = full_output
        .viewport_output
        .get(&egui::ViewportId::ROOT)
    {
        if viewport_output.repaint_delay.is_zero() {
            gpu.window.request_redraw();
        }
    }
}

/// UI layout — free function to avoid borrowing `self`.
fn run_ui(ui: &mut egui::Ui, active_tool: &mut ToolType) {
    let mut dump_requested = false;

    let menu_resp = egui::Panel::top("menu_bar").show_inside(ui, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open SVG...").clicked() {
                    // TODO: file dialog via rfd
                    ui.close();
                }
                if ui.button("Export SVG...").clicked() {
                    // TODO: export
                    ui.close();
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.button("Undo").clicked() {
                    // TODO
                    ui.close();
                }
                if ui.button("Redo").clicked() {
                    // TODO
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
        .show_inside(ui, |ui| {
            ui.vertical(|ui| {
                for &tool in ToolType::ALL {
                    let label = format!("{} {}", tool.icon(), tool.name());
                    if ui.selectable_label(*active_tool == tool, label).clicked() {
                        *active_tool = tool;
                    }
                }
            });
        });

    let mut properties_rect = egui::Rect::NOTHING;
    let mut layers_rect = egui::Rect::NOTHING;

    let inspector_resp = egui::Panel::right("inspector_panel")
        .default_size(220.0)
        .show_inside(ui, |ui| {
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
                            ui.label("No selection");
                        });
                });
            properties_rect = props_resp.response.rect;

            // Layers — bottom half
            let layers_resp = egui::CentralPanel::default()
                .show_inside(ui, |ui| {
                    ui.heading("Layers");
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("layers_scroll")
                        .show(ui, |ui| {
                            // TODO: tree view of scene nodes
                        });
                });
            layers_rect = layers_resp.response.rect;
        });

    if dump_requested {
        fn fmt_rect(name: &str, r: egui::Rect) -> String {
            format!(
                "  {name:20} pos=({:.0}, {:.0})  size=({:.0} x {:.0})",
                r.min.x, r.min.y, r.width(), r.height()
            )
        }
        let available = ui.max_rect();
        log::info!("=== egui layout dump ===");
        log::info!("{}", fmt_rect("root_ui", available));
        log::info!("{}", fmt_rect("menu_bar", menu_resp.response.rect));
        log::info!("{}", fmt_rect("tools_panel", tools_resp.response.rect));
        log::info!("{}", fmt_rect("inspector_panel", inspector_resp.response.rect));
        log::info!("{}", fmt_rect("  properties", properties_rect));
        log::info!("{}", fmt_rect("  layers", layers_rect));
        log::info!("========================");
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
