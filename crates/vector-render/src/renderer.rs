use std::collections::{HashMap, HashSet};
use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use vector_geom::{Affine, Color, Point, Segment};
use vector_scene::{
    Gradient, GradientKind, NodeData, NodeId, Paint, PaintRef, Pattern, Scene, SpreadMethod, Stroke,
};
use vector_tess::{
    DashPattern, FillParams, LineCap, LineJoin, StrokeParams, TessPaint, TessellatedMesh, Vertex,
    tessellate_path,
};
use vector_text::global_font_db;
use vector_tools::{
    GradientHandlePoint, PointKind, SelectState, SelectionMode, VertexRef,
    for_each_handle_of_gradient,
};

use crate::bool_cache::BoolPathCache;
use crate::pipeline;
use crate::raster_cache::{GpuRasterDraw, RasterCache};
use crate::tess_cache::{TessCache, TessCacheStats};

/// One ordered scene-content draw operation. Built up during `prepare()`
/// in scene order, consumed by `render()` to issue draw calls in that
/// same order — preserving z-order across the two pipelines (vector
/// geometry vs. raster images).
///
/// `prepare()` coalesces consecutive vector contributions (paths, text,
/// boolean groups) into a single `VectorBatch` and only breaks the batch
/// when a raster node is encountered. So a scene with a raster sandwiched
/// between two paths produces three ops; a scene with only paths produces
/// exactly one.
#[derive(Debug, Clone)]
enum DrawOp {
    /// Issue a single `draw_indexed(index_range, …)` against the vector
    /// pipeline, drawing every vector contribution between the previous
    /// and next pipeline switch.
    VectorBatch { index_range: Range<u32> },
    /// Issue a 6-vertex `draw(0..6, 0..1)` against the raster pipeline,
    /// pulling the texture + per-draw uniform from the raster cache.
    Raster { node_id: NodeId },
}

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
/// Maximum number of color stops per gradient on the GPU.
const MAX_STOPS: usize = 8;

