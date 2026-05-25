//! Per-node texture cache for `NodeData::Raster` nodes.
//!
//! Each cache entry owns the GPU texture, a small per-draw uniform buffer
//! (transform + size), and a pre-built bind group. The cache is keyed on
//! `NodeId`; texture re-upload only happens when the underlying
//! `Arc<RasterImage>` is replaced (detected by comparing the `Arc`'s
//! pointer + length, both of which we store on the cache entry).
//!
//! The per-draw uniform is overwritten every frame in `prepare()` since
//! the world transform changes on camera moves and parent edits, but the
//! texture is reused as long as the pixels haven't changed.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use vector_geom::Affine;
use vector_scene::{NodeId, RasterImage};

/// GPU layout of the per-raster uniform — must match the WGSL `RasterDraw`
/// struct in `raster.wgsl` byte-for-byte.
///
/// Encoding mirrors the vector pipeline's `GpuTransform` so callers familiar
/// with that layout don't have to learn a new convention:
/// * `row0` = (a, b, tx, _pad)
/// * `row1` = (c, d, ty, _pad)
/// * `size` = (width, height, _pad, _pad)
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct GpuRasterDraw {
    pub row0: [f32; 4],
    pub row1: [f32; 4],
    pub size: [f32; 4],
}

impl GpuRasterDraw {
    pub fn new(transform: &Affine, width: f64, height: f64, alpha_mul: f32) -> Self {
        Self {
            row0: [
                transform.a as f32,
                transform.b as f32,
                transform.tx as f32,
                alpha_mul,
            ],
            row1: [
                transform.c as f32,
                transform.d as f32,
                transform.ty as f32,
                0.0,
            ],
            size: [width as f32, height as f32, 0.0, 0.0],
        }
    }
}

/// One entry in the raster cache: GPU texture + its bind group + per-draw
/// uniform. Source-identity fields (`source_ptr`, `source_len`) detect
/// `Arc<RasterImage>` swaps so we know when to re-upload pixels.
pub struct GpuRaster {
    /// Underlying texture object — kept alive so the view stays valid.
    #[allow(dead_code)]
    pub texture: wgpu::Texture,
    /// Bind group bundling (texture, sampler, per-draw uniform). Built once
    /// on first upload and reused; the uniform buffer itself is reused too,
    /// so changing the transform / size only re-writes the buffer contents,
    /// not the bind group.
    pub bind_group: wgpu::BindGroup,
    /// Per-draw uniform buffer (transform + size). Same buffer for the
    /// lifetime of the cache entry; contents updated every frame.
    pub uniform_buffer: wgpu::Buffer,
    /// Pointer to the `Arc<RasterImage>`'s heap allocation. Used together
    /// with `source_len` to detect when the source pixels have been
    /// swapped out (cache invalidation).
    source_ptr: *const RasterImage,
    source_len: usize,
}

// Pointer kept only as a key/identity probe — never dereferenced — so the
// cache is safe to send across threads if the rest of the renderer ever is.
unsafe impl Send for GpuRaster {}
unsafe impl Sync for GpuRaster {}

pub struct RasterCache {
    entries: HashMap<NodeId, GpuRaster>,
}

impl RasterCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Number of cached rasters. Useful for tests and stats.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Look up an existing entry without uploading. Returns `None` if
    /// no entry exists for `id` yet.
    pub fn get(&self, id: NodeId) -> Option<&GpuRaster> {
        self.entries.get(&id)
    }

    /// Drop entries for any `NodeId` not in `alive`. Call once per
    /// `prepare()` after the scene walk to reclaim GPU memory for raster
    /// nodes that have been deleted.
    pub fn evict_missing(&mut self, alive: &HashSet<NodeId>) {
        self.entries.retain(|id, _| alive.contains(id));
    }

    /// Ensure the entry for `id` reflects the current `image`. If the
    /// `Arc` pointer + length match the existing entry, nothing is
    /// re-uploaded. If they differ (or the entry is missing), a new
    /// texture is created and the pixels uploaded.
    ///
    /// `per_draw_layout` is the bind group layout used for the per-raster
    /// bind group (texture + sampler + uniform). `sampler` is shared
    /// across all rasters.
    pub fn upload_if_needed(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        per_draw_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        id: NodeId,
        image: &Arc<RasterImage>,
    ) {
        let new_ptr = Arc::as_ptr(image);
        let new_len = image.pixels.len();

        if let Some(existing) = self.entries.get(&id)
            && existing.source_ptr == new_ptr
            && existing.source_len == new_len
        {
            return;
        }

        // (Re-)create the texture and bind group. We keep the previous
        // entry's uniform_buffer if it exists, but rebuilding the whole
        // entry is simpler and only happens on insertion / pixel swap.
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("raster image"),
            size: wgpu::Extent3d {
                width: image.pixel_width,
                height: image.pixel_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // sRGB texture: the pixel buffer is sRGB-encoded RGBA8 (matches
            // `image::ImageReader::decode().to_rgba8()`'s output) and we
            // want the GPU to convert to linear when sampling so that the
            // alpha blend happens in linear space alongside our vector
            // pipeline (which already works in linear).
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * image.pixel_width),
                rows_per_image: Some(image.pixel_height),
            },
            wgpu::Extent3d {
                width: image.pixel_width,
                height: image.pixel_height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("raster image view"),
            ..Default::default()
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("raster per-draw uniform"),
            size: std::mem::size_of::<GpuRasterDraw>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("raster per-draw bind group"),
            layout: per_draw_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform_buffer.as_entire_binding(),
                },
            ],
        });

        self.entries.insert(
            id,
            GpuRaster {
                texture,
                bind_group,
                uniform_buffer,
                source_ptr: new_ptr,
                source_len: new_len,
            },
        );
    }

    /// Write the per-draw uniform (transform + size) for an existing entry.
    /// Call after `upload_if_needed`. No-op if no entry exists for `id`.
    pub fn write_uniform(&self, queue: &wgpu::Queue, id: NodeId, draw: &GpuRasterDraw) {
        if let Some(entry) = self.entries.get(&id) {
            queue.write_buffer(&entry.uniform_buffer, 0, bytemuck::bytes_of(draw));
        }
    }
}

impl Default for RasterCache {
    fn default() -> Self {
        Self::new()
    }
}
