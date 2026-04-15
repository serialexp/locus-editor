use bytemuck::{Pod, Zeroable};

/// GPU vertex for tessellated path geometry.
///
/// - `position`: the final world-space position (transformed by the view-projection matrix
///   in the vertex shader to produce clip-space output).
/// - `color`: solid fill/stroke color. Used when `gradient_index < 0` and `pattern_index < 0`.
/// - `world_pos`: the world-space position passed through to the fragment shader
///   (not transformed by view-projection). The gradient/pattern evaluation shader uses this
///   to compute the gradient parameter `t` or pattern UV coordinates.
/// - `gradient_index`: index into the gradient storage buffer, or -1 for no gradient.
/// - `pattern_index`: index into the pattern storage buffer, or -1 for no pattern.
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct Vertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
    pub world_pos: [f32; 2],
    pub gradient_index: i32,
    pub pattern_index: i32,
}

impl Vertex {
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![
            0 => Float32x2,   // position
            1 => Float32x4,   // color
            2 => Float32x2,   // world_pos
            3 => Sint32,      // gradient_index
            4 => Sint32,      // pattern_index
        ],
    };

    /// Create a solid-color vertex (no gradient or pattern).
    #[inline]
    pub fn solid(position: [f32; 2], color: [f32; 4]) -> Self {
        Self {
            position,
            color,
            world_pos: position,
            gradient_index: -1,
            pattern_index: -1,
        }
    }

    /// Create a gradient vertex.
    #[inline]
    pub fn gradient(position: [f32; 2], world_pos: [f32; 2], gradient_index: i32) -> Self {
        Self {
            position,
            color: [0.0, 0.0, 0.0, 1.0],
            world_pos,
            gradient_index,
            pattern_index: -1,
        }
    }

    /// Create a pattern vertex.
    #[inline]
    pub fn pattern(position: [f32; 2], world_pos: [f32; 2], pattern_index: i32) -> Self {
        Self {
            position,
            color: [0.0, 0.0, 0.0, 1.0],
            world_pos,
            gradient_index: -1,
            pattern_index,
        }
    }
}