// ── GPU-side gradient data structures ───────────────────────────────────
//
// These must match the WGSL shader's `GpuGradient` / `GpuColorStop` layout
// exactly, including padding and alignment.

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct GpuColorStop {
    offset: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct GpuGradient {
    kind: u32,   // 0 = linear, 1 = radial
    spread: u32, // 0 = pad, 1 = reflect, 2 = repeat
    stop_count: u32,
    _pad: u32,
    // Linear: [start.x, start.y, end.x, end.y]
    // Radial: [center.x, center.y, radius, 0]
    p0: [f32; 4],
    // Linear: unused
    // Radial: [focal.x, focal.y, focal_radius, 0]
    p1: [f32; 4],
    // Inverse gradient transform: inv0 = [a, b, tx, 0], inv1 = [c, d, ty, 0]
    inv0: [f32; 4],
    inv1: [f32; 4],
    stops: [GpuColorStop; MAX_STOPS],
}

impl GpuGradient {
    fn from_gradient(gradient: &Gradient) -> Self {
        let inv = gradient.transform.inverse().unwrap_or(Affine::IDENTITY);

        let (kind, p0, p1) = match &gradient.kind {
            GradientKind::Linear { start, end } => (
                0u32,
                [start.x as f32, start.y as f32, end.x as f32, end.y as f32],
                [0.0; 4],
            ),
            GradientKind::Radial {
                center,
                radius,
                focal,
                focal_radius,
            } => (
                1u32,
                [center.x as f32, center.y as f32, *radius as f32, 0.0],
                [focal.x as f32, focal.y as f32, *focal_radius as f32, 0.0],
            ),
        };

        let spread = match gradient.spread {
            SpreadMethod::Pad => 0u32,
            SpreadMethod::Reflect => 1u32,
            SpreadMethod::Repeat => 2u32,
        };

        let stop_count = gradient.stops.len().min(MAX_STOPS) as u32;
        let mut stops = [GpuColorStop {
            offset: 0.0,
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
            _pad0: 0.0,
            _pad1: 0.0,
            _pad2: 0.0,
        }; MAX_STOPS];

        for (i, stop) in gradient.stops.iter().take(MAX_STOPS).enumerate() {
            stops[i] = GpuColorStop {
                offset: stop.offset,
                r: stop.color.r,
                g: stop.color.g,
                b: stop.color.b,
                a: stop.color.a,
                _pad0: 0.0,
                _pad1: 0.0,
                _pad2: 0.0,
            };
        }

        Self {
            kind,
            spread,
            stop_count,
            _pad: 0,
            p0,
            p1,
            inv0: [inv.a as f32, inv.b as f32, inv.tx as f32, 0.0],
            inv1: [inv.c as f32, inv.d as f32, inv.ty as f32, 0.0],
            stops,
        }
    }
}

// ── GPU-side pattern data structures ──────────────────────────────────
//
// Must match the WGSL shader's `GpuPattern` layout exactly.

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct GpuPattern {
    /// Inverse pattern transform: inv0 = [a, b, tx, 0]
    inv0: [f32; 4],
    /// Inverse pattern transform: inv1 = [c, d, ty, 0]
    inv1: [f32; 4],
    /// Tile rect: [x, y, width, height]
    tile_rect: [f32; 4],
    /// Index into the texture array layer dimension.
    layer: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

impl GpuPattern {
    fn from_pattern(pattern: &Pattern, layer: u32) -> Self {
        let inv = pattern.transform.inverse().unwrap_or(Affine::IDENTITY);
        Self {
            inv0: [inv.a as f32, inv.b as f32, inv.tx as f32, 0.0],
            inv1: [inv.c as f32, inv.d as f32, inv.ty as f32, 0.0],
            tile_rect: [
                pattern.rect[0] as f32,
                pattern.rect[1] as f32,
                pattern.rect[2] as f32,
                pattern.rect[3] as f32,
            ],
            layer,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
        }
    }
}

// ── GPU-side per-path transform ──────────────────────────────────────
//
// Must match the WGSL shader's `GpuTransform` layout exactly.
//   row0 = [a, b, tx, _]  — world_x = a*x + b*y + tx
//   row1 = [c, d, ty, _]  — world_y = c*x + d*y + ty

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct GpuTransform {
    row0: [f32; 4],
    row1: [f32; 4],
}

impl GpuTransform {
    const IDENTITY: Self = Self {
        row0: [1.0, 0.0, 0.0, 0.0],
        row1: [0.0, 1.0, 0.0, 0.0],
    };

    fn from_affine(a: &Affine) -> Self {
        Self {
            row0: [a.a as f32, a.b as f32, a.tx as f32, 0.0],
            row1: [a.c as f32, a.d as f32, a.ty as f32, 0.0],
        }
    }
}

/// Maximum pixel dimension for a single pattern tile texture.
const PATTERN_MAX_TEX_SIZE: u32 = 512;
/// Minimum pixel dimension for a single pattern tile texture.
const PATTERN_MIN_TEX_SIZE: u32 = 16;
/// Scale factor for pattern tile rasterization (2.0 = retina quality).
const PATTERN_SCALE: f64 = 2.0;

/// The main renderer — owns the wgpu pipeline state and draws the scene.
#[allow(clippy::struct_field_names)]
pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    /// Bind group layout (kept alive for bind group creation).
    bind_group_layout: wgpu::BindGroupLayout,
    /// Uniform buffer holding the view-projection matrix.
    globals_buffer: wgpu::Buffer,
    /// Storage buffer for gradient descriptors.
    gradient_buffer: wgpu::Buffer,
    /// Storage buffer for pattern descriptors.
    pattern_buffer: wgpu::Buffer,
    /// Texture array holding rasterized pattern tiles.
    pattern_texture: wgpu::Texture,
    /// View into the pattern texture array.
    pattern_texture_view: wgpu::TextureView,
    /// Sampler for pattern texture lookups.
    pattern_sampler: wgpu::Sampler,
    /// Combined bind group: globals + gradients + patterns.
    bind_group: wgpu::BindGroup,
    /// Cached vertex/index buffers for scene geometry. Rebuilt when scene changes.
    vertex_buffer: Option<wgpu::Buffer>,
    index_buffer: Option<wgpu::Buffer>,
    num_indices: u32,
    /// Number of vertices in the scene vertex buffer (for stats/debugging).
    num_vertices: u32,
    /// Number of path nodes contributing to the scene mesh (for stats/debugging).
    num_paths: u32,
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
    /// Minor grid spacing in canvas units (default 1.0).
    pub grid_minor_spacing: f32,
    /// Major grid spacing in canvas units (default 10.0).
    pub grid_major_spacing: f32,
    /// When true, the checkerboard uses a fixed screen-pixel size instead of
    /// scaling with zoom.
    pub checker_fixed_size: bool,
    /// Screen-pixel size of each checkerboard square when `checker_fixed_size`
    /// is true.
    pub checker_screen_px: f32,
    /// Text editing cursor to render (set by the app each frame).
    pub text_cursor: Option<TextCursorInfo>,
    /// Node currently being text-edited (for drawing a distinct bounding box).
    pub text_editing_node: Option<NodeId>,
    /// Per-boolean-group computed-path cache, keyed on each group's
    /// `Scene::subtree_revision`. Avoids re-running `i_overlay` polygon
    /// booleans every frame while a boolean group is visible but idle.
    bool_path_cache: BoolPathCache,
    /// Per-path tessellation cache, keyed on each node's
    /// `Scene::geometry_revision`. Transform-only edits do not bump
    /// `geometry_rev`, so dragging a selection just re-uploads the
    /// transforms buffer while every vertex buffer entry is reused.
    tess_cache: TessCache,
    /// GPU storage buffer of per-path world transforms (`GpuTransform`),
    /// indexed by each vertex's `path_id` attribute. Slot `0` is
    /// reserved for identity and used by overlay geometry.
    transforms_buffer: wgpu::Buffer,
    /// Current capacity (number of `GpuTransform` slots) of `transforms_buffer`.
    transforms_capacity: u32,
    // ── Raster pipeline ──────────────────────────────────────────────
    /// Pipeline for rendering `NodeData::Raster` nodes (textured quads).
    raster_pipeline: wgpu::RenderPipeline,
    /// Bind group layout for the raster pipeline's group 1 (per-raster
    /// texture + sampler + draw uniform). Stored so the cache can build
    /// per-entry bind groups against it.
    raster_per_draw_layout: wgpu::BindGroupLayout,
    /// Group 0 bind group for the raster pipeline (just view-proj, sourced
    /// from the same `globals_buffer` as the vector pipeline).
    raster_globals_bind_group: wgpu::BindGroup,
    /// Sampler shared by every raster (filtering, clamp). One per
    /// renderer, not per-cache-entry — wgpu allows reusing the same
    /// `Sampler` across many bind groups.
    raster_sampler: wgpu::Sampler,
    /// Per-raster-node texture cache.
    raster_cache: RasterCache,
    /// Ordered list of draw operations for the scene-content pass.
    /// Rebuilt every time `prepare()` walks the scene; each entry is
    /// either a contiguous range of indices to draw against the vector
    /// pipeline, or a `Raster` reference to draw a textured quad.
    draw_ops: Vec<DrawOp>,
}

/// Post-tessellation geometry stats for the scene (excluding grid / handle
/// overlays). Useful for a performance HUD.
#[derive(Copy, Clone, Debug, Default)]
pub struct RenderStats {
    /// Number of path (and text) nodes that contributed geometry.
    pub paths: u32,
    /// Total vertex count in the scene mesh.
    pub vertices: u32,
    /// Total triangle count in the scene mesh.
    pub triangles: u32,
}

/// Information needed to render a text editing cursor (caret).
#[derive(Clone)]
pub struct TextCursorInfo {
    /// X position in the text node's local space.
    pub local_x: f32,
    /// Top of the caret line in local space (ascent, typically negative).
    pub local_top: f32,
    /// Bottom of the caret line in local space (descent).
    pub local_bottom: f32,
    /// The text node's world transform.
    pub world_transform: [f32; 6],
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

        // Raster pipeline (image nodes). Compiled from a separate WGSL
        // file; shares only the `globals_buffer` with the vector pipeline.
        let raster_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("raster shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("raster.wgsl").into()),
        });
        let raster_globals_layout = pipeline::create_raster_globals_layout(device);
        let raster_per_draw_layout = pipeline::create_raster_per_draw_layout(device);
        let raster_pipeline = pipeline::create_raster_pipeline(
            device,
            surface_format,
            &raster_shader,
            &raster_globals_layout,
            &raster_per_draw_layout,
        );

        // Create the globals uniform buffer with an initial ortho matrix
        let view_proj = camera_matrix(800.0, 600.0, [0.0, 0.0], 1.0);
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vector globals"),
            size: 64, // mat4x4<f32> = 16 * 4 bytes
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Create an initial (empty) gradient storage buffer — one dummy element
        // so the bind group is valid even when no gradients exist.
        let gradient_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("gradient storage"),
            size: std::mem::size_of::<GpuGradient>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Create an initial (empty) pattern storage buffer.
        let pattern_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pattern storage"),
            size: std::mem::size_of::<GpuPattern>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Create a placeholder 1×1×1 texture array so the bind group is valid
        // even when no patterns exist.
        let pattern_texture = create_placeholder_pattern_texture(device);
        let pattern_texture_view = pattern_array_view(&pattern_texture);

        let pattern_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pattern sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Initial transforms storage buffer: one identity matrix at slot 0.
        // Overlay geometry (grid, handles, selection bbox) uses slot 0 for
        // its pre-computed world-space vertices.
        use wgpu::util::DeviceExt;
        let transforms_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vector transforms"),
            contents: bytemuck::bytes_of(&GpuTransform::IDENTITY),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = create_bind_group(
            device,
            &bind_group_layout,
            &globals_buffer,
            &gradient_buffer,
            &pattern_buffer,
            &pattern_texture_view,
            &pattern_sampler,
            &transforms_buffer,
        );

        // Raster pipeline's globals bind group: just view-proj, sourced
        // from the same `globals_buffer` so updating that one buffer in
        // `prepare()` affects both pipelines without a duplicate write.
        let raster_globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster globals bind group"),
            layout: &raster_globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        // Raster sampler: linear filtering with clamped edges (rasters are
        // standalone images, not tiled like patterns).
        let raster_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("raster sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            globals_buffer,
            gradient_buffer,
            pattern_buffer,
            pattern_texture,
            pattern_texture_view,
            pattern_sampler,
            bind_group,
            vertex_buffer: None,
            index_buffer: None,
            num_indices: 0,
            num_vertices: 0,
            num_paths: 0,
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
            grid_minor_spacing: GRID_MINOR_SPACING,
            grid_major_spacing: GRID_MAJOR_SPACING,
            checker_fixed_size: true,
            checker_screen_px: 24.0,
            text_cursor: None,
            text_editing_node: None,
            bool_path_cache: BoolPathCache::new(),
            tess_cache: TessCache::new(),
            transforms_buffer,
            transforms_capacity: 1,
            raster_pipeline,
            raster_per_draw_layout,
            raster_globals_bind_group,
            raster_sampler,
            raster_cache: RasterCache::new(),
            draw_ops: Vec::new(),
        }
    }

    /// Statistics for the per-path tessellation cache from the most
    /// recent `prepare()` call. Hits + misses sum to the number of
    /// path/text nodes tessellated that frame.
    pub fn tess_cache_stats(&self) -> TessCacheStats {
        self.tess_cache.stats()
    }

    /// Call when the scene has changed and needs re-tessellation.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Post-tessellation counts for the scene geometry — useful for perf
    /// HUDs and debugging. Does not include grid or handle overlays. Values
    /// reflect the most recent successful `prepare()` call.
    pub fn scene_stats(&self) -> RenderStats {
        RenderStats {
            paths: self.num_paths,
            vertices: self.num_vertices,
            triangles: self.num_indices / 3,
        }
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
        profiling::scope!("Renderer::prepare");
        // Always upload the view-projection matrix (it may have changed on resize)
        queue.write_buffer(
            &self.globals_buffer,
            0,
            bytemuck::cast_slice(&self.view_proj),
        );

        // Rebuild grid every frame (depends on camera, which changes without dirtying scene)
        {
            profiling::scope!("build_grid");
            self.build_grid(device);
        }

        // Always rebuild handle overlay (selection can change without scene changing)
        {
            profiling::scope!("build_handles");
            self.build_handles(device, scene, selection, self.zoom);
        }

        if !self.dirty {
            return;
        }
        profiling::scope!("scene_tessellation");
        self.dirty = false;

        // ── Collect gradient descriptors from the defs subtree ──────────
        let mut gpu_gradients: Vec<GpuGradient> = Vec::new();
        let mut gradient_index_map: HashMap<NodeId, i32> = HashMap::new();

        let defs = scene.defs();
        if let Some(defs_node) = scene.get(defs) {
            for &child_id in &defs_node.children {
                if let Some(child) = scene.get(child_id)
                    && let NodeData::Paint(Paint::Gradient(gradient)) = &child.data
                {
                    let idx = gpu_gradients.len() as i32;
                    gradient_index_map.insert(child_id, idx);
                    gpu_gradients.push(GpuGradient::from_gradient(gradient));
                }
            }
        }

        // Upload gradient buffer (or a dummy if empty).
        if gpu_gradients.is_empty() {
            gpu_gradients.push(GpuGradient::zeroed());
        }

        let grad_size = (gpu_gradients.len() * std::mem::size_of::<GpuGradient>()) as u64;
        if self.gradient_buffer.size() < grad_size {
            self.gradient_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("gradient storage"),
                size: grad_size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(
            &self.gradient_buffer,
            0,
            bytemuck::cast_slice(&gpu_gradients),
        );

        // ── Collect patterns from defs ─────────────────────────────────
        let mut gpu_patterns: Vec<GpuPattern> = Vec::new();
        let mut pattern_index_map: HashMap<NodeId, i32> = HashMap::new();
        // (pattern_data, paint_node_id) indexed by layer/pattern index
        let mut pattern_tiles: Vec<(Pattern, NodeId)> = Vec::new();
        // Set of all pattern paint node IDs (for dependency detection)
        let mut pattern_node_ids: std::collections::HashSet<NodeId> =
            std::collections::HashSet::new();

        if let Some(defs_node) = scene.get(defs) {
            for &child_id in &defs_node.children {
                if let Some(child) = scene.get(child_id)
                    && let NodeData::Paint(Paint::Pattern(pattern)) = &child.data
                {
                    let layer = pattern_tiles.len() as u32;
                    let idx = gpu_patterns.len() as i32;
                    pattern_index_map.insert(child_id, idx);
                    gpu_patterns.push(GpuPattern::from_pattern(pattern, layer));
                    pattern_tiles.push((pattern.clone(), child_id));
                    pattern_node_ids.insert(child_id);
                }
            }
        }

        // ── Render pattern tiles to textures ──────────────────────────
        if pattern_tiles.is_empty() {
            // No patterns: upload a dummy and use the placeholder texture.
            let mut dummy = gpu_patterns;
            if dummy.is_empty() {
                dummy.push(GpuPattern::zeroed());
            }
            let pat_size = (dummy.len() * std::mem::size_of::<GpuPattern>()) as u64;
            if self.pattern_buffer.size() < pat_size {
                self.pattern_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("pattern storage"),
                    size: pat_size,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            queue.write_buffer(&self.pattern_buffer, 0, bytemuck::cast_slice(&dummy));
            if self
                .pattern_texture
                .usage()
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
            {
                self.pattern_texture = create_placeholder_pattern_texture(device);
                self.pattern_texture_view = pattern_array_view(&self.pattern_texture);
            }
        } else {
            // ── Build dependency graph and topological sort ───────────
            // For each pattern, find which other patterns its content references.
            let num_patterns = pattern_tiles.len();
            let mut deps: Vec<Vec<usize>> = vec![Vec::new(); num_patterns];
            for (i, (pattern, _)) in pattern_tiles.iter().enumerate() {
                let refs = collect_pattern_paint_refs(scene, pattern.content, &pattern_node_ids);
                for ref_id in refs {
                    if let Some(&idx) = pattern_index_map.get(&ref_id) {
                        deps[i].push(idx as usize);
                    }
                }
            }

            let (render_order, cyclic_set) = topological_sort_patterns(&deps);

            if !cyclic_set.is_empty() {
                log::warn!(
                    "Detected circular pattern references; {} pattern(s) will render as transparent",
                    cyclic_set.len()
                );
            }

            // Determine uniform texture dimensions for the atlas.
            let mut tex_w: u32 = PATTERN_MIN_TEX_SIZE;
            let mut tex_h: u32 = PATTERN_MIN_TEX_SIZE;
            for (pat, _) in &pattern_tiles {
                let pw = ((pat.rect[2] * PATTERN_SCALE).ceil() as u32)
                    .clamp(PATTERN_MIN_TEX_SIZE, PATTERN_MAX_TEX_SIZE);
                let ph = ((pat.rect[3] * PATTERN_SCALE).ceil() as u32)
                    .clamp(PATTERN_MIN_TEX_SIZE, PATTERN_MAX_TEX_SIZE);
                tex_w = tex_w.max(pw);
                tex_h = tex_h.max(ph);
            }

            // Render each pattern to its own individual 2D texture (not the
            // final array) so that nested patterns can safely sample from
            // already-completed textures without wgpu subresource conflicts.
            let individual_textures: Vec<wgpu::Texture> = (0..num_patterns)
                .map(|i| {
                    device.create_texture(&wgpu::TextureDescriptor {
                        label: Some(&format!("pattern tile {i}")),
                        size: wgpu::Extent3d {
                            width: tex_w,
                            height: tex_h,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    })
                })
                .collect();

            // Track which patterns have been rendered (for building input arrays).
            let mut rendered: Vec<bool> = vec![false; num_patterns];

            // Render in topological order. Cyclic patterns are skipped (stay transparent).
            for &pat_idx in &render_order {
                let (ref pattern, _) = pattern_tiles[pat_idx];
                let has_pattern_deps = !deps[pat_idx].is_empty();

                let tex_view = individual_textures[pat_idx]
                    .create_view(&wgpu::TextureViewDescriptor::default());

                // Build the pattern_index_map subset visible during tessellation.
                // Only already-rendered (non-cyclic) patterns are available.
                let tess_pattern_map: HashMap<NodeId, i32> = if has_pattern_deps {
                    pattern_tiles
                        .iter()
                        .enumerate()
                        .filter(|&(i, _)| rendered[i])
                        .map(|(_, (_, node_id))| {
                            (*node_id, *pattern_index_map.get(node_id).unwrap())
                        })
                        .collect()
                } else {
                    HashMap::new()
                };

                let (pat_vertices, pat_indices) = tessellate_pattern_content(
                    scene,
                    pattern,
                    &gradient_index_map,
                    &tess_pattern_map,
                    tex_w,
                    tex_h,
                );

                if pat_indices.is_empty() {
                    rendered[pat_idx] = true;
                    continue;
                }

                use wgpu::util::DeviceExt;
                let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("pattern tile vb"),
                    contents: bytemuck::cast_slice(&pat_vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });
                let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("pattern tile ib"),
                    contents: bytemuck::cast_slice(&pat_indices),
                    usage: wgpu::BufferUsages::INDEX,
                });

                let tile_view_proj = camera_matrix_ortho(tex_w as f32, tex_h as f32, &pattern.rect);
                let tile_globals = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("pattern tile globals"),
                    contents: bytemuck::cast_slice(&tile_view_proj),
                    usage: wgpu::BufferUsages::UNIFORM,
                });

                // Build the bind group for this pattern's render pass.
                // If this pattern has deps on other patterns, build a temporary
                // texture array from the already-completed individual textures
                // so the fragment shader can sample them.
                let (tile_pat_buf, tile_pat_tex, tile_pat_view);
                if has_pattern_deps && rendered.iter().any(|&r| r) {
                    // Build a temporary texture array from completed textures.
                    let tmp_array = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("pattern deps array"),
                        size: wgpu::Extent3d {
                            width: tex_w,
                            height: tex_h,
                            depth_or_array_layers: num_patterns as u32,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    });
                    let mut copy_encoder =
                        device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("pattern deps copy"),
                        });
                    for (i, completed) in rendered.iter().enumerate() {
                        if *completed {
                            copy_encoder.copy_texture_to_texture(
                                wgpu::TexelCopyTextureInfo {
                                    texture: &individual_textures[i],
                                    mip_level: 0,
                                    origin: wgpu::Origin3d::ZERO,
                                    aspect: wgpu::TextureAspect::All,
                                },
                                wgpu::TexelCopyTextureInfo {
                                    texture: &tmp_array,
                                    mip_level: 0,
                                    origin: wgpu::Origin3d {
                                        x: 0,
                                        y: 0,
                                        z: i as u32,
                                    },
                                    aspect: wgpu::TextureAspect::All,
                                },
                                wgpu::Extent3d {
                                    width: tex_w,
                                    height: tex_h,
                                    depth_or_array_layers: 1,
                                },
                            );
                        }
                    }
                    queue.submit(std::iter::once(copy_encoder.finish()));

                    tile_pat_buf = device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("pattern tile pat buf"),
                        size: (gpu_patterns.len() * std::mem::size_of::<GpuPattern>()) as u64,
                        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    queue.write_buffer(&tile_pat_buf, 0, bytemuck::cast_slice(&gpu_patterns));
                    tile_pat_tex = tmp_array;
                    tile_pat_view = pattern_array_view(&tile_pat_tex);
                } else {
                    tile_pat_buf = device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("pat tile dummy buf"),
                        size: std::mem::size_of::<GpuPattern>() as u64,
                        usage: wgpu::BufferUsages::STORAGE,
                        mapped_at_creation: false,
                    });
                    tile_pat_tex = create_placeholder_pattern_texture(device);
                    tile_pat_view = pattern_array_view(&tile_pat_tex);
                }

                let tile_bind_group = create_bind_group(
                    device,
                    &self.bind_group_layout,
                    &tile_globals,
                    &self.gradient_buffer,
                    &tile_pat_buf,
                    &tile_pat_view,
                    &self.pattern_sampler,
                    // Pattern tile content uses path_id=0 with identity; slot 0 of
                    // the main transforms buffer is always identity by invariant.
                    &self.transforms_buffer,
                );

                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("pattern tile encoder"),
                });
                {
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("pattern tile render"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &tex_view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                store: wgpu::StoreOp::Store,
                            },
                            depth_slice: None,
                        })],
                        ..Default::default()
                    });
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &tile_bind_group, &[]);
                    pass.set_vertex_buffer(0, vb.slice(..));
                    pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..pat_indices.len() as u32, 0, 0..1);
                }
                queue.submit(std::iter::once(encoder.finish()));
                rendered[pat_idx] = true;
            }

            // ── Copy individual textures into the final array ─────────
            let layer_count = num_patterns as u32;
            self.pattern_texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("pattern tile atlas"),
                size: wgpu::Extent3d {
                    width: tex_w,
                    height: tex_h,
                    depth_or_array_layers: layer_count,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });

            let mut copy_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("pattern atlas copy"),
            });
            for (i, tex) in individual_textures.iter().enumerate() {
                copy_encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.pattern_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: 0,
                            z: i as u32,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::Extent3d {
                        width: tex_w,
                        height: tex_h,
                        depth_or_array_layers: 1,
                    },
                );
            }
            queue.submit(std::iter::once(copy_encoder.finish()));

            self.pattern_texture_view = pattern_array_view(&self.pattern_texture);

            // Upload pattern descriptors.
            let pat_size = (gpu_patterns.len() * std::mem::size_of::<GpuPattern>()) as u64;
            if self.pattern_buffer.size() < pat_size {
                self.pattern_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("pattern storage"),
                    size: pat_size,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            queue.write_buffer(&self.pattern_buffer, 0, bytemuck::cast_slice(&gpu_patterns));
        }

        // Always rebuild bind group (buffers/textures may have changed).
        self.bind_group = create_bind_group(
            device,
            &self.bind_group_layout,
            &self.globals_buffer,
            &self.gradient_buffer,
            &self.pattern_buffer,
            &self.pattern_texture_view,
            &self.pattern_sampler,
            &self.transforms_buffer,
        );

        // ── Helper: resolve PaintRef → TessPaint ───────────────────────
        let resolve_paint = |paint: &PaintRef| -> TessPaint {
            match paint {
                PaintRef::Solid(c) => TessPaint::Solid(*c),
                PaintRef::Ref(node_id) => {
                    if let Some(&idx) = gradient_index_map.get(node_id) {
                        TessPaint::Gradient { index: idx }
                    } else if let Some(&idx) = pattern_index_map.get(node_id) {
                        TessPaint::Pattern { index: idx }
                    } else {
                        TessPaint::Solid(Color::BLACK)
                    }
                }
            }
        };

        // ── Walk scene and tessellate ──────────────────────────────────
        //
        // Path and text nodes go through `tess_cache` — a hit reuses the
        // cached local-space vertex/index buffers verbatim. Each mesh's
        // vertices are appended with their `path_id` rewritten to point
        // at the slot we allocate in the `transforms` storage buffer.
        //
        // Boolean groups are re-tessellated every frame from the Phase B
        // cached computed path — that tessellation is cheap compared to
        // the `i_overlay` boolean, which is what Phase B actually saved.
        // Caching the tess on boolean groups is avoided because their
        // `subtree_rev` over-invalidates on group-own transform edits.
        //
        // Slot 0 of `transforms` is reserved for identity (overlays).
        let mut all_vertices: Vec<Vertex> = Vec::new();
        let mut all_indices: Vec<u32> = Vec::new();
        let mut transforms: Vec<GpuTransform> = vec![GpuTransform::IDENTITY];

        // Helper: resolve scene Style into tessellation parameters.
        let resolve_stroke = |s: &Stroke| -> StrokeParams {
            let cap = match s.style.cap {
                vector_scene::LineCap::Butt => LineCap::Butt,
                vector_scene::LineCap::Round => LineCap::Round,
                vector_scene::LineCap::Square => LineCap::Square,
            };
            let join = match s.style.join {
                vector_scene::LineJoin::Miter => LineJoin::Miter,
                vector_scene::LineJoin::Round => LineJoin::Round,
                vector_scene::LineJoin::Bevel => LineJoin::Bevel,
            };
            let dash = s.style.dash.as_ref().map(|d| DashPattern {
                array: d.array.clone(),
                offset: d.offset,
            });
            StrokeParams {
                paint: resolve_paint(&s.paint),
                width: s.style.width,
                cap,
                join,
                miter_limit: s.style.miter_limit,
                dash,
                opacity: s.opacity,
            }
        };

        // Helper: append a cached mesh's vertices/indices, rewriting each
        // vertex's `path_id` to the provided slot.
        let push_cached = |vertices: &[Vertex],
                           indices: &[u32],
                           path_id: u32,
                           all_vertices: &mut Vec<Vertex>,
                           all_indices: &mut Vec<u32>| {
            let base = all_vertices.len() as u32;
            all_vertices.reserve(vertices.len());
            for v in vertices {
                all_vertices.push(Vertex {
                    position: v.position,
                    color: v.color,
                    path_id,
                    gradient_index: v.gradient_index,
                    pattern_index: v.pattern_index,
                });
            }
            all_indices.extend(indices.iter().map(|i| i + base));
        };

        let mut font_db = global_font_db();

        let root = scene.root();
        let mut path_count: u32 = 0;
        // Split the borrow of `self` so the closure can hold `&mut` on the
        // tess cache, the bool-path cache, and the raster cache independently
        // of `scene`. We also capture the per-draw layout + sampler by
        // reference for raster uploads inside the walk.
        let bool_cache = &mut self.bool_path_cache;
        let tess_cache = &mut self.tess_cache;
        let raster_cache = &mut self.raster_cache;
        let raster_per_draw_layout = &self.raster_per_draw_layout;
        let raster_sampler = &self.raster_sampler;
        tess_cache.reset_stats();

        // Build the ordered draw-op list as we walk. `batch_start` marks
        // the start of the currently-open vector batch (an index range of
        // `all_indices`); when we hit a raster node we close that batch
        // (if any), emit a Raster op, and reopen lazily when the next
        // vector contribution arrives.
        let mut draw_ops: Vec<DrawOp> = Vec::new();
        let mut batch_start: Option<u32> = None;
        let mut alive_rasters: HashSet<NodeId> = HashSet::new();
        scene.walk_depth_first(root, Affine::IDENTITY, &mut |id, node, world_transform| {
            if !node.visible {
                return false;
            }
            // Lazy-open a vector batch on the first vector contribution
            // since the last raster (or scene start). `open_batch_here`
            // captures the current `all_indices` length; the batch is
            // closed when a raster is encountered or the walk ends.
            let mut open_batch_here = || {
                if batch_start.is_none() {
                    batch_start = Some(all_indices.len() as u32);
                }
            };
            match node.data {
                NodeData::Group {
                    kind: vector_scene::GroupKind::Boolean { ref style, .. },
                    is_defs: false,
                } => {
                    // Non-destructive boolean group: render the computed
                    // path with the group's own style, and skip recursion
                    // into children (they're operands, not drawables).
                    let computed = bool_cache.get_or_compute(scene, id);
                    if !computed.subpaths.is_empty() {
                        open_batch_here();
                        path_count += 1;
                        let fill = style.fill.as_ref().map(|f| FillParams {
                            paint: resolve_paint(&f.paint),
                            opacity: f.opacity,
                        });
                        let stroke = style.stroke.as_ref().map(&resolve_stroke);
                        let mesh = tessellate_path(computed, fill, stroke);
                        let slot = transforms.len() as u32;
                        transforms.push(GpuTransform::from_affine(&world_transform));
                        push_cached(
                            &mesh.vertices,
                            &mesh.indices,
                            slot,
                            &mut all_vertices,
                            &mut all_indices,
                        );
                    }
                    return false;
                }
                NodeData::Path {
                    ref path,
                    ref style,
                } => {
                    open_batch_here();
                    path_count += 1;
                    let rev = scene.geometry_revision(id);
                    let fill = style.fill.as_ref().map(|f| FillParams {
                        paint: resolve_paint(&f.paint),
                        opacity: f.opacity,
                    });
                    let stroke = style.stroke.as_ref().map(&resolve_stroke);
                    let cached = tess_cache
                        .get_or_insert_with(id, rev, || tessellate_path(path, fill, stroke));
                    let slot = transforms.len() as u32;
                    transforms.push(GpuTransform::from_affine(&world_transform));
                    push_cached(
                        &cached.vertices,
                        &cached.indices,
                        slot,
                        &mut all_vertices,
                        &mut all_indices,
                    );
                }
                NodeData::Text(ref text) => {
                    open_batch_here();
                    path_count += 1;
                    let rev = scene.geometry_revision(id);
                    let fill = text.style.fill.as_ref().map(|f| FillParams {
                        paint: resolve_paint(&f.paint),
                        opacity: f.opacity,
                    });
                    let stroke = text.style.stroke.as_ref().map(&resolve_stroke);
                    let font_family = text.font_family.clone();
                    let font_size = text.font_size;
                    let content = text.content.clone();
                    let cached = tess_cache.get_or_insert_with(id, rev, || {
                        let shaped = font_db.shape_text(&content, &font_family, font_size);
                        tessellate_path(&shaped.path, fill, stroke)
                    });
                    let slot = transforms.len() as u32;
                    transforms.push(GpuTransform::from_affine(&world_transform));
                    push_cached(
                        &cached.vertices,
                        &cached.indices,
                        slot,
                        &mut all_vertices,
                        &mut all_indices,
                    );
                }
                NodeData::Raster {
                    ref image,
                    width,
                    height,
                } => {
                    // Close any open vector batch — we need to switch
                    // pipelines before emitting this raster.
                    if let Some(start) = batch_start.take() {
                        let end = all_indices.len() as u32;
                        if end > start {
                            draw_ops.push(DrawOp::VectorBatch {
                                index_range: start..end,
                            });
                        }
                    }
                    // Ensure the GPU texture exists (re-uploaded only if
                    // the source `Arc<RasterImage>` has been swapped).
                    raster_cache.upload_if_needed(
                        device,
                        queue,
                        raster_per_draw_layout,
                        raster_sampler,
                        id,
                        image,
                    );
                    // Update per-draw uniform with the current world
                    // transform and box size. Cheap (48 bytes per raster).
                    let draw = GpuRasterDraw::new(&world_transform, width, height);
                    raster_cache.write_uniform(queue, id, &draw);

                    draw_ops.push(DrawOp::Raster { node_id: id });
                    alive_rasters.insert(id);
                    // Rasters have no children to render (children would
                    // not draw on top of the image — that's a vector
                    // pipeline concern). Skip recursion.
                    return false;
                }
                _ => {}
            }
            true
        });

        // Close the final vector batch (if any) after the walk.
        if let Some(start) = batch_start.take() {
            let end = all_indices.len() as u32;
            if end > start {
                draw_ops.push(DrawOp::VectorBatch {
                    index_range: start..end,
                });
            }
        }
        // Free GPU memory for raster nodes that have been deleted.
        raster_cache.evict_missing(&alive_rasters);
        self.draw_ops = draw_ops;

        self.num_indices = all_indices.len() as u32;
        self.num_vertices = all_vertices.len() as u32;
        self.num_paths = path_count;

        // Upload the transforms buffer first — reallocate if it grew past
        // the current capacity, rebuild the bind group to point at the new
        // buffer when that happens.
        self.upload_transforms(device, queue, &transforms);

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

    /// Upload `transforms` into the `transforms_buffer`, reallocating and
    /// rebuilding the bind group if the buffer needs to grow.
    fn upload_transforms(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        transforms: &[GpuTransform],
    ) {
        let needed = transforms.len() as u32;
        if needed > self.transforms_capacity {
            // Grow with a bit of slack so we don't reallocate on every small
            // scene change.
            let new_cap = needed.next_power_of_two().max(4);
            self.transforms_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vector transforms"),
                size: (new_cap as usize * std::mem::size_of::<GpuTransform>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.transforms_capacity = new_cap;
            // Bind group references the old buffer — rebuild it.
            self.bind_group = create_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals_buffer,
                &self.gradient_buffer,
                &self.pattern_buffer,
                &self.pattern_texture_view,
                &self.pattern_sampler,
                &self.transforms_buffer,
            );
        }
        queue.write_buffer(&self.transforms_buffer, 0, bytemuck::cast_slice(transforms));
    }

    /// Build grid line geometry covering the visible canvas area.
    ///
    /// Major lines appear every `GRID_MAJOR_SPACING` canvas units.
    /// Minor lines appear every `GRID_MINOR_SPACING` canvas units, but only
    /// when zoomed in enough that they are at least `GRID_MINOR_MIN_SCREEN_PX`
    /// screen pixels apart.
    ///
    /// A two-tone checkerboard is also drawn behind the lines so that
    /// transparent fills in the scene visibly composite over a "missing
    /// pixels" pattern (the same convention used by Photoshop, Krita, etc).
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

        // Grid line colors. A slight blue tint ensures lines remain
        // visible against both checkerboard tones (dark 0.18, light 0.28).
        // Pure-black lines with low alpha blend to ~0.18 on the light
        // checks, making them invisible against the dark checks.
        let minor_color: [f32; 4] = [0.25, 0.35, 0.55, 0.25];
        let major_color: [f32; 4] = [0.30, 0.45, 0.70, 0.45];

        let minor_spacing = self.grid_minor_spacing;
        let major_spacing = self.grid_major_spacing;

        // ── Checkerboard background ──────────────────────────────────
        // Two modes:
        //  • Fixed: each square is a constant screen-pixel size (like
        //    GIMP/Figma/Inkscape) — the pattern is a viewport UI element.
        //  • Scaled: squares are 2.5× the major-grid spacing so they
        //    scale with zoom but never align with grid lines.
        //
        // When zoomed out far enough that cells would be smaller than a
        // few pixels, fall back to a single solid quad — drawing
        // thousands of sub-pixel quads is wasteful and just averages out
        // to a flat tone anyway.
        let cell_a: [f32; 4] = [0.18, 0.18, 0.18, 1.0]; // darker
        let cell_b: [f32; 4] = [0.28, 0.28, 0.28, 1.0]; // lighter

        // In fixed mode the cell size is expressed in canvas units so
        // that it maps to the desired number of screen pixels.
        let cell_size = if self.checker_fixed_size {
            self.checker_screen_px / zoom
        } else {
            major_spacing * 2.5
        };
        let cell_screen_px = cell_size * zoom;
        const MIN_CELL_SCREEN_PX: f32 = 4.0;

        if cell_screen_px < MIN_CELL_SCREEN_PX {
            // Solid average fill across the whole viewport.
            let avg = [
                (cell_a[0] + cell_b[0]) * 0.5,
                (cell_a[1] + cell_b[1]) * 0.5,
                (cell_a[2] + cell_b[2]) * 0.5,
                1.0,
            ];
            push_quad(
                &mut verts,
                &mut idxs,
                (canvas_left + canvas_right) * 0.5,
                (canvas_top + canvas_bottom) * 0.5,
                (canvas_right - canvas_left) * 0.5,
                (canvas_bottom - canvas_top) * 0.5,
                avg,
            );
        } else if self.checker_fixed_size {
            // Fixed mode: anchor the checkerboard to the screen viewport so
            // it never moves when panning or zooming. We compute cell
            // positions in screen space (multiples of checker_screen_px from
            // screen origin 0,0) and convert each position to canvas space
            // for the vertex data.
            let spx = self.checker_screen_px;

            // Screen-space bounds of the visible area: [0, vw) × [0, vh).
            // Snap to cell boundaries in screen space.
            let start_col = 0_i32; // screen left is always 0
            let start_row = 0_i32;
            let cols = (vw / spx).ceil() as i32 + 1;
            let rows = (vh / spx).ceil() as i32 + 1;

            for r in 0..rows {
                for c in 0..cols {
                    let parity = ((start_col + c) + (start_row + r)).rem_euclid(2);
                    let color = if parity == 0 { cell_a } else { cell_b };

                    // Screen-space position of this cell's top-left corner.
                    let sx = c as f32 * spx;
                    let sy = r as f32 * spx;

                    // Convert screen centre of the cell to canvas coords.
                    // canvas = (screen - pan) / zoom
                    let cx = (sx + spx * 0.5 - pan[0]) / zoom;
                    let cy = (sy + spx * 0.5 - pan[1]) / zoom;
                    let half = spx * 0.5 / zoom;

                    push_quad(&mut verts, &mut idxs, cx, cy, half, half, color);
                }
            }
        } else {
            // Scaled mode: snap visible region to cell boundaries so cells
            // stay aligned to the canvas origin (and to the major grid).
            let start_col = (canvas_left / cell_size).floor() as i32;
            let start_row = (canvas_top / cell_size).floor() as i32;
            let start_x = start_col as f32 * cell_size;
            let start_y = start_row as f32 * cell_size;

            let mut row: i32 = 0;
            let mut y = start_y;
            while y < canvas_bottom {
                let mut col: i32 = 0;
                let mut x = start_x;
                while x < canvas_right {
                    let parity = (start_col + col + start_row + row).rem_euclid(2);
                    let color = if parity == 0 { cell_a } else { cell_b };
                    push_quad(
                        &mut verts,
                        &mut idxs,
                        x + cell_size * 0.5,
                        y + cell_size * 0.5,
                        cell_size * 0.5,
                        cell_size * 0.5,
                        color,
                    );
                    x += cell_size;
                    col += 1;
                }
                y += cell_size;
                row += 1;
            }
        }

        // Determine whether minor lines are visible:
        // minor spacing in screen pixels = minor_spacing * zoom
        let show_minor = minor_spacing * zoom >= GRID_MINOR_MIN_SCREEN_PX;

        // Helper: snap `lo` down to nearest multiple of `spacing`
        let snap_down = |val: f32, spacing: f32| -> f32 { (val / spacing).floor() * spacing };

        // --- Minor grid lines (drawn first, behind major) ---
        if show_minor {
            // Vertical minor lines
            let mut x = snap_down(canvas_left, minor_spacing);
            while x <= canvas_right {
                // Skip lines that fall on major grid (they'll be drawn with major color)
                let on_major = (x / major_spacing).round() * major_spacing;
                if (x - on_major).abs() > minor_spacing * 0.01 {
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
                x += minor_spacing;
            }
            // Horizontal minor lines
            let mut y = snap_down(canvas_top, minor_spacing);
            while y <= canvas_bottom {
                let on_major = (y / major_spacing).round() * major_spacing;
                if (y - on_major).abs() > minor_spacing * 0.01 {
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
                y += minor_spacing;
            }
        }

        // --- Major grid lines ---
        // When minor lines are visible, major lines are thicker to
        // distinguish them. When minor lines are hidden (zoomed out),
        // use the thinner weight so the grid stays subtle.
        {
            let major_thickness = if show_minor { 1.0 / zoom } else { thickness };
            // Vertical major lines
            let mut x = snap_down(canvas_left, major_spacing);
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
                x += major_spacing;
            }
            // Horizontal major lines
            let mut y = snap_down(canvas_top, major_spacing);
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
                y += major_spacing;
            }
        }

        // --- Origin crosshair ---
        {
            let crosshair_color: [f32; 4] = [1.0, 0.4, 0.4, 0.6];
            let crosshair_thickness = 1.0 / zoom;
            let crosshair_half_len = 12.0 / zoom;

            // Horizontal arm
            push_quad(
                &mut verts,
                &mut idxs,
                0.0,
                0.0,
                crosshair_half_len,
                crosshair_thickness,
                crosshair_color,
            );
            // Vertical arm
            push_quad(
                &mut verts,
                &mut idxs,
                0.0,
                0.0,
                crosshair_thickness,
                crosshair_half_len,
                crosshair_color,
            );
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

        // ── Selection bounding boxes (Object mode only) ─────────────
        // Draw a thin outline around each object-selected node's world-space
        // bounding box. For groups, this encompasses all children.
        //
        // The outline is a fixed width in screen pixels regardless of camera
        // zoom: we divide `BBOX_THICKNESS_PX` by the current zoom so that
        // the world-space thickness shrinks proportionally as you zoom in
        // and grows as you zoom out, keeping the rendered bar ~constant.
        if selection.mode == SelectionMode::Object {
            let bbox_color: [f32; 4] = [0.3, 0.6, 1.0, 0.6]; // selection blue

            const BBOX_THICKNESS_PX: f32 = 1.5;
            let thickness = BBOX_THICKNESS_PX / zoom; // full thickness, world units
            let half_thickness = thickness * 0.5;

            let bool_cache = &mut self.bool_path_cache;
            // Iterate the selection directly rather than walking the entire
            // scene from root: with thousands of selected paths, the old
            // `walk_depth_first` from root was O(N) per frame, and the
            // `is_node_selected` filter (Vec::contains over `selected_nodes`)
            // made it O(N×S) — pathological for large traced rasters.
            // Direct iteration is O(S × ancestor-depth).
            let selected_for_bbox: Vec<NodeId> = selection
                .selected_nodes
                .iter()
                .chain(selection.marquee_preview_nodes.iter())
                .copied()
                .collect();
            for id in selected_for_bbox {
                let Some(node) = scene.get(id) else { continue };
                if !scene.is_visible_in_world(id) {
                    continue;
                }
                let world_transform = scene.world_transform(id);

                // Compute the world-space bounding box for this node,
                // including the visible stroke area.
                let bounds = match &node.data {
                    NodeData::Group {
                        kind: vector_scene::GroupKind::Boolean { .. },
                        ..
                    } => {
                        // Boolean group: use the bounds of the computed
                        // result path, transformed into world space.
                        let computed = bool_cache.get_or_compute(scene, id);
                        let local = vector_tess::path_visual_bounds(computed, true, None);
                        local.transform(world_transform)
                    }
                    NodeData::Group { .. } => {
                        // For groups, walk the subtree to get the aggregate bounds.
                        let mut b = vector_geom::Bounds::EMPTY;
                        scene.walk_depth_first(id, world_transform, &mut |_cid, cnode, cworld| {
                            if !cnode.visible {
                                return false;
                            }
                            let cb = cnode.data.visual_bounds(cworld);
                            if !cb.is_empty() {
                                b = b.union(cb);
                            }
                            true
                        });
                        b
                    }
                    _ => node.data.visual_bounds(world_transform),
                };

                if bounds.is_empty() {
                    continue;
                }

                let x0 = bounds.min.x as f32;
                let y0 = bounds.min.y as f32;
                let x1 = bounds.max.x as f32;
                let y1 = bounds.max.y as f32;

                // Corner-cover strategy: horizontal (top/bottom) bars are
                // stretched past the bounds by half a thickness on each
                // end, while vertical (left/right) bars are shortened by
                // half a thickness on each end so they fit between the
                // horizontal bars. No overlap, no notch. Equivalent to
                // "draw the horizontal borders half a border width longer"
                // from the user's description.
                let horiz_half_width = (x1 - x0) * 0.5 + half_thickness;
                let vert_half_height = ((y1 - y0) * 0.5 - half_thickness).max(0.0);

                // Top edge
                push_quad(
                    &mut verts,
                    &mut idxs,
                    (x0 + x1) * 0.5,
                    y0,
                    horiz_half_width,
                    half_thickness,
                    bbox_color,
                );
                // Bottom edge
                push_quad(
                    &mut verts,
                    &mut idxs,
                    (x0 + x1) * 0.5,
                    y1,
                    horiz_half_width,
                    half_thickness,
                    bbox_color,
                );
                // Left edge
                push_quad(
                    &mut verts,
                    &mut idxs,
                    x0,
                    (y0 + y1) * 0.5,
                    half_thickness,
                    vert_half_height,
                    bbox_color,
                );
                // Right edge
                push_quad(
                    &mut verts,
                    &mut idxs,
                    x1,
                    (y0 + y1) * 0.5,
                    half_thickness,
                    vert_half_height,
                    bbox_color,
                );
            }

            // ── Scale handles (8 squares on the combined selection bbox) ──
            // Compute the combined bounds of all selected nodes.
            let mut combined_bounds = vector_geom::Bounds::EMPTY;
            for &id in &selection.selected_nodes {
                if let Some(node) = scene.get(id) {
                    if !node.visible {
                        continue;
                    }
                    let world = scene.world_transform(id);
                    let b = match &node.data {
                        NodeData::Group {
                            kind: vector_scene::GroupKind::Boolean { .. },
                            ..
                        } => {
                            let computed = bool_cache.get_or_compute(scene, id);
                            let local = vector_tess::path_visual_bounds(computed, true, None);
                            local.transform(world)
                        }
                        NodeData::Group { .. } => {
                            let mut gb = vector_geom::Bounds::EMPTY;
                            scene.walk_depth_first(id, world, &mut |_cid, cnode, cworld| {
                                if !cnode.visible {
                                    return false;
                                }
                                let cb = cnode.data.visual_bounds(cworld);
                                if !cb.is_empty() {
                                    gb = gb.union(cb);
                                }
                                true
                            });
                            gb
                        }
                        _ => node.data.visual_bounds(world),
                    };
                    if !b.is_empty() {
                        combined_bounds = combined_bounds.union(b);
                    }
                }
            }

            if !combined_bounds.is_empty() {
                use vector_tools::ScaleHandle;
                let handle_half = HANDLE_SIZE / zoom;
                let handle_fill: [f32; 4] = [1.0, 1.0, 1.0, 1.0]; // white
                let handle_border: [f32; 4] = [0.3, 0.6, 1.0, 0.9]; // selection blue

                for handle in ScaleHandle::ALL {
                    let pos = handle.position(combined_bounds);
                    push_handle(
                        &mut verts,
                        &mut idxs,
                        pos,
                        handle_half,
                        handle_fill,
                        handle_border,
                    );
                }
            }
        }

        // ── Text-editing bounding box ────────────────────────────────────
        // When a text node is being edited, draw an amber bounding box
        // regardless of selection mode so the node stays visible.
        if let Some(edit_id) = self.text_editing_node {
            let text_bbox_color: [f32; 4] = [1.0, 0.7, 0.2, 0.7]; // amber

            const TEXT_BBOX_THICKNESS_PX: f32 = 1.5;
            let thickness = TEXT_BBOX_THICKNESS_PX / zoom;
            let half_thickness = thickness * 0.5;

            scene.walk_depth_first(
                scene.root(),
                Affine::IDENTITY,
                &mut |id, node, world_transform| {
                    if !node.visible {
                        return false;
                    }
                    if id != edit_id {
                        return true;
                    }
                    let bounds = node.data.visual_bounds(world_transform);
                    // For empty text, use font metrics to show a minimal box.
                    let bounds = if bounds.is_empty() {
                        if let NodeData::Text(text) = &node.data {
                            let shaped = vector_text::shape_text(
                                &text.content,
                                &text.font_family,
                                text.font_size,
                            );
                            let local = vector_geom::Bounds {
                                min: vector_geom::Point::new(0.0, -shaped.ascent),
                                max: vector_geom::Point::new(
                                    shaped.advance_width.max(text.font_size * 0.5),
                                    -shaped.descent,
                                ),
                            };
                            local.transform(world_transform)
                        } else {
                            return true;
                        }
                    } else {
                        bounds
                    };

                    let x0 = bounds.min.x as f32;
                    let y0 = bounds.min.y as f32;
                    let x1 = bounds.max.x as f32;
                    let y1 = bounds.max.y as f32;

                    let horiz_half_width = (x1 - x0) * 0.5 + half_thickness;
                    let vert_half_height = ((y1 - y0) * 0.5 - half_thickness).max(0.0);

                    // Top
                    push_quad(
                        &mut verts,
                        &mut idxs,
                        (x0 + x1) * 0.5,
                        y0,
                        horiz_half_width,
                        half_thickness,
                        text_bbox_color,
                    );
                    // Bottom
                    push_quad(
                        &mut verts,
                        &mut idxs,
                        (x0 + x1) * 0.5,
                        y1,
                        horiz_half_width,
                        half_thickness,
                        text_bbox_color,
                    );
                    // Left
                    push_quad(
                        &mut verts,
                        &mut idxs,
                        x0,
                        (y0 + y1) * 0.5,
                        half_thickness,
                        vert_half_height,
                        text_bbox_color,
                    );
                    // Right
                    push_quad(
                        &mut verts,
                        &mut idxs,
                        x1,
                        (y0 + y1) * 0.5,
                        half_thickness,
                        vert_half_height,
                        text_bbox_color,
                    );
                    true
                },
            );
        }

        // ── Vertex handles (Node mode only) ─────────────────────────────
        if selection.mode == SelectionMode::Node {
            let line_color: [f32; 4] = [0.5, 0.7, 1.0, 0.7];
            let line_thickness = 1.0 / zoom;
            let dash_len = 4.0 / zoom;
            let gap_len = 3.0 / zoom;

            /// Draw a handle line between an anchor and a control point.
            /// Solid for Smooth/Symmetric vertices, dashed for Corner.
            #[allow(clippy::too_many_arguments)]
            fn draw_handle_line(
                verts: &mut Vec<Vertex>,
                idxs: &mut Vec<u32>,
                anchor: Point,
                ctrl: Point,
                mode: vector_geom::VertexMode,
                thickness: f32,
                dash_len: f32,
                gap_len: f32,
                color: [f32; 4],
            ) {
                match mode {
                    vector_geom::VertexMode::Smooth | vector_geom::VertexMode::Symmetric => {
                        push_line(verts, idxs, anchor, ctrl, thickness, color);
                    }
                    vector_geom::VertexMode::Corner => {
                        push_dashed_line(
                            verts, idxs, anchor, ctrl, thickness, dash_len, gap_len, color,
                        );
                    }
                }
            }

            // First pass: draw handle lines (behind the handle squares).
            // Iterate the selection directly to avoid an O(N×S) scene walk.
            let node_handle_iter: Vec<NodeId> = selection
                .selected_nodes
                .iter()
                .chain(selection.marquee_preview_nodes.iter())
                .copied()
                .collect();
            for id in node_handle_iter.iter().copied() {
                let Some(node) = scene.get(id) else { continue };
                if !scene.is_visible_in_world(id) {
                    continue;
                }
                if let NodeData::Path { ref path, .. } = node.data {
                    let world_transform = scene.world_transform(id);
                    let xf = |p: Point| -> Point {
                        if world_transform.is_identity() {
                            p
                        } else {
                            world_transform.apply(p)
                        }
                    };

                    for subpath in &path.subpaths {
                        let mut prev_anchor = subpath.start;

                        for (seg_idx, seg) in subpath.segments.iter().enumerate() {
                            // Vertex mode of the "from" anchor (index seg_idx for
                            // prev endpoint, or 0 for the start point).
                            let from_mode = subpath
                                .vertex_modes
                                .get(seg_idx)
                                .copied()
                                .unwrap_or(vector_geom::VertexMode::Corner);
                            // Vertex mode of the "to" anchor (index seg_idx + 1).
                            let to_mode = subpath
                                .vertex_modes
                                .get(seg_idx + 1)
                                .copied()
                                .unwrap_or(vector_geom::VertexMode::Corner);

                            match seg {
                                Segment::Quad { ctrl, to } => {
                                    draw_handle_line(
                                        &mut verts,
                                        &mut idxs,
                                        xf(prev_anchor),
                                        xf(*ctrl),
                                        from_mode,
                                        line_thickness,
                                        dash_len,
                                        gap_len,
                                        line_color,
                                    );
                                    draw_handle_line(
                                        &mut verts,
                                        &mut idxs,
                                        xf(*to),
                                        xf(*ctrl),
                                        to_mode,
                                        line_thickness,
                                        dash_len,
                                        gap_len,
                                        line_color,
                                    );
                                    prev_anchor = *to;
                                }
                                Segment::Cubic { ctrl1, ctrl2, to } => {
                                    // ctrl1 belongs to prev_anchor (outgoing handle).
                                    draw_handle_line(
                                        &mut verts,
                                        &mut idxs,
                                        xf(prev_anchor),
                                        xf(*ctrl1),
                                        from_mode,
                                        line_thickness,
                                        dash_len,
                                        gap_len,
                                        line_color,
                                    );
                                    // ctrl2 belongs to `to` (incoming handle).
                                    draw_handle_line(
                                        &mut verts,
                                        &mut idxs,
                                        xf(*to),
                                        xf(*ctrl2),
                                        to_mode,
                                        line_thickness,
                                        dash_len,
                                        gap_len,
                                        line_color,
                                    );
                                    prev_anchor = *to;
                                }
                                Segment::Line { to } | Segment::Arc { to, .. } => {
                                    prev_anchor = *to;
                                }
                            }
                        }
                    }
                }
            }

            // Second pass: draw handle squares (on top of lines).
            for id in node_handle_iter.iter().copied() {
                let Some(node) = scene.get(id) else { continue };
                if !scene.is_visible_in_world(id) {
                    continue;
                }
                if let NodeData::Path { ref path, .. } = node.data {
                    let world_transform = scene.world_transform(id);
                    // Helper: transform a point from local to world coordinates for handle display.
                    let xform = |p: Point| -> Point {
                        if world_transform.is_identity() {
                            p
                        } else {
                            world_transform.apply(p)
                        }
                    };

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
                            xform(subpath.start),
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
                                    push_handle(
                                        &mut verts,
                                        &mut idxs,
                                        xform(*to),
                                        handle_size,
                                        fill,
                                        border,
                                    );
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
                                        xform(*ctrl),
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
                                    push_handle(
                                        &mut verts,
                                        &mut idxs,
                                        xform(*to),
                                        handle_size,
                                        fill,
                                        border,
                                    );
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
                                        xform(*ctrl1),
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
                                        xform(*ctrl2),
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
                                    push_handle(
                                        &mut verts,
                                        &mut idxs,
                                        xform(*to),
                                        handle_size,
                                        fill,
                                        border,
                                    );
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
                                    push_handle(
                                        &mut verts,
                                        &mut idxs,
                                        xform(*to),
                                        handle_size,
                                        fill,
                                        border,
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Ghost vertex on edge hover — shows where a double-click would insert.
            if let Some(pt) = selection.edge_hover_point {
                let ghost_fill: [f32; 4] = [1.0, 1.0, 1.0, 0.4];
                let ghost_border: [f32; 4] = [0.3, 0.5, 1.0, 0.5];
                push_handle(
                    &mut verts,
                    &mut idxs,
                    pt,
                    handle_size,
                    ghost_fill,
                    ghost_border,
                );
            }
        } // end Node mode vertex handles

        // ── Gradient handles ────────────────────────────────────────
        // For every selected path / boolean group / text node whose
        // fill or stroke references a gradient, draw the gradient's
        // editing handles on top of the canvas. This is mode-agnostic:
        // gradient handles appear in both Object and Node mode so the
        // user can grab them without first dropping into vertex-edit.
        //
        // World-space layout: gradient.kind coordinates are in
        // gradient-local space and are mapped to world by
        // `gradient.transform` (SVG userSpaceOnUse semantics — the
        // path's own transform does NOT apply). The
        // `for_each_handle_of_gradient` helper resolves this for us.
        {
            // Distinct from vertex-handle blue, so the two systems are
            // visually unambiguous when they sit close together.
            let gradient_axis_color: [f32; 4] = [1.0, 0.7, 0.3, 0.9]; // warm orange
            let gradient_axis_thickness = 1.0 / zoom;
            let gradient_axis_dash = 6.0 / zoom;
            let gradient_axis_gap = 4.0 / zoom;
            let endpoint_fill: [f32; 4] = [1.0, 0.85, 0.6, 1.0];
            let endpoint_border: [f32; 4] = [0.6, 0.35, 0.0, 1.0];
            let stop_fill: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
            let stop_border: [f32; 4] = [0.6, 0.35, 0.0, 1.0];
            let hovered_fill_g: [f32; 4] = [1.0, 0.85, 0.3, 1.0];
            let hovered_border_g: [f32; 4] = [0.8, 0.45, 0.0, 1.0];
            let selected_fill_g: [f32; 4] = [0.2, 0.6, 1.0, 1.0];
            let selected_border_g: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

            // Track which gradient nodes we've already drawn the
            // axis/circle for, so a gradient referenced by both fill
            // and stroke (or by both fill and another shape's fill)
            // doesn't double-draw the axis.
            let mut drawn_axes: HashSet<NodeId> = HashSet::new();

            // Gather gradient refs from selected nodes — we walk the
            // scene directly here (rather than re-using a vector-tools
            // helper) so we have access to both the gradient AND the
            // selection state at the same time.
            //
            // Currently-dragged handle counts as "selected" visually so
            // the user gets immediate feedback during a drag.
            let active_handle = selection
                .dragging_gradient_handle()
                .or(selection.gradient_hovered);
            // The hovered/dragged distinction matters only for colour:
            // an active drag uses the brighter "selected" palette,
            // hover-without-drag uses the warm hover palette.
            let dragging = selection.is_dragging_gradient_handle();

            for &owner in &selection.selected_nodes {
                let Some(owner_node) = scene.get(owner) else {
                    continue;
                };

                // Collect referenced gradient ids — dedups so a gradient
                // shared by both fill and stroke isn't drawn twice.
                let mut grad_ids: Vec<NodeId> = Vec::new();
                owner_node.for_each_paint_ref(|paint| {
                    if let PaintRef::Ref(id) = paint
                        && !grad_ids.contains(id)
                    {
                        grad_ids.push(*id);
                    }
                });

                for grad_id in grad_ids {
                    let Some(grad_node) = scene.get(grad_id) else {
                        continue;
                    };
                    let NodeData::Paint(Paint::Gradient(g)) = &grad_node.data else {
                        continue;
                    };

                    // Draw the axis / radius circle once per gradient.
                    if drawn_axes.insert(grad_id) {
                        match g.kind {
                            GradientKind::Linear { start, end } => {
                                let ws = g.transform.apply(start);
                                let we = g.transform.apply(end);
                                push_dashed_line(
                                    &mut verts,
                                    &mut idxs,
                                    ws,
                                    we,
                                    gradient_axis_thickness,
                                    gradient_axis_dash,
                                    gradient_axis_gap,
                                    gradient_axis_color,
                                );
                            }
                            GradientKind::Radial {
                                center,
                                radius,
                                focal,
                                focal_radius: _,
                            } => {
                                let wc = g.transform.apply(center);
                                let we = g.transform.apply(Point::new(center.x + radius, center.y));
                                // Radius axis line.
                                push_dashed_line(
                                    &mut verts,
                                    &mut idxs,
                                    wc,
                                    we,
                                    gradient_axis_thickness,
                                    gradient_axis_dash,
                                    gradient_axis_gap,
                                    gradient_axis_color,
                                );
                                // Approximated radius circle (32 dashed
                                // segments). The circle is in
                                // *gradient-local* space, so we apply
                                // the transform to each sample.
                                const SEGMENTS: usize = 32;
                                let mut prev =
                                    g.transform.apply(Point::new(center.x + radius, center.y));
                                for i in 1..=SEGMENTS {
                                    let theta =
                                        (i as f64) * std::f64::consts::TAU / (SEGMENTS as f64);
                                    let p = Point::new(
                                        center.x + radius * theta.cos(),
                                        center.y + radius * theta.sin(),
                                    );
                                    let wp = g.transform.apply(p);
                                    // Alternate dash / gap by parity.
                                    if i % 2 == 1 {
                                        push_line(
                                            &mut verts,
                                            &mut idxs,
                                            prev,
                                            wp,
                                            gradient_axis_thickness,
                                            gradient_axis_color,
                                        );
                                    }
                                    prev = wp;
                                }
                                // Focal indicator (only when focal is
                                // visibly displaced from centre). Draw
                                // a thin line from centre to focal so
                                // the relationship is legible.
                                let dx = focal.x - center.x;
                                let dy = focal.y - center.y;
                                if dx.hypot(dy) > 1e-6 {
                                    let wf = g.transform.apply(focal);
                                    push_line(
                                        &mut verts,
                                        &mut idxs,
                                        wc,
                                        wf,
                                        gradient_axis_thickness,
                                        gradient_axis_color,
                                    );
                                }
                            }
                        }
                    }

                    // Now the per-handle pass: positions + state →
                    // styled square / round handles.
                    for_each_handle_of_gradient(grad_id, owner, g, |h, world| {
                        let is_active = active_handle == Some(h);
                        let (fill, border) = if is_active && dragging {
                            (selected_fill_g, selected_border_g)
                        } else if is_active {
                            (hovered_fill_g, hovered_border_g)
                        } else {
                            match h.point {
                                GradientHandlePoint::Stop(_) => (stop_fill, stop_border),
                                _ => (endpoint_fill, endpoint_border),
                            }
                        };
                        // Endpoints / centre / focal: standard square
                        // handle. Stops: slightly smaller handle so
                        // they read as secondary on the axis line.
                        let size = match h.point {
                            GradientHandlePoint::Stop(_) => handle_size * 0.75,
                            _ => handle_size,
                        };
                        push_handle(&mut verts, &mut idxs, world, size, fill, border);
                    });
                }
            }
        }

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

        // ── Text editing cursor (caret) ─────────────────────────────
        if let Some(tc) = &self.text_cursor {
            let [a, b, c, d, e, f] = tc.world_transform;
            // Transform local caret endpoints to world space.
            let top_x = a * tc.local_x + c * tc.local_top + e;
            let top_y = b * tc.local_x + d * tc.local_top + f;
            let bot_x = a * tc.local_x + c * tc.local_bottom + e;
            let bot_y = b * tc.local_x + d * tc.local_bottom + f;

            let cx = (top_x + bot_x) * 0.5;
            let cy = (top_y + bot_y) * 0.5;
            let half_h = ((bot_x - top_x).powi(2) + (bot_y - top_y).powi(2)).sqrt() * 0.5;
            let caret_thickness = 1.0 / self.zoom;
            let caret_color: [f32; 4] = [1.0, 1.0, 1.0, 0.9];

            push_quad(
                &mut verts,
                &mut idxs,
                cx,
                cy,
                caret_thickness,
                half_h,
                caret_color,
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
    ///
    /// Render order:
    ///
    /// 1. **Grid** — vector pipeline, drawn behind everything else.
    /// 2. **Scene content** — iterates `draw_ops` in scene order, switching
    ///    between the vector and raster pipelines as needed. This is what
    ///    preserves z-order: a raster sandwiched between two paths renders
    ///    under one and on top of the other.
    /// 3. **Handle overlay** — vector pipeline, drawn on top of everything
    ///    so vertex handles / selection boxes are always visible.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'static>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);

        // Draw grid (behind everything)
        if self.grid_num_indices > 0
            && let (Some(vb), Some(ib)) = (&self.grid_vertex_buffer, &self.grid_index_buffer)
        {
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.grid_num_indices, 0, 0..1);
        }

        // Draw scene content. We track which pipeline is currently bound
        // (`PipelineMode::Vector` after the grid draw) and only re-bind
        // when switching kinds — wgpu draw calls are cheap relative to
        // pipeline state changes, but consecutive ops of the same kind
        // are common (e.g. many paths in a row) so this still matters.
        #[derive(PartialEq)]
        enum PipelineMode {
            Vector,
            Raster,
        }
        let mut mode = PipelineMode::Vector;

        // We may or may not have vector buffers (a scene of only rasters
        // produces zero vector indices and skips the buffer creation).
        // Bind them up front if they exist, but the per-op `VectorBatch`
        // arm gates on them anyway so we don't draw stale geometry.
        let vector_buffers = match (&self.vertex_buffer, &self.index_buffer) {
            (Some(vb), Some(ib)) => Some((vb, ib)),
            _ => None,
        };
        if let Some((vb, ib)) = vector_buffers {
            pass.set_vertex_buffer(0, vb.slice(..));
            pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
        }

        for op in &self.draw_ops {
            match op {
                DrawOp::VectorBatch { index_range } => {
                    let Some((vb, ib)) = vector_buffers else {
                        // No vector buffers this frame — should never
                        // happen if a `VectorBatch` was emitted, but
                        // defensively skip rather than panic.
                        continue;
                    };
                    if mode != PipelineMode::Vector {
                        pass.set_pipeline(&self.pipeline);
                        pass.set_bind_group(0, &self.bind_group, &[]);
                        pass.set_vertex_buffer(0, vb.slice(..));
                        pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                        mode = PipelineMode::Vector;
                    }
                    pass.draw_indexed(index_range.clone(), 0, 0..1);
                }
                DrawOp::Raster { node_id } => {
                    if mode != PipelineMode::Raster {
                        pass.set_pipeline(&self.raster_pipeline);
                        pass.set_bind_group(0, &self.raster_globals_bind_group, &[]);
                        mode = PipelineMode::Raster;
                    }
                    if let Some(entry) = self.raster_cache.get(*node_id) {
                        pass.set_bind_group(1, &entry.bind_group, &[]);
                        // Six vertices, two triangles, one instance.
                        pass.draw(0..6, 0..1);
                    }
                }
            }
        }

        // Restore vector pipeline + bind group for the handle overlay.
        if mode != PipelineMode::Vector {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
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

/// Push a thin line (as a rotated quad) between two points.
fn push_line(
    verts: &mut Vec<Vertex>,
    idxs: &mut Vec<u32>,
    a: Point,
    b: Point,
    thickness: f32,
    color: [f32; 4],
) {
    let ax = a.x as f32;
    let ay = a.y as f32;
    let bx = b.x as f32;
    let by = b.y as f32;

    let dx = bx - ax;
    let dy = by - ay;
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return;
    }

    // Perpendicular unit vector scaled by half-thickness.
    let half = thickness * 0.5;
    let nx = -dy / len * half;
    let ny = dx / len * half;

    let base = verts.len() as u32;
    verts.push(Vertex::solid([ax + nx, ay + ny], color));
    verts.push(Vertex::solid([ax - nx, ay - ny], color));
    verts.push(Vertex::solid([bx - nx, by - ny], color));
    verts.push(Vertex::solid([bx + nx, by + ny], color));
    idxs.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// Push a dashed line between two points. Each dash and gap has the given length.
#[allow(clippy::too_many_arguments)]
fn push_dashed_line(
    verts: &mut Vec<Vertex>,
    idxs: &mut Vec<u32>,
    a: Point,
    b: Point,
    thickness: f32,
    dash_len: f32,
    gap_len: f32,
    color: [f32; 4],
) {
    let dx = (b.x - a.x) as f32;
    let dy = (b.y - a.y) as f32;
    let total_len = dx.hypot(dy);
    if total_len < 1e-6 {
        return;
    }

    let ux = dx / total_len;
    let uy = dy / total_len;

    let mut t = 0.0f32;
    let ax = a.x as f32;
    let ay = a.y as f32;

    while t < total_len {
        let end = (t + dash_len).min(total_len);
        push_line(
            verts,
            idxs,
            Point::new((ax + ux * t) as f64, (ay + uy * t) as f64),
            Point::new((ax + ux * end) as f64, (ay + uy * end) as f64),
            thickness,
            color,
        );
        t = end + gap_len;
    }
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
    verts.push(Vertex::solid([cx - half_x, cy - half_y], color));
    verts.push(Vertex::solid([cx + half_x, cy - half_y], color));
    verts.push(Vertex::solid([cx + half_x, cy + half_y], color));
    verts.push(Vertex::solid([cx - half_x, cy + half_y], color));
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

/// Orthographic projection matrix that maps a tile rect [x, y, w, h] to fill
/// a viewport of `width × height` pixels. No pan or zoom — just direct mapping.
fn camera_matrix_ortho(width: f32, height: f32, tile_rect: &[f64; 4]) -> [f32; 16] {
    let rx = tile_rect[0] as f32;
    let ry = tile_rect[1] as f32;
    let rw = tile_rect[2] as f32;
    let rh = tile_rect[3] as f32;

    // Map [rx, rx+rw] → [-1, 1] horizontally
    // Map [ry, ry+rh] → [1, -1] vertically (Y-flip for top-left origin)
    let sx = 2.0 / rw;
    let sy = -2.0 / rh;
    let tx = -(2.0 * rx / rw + 1.0);
    let ty = 2.0 * ry / rh + 1.0;

    // The pattern is rendered at the texture's pixel dimensions, but
    // geometrically the tile rect may be a different aspect ratio.
    // We don't need to compensate here because the UV computation in the
    // shader normalises by tile_rect dimensions.
    let _ = (width, height); // suppress unused warnings; sizes drive texture creation only

    [
        sx, 0.0, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, tx, ty, 0.0, 1.0,
    ]
}

/// Create a 1×1 placeholder pattern texture array (1 layer, transparent pixel).
fn create_placeholder_pattern_texture(device: &wgpu::Device) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("placeholder pattern texture"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// Create a texture view with `D2Array` dimension. wgpu defaults to `D2`
/// when `depth_or_array_layers == 1`, but the shader always expects
/// `texture_2d_array`, so we must be explicit.
fn pattern_array_view(texture: &wgpu::Texture) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    })
}

/// Create the full bind group with all five bindings.
#[allow(clippy::too_many_arguments)]
fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals_buffer: &wgpu::Buffer,
    gradient_buffer: &wgpu::Buffer,
    pattern_buffer: &wgpu::Buffer,
    pattern_texture_view: &wgpu::TextureView,
    pattern_sampler: &wgpu::Sampler,
    transforms_buffer: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vector bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: gradient_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: pattern_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(pattern_texture_view),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::Sampler(pattern_sampler),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: transforms_buffer.as_entire_binding(),
            },
        ],
    })
}

