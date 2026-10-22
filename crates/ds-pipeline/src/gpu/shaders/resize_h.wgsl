// Export resize, horizontal pass: a stripe of the (output-transformed, encoded) full
// resolution image → linear light, area-correct Catmull-Rom (mirrors cpu::resize_linear),
// stored encoded. u.v[0] = stripe width, rows, dst width, destination row offset.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<vec2<u32>>;
@group(0) @binding(2) var<storage, read_write> tmp: array<vec2<u32>>;

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
    let w = u32(u.v[0].x);
    let rows = u32(u.v[0].y);
    let dw = u32(u.v[0].z);
    if (gid.x >= dw || gid.y >= rows) {
        return;
    }
    let ratio = f32(w) / f32(dw);
    let fscale = max(ratio, 1.0);
    let center = (f32(gid.x) + 0.5) * ratio;
    let lo = u32(max(floor(center - 2.0 * fscale), 0.0));
    let hi = min(u32(ceil(center + 2.0 * fscale)), w);
    let row = gid.y * w;
    var acc = vec3<f32>(0.0);
    var ws = 0.0;
    for (var i = lo; i < hi; i++) {
        let wv = catmull_rom((f32(i) + 0.5 - center) / fscale);
        acc += wv * dec3(u16x4_get(src[row + i]).xyz);
        ws += wv;
    }
    let lin = acc / select(ws, 1.0, abs(ws) < 1e-7);
    tmp[(gid.y + u32(u.v[0].w)) * dw + gid.x] = u16x4_put(vec4<f32>(enc3(lin), 0.0));
}
