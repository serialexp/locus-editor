use vector_geom::{Affine, Color, Point, Segment};
use vector_scene::{NodeData, Scene};
use vector_tess::{Vertex, tessellate_path};
use vector_tools::{PointKind, SelectState, VertexRef};

use crate::pipeline;

/// Size of an anchor point handle in pixels (half-width).
const HANDLE_SIZE: f32 = 4.0;
/// Size of a control point handle in pixels (half-width).
const CTRL_HANDLE_SIZE: f32 = 3.0;

/// Spacing (in canvas units) between minor grid lines.
const GRID_MINOR_SPACING: f32 = 1.0;
/// Spacing (in canvas units) between major grid lines.
const GRID_MAJOR_SPACING: f32 = 10.0;
/// Minimum screen-pixel distance between minor lines before they appear.
const GRID_MINOR_MIN_SCREEN_PX: f32 = 4.0;

/// The main renderer — owns the wgpu pipeline state and draws the scene.
#[allow(clippy::struct_field_names)]
pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    // Kept alive — wgpu requires the layout to outlive bind groups created from it.
    _bind_group_layout: wgpu::BindGroupLayout,
    /// Uniform buffer holding the view-projection matrix.
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    /// Cached vertex/index buffers for scene geometry. Rebuilt when scene changes.
    vertex_buffer: Option<wgpu::Buffer>,
    index_buffer: Option<wgpu::Buffer>,
    num_indices: u32,
    /// Grid overlay buffers — rebuilt every frame (depends on camera).
    grid_vertex_buffer: Option<wgpu::Buffer>,
    grid_index_buffer: Option<wgpu::Buffer>,
    grid_num_indices: u32,
    /// Overlay buffers for vertex handles.
    handle_vertex_buffer: Option<wgpu::Buffer>,
    handle_index_buffer: Option<wgpu::Buffer>,
    handle_num_indices: u32,
    /// View-projection matrix as column-major f32 array.
    view_proj: [f32; 16],
    /// Current zoom level (for screen-space handle sizing).
    zoom: f32,
    /// Current viewport dimensions in screen pixels.
    viewport_width: f32,
    viewport_height: f32,
    /// Current camera pan in screen pixels.
    pan: [f32; 2],
    dirty: bool,
}

impl Renderer {
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vector shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let bind_group_layout = pipeline::create_bind_group_layout(device);
        let pipeline =
            pipeline::create_pipeline(device, surface_format, &shader, &bind_group_layout);

