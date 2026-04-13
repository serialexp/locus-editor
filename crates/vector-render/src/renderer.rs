use vector_geom::{Affine, Color};
use vector_scene::{NodeData, Scene};
use vector_tess::{tessellate_path, Vertex};

use crate::pipeline;

/// The main renderer — owns the wgpu pipeline state and draws the scene.
pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    /// Uniform buffer holding the view-projection matrix.
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    /// Cached vertex/index buffers. Rebuilt when scene changes.
    vertex_buffer: Option<wgpu::Buffer>,
    index_buffer: Option<wgpu::Buffer>,
    num_indices: u32,
    /// View-projection matrix as column-major f32 array.
    view_proj: [f32; 16],
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
        let view_proj = ortho_matrix(800.0, 600.0);
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
            bind_group_layout,
            globals_buffer,
            globals_bind_group,
            vertex_buffer: None,
            index_buffer: None,
            num_indices: 0,
            view_proj,
            dirty: true,
        }
    }

    /// Call when the scene has changed and needs re-tessellation.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Update the viewport size.
    pub fn resize(&mut self, width: f32, height: f32) {
        self.view_proj = ortho_matrix(width, height);
    }

    /// Rebuild GPU buffers from the scene graph if dirty.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, scene: &Scene) {
        // Always upload the view-projection matrix (it may have changed on resize)
        queue.write_buffer(
            &self.globals_buffer,
            0,
            bytemuck::cast_slice(&self.view_proj),
        );

        if !self.dirty {
            return;
        }
        self.dirty = false;

        let mut all_vertices: Vec<Vertex> = Vec::new();
        let mut all_indices: Vec<u32> = Vec::new();

        // Walk the visible tree (skip defs) and tessellate each path
        let root = scene.root();
        scene.walk_depth_first(root, Affine::IDENTITY, &mut |_id, node, _world_transform| {
            if !node.visible {
                return;
            }
            if let NodeData::Path { ref path, ref style } = node.data {
                // Determine fill color
                let fill_color = style.fill.as_ref().map(|f| match &f.paint {
                    vector_scene::PaintRef::Solid(c) => *c,
                    _ => Color::BLACK, // TODO: gradient/pattern rendering
                });

                let stroke_width = style.stroke.as_ref().map(|s| s.style.width);

                let mesh = tessellate_path(
                    path,
                    fill_color.unwrap_or(Color::TRANSPARENT),
                    style.fill.is_some(),
                    stroke_width,
                );

                let base = all_vertices.len() as u32;
                all_vertices.extend_from_slice(&mesh.vertices);
                all_indices.extend(mesh.indices.iter().map(|i| i + base));
            }
        });

        self.num_indices = all_indices.len() as u32;

        if self.num_indices > 0 {
            use wgpu::util::DeviceExt;
            self.vertex_buffer =
                Some(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vector vertices"),
                    contents: bytemuck::cast_slice(&all_vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                }));
            self.index_buffer =
                Some(device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vector indices"),
                    contents: bytemuck::cast_slice(&all_indices),
                    usage: wgpu::BufferUsages::INDEX,
                }));
        }
    }

    /// Record draw commands into a render pass.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'static>) {
        if self.num_indices == 0 {
            return;
        }
        if let (Some(vb), Some(ib)) = (&self.vertex_buffer, &self.index_buffer) {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.globals_bind_group, &[]);
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.num_indices, 0, 0..1);
        }
    }
}

/// Simple orthographic projection: top-left origin, Y-down, pixel coordinates.
fn ortho_matrix(width: f32, height: f32) -> [f32; 16] {
    [
        2.0 / width,  0.0,           0.0, 0.0,
        0.0,          -2.0 / height, 0.0, 0.0,
        0.0,          0.0,           1.0, 0.0,
        -1.0,         1.0,           0.0, 1.0,
    ]
}
