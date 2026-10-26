// BASE pyramid: 2x box downsample. u.v[0] = out w, out h, in w, in h.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<BaseT>;
@group(0) @binding(2) var<storage, read_write> outb: array<BaseT>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ow = u32(u.v[0].x);
    let oh = u32(u.v[0].y);
    let iw = u32(u.v[0].z);
    let ih = u32(u.v[0].w);
    if (gid.x >= ow || gid.y >= oh) {
        return;
    }
    let x0 = min(2u * gid.x, iw - 1u);
    let y0 = min(2u * gid.y, ih - 1u);
    let x1 = min(2u * gid.x + 1u, iw - 1u);
    let y1 = min(2u * gid.y + 1u, ih - 1u);
    let a = base_get(src[y0 * iw + x0]);
    let b = base_get(src[y0 * iw + x1]);
    let c = base_get(src[y1 * iw + x0]);
    let d = base_get(src[y1 * iw + x1]);
    outb[gid.y * ow + gid.x] = base_put(0.25 * (a + b + c + d));
}
