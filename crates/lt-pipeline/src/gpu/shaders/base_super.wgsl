// BASE: 2x2 superpixel demosaic at the reference WB (camera RGB).
// u.v[0] = out w, out h, sensor w; [1] CFA color per site; [2] black per site;
// [3] 1/(white-black) per site; [4] reference WB mults.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> raw: array<u32>;
@group(0) @binding(2) var<storage, read_write> outb: array<BaseT>;

fn sample_raw(i: u32) -> f32 {
    let w = raw[i >> 1u];
    return f32(select(w & 0xffffu, w >> 16u, (i & 1u) == 1u));
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ow = u32(u.v[0].x);
    let oh = u32(u.v[0].y);
    let sw = u32(u.v[0].z);
    if (gid.x >= ow || gid.y >= oh) {
        return;
    }
    var acc = vec3<f32>(0.0);
    var n = vec3<f32>(0.0);
    for (var pos = 0u; pos < 4u; pos++) {
        let x = 2u * gid.x + (pos & 1u);
        let y = 2u * gid.y + (pos >> 1u);
        let c = u32(u.v[1][pos]);
        let v = clamp((sample_raw(y * sw + x) - u.v[2][pos]) * u.v[3][pos], 0.0, 1.0);
        acc[c] += v;
        n[c] += 1.0;
    }
    let mults = u.v[4].xyz;
    var cam = vec3<f32>(0.0);
    for (var c = 0u; c < 3u; c++) {
        if (n[c] > 0.0) {
            cam[c] = acc[c] / n[c] * mults[c];
        }
    }
    outb[gid.y * ow + gid.x] = base_put(cam);
}
