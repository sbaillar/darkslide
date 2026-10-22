// Colour-range refinement (mirrors local::guided_cross): block means
// of (g, g², w, g·w) with g the refinement guide (mask::cr_guide) and w the weight.
// u.v[0] = w, h, s; [1] = lw, lh, phase x, y.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> guide: array<f32>;
@group(0) @binding(2) var<storage, read> weight: array<f32>;
@group(0) @binding(3) var<storage, read_write> low: array<vec4<f32>>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    let s = u32(u.v[0].z);
    let lw = u32(u.v[1].x);
    let lh = u32(u.v[1].y);
    if (gid.x >= lw || gid.y >= lh) {
        return;
    }
    let ox = u32(u.v[1].z);
    let oy = u32(u.v[1].w);
    let x0 = max(gid.x * s, ox) - ox;
    let y0 = max(gid.y * s, oy) - oy;
    let x1 = min((gid.x + 1u) * s - ox, w);
    let y1 = min((gid.y + 1u) * s - oy, h);
    var m = vec4<f32>(0.0);
    for (var y = y0; y < y1; y++) {
        for (var x = x0; x < x1; x++) {
            let i = y * w + x;
            let g = guide[i];
            let p = weight[i];
            m += vec4<f32>(g, g * g, p, g * p);
        }
    }
    let n = f32((y1 - y0) * (x1 - x0));
    low[gid.y * lw + gid.x] = m / n;
}
