// Glow / halation 2: separable Gaussian of a vec4 grid plane, replicated edges.
// u.v[0] = lw, lh, radius, dir (0 = h, 1 = v); [2..] = half-kernel weights, centre first.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> dst: array<vec4<f32>>;

fn weight(k: u32) -> f32 {
    return u.v[2u + k / 4u][k % 4u];
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = i32(u.v[0].x);
    let h = i32(u.v[0].y);
    let r = u32(u.v[0].z);
    let vertical = u.v[0].w > 0.5;
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= w || y >= h) {
        return;
    }
    var acc = weight(0u) * src[y * w + x];
    for (var k = 1u; k <= r; k++) {
        let d = i32(k);
        if (vertical) {
            acc += weight(k) * (src[max(y - d, 0) * w + x] + src[min(y + d, h - 1) * w + x]);
        } else {
            acc += weight(k) * (src[y * w + max(x - d, 0)] + src[y * w + min(x + d, w - 1)]);
        }
    }
    dst[y * w + x] = acc;
}