/// Walk a subtree and collect all `PaintRef::Ref` node IDs that point to
/// one of the known pattern paint nodes. Used to build the dependency graph
/// for topological sorting of pattern rendering.
fn collect_pattern_paint_refs(
    scene: &Scene,
    content_id: NodeId,
    pattern_node_ids: &std::collections::HashSet<NodeId>,
) -> Vec<NodeId> {
    let mut refs = Vec::new();

    scene.walk_depth_first(content_id, Affine::IDENTITY, &mut |_id, node, _world| {
        let paint_refs: Vec<&PaintRef> = match &node.data {
            NodeData::Path { style, .. } => {
                let mut p = Vec::new();
                if let Some(f) = &style.fill {
                    p.push(&f.paint);
                }
                if let Some(s) = &style.stroke {
                    p.push(&s.paint);
                }
                p
            }
            NodeData::Text(t) => {
                let mut p = Vec::new();
                if let Some(f) = &t.style.fill {
                    p.push(&f.paint);
                }
                if let Some(s) = &t.style.stroke {
                    p.push(&s.paint);
                }
                p
            }
            _ => Vec::new(),
        };
        for paint in paint_refs {
            if let PaintRef::Ref(ref_id) = paint
                && pattern_node_ids.contains(ref_id)
                && !refs.contains(ref_id)
            {
                refs.push(*ref_id);
            }
        }
        true
    });

    refs
}

