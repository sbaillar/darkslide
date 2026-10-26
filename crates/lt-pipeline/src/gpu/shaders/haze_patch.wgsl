// Haze estimate, step 2 (mirrors local::guided_haze): the block dark channel min-filtered
// over 3×3 blocks (the patch) -> (mean g, mean g², patch, mean g · patch), ready for the
// box means of the cross guided filter. u.v[0] = lw, lh.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> dst: array<vec4<f32>>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    var m = 3.4e38;
    for (var y = max(gid.y, 1u) - 1u; y < min(gid.y + 2u, h); y++) {
        for (var x = max(gid.x, 1u) - 1u; x < min(gid.x + 2u, w); x++) {
            m = min(m, src[y * w + x].z);
        }
    }
    let c = src[gid.y * w + gid.x];
    dst[gid.y * w + gid.x] = vec4<f32>(c.x, c.y, m, c.x * m);
}
