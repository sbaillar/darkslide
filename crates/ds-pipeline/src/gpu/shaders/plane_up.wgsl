// Bilinear upsample of a low-res scalar plane (block f) to w × h (mirrors upsample_at).
// u.v[0] = w, h, f; [1] = lw, lh.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> low: array<f32>;
@group(0) @binding(2) var<storage, read_write> outp: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let up = up_pos(gid.x, gid.y, u.v[0].z, u32(u.v[1].x), u32(u.v[1].y));
    outp[gid.y * w + gid.x] = bilerp1(low[up.i00], low[up.i10], low[up.i01], low[up.i11], up.t);
}
