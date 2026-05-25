// Raster image pipeline.
//
// Draws a single textured quad per raster node. The quad covers the local
// space rectangle (0, 0)..(width, height); the per-raster uniform supplies
// both the world-space affine transform (matching the vector pipeline's
// `GpuTransform` layout) and the box dimensions.
//
// Shares bind group 0 (view-projection uniform) with the vector pipeline's
// shader.wgsl so the camera matrix and viewport are consistent across both
// pipelines without re-uploading.
//
// Vertex positions are computed from `vertex_index` (no vertex buffer
// binding required) — six verts forming two triangles of a unit quad.

struct Globals {
    view_proj: mat4x4<f32>,
};

struct RasterDraw {
    // World-space affine transform, matching `GpuTransform` packing in
    // shader.wgsl: row0 = (a, b, tx, alpha_mul), row1 = (c, d, ty, _).
    // `alpha_mul` is a per-instance opacity multiplier (1.0 = no effect),
    // used to dim out-of-scope nodes when group isolation is active.
    row0: vec4<f32>,
    row1: vec4<f32>,
    // Local-space box dimensions: (width, height, _pad, _pad).
    size: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> globals: Globals;

@group(1) @binding(0)
var raster_tex: texture_2d<f32>;

@group(1) @binding(1)
var raster_samp: sampler;

@group(1) @binding(2)
var<uniform> raster: RasterDraw;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) alpha_mul: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    // Two CCW triangles of a unit quad (0,0)..(1,1).
    var positions = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );

    let p_unit = positions[vid];
    let p_local = p_unit * raster.size.xy;

    // Apply the affine: world = (a*lx + b*ly + tx, c*lx + d*ly + ty).
    let world = vec2<f32>(
        raster.row0.x * p_local.x + raster.row0.y * p_local.y + raster.row0.z,
        raster.row1.x * p_local.x + raster.row1.y * p_local.y + raster.row1.z,
    );

    var out: VsOut;
    out.clip = globals.view_proj * vec4<f32>(world, 0.0, 1.0);
    // UVs are the unit-square coordinates; (0,0) = top-left of the image
    // since textures and our local space share a y-down convention here.
    out.uv = p_unit;
    out.alpha_mul = raster.row0.w;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var color = textureSample(raster_tex, raster_samp, in.uv);
    color.a *= in.alpha_mul;
    return color;
}