/// Topological sort of patterns using Kahn's algorithm.
///
/// `deps[i]` lists the pattern indices that pattern `i` depends on (i.e.,
/// pattern `i`'s content references those patterns as fills/strokes).
///
/// Returns `(order, cyclic)` where `order` is the rendering order (leaf
/// patterns first) and `cyclic` is the set of pattern indices that are
/// part of a dependency cycle. Cyclic patterns are not included in `order`
/// and their texture layers will stay transparent.
fn topological_sort_patterns(
    deps: &[Vec<usize>],
) -> (Vec<usize>, std::collections::HashSet<usize>) {
    let n = deps.len();

    // Build in-degree counts and reverse adjacency (who depends on me).
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, d) in deps.iter().enumerate() {
        for &dep in d {
            if dep < n {
                in_degree[i] += 1;
                dependents[dep].push(i);
            }
        }
    }

    // Seed the queue with patterns that have no dependencies.
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for (i, &deg) in in_degree.iter().enumerate() {
        if deg == 0 {
            queue.push_back(i);
        }
    }

    let mut order = Vec::with_capacity(n);
    while let Some(idx) = queue.pop_front() {
        order.push(idx);
        for &dependent in &dependents[idx] {
            in_degree[dependent] -= 1;
            if in_degree[dependent] == 0 {
                queue.push_back(dependent);
            }
        }
    }

    // Any patterns not in `order` are part of a cycle.
    let cyclic: std::collections::HashSet<usize> = (0..n).filter(|i| !order.contains(i)).collect();

    (order, cyclic)
}

