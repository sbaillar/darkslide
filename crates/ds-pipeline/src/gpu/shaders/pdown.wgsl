// Block mean of a scalar plane. u.v[0] = w, h, s; [1] = lw, lh.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> low: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    let s = u32(u.v[0].z);
    let lw = u32(u.v[1].x);
    let lh = u32(u.v[1].y);
    if (gid.x >= lw || gid.y >= lh) {
        return;
    }
    let x0 = gid.x * s;
    let y0 = gid.y * s;
    let x1 = min(x0 + s, w);
    let y1 = min(y0 + s, h);
    var a = 0.0;
    for (var y = y0; y < y1; y++) {
        for (var x = x0; x < x1; x++) {
            a += src[y * w + x];
        }
    }
    low[gid.y * lw + gid.x] = a / f32((y1 - y0) * (x1 - x0));
}
