// RCD 0: normalized, white-balanced CFA plane.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read_write> cfa: array<f32>;
@group(0) @binding(2) var<storage, read> raw: array<u32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let si = (u32(u.v[8].y) + gid.y) * u32(u.v[0].z) + u32(u.v[8].x) + gid.x;
    let word = raw[si >> 1u];
    let s = f32(select(word & 0xffffu, word >> 16u, (si & 1u) == 1u));
    let site = ((gid.y & 1u) << 1u) | (gid.x & 1u);
    let c = u32(u.v[1][site]);
    cfa[i] = clamp((s - u.v[2][site]) * u.v[3][site], 0.0, 1.0) * u.v[4][c];
}
