// Self-guided chroma coefficients: (a_cb, b_cb, a_cr, b_cr) from (cb, cb², cr, cr²)
// means. u.v[0] = w, h, eps.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> m0: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> m1: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> dst: array<vec4<f32>>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let m = m0[i];
    let eps = u.v[0].z;
    let vb = max(m.y - m.x * m.x, 0.0);
    let vr = max(m.w - m.z * m.z, 0.0);
    let ab = vb / (vb + eps);
    let ar = vr / (vr + eps);
    // `m1` is bound for layout compatibility only.
    let unused = m1[i].x * 0.0;
    dst[i] = vec4<f32>(ab, m.x - ab * m.x + unused, ar, m.z - ar * m.z);
}
