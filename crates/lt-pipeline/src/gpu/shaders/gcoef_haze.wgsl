// Haze estimate, step 3 (mirrors local::guided_haze): cross guided-filter coefficients
// (a, b) and the guide's self-guided coefficients (detail::haze_local), from the box
// means (g, g², p, g·p).
// u.v[0] = w, h, eps.
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
    let a = (m.w - m.x * m.z) / (var_ + u.v[0].z);
    let sa = var_ / (var_ + DH_LC_EPS);
    dst[i] = vec4<f32>(a, m.z - a * m.x, sa, m.x * (1.0 - sa));
}
