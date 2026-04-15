// Vector rendering shader with gradient and pattern support.
// Takes 2D positions + RGBA color per vertex, applies an orthographic projection.
// Gradient vertices carry a gradient_index >= 0 that indexes into a storage
// buffer of gradient descriptors; the fragment shader evaluates the gradient
// color per-pixel using the interpolated world_pos.
// Pattern vertices carry a pattern_index >= 0 that indexes into a storage
// buffer of pattern descriptors; the fragment shader samples a tiled texture.

// ── Vertex types ────────────────────────────────────────────────────────

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) world_pos: vec2<f32>,
    @location(3) gradient_index: i32,
    @location(4) pattern_index: i32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) world_pos: vec2<f32>,
    @location(2) @interpolate(flat) gradient_index: i32,
    @location(3) @interpolate(flat) pattern_index: i32,
};

// ── Uniform / storage bindings ──────────────────────────────────────────

struct Globals {
    view_proj: mat4x4<f32>,
};

struct GpuColorStop {
    offset: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

struct GpuGradient {
    // kind: 0 = linear, 1 = radial
    kind: u32,
    // spread: 0 = pad, 1 = reflect, 2 = repeat
    spread: u32,
    stop_count: u32,
    _pad: u32,
    // Linear: [start.x, start.y, end.x, end.y, 0, 0, 0, 0]
    // Radial: [center.x, center.y, radius, focal.x, focal.y, focal_radius, 0, 0]
    p0: vec4<f32>,
    p1: vec4<f32>,
    // Inverse gradient transform as two vec4s:
    // inv0 = [a, b, tx, 0]
    // inv1 = [c, d, ty, 0]
    inv0: vec4<f32>,
    inv1: vec4<f32>,
    // Up to 8 color stops.
    stop0: GpuColorStop,
    stop1: GpuColorStop,
    stop2: GpuColorStop,
    stop3: GpuColorStop,
    stop4: GpuColorStop,
    stop5: GpuColorStop,
    stop6: GpuColorStop,
    stop7: GpuColorStop,
};

struct GpuPattern {
    // Inverse pattern transform: same layout as gradient inv0/inv1.
    inv0: vec4<f32>,
    inv1: vec4<f32>,
    // Tile rect: [x, y, width, height]
    tile_rect: vec4<f32>,
    // Index into the texture array layer dimension.
    layer: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0)
var<uniform> globals: Globals;

@group(0) @binding(1)
var<storage, read> gradients: array<GpuGradient>;

@group(0) @binding(2)
var<storage, read> patterns: array<GpuPattern>;

@group(0) @binding(3)
var pattern_texture: texture_2d_array<f32>;

@group(0) @binding(4)
var pattern_sampler: sampler;

// ── Vertex shader ───────────────────────────────────────────────────────

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = globals.view_proj * vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    out.world_pos = in.world_pos;
    out.gradient_index = in.gradient_index;
    out.pattern_index = in.pattern_index;
    return out;
}

// ── Gradient helpers ────────────────────────────────────────────────────

fn get_stop(grad: GpuGradient, i: u32) -> GpuColorStop {
    // WGSL doesn't allow runtime indexing of named struct fields,
    // so we use a switch.
    switch i {
        case 0u: { return grad.stop0; }
        case 1u: { return grad.stop1; }
        case 2u: { return grad.stop2; }
        case 3u: { return grad.stop3; }
        case 4u: { return grad.stop4; }
        case 5u: { return grad.stop5; }
        case 6u: { return grad.stop6; }
        case 7u: { return grad.stop7; }
        default: { return grad.stop0; }
    }
}

fn apply_spread(t: f32, spread: u32) -> f32 {
    switch spread {
        // Pad: clamp to [0, 1]
        case 0u: { return clamp(t, 0.0, 1.0); }
        // Reflect: triangle wave with period 2
        case 1u: {
            let u = t - 2.0 * floor(t / 2.0); // mod 2
            if u > 1.0 {
                return 2.0 - u;
            }
            return u;
        }
        // Repeat: sawtooth with period 1
        case 2u: { return t - floor(t); }
        default: { return clamp(t, 0.0, 1.0); }
    }
}

