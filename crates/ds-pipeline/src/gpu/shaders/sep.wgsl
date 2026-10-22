// Separable symmetric convolution of a scalar plane with replicated edges and optional
// dilation (à-trous). u.v[0] = w, h, radius, step; [1].x = dir (0 = h, 1 = v);
// [2..6] = half-kernel weights, center first.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;

fn weight(k: u32) -> f32 {
    return u.v[2u + k / 4u][k % 4u];
}

fn at(x: i32, y: i32, w: u32) -> f32 {
    return src[u32(y) * w + u32(x)];
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    let r = u32(u.v[0].z);
    let step = i32(u.v[0].w);
    let vertical = u.v[1].x > 0.5;
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    var acc = weight(0u) * at(x, y, w);
    for (var k = 1u; k <= r; k++) {
        let d = i32(k) * step;
        if (vertical) {
            acc += weight(k) * (at(x, max(y - d, 0), w) + at(x, min(y + d, i32(h) - 1), w));
        } else {
            acc += weight(k) * (at(max(x - d, 0), y, w) + at(min(x + d, i32(w) - 1), y, w));
        }
    }
    dst[gid.y * w + gid.x] = acc;
}
