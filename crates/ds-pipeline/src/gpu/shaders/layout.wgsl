// Uniform blocks are plain arrays of vec4 so Rust and WGSL agree without padding rules.
struct P {
    v: array<vec4<f32>, 64>,
}

// Develop uniform slots (see gpu/params.rs).
const D_SIZE: u32 = 0u;     // out_w, out_h, base_w, base_h (stored rectangle)
const D_HSM: u32 = 1u;      // profile hue/sat map: table offset (-1 = none), hue, sat, value divisions
const D_LOOK: u32 = 2u;     // profile look table: table offset (-1 = none), hue, sat, value divisions
const D_VIG0: u32 = 3u;     // output px -> vignette radius frame
const D_VIG1: u32 = 4u;
const D_EXPO: u32 = 5u;     // exposure gain, vignette amount EV, midpoint, on
const D_HDR: u32 = 6u;      // highlights, shadows, block size, global on
const D_HDRD: u32 = 7u;     // low w, low h, needs plane, _
const D_TAB0: u32 = 8u;     // base tone LUT, channel LUTs (-1 = none), _, luma LUT (-1 = none)
const D_TAB1: u32 = 9u;     // tone n, enc n, base fwd offset, base inv offset
const D_BM: u32 = 10u;      // balance master xyz, saturation
const D_BS: u32 = 11u;      // balance shadows xyz, range count
const D_BMID: u32 = 12u;    // balance midtones xyz, range offset
const D_BH: u32 = 13u;      // balance highlights xyz, balance on
const D_DET: u32 = 14u;     // clarity, structure, needs planes, global on
const D_DETC: u32 = 15u;    // clarity block size, low w, low h
const D_DETS: u32 = 16u;    // structure block size, low w, low h
const D_OUT0: u32 = 17u;    // working -> output matrix rows
const D_OUT1: u32 = 18u;
const D_OUT2: u32 = 19u;
const D_NR: u32 = 20u;      // t1, t2, chroma amount, block size
const D_NRD: u32 = 21u;     // low w, low h, luma on, chroma on
const D_SH: u32 = 22u;      // sharpen gain, threshold, on, inline radius (0 = blur plane given)
const D_NORM0: u32 = 23u;   // output px -> E-normalized affine
const D_NORM1: u32 = 24u;
const D_ISO: u32 = 25u;     // iso x, iso y, long edge px, log2(mid gray)
const D_PIV: u32 = 26u;     // base-curve pivot
const D_TOE0: u32 = 27u;    // output px -> corrected E px affine
const D_TOE1: u32 = 28u;
const D_KEY0: u32 = 29u;    // keystone homography rows (corrected -> pre-keystone E px)
const D_KEY1: u32 = 30u;
const D_KEY2: u32 = 31u;
const D_LENS: u32 = 32u;    // optical center x, y, 1/half short side, 1/half diagonal
const D_DIST: u32 = 33u;    // ptlens a, b, c, has lens
const D_TCAR: u32 = 34u;    // red TCA v, c, b, has keystone
const D_TCAB: u32 = 35u;    // blue TCA v, c, b
const D_VIGK: u32 = 36u;    // pa k1, k2, k3
const D_ETB0: u32 = 37u;    // E px -> level px affine
const D_ETB1: u32 = 38u;
const D_LVL: u32 = 39u;     // level w, level h, stored rect x0, y0
const D_WBR: u32 = 40u;     // develop WB ratio
const D_CLIP: u32 = 41u;    // clip level per channel (user multipliers)
const D_CAM0: u32 = 42u;    // camera -> working matrix rows (with baseline gain)
const D_CAM1: u32 = 43u;
const D_CAM2: u32 = 44u;
const D_SHK: u32 = 45u;     // inline sharpen half-kernel (center first, radius ≤ 3)
const D_OFF0: u32 = 46u;    // block-grid phase: HDR x, y, clarity x, y
const D_OFF1: u32 = 47u;    // block-grid phase: structure x, y, NR x, y
const D_CROP: u32 = 48u;    // output window x, y, w, h inside the developed image
const D_UPS: u32 = 49u;     // draft upscale: output w, h, on
const D_OUTF: u32 = 50u;    // output pass: values already gamut-mapped (resized exports)
const D_PRES: u32 = 51u;    // vibrance, texture, dehaze (global, -1..1), texture global on
const D_TEX: u32 = 52u;     // texture block size, low w, low h
const D_DHA: u32 = 53u;     // atmospheric light rgb, haze estimate needed
const D_DHG: u32 = 54u;     // dehaze block size, low w, low h
const D_OFF2: u32 = 55u;    // block-grid phase: texture x, y, dehaze x, y
const D_SPI0: u32 = 56u;    // E-normalized -> output px affine (inverse of D_NORM)
const D_SPI1: u32 = 57u;
const D_SPN: u32 = 58u;     // spots: count, bin tiles across, bin offsets / indices word offsets
const D_DETN: u32 = 59u;    // detail planes built: clarity, structure, texture
const D_FINE: u32 = 60u;    // layer sharpening 5-tap Gaussian (center, ±1, ±2), detail noise
const D_FINEG: u32 = 61u;   // layer sharpening detail gain
const D_PMAP: u32 = 62u;    // working <-> ProPhoto matrices offset, hue/sat sRGB value, look sRGB value, hue/sat value scale
const D_GRAIN: u32 = 63u;   // grain amplitude, cell px, roughness, on

// Layer uniform slots.
const L_A: u32 = 0u;        // opacity, retone, ev, highlights
const L_B: u32 = 1u;        // shadows, contrast k, brightness gamma, saturation
const L_WB0: u32 = 2u;      // WB matrix rows
const L_WB1: u32 = 3u;
const L_WB2: u32 = 4u;
const L_CUR: u32 = 5u;      // curve LUT offsets R, G, B, luma (-1 = none)
const L_C: u32 = 6u;        // curves on, range count, range offset, clarity
const L_D: u32 = 7u;        // structure, invert, luma range on, shape count
const L_LR: u32 = 8u;       // luma range low, high, low falloff, high falloff
const L_F: u32 = 9u;        // feather mode (0 none, 1 full-res, 2 low-res), block, low w, low h
const L_BM: u32 = 10u;      // balance master xyz, balance on
const L_BS: u32 = 11u;
const L_BMID: u32 = 12u;
const L_BH: u32 = 13u;
const L_E: u32 = 14u;       // shape offset (vec4 index)
const L_P: u32 = 15u;       // vibrance, texture (incl. softening), dehaze, sharpening
const L_WK: u32 = 16u;      // whites factor, blacks toe, mid gray
const L_CR: u32 = 17u;      // colour-range refinement: OK hue center, half width, smoothness, on

fn row_apply(r: vec4<f32>, p: vec2<f32>) -> f32 {
    return r.x * p.x + r.y * p.y + r.z;
}

fn mat_rows(a: vec4<f32>, b: vec4<f32>, c: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(a.xyz, v), dot(b.xyz, v), dot(c.xyz, v));
}
