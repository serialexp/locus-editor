use bytemuck::{Pod, Zeroable};

/// GPU vertex for tessellated path geometry.
///
/// Positions are stored in **local space** — the vertex shader applies the
/// owning path's world transform from a `transforms` storage buffer indexed
/// by `path_id`. This lets the renderer cache tessellation output once per
/// path and reuse it across transform-only edits (drag-to-move, rotate,
/// scale), bumping only the GPU-side transforms buffer.
///
/// - `position`: local-space XY. World position = `transforms[path_id] * position`.
/// - `color`: solid fill/stroke color. Used when both indices are `< 0`.
/// - `path_id`: index into the transforms storage buffer. Slot `0` is
///   reserved for the identity transform — overlay geometry (grid, handles,
///   selection bbox) is pre-computed in world space and uses `path_id = 0`.
/// - `gradient_index`: index into the gradient storage buffer, or `-1` for
///   no gradient.
/// - `pattern_index`: index into the pattern storage buffer, or `-1` for
///   no pattern.
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct Vertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
    pub path_id: u32,
    pub gradient_index: i32,
    pub pattern_index: i32,
}

impl Vertex {
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![
            0 => Float32x2,   // position (local space)
            1 => Float32x4,   // color
            2 => Uint32,      // path_id
            3 => Sint32,      // gradient_index
            4 => Sint32,      // pattern_index
        ],
    };

    /// Create a solid-color vertex (no gradient or pattern). `path_id`
    /// defaults to `0` (identity transform slot); the renderer rewrites
    /// it before upload for path/text meshes.
    #[inline]
    pub fn solid(position: [f32; 2], color: [f32; 4]) -> Self {
        Self {
            position,
            color,
            path_id: 0,
            gradient_index: -1,
            pattern_index: -1,
        }
    }

    /// Create a gradient vertex. `opacity` is stored in `color.a` for the
    /// fragment shader to apply to the gradient sample.
    #[inline]
    pub fn gradient(position: [f32; 2], gradient_index: i32, opacity: f32) -> Self {
        Self {
            position,
            color: [0.0, 0.0, 0.0, opacity],
            path_id: 0,
            gradient_index,
            pattern_index: -1,
        }
    }

    /// Create a pattern vertex. `opacity` is stored in `color.a` for the
    /// fragment shader to apply to the pattern sample.
    #[inline]
    pub fn pattern(position: [f32; 2], pattern_index: i32, opacity: f32) -> Self {
        Self {
            position,
            color: [0.0, 0.0, 0.0, opacity],
            path_id: 0,
            gradient_index: -1,
            pattern_index,
        }
    }
}