        // Create the globals uniform buffer with an initial ortho matrix
        let view_proj = camera_matrix(800.0, 600.0, [0.0, 0.0], 1.0);
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vector globals"),
            size: 64, // mat4x4<f32> = 16 * 4 bytes
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vector globals bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        Self {
            pipeline,
            _bind_group_layout: bind_group_layout,
            globals_buffer,
            globals_bind_group,
            vertex_buffer: None,
            index_buffer: None,
            num_indices: 0,
            grid_vertex_buffer: None,
            grid_index_buffer: None,
            grid_num_indices: 0,
            handle_vertex_buffer: None,
            handle_index_buffer: None,
            handle_num_indices: 0,
            view_proj,
            zoom: 1.0,
            viewport_width: 800.0,
            viewport_height: 600.0,
            pan: [0.0, 0.0],
            dirty: true,
        }
    }

    /// Call when the scene has changed and needs re-tessellation.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Update the viewport size and camera transform.
    pub fn resize(&mut self, width: f32, height: f32) {
        self.set_camera(width, height, [0.0, 0.0], 1.0);
    }

    /// Update the view-projection matrix with camera pan/zoom.
    pub fn set_camera(&mut self, width: f32, height: f32, pan: [f32; 2], zoom: f32) {
        self.view_proj = camera_matrix(width, height, pan, zoom);
        self.zoom = zoom;
        self.viewport_width = width;
        self.viewport_height = height;
        self.pan = pan;
    }

    /// Rebuild GPU buffers from the scene graph if dirty.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &Scene,
        selection: &SelectState,
    ) {
        // Always upload the view-projection matrix (it may have changed on resize)
        queue.write_buffer(
            &self.globals_buffer,
            0,
            bytemuck::cast_slice(&self.view_proj),
        );

        // Rebuild grid every frame (depends on camera, which changes without dirtying scene)
        self.build_grid(device);

        // Always rebuild handle overlay (selection can change without scene changing)
        self.build_handles(device, scene, selection, self.zoom);

        if !self.dirty {
            return;
        }
        self.dirty = false;

        let mut all_vertices: Vec<Vertex> = Vec::new();
        let mut all_indices: Vec<u32> = Vec::new();

        // Walk the visible tree (skip defs) and tessellate each path
        let root = scene.root();
        scene.walk_depth_first(
            root,
            Affine::IDENTITY,
            &mut |_id, node, _world_transform| {
                if !node.visible {
                    return;
                }
                if let NodeData::Path {
                    ref path,
                    ref style,
                } = node.data
                {
                    let fill_color = style.fill.as_ref().map(|f| match &f.paint {
                        vector_scene::PaintRef::Solid(c) => *c,
                        _ => Color::BLACK, // TODO: gradient/pattern rendering
                    });

                    let stroke = style.stroke.as_ref().map(|s| {
                        let color = match &s.paint {
                            vector_scene::PaintRef::Solid(c) => *c,
                            _ => Color::BLACK,
                        };
                        (color, s.style.width)
                    });

                    let mesh = tessellate_path(path, fill_color, stroke);

                    let base = all_vertices.len() as u32;
                    all_vertices.extend_from_slice(&mesh.vertices);
                    all_indices.extend(mesh.indices.iter().map(|i| i + base));
                }
            },
        );

        self.num_indices = all_indices.len() as u32;

        if self.num_indices > 0 {
            use wgpu::util::DeviceExt;
            self.vertex_buffer = Some(device.create_buffer_init(
                &wgpu::util::BufferInitDescriptor {
                    label: Some("vector vertices"),
                    contents: bytemuck::cast_slice(&all_vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                },
            ));
            self.index_buffer = Some(device.create_buffer_init(
                &wgpu::util::BufferInitDescriptor {
                    label: Some("vector indices"),
                    contents: bytemuck::cast_slice(&all_indices),
                    usage: wgpu::BufferUsages::INDEX,
                },
            ));
        }
    }

    /// Build grid line geometry covering the visible canvas area.
    ///
    /// Major lines appear every `GRID_MAJOR_SPACING` canvas units.
    /// Minor lines appear every `GRID_MINOR_SPACING` canvas units, but only
    /// when zoomed in enough that they are at least `GRID_MINOR_MIN_SCREEN_PX`
    /// screen pixels apart.
    fn build_grid(&mut self, device: &wgpu::Device) {
        let mut verts: Vec<Vertex> = Vec::new();
        let mut idxs: Vec<u32> = Vec::new();

        let zoom = self.zoom;
        let pan = self.pan;
        let vw = self.viewport_width;
        let vh = self.viewport_height;

        // Compute the visible canvas-coordinate range.
        // screen_to_canvas: canvas = (screen - pan) / zoom
        let canvas_left = -pan[0] / zoom;
        let canvas_top = -pan[1] / zoom;
        let canvas_right = (vw - pan[0]) / zoom;
        let canvas_bottom = (vh - pan[1]) / zoom;

        // Line thickness in canvas units (1 screen pixel wide)
        let thickness = 0.5 / zoom;

        // Colors
        let minor_color: [f32; 4] = [1.0, 1.0, 1.0, 0.06];
        let major_color: [f32; 4] = [1.0, 1.0, 1.0, 0.15];

        // Determine whether minor lines are visible:
        // minor spacing in screen pixels = GRID_MINOR_SPACING * zoom
        let show_minor = GRID_MINOR_SPACING * zoom >= GRID_MINOR_MIN_SCREEN_PX;

        // Helper: snap `lo` down to nearest multiple of `spacing`
        let snap_down = |val: f32, spacing: f32| -> f32 { (val / spacing).floor() * spacing };

        // --- Minor grid lines (drawn first, behind major) ---
        if show_minor {
            let minor = GRID_MINOR_SPACING;
            // Vertical minor lines
            let mut x = snap_down(canvas_left, minor);
            while x <= canvas_right {
                // Skip lines that fall on major grid (they'll be drawn with major color)
                let on_major = (x / GRID_MAJOR_SPACING).round() * GRID_MAJOR_SPACING;
                if (x - on_major).abs() > minor * 0.01 {
                    push_quad(
                        &mut verts,
                        &mut idxs,
                        x,
                        (canvas_top + canvas_bottom) * 0.5,
                        thickness,
                        (canvas_bottom - canvas_top) * 0.5,
                        minor_color,
                    );
                }
                x += minor;
            }
            // Horizontal minor lines
            let mut y = snap_down(canvas_top, minor);
            while y <= canvas_bottom {
                let on_major = (y / GRID_MAJOR_SPACING).round() * GRID_MAJOR_SPACING;
                if (y - on_major).abs() > minor * 0.01 {
                    push_quad(
                        &mut verts,
                        &mut idxs,
                        (canvas_left + canvas_right) * 0.5,
                        y,
                        (canvas_right - canvas_left) * 0.5,
                        thickness,
                        minor_color,
                    );
                }
                y += minor;
            }
        }

        // --- Major grid lines ---
        {
            let major = GRID_MAJOR_SPACING;
            let major_thickness = 1.0 / zoom;
            // Vertical major lines
            let mut x = snap_down(canvas_left, major);
            while x <= canvas_right {
                push_quad(
                    &mut verts,
                    &mut idxs,
                    x,
                    (canvas_top + canvas_bottom) * 0.5,
                    major_thickness,
                    (canvas_bottom - canvas_top) * 0.5,
                    major_color,
                );
                x += major;
            }
            // Horizontal major lines
            let mut y = snap_down(canvas_top, major);
            while y <= canvas_bottom {
                push_quad(
                    &mut verts,
                    &mut idxs,
                    (canvas_left + canvas_right) * 0.5,
                    y,
                    (canvas_right - canvas_left) * 0.5,
                    major_thickness,
                    major_color,
                );
                y += major;
            }
        }

        self.grid_num_indices = idxs.len() as u32;

        if self.grid_num_indices > 0 {
            use wgpu::util::DeviceExt;
            self.grid_vertex_buffer = Some(device.create_buffer_init(
                &wgpu::util::BufferInitDescriptor {
                    label: Some("grid vertices"),
                    contents: bytemuck::cast_slice(&verts),
                    usage: wgpu::BufferUsages::VERTEX,
                },
            ));
            self.grid_index_buffer = Some(device.create_buffer_init(
                &wgpu::util::BufferInitDescriptor {
                    label: Some("grid indices"),
                    contents: bytemuck::cast_slice(&idxs),
                    usage: wgpu::BufferUsages::INDEX,
                },
            ));
        } else {
            self.grid_vertex_buffer = None;
            self.grid_index_buffer = None;
        }
    }

    /// Generate small quads at each path vertex for the handle overlay.
    /// `zoom` is used to keep handles at constant screen-pixel size.
    fn build_handles(
        &mut self,
        device: &wgpu::Device,
        scene: &Scene,
        selection: &SelectState,
        zoom: f32,
    ) {
        let mut verts: Vec<Vertex> = Vec::new();
        let mut idxs: Vec<u32> = Vec::new();

        // Colors for handle types
        // Scale handle sizes to stay constant on screen regardless of zoom
        let handle_size = HANDLE_SIZE / zoom;
        let ctrl_handle_size = CTRL_HANDLE_SIZE / zoom;

        let anchor_fill: [f32; 4] = [1.0, 1.0, 1.0, 1.0]; // white
        let anchor_border: [f32; 4] = [0.2, 0.2, 0.2, 1.0]; // dark grey
        let ctrl_fill: [f32; 4] = [0.3, 0.5, 1.0, 1.0]; // blue
        let ctrl_border: [f32; 4] = [0.1, 0.2, 0.5, 1.0]; // dark blue
        let selected_fill: [f32; 4] = [0.2, 0.6, 1.0, 1.0]; // bright blue
        let selected_border: [f32; 4] = [1.0, 1.0, 1.0, 1.0]; // white
        let hovered_fill: [f32; 4] = [1.0, 0.85, 0.3, 1.0]; // warm yellow
        let hovered_border: [f32; 4] = [0.8, 0.6, 0.0, 1.0]; // dark gold

        /// Pick the (fill, border) colors and size for a handle based on its state.
        #[inline]
        #[allow(clippy::too_many_arguments)]
        fn pick_style(
            is_ctrl: bool,
            selected: bool,
            hovered: bool,
            anchor_fill: [f32; 4],
            anchor_border: [f32; 4],
            ctrl_fill: [f32; 4],
            ctrl_border: [f32; 4],
            selected_fill: [f32; 4],
            selected_border: [f32; 4],
            hovered_fill: [f32; 4],
            hovered_border: [f32; 4],
        ) -> ([f32; 4], [f32; 4]) {
            if selected {
                (selected_fill, selected_border)
            } else if hovered {
                (hovered_fill, hovered_border)
            } else if is_ctrl {
                (ctrl_fill, ctrl_border)
            } else {
                (anchor_fill, anchor_border)
            }
        }

        let root = scene.root();
        scene.walk_depth_first(root, Affine::IDENTITY, &mut |id, node, _world_transform| {
            if !node.visible {
                return;
            }
            // Only show handles for object-selected nodes.
            if !selection.is_node_selected(id) {
                return;
            }
            if let NodeData::Path { ref path, .. } = node.data {
                for (sp_idx, subpath) in path.subpaths.iter().enumerate() {
                    let make_vr = |kind: PointKind, seg_idx: usize| -> VertexRef {
                        VertexRef {
                            node: id,
                            subpath: sp_idx,
                            segment: seg_idx,
                            kind,
                        }
                    };

                    // Start point
                    let vr = make_vr(PointKind::SubpathStart, 0);
                    let (fill, border) = pick_style(
                        false,
                        selection.is_highlighted(&vr),
                        selection.is_hovered(&vr),
                        anchor_fill,
                        anchor_border,
                        ctrl_fill,
                        ctrl_border,
                        selected_fill,
                        selected_border,
                        hovered_fill,
                        hovered_border,
                    );
                    push_handle(
                        &mut verts,
                        &mut idxs,
                        subpath.start,
                        handle_size,
                        fill,
                        border,
                    );

                    for (seg_idx, seg) in subpath.segments.iter().enumerate() {
                        match seg {
                            Segment::Line { to } => {
                                let vr = make_vr(PointKind::Endpoint, seg_idx);
                                let (fill, border) = pick_style(
                                    false,
                                    selection.is_highlighted(&vr),
                                    selection.is_hovered(&vr),
                                    anchor_fill,
                                    anchor_border,
                                    ctrl_fill,
                                    ctrl_border,
                                    selected_fill,
                                    selected_border,
                                    hovered_fill,
                                    hovered_border,
                                );
                                push_handle(&mut verts, &mut idxs, *to, handle_size, fill, border);
                            }
                            Segment::Quad { ctrl, to } => {
                                let vr = make_vr(PointKind::QuadCtrl, seg_idx);
                                let (fill, border) = pick_style(
                                    true,
                                    selection.is_highlighted(&vr),
                                    selection.is_hovered(&vr),
                                    anchor_fill,
                                    anchor_border,
                                    ctrl_fill,
                                    ctrl_border,
                                    selected_fill,
                                    selected_border,
                                    hovered_fill,
                                    hovered_border,
                                );
                                push_handle(
                                    &mut verts,
                                    &mut idxs,
                                    *ctrl,
                                    ctrl_handle_size,
                                    fill,
                                    border,
                                );

                                let vr = make_vr(PointKind::Endpoint, seg_idx);
                                let (fill, border) = pick_style(
                                    false,
                                    selection.is_highlighted(&vr),
                                    selection.is_hovered(&vr),
                                    anchor_fill,
                                    anchor_border,
                                    ctrl_fill,
                                    ctrl_border,
                                    selected_fill,
                                    selected_border,
                                    hovered_fill,
                                    hovered_border,
                                );
                                push_handle(&mut verts, &mut idxs, *to, handle_size, fill, border);
                            }
                            Segment::Cubic { ctrl1, ctrl2, to } => {
                                let vr = make_vr(PointKind::CubicCtrl1, seg_idx);
                                let (fill, border) = pick_style(
                                    true,
                                    selection.is_highlighted(&vr),
                                    selection.is_hovered(&vr),
                                    anchor_fill,
                                    anchor_border,
                                    ctrl_fill,
                                    ctrl_border,
                                    selected_fill,
                                    selected_border,
                                    hovered_fill,
                                    hovered_border,
                                );
                                push_handle(
                                    &mut verts,
                                    &mut idxs,
                                    *ctrl1,
                                    ctrl_handle_size,
                                    fill,
                                    border,
                                );

                                let vr = make_vr(PointKind::CubicCtrl2, seg_idx);
                                let (fill, border) = pick_style(
                                    true,
                                    selection.is_highlighted(&vr),
                                    selection.is_hovered(&vr),
                                    anchor_fill,
                                    anchor_border,
                                    ctrl_fill,
                                    ctrl_border,
                                    selected_fill,
                                    selected_border,
                                    hovered_fill,
                                    hovered_border,
                                );
                                push_handle(
                                    &mut verts,
                                    &mut idxs,
                                    *ctrl2,
                                    ctrl_handle_size,
                                    fill,
                                    border,
                                );

                                let vr = make_vr(PointKind::Endpoint, seg_idx);
                                let (fill, border) = pick_style(
                                    false,
                                    selection.is_highlighted(&vr),
                                    selection.is_hovered(&vr),
                                    anchor_fill,
                                    anchor_border,
                                    ctrl_fill,
                                    ctrl_border,
                                    selected_fill,
                                    selected_border,
                                    hovered_fill,
                                    hovered_border,
                                );
                                push_handle(&mut verts, &mut idxs, *to, handle_size, fill, border);
                            }
                            Segment::Arc { to, .. } => {
                                let vr = make_vr(PointKind::Endpoint, seg_idx);
                                let (fill, border) = pick_style(
                                    false,
                                    selection.is_highlighted(&vr),
                                    selection.is_hovered(&vr),
                                    anchor_fill,
                                    anchor_border,
                                    ctrl_fill,
                                    ctrl_border,
                                    selected_fill,
                                    selected_border,
                                    hovered_fill,
                                    hovered_border,
                                );
                                push_handle(&mut verts, &mut idxs, *to, handle_size, fill, border);
                            }
                        }
                    }
                }
            }
        });

        // Marquee rectangle (if active)
        if let Some((min, max)) = selection.marquee() {
            let x0 = min[0] as f32;
            let y0 = min[1] as f32;
            let x1 = max[0] as f32;
            let y1 = max[1] as f32;
            let fill: [f32; 4] = [0.3, 0.5, 1.0, 0.15]; // translucent blue
            let border: [f32; 4] = [0.3, 0.5, 1.0, 0.8]; // blue border
            let t = 1.0; // border thickness

            // Fill
            push_quad(
                &mut verts,
                &mut idxs,
                (x0 + x1) * 0.5,
                (y0 + y1) * 0.5,
                (x1 - x0) * 0.5,
                (y1 - y0) * 0.5,
                fill,
            );

            // Top edge
            push_quad(
                &mut verts,
                &mut idxs,
                (x0 + x1) * 0.5,
                y0,
                (x1 - x0) * 0.5,
                t * 0.5,
                border,
            );
            // Bottom edge
            push_quad(
                &mut verts,
                &mut idxs,
                (x0 + x1) * 0.5,
                y1,
                (x1 - x0) * 0.5,
                t * 0.5,
                border,
            );
            // Left edge
            push_quad(
                &mut verts,
                &mut idxs,
                x0,
                (y0 + y1) * 0.5,
                t * 0.5,
                (y1 - y0) * 0.5,
                border,
            );
            // Right edge
            push_quad(
                &mut verts,
                &mut idxs,
                x1,
                (y0 + y1) * 0.5,
                t * 0.5,
                (y1 - y0) * 0.5,
                border,
            );
        }

        self.handle_num_indices = idxs.len() as u32;

        if self.handle_num_indices > 0 {
            use wgpu::util::DeviceExt;
            self.handle_vertex_buffer = Some(device.create_buffer_init(
                &wgpu::util::BufferInitDescriptor {
                    label: Some("handle vertices"),
                    contents: bytemuck::cast_slice(&verts),
                    usage: wgpu::BufferUsages::VERTEX,
                },
            ));
            self.handle_index_buffer = Some(device.create_buffer_init(
                &wgpu::util::BufferInitDescriptor {
                    label: Some("handle indices"),
                    contents: bytemuck::cast_slice(&idxs),
                    usage: wgpu::BufferUsages::INDEX,
                },
            ));
        } else {
            self.handle_vertex_buffer = None;
            self.handle_index_buffer = None;
        }
    }

    /// Record draw commands into a render pass.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'static>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_bind_group, &[]);

        // Draw grid (behind everything)
        if self.grid_num_indices > 0
            && let (Some(vb), Some(ib)) = (&self.grid_vertex_buffer, &self.grid_index_buffer)
        {
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.grid_num_indices, 0, 0..1);
        }

        // Draw scene geometry
        if self.num_indices > 0
            && let (Some(vb), Some(ib)) = (&self.vertex_buffer, &self.index_buffer)
        {
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.num_indices, 0, 0..1);
        }

        // Draw handle overlay on top
        if self.handle_num_indices > 0
            && let (Some(vb), Some(ib)) = (&self.handle_vertex_buffer, &self.handle_index_buffer)
        {
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.handle_num_indices, 0, 0..1);
        }
    }
}