fn sample_gradient(grad: GpuGradient, t: f32) -> vec4<f32> {
    let n = grad.stop_count;
    if n == 0u {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let first = get_stop(grad, 0u);
    if n == 1u || t <= first.offset {
        return vec4<f32>(first.r, first.g, first.b, first.a);
    }

    let last = get_stop(grad, n - 1u);
    if t >= last.offset {
        return vec4<f32>(last.r, last.g, last.b, last.a);
    }

    // Find the two bracketing stops and interpolate.
    var prev = first;
    for (var i = 1u; i < n; i = i + 1u) {
        let curr = get_stop(grad, i);
        if t <= curr.offset {
            let range = curr.offset - prev.offset;
            var f = 0.0;
            if range > 0.0 {
                f = (t - prev.offset) / range;
            }
            return vec4<f32>(
                mix(prev.r, curr.r, f),
                mix(prev.g, curr.g, f),
                mix(prev.b, curr.b, f),
                mix(prev.a, curr.a, f),
            );
        }
        prev = curr;
    }

    // Fallback (shouldn't reach here).
    return vec4<f32>(last.r, last.g, last.b, last.a);
}

// ── Fragment shader ─────────────────────────────────────────────────────

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // ── Pattern path ─────────────────────────────────────────────────
    if in.pattern_index >= 0 {
        let pat = patterns[in.pattern_index];

        // Transform world_pos into pattern space via inverse patternTransform.
        let px = pat.inv0.x * in.world_pos.x + pat.inv0.y * in.world_pos.y + pat.inv0.z;
        let py = pat.inv1.x * in.world_pos.x + pat.inv1.y * in.world_pos.y + pat.inv1.z;

        // Map into tile UV space: (pos - tile_origin) / tile_size, then wrap.
        let u = (px - pat.tile_rect.x) / pat.tile_rect.z;
        let v = (py - pat.tile_rect.y) / pat.tile_rect.w;

        // Tile via fract (repeat).
        let uv = fract(vec2<f32>(u, v));

        return textureSample(pattern_texture, pattern_sampler, uv, pat.layer);
    }

    // ── Solid color path ─────────────────────────────────────────────
    if in.gradient_index < 0 {
        return in.color;
    }

    // ── Gradient path ────────────────────────────────────────────────
    let grad = gradients[in.gradient_index];

    // Apply inverse gradient transform to world position.
    let px = grad.inv0.x * in.world_pos.x + grad.inv0.y * in.world_pos.y + grad.inv0.z;
    let py = grad.inv1.x * in.world_pos.x + grad.inv1.y * in.world_pos.y + grad.inv1.z;
    let p = vec2<f32>(px, py);

    var t: f32;
    if grad.kind == 0u {
        // Linear gradient: t = dot(p - start, end - start) / |end - start|^2
        let start = grad.p0.xy;
        let end = grad.p0.zw;
        let d = end - start;
        let len_sq = dot(d, d);
        if len_sq < 1e-12 {
            t = 0.0;
        } else {
            t = dot(p - start, d) / len_sq;
        }
    } else {
        // Radial gradient: two-circle interpolation.
        // Inner circle: (focal, focal_radius), outer circle: (center, radius).
        let center = grad.p0.xy;
        let radius = grad.p0.z;
        let focal = grad.p1.xy;
        let focal_radius = grad.p1.z;

        // Vector from focal to p
        let dp = p - focal;
        let dc = center - focal;
        let dr = radius - focal_radius;

        // Solve for t: |focal + t*dc - p|^2 = (focal_radius + t*dr)^2
        // This reduces to: A*t^2 + B*t + C = 0
        let a_coef = dot(dc, dc) - dr * dr;
        let b_coef = 2.0 * (dot(dp, dc) + focal_radius * dr);
        let c_coef = dot(dp, dp) - focal_radius * focal_radius;

        if abs(a_coef) < 1e-12 {
            // Degenerate to linear: B*t + C = 0
            if abs(b_coef) < 1e-12 {
                t = 0.0;
            } else {
                t = -c_coef / b_coef;
            }
        } else {
            let disc = b_coef * b_coef - 4.0 * a_coef * c_coef;
            if disc < 0.0 {
                t = 0.0;
            } else {
                let sq = sqrt(disc);
                // We want the largest positive t.
                let t1 = (-b_coef + sq) / (2.0 * a_coef);
                let t2 = (-b_coef - sq) / (2.0 * a_coef);
                t = max(t1, t2);
            }
        }
    }

    t = apply_spread(t, grad.spread);
    return sample_gradient(grad, t);
}
