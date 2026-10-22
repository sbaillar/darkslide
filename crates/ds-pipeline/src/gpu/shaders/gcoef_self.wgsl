// Self-guided coefficients from box means (mI, mII) -> (a, b). u.v[0] = w, h, eps.
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
    let i = gid.y * w + gid.x;
    let m = src[i];
    let var_ = max(m.y - m.x * m.x, 0.0);
    let a = var_ / (var_ + u.v[0].z);
    dst[i] = vec4<f32>(a, m.x - a * m.x, 0.0, 0.0);
}
