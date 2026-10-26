// Export resize, vertical pass: encoded intermediate → linear, area-correct Catmull-Rom →
// encoded image (rgb + luma) and its luma plane at the target size (for output
// sharpening). u.v[0] = source height, dst w, dst h.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> tmp: array<vec2<u32>>;
@group(0) @binding(2) var<storage, read_write> outb: array<vec2<u32>>;
@group(0) @binding(3) var<storage, read_write> ypl: array<f32>;

fn catmull_rom(t: f32) -> f32 {
    let x = abs(t);
    if (x < 1.0) {
        return 1.5 * x * x * x - 2.5 * x * x + 1.0;
    }
    if (x < 2.0) {
        return -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0;
    }
    return 0.0;
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let sh = u.v[0].x;
    let dw = u32(u.v[0].y);
    let dh = u32(u.v[0].z);
    if (gid.x >= dw || gid.y >= dh) {
        return;
    }
    let ratio = sh / f32(dh);
    let fscale = max(ratio, 1.0);
    let center = (f32(gid.y) + 0.5) * ratio;
    let lo = u32(max(floor(center - 2.0 * fscale), 0.0));
    let hi = min(u32(ceil(center + 2.0 * fscale)), u32(sh));
    var acc = vec3<f32>(0.0);
    var ws = 0.0;
    for (var i = lo; i < hi; i++) {
        let wv = catmull_rom((f32(i) + 0.5 - center) / fscale);
        acc += wv * dec3(u16x4_get(tmp[i * dw + gid.x]).xyz);
        ws += wv;
    }
    let e = enc3(acc / select(ws, 1.0, abs(ws) < 1e-7));
    let l = dot(LUMA_W, e);
    let i = gid.y * dw + gid.x;
    outb[i] = u16x4_put(vec4<f32>(e, l));
    ypl[i] = l;
}
