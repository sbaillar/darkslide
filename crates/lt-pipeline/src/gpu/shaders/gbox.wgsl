// Separable box blur of a vec4 grid with replicated edges. u.v[0] = w, h, r, dir (0 = h, 1 = v).
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> dst: array<vec4<f32>>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    let r = i32(u.v[0].z);
    let vertical = u.v[0].w > 0.5;
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    var acc = vec4<f32>(0.0);
    for (var d = -r; d <= r; d++) {
        if (vertical) {
            let yy = u32(clamp(i32(gid.y) + d, 0, i32(h) - 1));
            acc += src[yy * w + gid.x];
        } else {
            let xx = u32(clamp(i32(gid.x) + d, 0, i32(w) - 1));
            acc += src[gid.y * w + xx];
        }
    }
    dst[gid.y * w + gid.x] = acc * (1.0 / f32(2 * r + 1));
}
