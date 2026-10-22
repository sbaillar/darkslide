// DEVELOP 6: sharpening + output transform (Rec.2020 → sRGB/P3, soft gamut compression,
// transfer function) + quantization, and optionally the display histogram of the output
// (workgroup-local atomic bins, flushed once per workgroup). OUT_T/OUT_PACK are
// substituted per output format (8-bit, 16-bit, and extended-range linear half float).
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<storage, read> enc: array<EncT>;
// `blur` is either the pre-blurred luma plane or (inline radius > 0) the luma plane itself,
// blurred here with a small 2-D Gaussian so no extra passes are needed.
@group(0) @binding(2) var<storage, read> blur: array<f32>;
@group(0) @binding(3) var<storage, read_write> outb: array<OUT_T>;
// R, G, B, L bins (256 each), clipped low R/G/B, clipped high R/G/B, clipped shadows,
// clipped highlights, total (mirrors Histogram::GPU_WORDS).
@group(0) @binding(4) var<storage, read_write> hist: array<atomic<u32>>;

const HIST_N: u32 = 1033u;
//HIST_WG

// Output-space linear value of output pixel `gid` (inside the output).
fn shade(gid: vec2<u32>, ow: u32) -> vec3<f32> {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    // Only the output window of the developed image is written (region renders develop
    // with an apron of context around it).
    let win = dev.v[D_CROP];
    let ups = dev.v[D_UPS];
    let up = ups.z > 0.5;
    let px = gid.x + u32(win.x);
    let py = gid.y + u32(win.y);
    let i = py * w + px;
    var p: vec4<f32>;
    // Developed position this output pixel shows (grain is fixed to the image).
    var gpos = vec2<f32>(f32(px) + 0.5, f32(py) + 0.5);
    if (up) {
        // Draft: bilinear upscale of the developed window (mirrors cpu::upscale_bilinear).
        let fx = clamp((f32(gid.x) + 0.5) * win.z / ups.x - 0.5, 0.0, win.z - 1.0);
        let fy = clamp((f32(gid.y) + 0.5) * win.w / ups.y - 0.5, 0.0, win.w - 1.0);
        let x0 = u32(fx);
        let y0 = u32(fy);
        let x1 = min(x0 + 1u, u32(win.z) - 1u);
        let y1 = min(y0 + 1u, u32(win.w) - 1u);
        let bx = u32(win.x);
        let by = u32(win.y);
        let a = enc_get(enc[(by + y0) * w + bx + x0]);
        let b = enc_get(enc[(by + y0) * w + bx + x1]);
        let c = enc_get(enc[(by + y1) * w + bx + x0]);
        let d = enc_get(enc[(by + y1) * w + bx + x1]);
        let tx = fx - f32(x0);
        let ty = fy - f32(y0);
        let top = a + (b - a) * tx;
        let bot = c + (d - c) * tx;
        p = top + (bot - top) * ty;
        gpos = vec2<f32>(fx + win.x + 0.5, fy + win.y + 0.5);
    } else {
        p = enc_get(enc[i]);
    }
    var e = p.xyz;
    let sh = dev.v[D_SH];
    if (sh.z > 0.5 && !up) {
        var b: f32;
        let r = i32(sh.w);
        if (r > 0) {
            let k = dev.v[D_SHK];
            b = 0.0;
            for (var dy = -r; dy <= r; dy++) {
                let yy = u32(clamp(i32(py) + dy, 0, i32(h) - 1));
                var row = 0.0;
                for (var dx = -r; dx <= r; dx++) {
                    let xx = u32(clamp(i32(px) + dx, 0, i32(w) - 1));
                    row += k[u32(abs(dx))] * blur[yy * w + xx];
                }
                b += k[u32(abs(dy))] * row;
            }
        } else {
            b = blur[i];
        }
        e = e + vec3<f32>(sh.x * sharpen_thr(p.w - b, sh.y));
    }
    let gr = dev.v[D_GRAIN];
    if (gr.w > 0.5) {
        let iso = dev.v[D_ISO];
        let fp = vec2<f32>(row_apply(dev.v[D_NORM0], gpos) * iso.x * iso.z,
                           row_apply(dev.v[D_NORM1], gpos) * iso.y * iso.z);
        e = grain_add(e, fp, gr);
    }
    let lin0 = mat_rows(dev.v[D_OUT0], dev.v[D_OUT1], dev.v[D_OUT2], dec3(e));
    // NaN never reaches the quantizer (the CPU's saturating casts map NaN to 0 too).
    return select(lin0, vec3<f32>(0.0), lin0 != lin0);
}

@compute @workgroup_size(16, 16)
fn main(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) lid: u32,
) {
    let win = dev.v[D_CROP];
    let ups = dev.v[D_UPS];
    let up = ups.z > 0.5;
    let ow = select(u32(win.z), u32(ups.x), up);
    let oh = select(u32(win.w), u32(ups.y), up);
    let inside = gid.x < ow && gid.y < oh;
    var codes = vec3<u32>(0u);
    if (inside) {
        let lin = shade(gid.xy, ow);
        // Into the output gamut, once: resized exports hold gamut-mapped values already
        // (D_OUTF.x), and the compression isn't idempotent.
        let g = select(gamut(lin), max(lin, vec3<f32>(0.0)), dev.v[D_OUTF].x > 0.5);
        // 8-bit display codes (the 8-bit output itself: PACK8 is `q`).
        let q = pack4x8unorm(vec4<f32>(enc3(min(g, vec3<f32>(1.0))), 1.0));
        outb[gid.y * ow + gid.x] = OUT_PACK;
        codes = vec3<u32>(q & 255u, (q >> 8u) & 255u, (q >> 16u) & 255u);
    }
    //HIST_BODY
}