/// Tessellate the content of a pattern group into vertices and indices,
/// with an orthographic camera set up for the tile rect.
fn tessellate_pattern_content(
    scene: &Scene,
    pattern: &Pattern,
    gradient_index_map: &HashMap<NodeId, i32>,
    pattern_index_map: &HashMap<NodeId, i32>,
    _tex_w: u32,
    _tex_h: u32,
) -> (Vec<Vertex>, Vec<u32>) {
    let mut all_vertices: Vec<Vertex> = Vec::new();
    let mut all_indices: Vec<u32> = Vec::new();

    let resolve_paint = |paint: &PaintRef| -> TessPaint {
        match paint {
            PaintRef::Solid(c) => TessPaint::Solid(*c),
            PaintRef::Ref(node_id) => {
                if let Some(&idx) = gradient_index_map.get(node_id) {
                    TessPaint::Gradient { index: idx }
                } else if let Some(&idx) = pattern_index_map.get(node_id) {
                    TessPaint::Pattern { index: idx }
                } else {
                    // Unresolved ref (cyclic pattern or unknown) → transparent (SVG spec:
                    // invalid paint server references result in no paint).
                    TessPaint::Solid(Color::TRANSPARENT)
                }
            }
        }
    };

    let resolve_stroke = |s: &Stroke| -> StrokeParams {
        let cap = match s.style.cap {
            vector_scene::LineCap::Butt => LineCap::Butt,
            vector_scene::LineCap::Round => LineCap::Round,
            vector_scene::LineCap::Square => LineCap::Square,
        };
        let join = match s.style.join {
            vector_scene::LineJoin::Miter => LineJoin::Miter,
            vector_scene::LineJoin::Round => LineJoin::Round,
            vector_scene::LineJoin::Bevel => LineJoin::Bevel,
        };
        let dash = s.style.dash.as_ref().map(|d| DashPattern {
            array: d.array.clone(),
            offset: d.offset,
        });
        StrokeParams {
            paint: resolve_paint(&s.paint),
            width: s.style.width,
            cap,
            join,
            miter_limit: s.style.miter_limit,
            dash,
            opacity: s.opacity,
        }
    };

    let push_mesh = |mesh: TessellatedMesh,
                     world_transform: &Affine,
                     all_vertices: &mut Vec<Vertex>,
                     all_indices: &mut Vec<u32>| {
        let base = all_vertices.len() as u32;
        if world_transform.is_identity() {
            all_vertices.extend_from_slice(&mesh.vertices);
        } else {
            all_vertices.extend(mesh.vertices.iter().map(|v| {
                let p =
                    world_transform.apply(Point::new(v.position[0] as f64, v.position[1] as f64));
                Vertex {
                    position: [p.x as f32, p.y as f32],
                    color: v.color,
                    // Pattern tile content uses path_id=0 (identity). The tile
                    // render pass binds the main transforms_buffer whose slot
                    // 0 is always identity, so shader output = input position.
                    path_id: 0,
                    gradient_index: v.gradient_index,
                    pattern_index: v.pattern_index,
                }
            }));
        }
        all_indices.extend(mesh.indices.iter().map(|i| i + base));
    };

    let mut font_db = global_font_db();

    // Walk the pattern content subtree.
    scene.walk_depth_first(
        pattern.content,
        Affine::IDENTITY,
        &mut |_id, node, world_transform| {
            if !node.visible {
                return false;
            }
            match node.data {
                NodeData::Path {
                    ref path,
                    ref style,
                } => {
                    let fill = style.fill.as_ref().map(|f| FillParams {
                        paint: resolve_paint(&f.paint),
                        opacity: f.opacity,
                    });
                    let stroke = style.stroke.as_ref().map(&resolve_stroke);
                    let mesh = tessellate_path(path, fill, stroke);
                    push_mesh(mesh, &world_transform, &mut all_vertices, &mut all_indices);
                }
                NodeData::Text(ref text) => {
                    let shaped =
                        font_db.shape_text(&text.content, &text.font_family, text.font_size);
                    let fill = text.style.fill.as_ref().map(|f| FillParams {
                        paint: resolve_paint(&f.paint),
                        opacity: f.opacity,
                    });
                    let stroke = text.style.stroke.as_ref().map(&resolve_stroke);
                    let mesh = tessellate_path(&shaped.path, fill, stroke);
                    push_mesh(mesh, &world_transform, &mut all_vertices, &mut all_indices);
                }
                _ => {}
            }
            true
        },
    );

    (all_vertices, all_indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topo_sort_no_deps() {
        // Three independent patterns.
        let deps = vec![vec![], vec![], vec![]];
        let (order, cyclic) = topological_sort_patterns(&deps);
        assert_eq!(order.len(), 3);
        assert!(cyclic.is_empty());
    }

    #[test]
    fn topo_sort_linear_chain() {
        // 0 → 1 → 2 (2 depends on 1, 1 depends on 0)
        let deps = vec![vec![], vec![0], vec![1]];
        let (order, cyclic) = topological_sort_patterns(&deps);
        assert_eq!(order.len(), 3);
        assert!(cyclic.is_empty());
        // 0 must come before 1, and 1 must come before 2.
        let pos = |x: usize| order.iter().position(|&v| v == x).unwrap();
        assert!(pos(0) < pos(1));
        assert!(pos(1) < pos(2));
    }

    #[test]
    fn topo_sort_direct_cycle() {
        // 0 ↔ 1 (mutual dependency)
        let deps = vec![vec![1], vec![0]];
        let (order, cyclic) = topological_sort_patterns(&deps);
        assert!(order.is_empty());
        assert_eq!(cyclic.len(), 2);
        assert!(cyclic.contains(&0));
        assert!(cyclic.contains(&1));
    }

    #[test]
    fn topo_sort_self_reference() {
        // 0 → 0 (self-reference)
        let deps = vec![vec![0]];
        let (order, cyclic) = topological_sort_patterns(&deps);
        assert!(order.is_empty());
        assert_eq!(cyclic.len(), 1);
        assert!(cyclic.contains(&0));
    }

    #[test]
    fn topo_sort_partial_cycle() {
        // 0 is independent, 1 ↔ 2 are cyclic, 3 depends on 0.
        let deps = vec![vec![], vec![2], vec![1], vec![0]];
        let (order, cyclic) = topological_sort_patterns(&deps);
        assert_eq!(order.len(), 2); // 0 and 3
        assert_eq!(cyclic.len(), 2); // 1 and 2
        assert!(cyclic.contains(&1));
        assert!(cyclic.contains(&2));
        assert!(order.contains(&0));
        assert!(order.contains(&3));
        let pos = |x: usize| order.iter().position(|&v| v == x).unwrap();
        assert!(pos(0) < pos(3));
    }

    #[test]
    fn topo_sort_diamond() {
        // 0 and 1 are leaves; 2 depends on both; 3 depends on 2.
        let deps = vec![vec![], vec![], vec![0, 1], vec![2]];
        let (order, cyclic) = topological_sort_patterns(&deps);
        assert_eq!(order.len(), 4);
        assert!(cyclic.is_empty());
        let pos = |x: usize| order.iter().position(|&v| v == x).unwrap();
        assert!(pos(0) < pos(2));
        assert!(pos(1) < pos(2));
        assert!(pos(2) < pos(3));
    }
}
