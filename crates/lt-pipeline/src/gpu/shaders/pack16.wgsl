// Pack RGBA16 (two u32 per pixel: R|G<<16, B|A<<16) into RGB16 rows: 6 bytes per pixel,
// rows padded to 4 bytes. One invocation per pixel pair writes three whole words.
// u.v[0] = w, h, row pitch in words.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<vec2<u32>>;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    let pitch = u32(u.v[0].z);
    let x = 2u * gid.x;
    if (x >= w || gid.y >= h) {
        return;
    }
    let a = src[gid.y * w + x];
    let o = gid.y * pitch + 3u * gid.x;
    dst[o] = a.x;
    if (x + 1u < w) {
        let b = src[gid.y * w + x + 1u];
        dst[o + 1u] = (a.y & 0xffffu) | (b.x << 16u);
        dst[o + 2u] = (b.x >> 16u) | (b.y << 16u);
    } else {
        dst[o + 1u] = a.y & 0xffffu;
    }
}
