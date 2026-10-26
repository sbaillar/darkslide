// Shared helpers. Mirrors the Rust reference implementations 1:1 (see color.rs, tone.rs,
// color_edit.rs, detail.rs, mask.rs). Constants such as LUMA_W, M_LMS, M_OK2, M_OK2_INV,
// M_LMS_INV, GAMUT_THR, GAMUT_S and MID_LOG are generated from Rust and prepended.

fn ss(e0: f32, e1: f32, x: f32) -> f32 {
    if (e1 <= e0) {
        return select(1.0, 0.0, x < e0);
    }
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn luma3(c: vec3<f32>) -> f32 {
    return dot(c, LUMA_W);
}

fn srgb_enc1(x: f32) -> f32 {
    let a = abs(x);
    var e: f32;
    if (a <= 0.0031308) {
        e = a * 12.92;
    } else {
        e = 1.055 * pow(a, 1.0 / 2.4) - 0.055;
    }
    return select(e, -e, x < 0.0);
}

fn srgb_dec1(y: f32) -> f32 {
    let a = abs(y);
    var l: f32;
    if (a <= 0.04045) {
        l = a / 12.92;
    } else {
        l = pow((a + 0.055) / 1.055, 2.4);
    }
    return select(l, -l, y < 0.0);
}

fn enc3(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(srgb_enc1(c.x), srgb_enc1(c.y), srgb_enc1(c.z));
}

fn dec3(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(srgb_dec1(c.x), srgb_dec1(c.y), srgb_dec1(c.z));
}

fn gamut(c: vec3<f32>) -> vec3<f32> {
    let ach = max(c.x, max(c.y, c.z));
    if (ach <= 1e-10) {
        return max(c, vec3<f32>(0.0));
    }
    var o = c;
    for (var k = 0u; k < 3u; k++) {
        let d = (ach - o[k]) / ach;
        if (d > GAMUT_THR) {
            let x = d - GAMUT_THR;
            let cd = GAMUT_THR + x / (1.0 + x / GAMUT_S);
            o[k] = ach - cd * ach;
        }
    }
    return max(o, vec3<f32>(0.0));
}

fn cbrt1(x: f32) -> f32 {
    return sign(x) * pow(abs(x), 1.0 / 3.0);
}

fn wrap360(h: f32) -> f32 {
    return h - 360.0 * floor(h / 360.0);
}

fn enc_to_oklch(e: vec3<f32>) -> vec3<f32> {
    let lms = M_LMS * dec3(e);
    let lab = M_OK2 * vec3<f32>(cbrt1(lms.x), cbrt1(lms.y), cbrt1(lms.z));
    return vec3<f32>(lab.x, length(lab.yz), wrap360(degrees(atan2(lab.z, lab.y))));
}

// OKLab (a, b) of an encoded pixel (mirrors mask::auto_ab).
fn auto_ab(e: vec3<f32>) -> vec2<f32> {
    let lch = enc_to_oklch(e);
    let h = radians(lch.z);
    return vec2<f32>(lch.y * cos(h), lch.y * sin(h));
}

fn oklch_to_enc(lch: vec3<f32>) -> vec3<f32> {
    let h = radians(lch.z);
    let lms = M_OK2_INV * vec3<f32>(lch.x, lch.y * cos(h), lch.y * sin(h));
    return enc3(M_LMS_INV * (lms * lms * lms));
}

fn range_weight(center: f32, half: f32, smooth_: f32, hue: f32, chroma: f32) -> f32 {
    var d = wrap360(hue - center);
    d = min(d, 360.0 - d);
    let lo = half * (1.0 - 0.5 * smooth_);
    let hi = half * (1.0 + 0.5 * smooth_);
    return (1.0 - ss(lo, hi, d)) * ss(0.005, 0.03, chroma);
}

fn contrast1(y: f32, k: f32, pivot: f32) -> f32 {
    let v = clamp(y, 0.0, 1.0);
    if (abs(k - 1.0) < 1e-6) {
        return v;
    }
    if (v < pivot) {
        return pivot * pow(v / pivot, k);
    }
    return 1.0 - (1.0 - pivot) * pow((1.0 - v) / (1.0 - pivot), k);
}

fn brightness1(y: f32, g: f32) -> f32 {
    return pow(clamp(y, 0.0, 1.0), g);
}

fn hdr_gain_ev(b_rel: f32, hl: f32, sh: f32) -> f32 {
    let w_sh = 1.0 - ss(-6.0, 0.0, b_rel);
    let w_hi = ss(-1.0, 3.0, b_rel);
    return 1.5 * sh * w_sh - 1.5 * hl * w_hi;
}

fn balance(e: vec3<f32>, m: vec3<f32>, s: vec3<f32>, mid: vec3<f32>, h: vec3<f32>) -> vec3<f32> {
    let y = clamp(luma3(e), 0.0, 1.0);
    let ws = (1.0 - y) * (1.0 - y);
    let wh = y * y;
    let wm = 2.0 * y * (1.0 - y);
    return e + m + ws * s + wm * mid + wh * h;
}

fn saturate3(e: vec3<f32>, f: f32) -> vec3<f32> {
    let y = luma3(e);
    return vec3<f32>(y) + (e - vec3<f32>(y)) * f;
}

// Vibrance (mirrors color_edit::apply_vibrance).
fn vibrance3(e: vec3<f32>, k: f32) -> vec3<f32> {
    if (k == 0.0) {
        return e;
    }
    let mx = max(e.x, max(e.y, e.z));
    let mn = min(e.x, min(e.y, e.z));
    var s = 0.0;
    if (mx > 1e-6) {
        s = clamp((mx - mn) / mx, 0.0, 1.0);
    }
    var w = (1.0 - s) * (1.0 - s);
    if (k > 0.0 && e.x >= e.y && e.y >= e.z && e.x > e.z) {
        let t = (e.y - e.z) / (e.x - e.z);
        let skin = ss(0.1, 0.25, t) * (1.0 - ss(0.55, 0.7, t));
        w *= 1.0 - 0.7 * skin;
    }
    return saturate3(e, 1.0 + k * w);
}

// Dark channel relative to the atmospheric light (mirrors detail::dark_channel).
fn dark_ch(p: vec3<f32>, a: vec3<f32>) -> f32 {
    let q = p / a;
    return clamp(min(q.x, min(q.y, q.z)), 0.0, 1.5);
}

// Luminance relative to the atmospheric light's (mirrors detail::haze_guide).
fn haze_guide(p: vec3<f32>, a: vec3<f32>) -> f32 {
    return luma3(p) / max(luma3(a), 1e-6);
}

fn dh_sat(v: vec3<f32>, l: f32) -> f32 {
    let d = abs(v / l - vec3<f32>(1.0));
    return max(d.x, max(d.y, d.z));
}

// Linear Rec.2020 -> OKLab, and back.
fn oklab_lin(l: vec3<f32>) -> vec3<f32> {
    let lms = M_LMS * l;
    return M_OK2 * vec3<f32>(cbrt1(lms.x), cbrt1(lms.y), cbrt1(lms.z));
}

fn lin_oklab(lab: vec3<f32>) -> vec3<f32> {
    let q = M_OK2_INV * lab;
    return M_LMS_INV * (q * q * q);
}

// Smoothness and edge-aware mean of a pixel's neighbourhood (mirrors detail::haze_local).
fn haze_local(sa0: f32, sb: f32, g: f32) -> vec2<f32> {
    let sa = clamp(sa0, 0.0, 1.0);
    let cv = sqrt(DH_LC_EPS * sa * (1.0 - sa)) / max(sb, 1e-3);
    return vec2<f32>(1.0 - ss(DH_CV.x, DH_CV.y, cv), sa * g + sb);
}

// Dehaze one linear pixel (mirrors detail::dehaze_px).
fn dehaze3(p: vec3<f32>, hz: vec3<f32>, a: vec3<f32>, k: f32) -> vec3<f32> {
    let haze = clamp(hz.x, 0.0, 1.0);
    if (k <= 0.0) {
        return p + (a - p) * (-k * (0.1 + 0.3 * haze));
    }
    let y = luma3(p);
    let ya = max(luma3(a), 1e-6);
    if (y <= 1e-9) {
        return p;
    }
    let yn = y / ya;
    let dy = abs(yn - 1.0);
    var mean = yn;
    if (hz.z > 1e-4) {
        mean = hz.z;
    }
    var t = max(max(1.0 - DH_OMEGA * k * haze, 1.0 - (DH_FLOOR.x + DH_FLOOR.y * haze) * k), min(DH_BRIGHT / max(dy, 1e-6), 1.0));
    t = max(t, 1.0 - DH_VEIL * mean);
    let td = abs(p / max(a, vec3<f32>(1e-6)) / yn - vec3<f32>(1.0));
    let tint = max(td.x, max(td.y, td.z));
    var protect = 0.0;
    let bright = ss(DH_PY.x, DH_PY.y, yn) * clamp(hz.y, 0.0, 1.0);
    if (bright > 0.0) {
        protect = bright * (1.0 - ss(DH_PT.x, DH_PT.y, tint));
    }
    t = t + (1.0 - t) * protect;
    var yj = max((y - ya) / t + ya, DH_KEEP * y);
    if (hz.z > 1e-4) {
        let e = DH_LC * k * (0.3 + 0.7 * haze) * (1.0 - protect);
        yj *= pow(clamp(yn / hz.z, 0.25, 4.0), e);
    }
    let full = (p - a) / t + a;
    let yf = max(luma3(full), 1e-6);
    let s0 = dh_sat(p, y);
    let s1 = dh_sat(full, yf);
    var g = 1.0;
    if (s0 > 1e-6) {
        g = clamp(s1 / s0, 1.0, 1.0 + DH_SAT * k);
    }
    let j = p * (yj / y);
    if (g <= 1.0) {
        return j;
    }
    let lab = oklab_lin(j);
    let gv = 1.0 + (g - 1.0) * (1.0 - ss(DH_VIVID.x, DH_VIVID.y, length(lab.yz)));
    let out = lin_oklab(vec3<f32>(lab.x, lab.yz * gv));
    let s1v = M_709 * j;
    let sgv = M_709 * out;
    let v1 = min(s1v.x, min(s1v.y, s1v.z));
    let vg = min(sgv.x, min(sgv.y, sgv.z));
    if (vg >= 0.0) {
        return out;
    }
    if (v1 > 0.0) {
        return lin_oklab(vec3<f32>(lab.x, lab.yz * (1.0 + (gv - 1.0) * v1 / (v1 - vg))));
    }
    return j;
}

// Blacks toe (mirrors tone::blacks_toe).
fn blacks_toe(y0: f32, blacks: f32) -> f32 {
    let y = clamp(y0, 0.0, 1.0);
    let t = 1.0 - y;
    let t2 = t * t;
    return clamp(y + 0.08 * (blacks / 100.0) * t2 * t2 * t2, 0.0, 1.0);
}

fn midtone_weight(y: f32) -> f32 {
    let t = abs(2.0 * clamp(y, 0.0, 1.0) - 1.0);
    return 1.0 - t * t * t * t;
}

fn soft_detail(d: f32) -> f32 {
    let t = d / DETAIL_KNEE;
    return d / (1.0 + t * t);
}

fn garrote(d: f32, t: f32) -> f32 {
    if (t <= 0.0) {
        return d;
    }
    if (abs(d) <= t) {
        return 0.0;
    }
    return d - t * t / d;
}

// Texture delta without the midtone weight (mirrors detail::texture_gain).
fn texture_gain(det: f32, k: f32, noise: f32) -> f32 {
    if (k == 0.0) {
        return 0.0;
    }
    var d = det;
    var g = k;
    if (k > 0.0) {
        d = sharpen_thr(det, noise);
        g = k * TEXTURE_BOOST;
    }
    let t = d / TEXTURE_KNEE;
    return g * d / (1.0 + t * t);
}

// Layer sharpening (mirrors detail::sharpen_delta).
fn sharpen_delta(fine: f32, k: f32, noise: f32) -> f32 {
    if (k > 0.0) {
        return 1.5 * k * sharpen_thr(fine, noise);
    }
    return k * fine;
}

fn sharpen_thr(d: f32, t: f32) -> f32 {
    if (t <= 0.0) {
        return d;
    }
    let w = min(abs(d) / t, 1.0);
    return d * w * w;
}

fn from_luma_chroma(y: f32, cb: f32, cr: f32) -> vec3<f32> {
    let r = y + cr;
    let b = y + cb;
    let g = (y - LUMA_W.x * r - LUMA_W.z * b) / LUMA_W.y;
    return vec3<f32>(r, g, b);
}

fn unpack_rgb(p: vec2<u32>) -> vec3<f32> {
    let a = unpack2x16float(p.x);
    let b = unpack2x16float(p.y);
    return vec3<f32>(a.x, a.y, b.x);
}

fn pack_rgb(c: vec3<f32>) -> vec2<u32> {
    return vec2<u32>(pack2x16float(c.xy), pack2x16float(vec2<f32>(c.z, 0.0)));
}

// Bilinear sample position on a low-res grid of block size `s` for full-res pixel (x, y).
struct UpPos {
    i00: u32,
    i10: u32,
    i01: u32,
    i11: u32,
    t: vec2<f32>,
}

fn up_pos(x: u32, y: u32, s: f32, lw: u32, lh: u32) -> UpPos {
    let fx = clamp((f32(x) + 0.5) / s - 0.5, 0.0, f32(lw - 1u));
    let fy = clamp((f32(y) + 0.5) / s - 0.5, 0.0, f32(lh - 1u));
    let x0 = u32(fx);
    let y0 = u32(fy);
    let x1 = min(x0 + 1u, lw - 1u);
    let y1 = min(y0 + 1u, lh - 1u);
    var p: UpPos;
    p.i00 = y0 * lw + x0;
    p.i10 = y0 * lw + x1;
    p.i01 = y1 * lw + x0;
    p.i11 = y1 * lw + x1;
    p.t = vec2<f32>(fx - f32(x0), fy - f32(y0));
    return p;
}

fn bilerp4(a: vec4<f32>, b: vec4<f32>, c: vec4<f32>, d: vec4<f32>, t: vec2<f32>) -> vec4<f32> {
    let top = a + (b - a) * t.x;
    let bot = c + (d - c) * t.x;
    return top + (bot - top) * t.y;
}

fn bilerp1(a: f32, b: f32, c: f32, d: f32, t: vec2<f32>) -> f32 {
    let top = a + (b - a) * t.x;
    let bot = c + (d - c) * t.x;
    return top + (bot - top) * t.y;
}

// 16-bit fixed point over [-0.5, 1.5], 4 channels in 2 words (the packing of the f16-class
// develop image, ENC_H): export stripes and resize intermediates.
fn u16x4_put(c: vec4<f32>) -> vec2<u32> {
    let q = (c + 0.5) * 0.5;
    return vec2<u32>(pack2x16unorm(q.xy), pack2x16unorm(q.zw));
}

fn u16x4_get(v: vec2<u32>) -> vec4<f32> {
    return vec4<f32>(unpack2x16unorm(v.x), unpack2x16unorm(v.y)) * 2.0 - 0.5;
}

// Film grain (mirrors effects::lattice / value_noise / grain_px).
fn grain_lattice(ix: i32, iy: i32, seed: u32) -> f32 {
    var h = (bitcast<u32>(ix) * 0x8da6b343u) ^ (bitcast<u32>(iy) * 0xd8163841u) ^ (seed * 0xcb1ab31fu);
    h ^= h >> 16u;
    h *= 0x7feb352du;
    h ^= h >> 15u;
    h *= 0x846ca68bu;
    h ^= h >> 16u;
    let a = f32(h & 0xffffu) / 65535.0;
    let b = f32(h >> 16u) / 65535.0;
    return a + b - 1.0;
}

fn grain_noise(x: f32, y: f32, seed: u32) -> f32 {
    let fx = floor(x);
    let fy = floor(y);
    let tx = x - fx;
    let ty = y - fy;
    let sx = tx * tx * (3.0 - 2.0 * tx);
    let sy = ty * ty * (3.0 - 2.0 * ty);
    let ix = i32(fx);
    let iy = i32(fy);
    let a = grain_lattice(ix, iy, seed);
    let b = grain_lattice(ix + 1, iy, seed);
    let c = grain_lattice(ix, iy + 1, seed);
    let d = grain_lattice(ix + 1, iy + 1, seed);
    let top = a + (b - a) * sx;
    let bot = c + (d - c) * sx;
    return top + (bot - top) * sy;
}

// `g` = amplitude, cell (px), roughness, on; `p` = frame position (px).
fn grain_add(e: vec3<f32>, p: vec2<f32>, g: vec4<f32>) -> vec3<f32> {
    let x = p.x / g.y;
    let y = p.y / g.y;
    let n = (1.0 - g.z) * grain_noise(x, y, 1u) + g.z * grain_noise(x * 2.17 + 13.1, y * 2.17 + 7.3, 2u);
    let l = clamp(luma3(e), 0.0, 1.0);
    return e + vec3<f32>(g.x * n * (0.3 + 2.8 * l * (1.0 - l)));
}
