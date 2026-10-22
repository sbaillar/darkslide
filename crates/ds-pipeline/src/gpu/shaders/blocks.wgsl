// Auto tools: mean normalized camera RGB of each block × block mosaic block, skipping
// blocks with a clipped sample (mirrors auto::camera_blocks). Output .w = 1 when valid.
// u.v[0] = sensor w, h, block, blocks per row; u.v[1] = site colors; u.v[2] = black;
// u.v[3] = 1 / (white − black).
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> raw: array<u32>;
@group(0) @binding(2) var<storage, read_write> outb: array<vec4<f32>>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let sw = u32(u.v[0].x);
    let sh = u32(u.v[0].y);
    let block = u32(u.v[0].z);
    let bw = sw / block;
    let bh = sh / block;
    if (gid.x >= bw || gid.y >= bh) {
        return;
    }
    var acc = vec3<f32>(0.0);
    var n = vec3<f32>(0.0);
    var valid = 1.0;
    for (var y = gid.y * block; y < (gid.y + 1u) * block; y++) {
        for (var x = gid.x * block; x < (gid.x + 1u) * block; x++) {
            let si = y * sw + x;
            let word = raw[si >> 1u];
            let s = f32(select(word & 0xffffu, word >> 16u, (si & 1u) == 1u));
            let pos = ((y & 1u) << 1u) | (x & 1u);
            let v = (s - u.v[2][pos]) * u.v[3][pos];
            if (v >= 0.97) {
                valid = 0.0;
            }
            let c = u32(u.v[1][pos]);
            acc[c] += max(v, 0.0);
            n[c] += 1.0;
        }
    }
    outb[gid.y * bw + gid.x] = vec4<f32>(acc / max(n, vec3<f32>(1.0)), valid);
}
