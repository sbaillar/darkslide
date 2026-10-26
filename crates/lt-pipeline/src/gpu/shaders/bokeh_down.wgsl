// Lens blur 2: the gather grid (mirrors effects::blur_down): per cell the mean colour of
// its out-of-focus pixels, their weight × highlight gain, and their mean signed blur in
// cells. In-focus pixels weigh 0, so the sharp subject never enters the blur.
// u.v[0] = w, h, f, boost; [1] = lw, lh, phase x, y.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> lin: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> coc: array<f32>;
@group(0) @binding(3) var<storage, read_write> lo: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read_write> loz: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    let f = u32(u.v[0].z);
    let lw = u32(u.v[1].x);
    let lh = u32(u.v[1].y);
    if (gid.x >= lw || gid.y >= lh) {
        return;
    }
    let ox = u32(u.v[1].z);
    let oy = u32(u.v[1].w);
    let x0 = max(gid.x * f, ox) - ox;
    let y0 = max(gid.y * f, oy) - oy;
    let x1 = min((gid.x + 1u) * f - ox, w);
    let y1 = min((gid.y + 1u) * f - oy, h);
    var rgb = vec3<f32>(0.0);
    var bsum = 0.0;
    var zsum = 0.0;
    for (var y = y0; y < y1; y++) {
        for (var x = x0; x < x1; x++) {
            let i = y * w + x;
            let c = coc[i];
            let b = clamp(abs(c) - 0.5, 0.0, 1.0);
            rgb += b * lin[i].xyz;
            bsum += b;
            zsum += b * c;
        }
    }
    let o = gid.y * lw + gid.x;
    if (bsum <= 0.0) {
        lo[o] = vec4<f32>(0.0);
        loz[o] = 0.0;
        return;
    }
    let m = rgb / bsum;
    let n = f32((x1 - x0) * (y1 - y0));
    let t = clamp((luma3(m) - 0.7) / 1.3, 0.0, 1.0);
    let gain = 1.0 + 6.0 * u.v[0].w * t * t;
    lo[o] = vec4<f32>(m, bsum / n * gain);
    loz[o] = zsum / bsum / f32(f);
}
