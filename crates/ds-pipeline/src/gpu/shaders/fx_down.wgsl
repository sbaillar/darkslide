// Glow / halation 1: block means of the overlay's source on its grid (mirrors
// effects::overlay_plane). Mode 0: glow (colour clamped to 0..1); mode 1: halation (how
// far luma is into the highlights).
// u.v[0] = w, h, f, mode; [1] = lw, lh, phase x, y.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> enc: array<EncT>;
@group(0) @binding(2) var<storage, read_write> low: array<vec4<f32>>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    let f = u32(u.v[0].z);
    let lw = u32(u.v[1].x);
    let lh = u32(u.v[1].y);
    if (gid.x >= lw || gid.y >= lh) {
        return;
    }
    let ox = u32(u.v[1].z);
    let oy = u32(u.v[1].w);
    let x0 = max(gid.x * f, ox) - ox;
    let y0 = max(gid.y * f, oy) - oy;
    let x1 = min((gid.x + 1u) * f - ox, w);
    let y1 = min((gid.y + 1u) * f - oy, h);
    let halation = u.v[0].w > 0.5;
    var a = vec4<f32>(0.0);
    for (var y = y0; y < y1; y++) {
        for (var x = x0; x < x1; x++) {
            let e = enc_get(enc[y * w + x]).xyz;
            if (halation) {
                a.x += clamp((luma3(e) - 0.7) / 0.3, 0.0, 1.5);
            } else {
                a += vec4<f32>(clamp(e, vec3<f32>(0.0), vec3<f32>(1.0)), 0.0);
            }
        }
    }
    low[gid.y * lw + gid.x] = a / f32((y1 - y0) * (x1 - x0));
}
