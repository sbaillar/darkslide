// Haze estimate, step 1 (mirrors local::guided_haze): per block, the means of the guide
// g = haze_guide(lin, A) and g², and the dark channel of the block's mean colour.
// u.v[0] = w, h, s; [1] = lw, lh, phase x, y; [2] = atmospheric light rgb.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> lin: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> low: array<vec4<f32>>;

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
    let a = u.v[2].xyz;
    var m = 0.0;
    var q = 0.0;
    var c = vec3<f32>(0.0);
    for (var y = y0; y < y1; y++) {
        for (var x = x0; x < x1; x++) {
            let p = lin[y * w + x].xyz;
            let g = haze_guide(p, a);
            m += g;
            q += g * g;
            c += p;
        }
    }
    let n = f32((y1 - y0) * (x1 - x0));
    low[gid.y * lw + gid.x] = vec4<f32>(m / n, q / n, dark_ch(c / n, a), 0.0);
}