/// Push a square handle (border + fill) centered on `pos`.
///
/// Generates a border quad (outer) and a fill quad (inner), total 12 triangles indices.
fn push_handle(
    verts: &mut Vec<Vertex>,
    idxs: &mut Vec<u32>,
    pos: Point,
    half_size: f32,
    fill_color: [f32; 4],
    border_color: [f32; 4],
) {
    let cx = pos.x as f32;
    let cy = pos.y as f32;
    // Border is 25% of the handle size — scales proportionally
    let border = half_size * 0.25;

    // Outer quad (border)
    let b = half_size + border;
    push_quad(verts, idxs, cx, cy, b, b, border_color);
    // Inner quad (fill, drawn on top)
    push_quad(verts, idxs, cx, cy, half_size, half_size, fill_color);
}

/// Push a single axis-aligned quad as two triangles.
fn push_quad(
    verts: &mut Vec<Vertex>,
    idxs: &mut Vec<u32>,
    cx: f32,
    cy: f32,
    half_x: f32,
    half_y: f32,
    color: [f32; 4],
) {
    let base = verts.len() as u32;
    verts.push(Vertex {
        position: [cx - half_x, cy - half_y],
        color,
    });
    verts.push(Vertex {
        position: [cx + half_x, cy - half_y],
        color,
    });
    verts.push(Vertex {
        position: [cx + half_x, cy + half_y],
        color,
    });
    verts.push(Vertex {
        position: [cx - half_x, cy + half_y],
        color,
    });
    idxs.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// Orthographic projection with pan and zoom.
///
/// Canvas coordinates are: top-left origin, Y-down, pixel units.
/// Pan shifts the origin, zoom scales around the origin.
/// The resulting matrix maps canvas coords → NDC.
fn camera_matrix(width: f32, height: f32, pan: [f32; 2], zoom: f32) -> [f32; 16] {
    // Scale by zoom, then offset by pan, then map to NDC.
    // NDC x = (canvas_x * zoom + pan_x) * (2/width)  - 1
    // NDC y = (canvas_y * zoom + pan_y) * (-2/height) + 1
    let sx = 2.0 * zoom / width;
    let sy = -2.0 * zoom / height;
    let tx = 2.0 * pan[0] / width - 1.0;
    let ty = -2.0 * pan[1] / height + 1.0;

    [
        sx, 0.0, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, tx, ty, 0.0, 1.0,
    ]
}
