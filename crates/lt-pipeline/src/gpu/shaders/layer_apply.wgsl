// DEVELOP 4: one adjustment layer, blended by its (feathered) mask × opacity. In place on enc.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<uniform> lay: P;
@group(0) @binding(2) var<storage, read> maskp: array<f32>;
@group(0) @binding(3) var<storage, read> graded: array<EncT>;
@group(0) @binding(4) var<storage, read_write> enc: array<EncT>;
@group(0) @binding(5) var<storage, read> tables: array<f32>;
@group(0) @binding(6) var<storage, read> ranges: array<vec4<f32>>;
@group(0) @binding(7) var<storage, read> hdr: array<vec4<f32>>;
@group(0) @binding(8) var<storage, read> lin: array<vec4<f32>>;
@group(0) @binding(9) var<storage, read> clar: array<vec4<f32>>;
@group(0) @binding(10) var<storage, read> strc: array<vec4<f32>>;
@group(0) @binding(11) var<storage, read_write> ypl: array<f32>;
@group(0) @binding(12) var<storage, read> tex: array<vec4<f32>>;
@group(0) @binding(13) var<storage, read> dhz: array<vec4<f32>>;

fn texture_detail(x: u32, y: u32, v: f32) -> f32 {
    let t = dev.v[D_TEX];
    let o = dev.v[D_OFF2];
    let up = up_pos(x + u32(o.x), y + u32(o.y), t.x, u32(t.y), u32(t.z));
    let ab = bilerp4(tex[up.i00], tex[up.i10], tex[up.i01], tex[up.i11], up.t);
    return v - (ab.x * v + ab.y);
}

fn haze_at(x: u32, y: u32, p: vec3<f32>) -> vec3<f32> {
    let a = dev.v[D_DHA];
    let g = dev.v[D_DHG];
    let o = dev.v[D_OFF2];
    let up = up_pos(x + u32(o.z), y + u32(o.w), g.x, u32(g.y), u32(g.z));
    let ab = bilerp4(dhz[up.i00], dhz[up.i10], dhz[up.i01], dhz[up.i11], up.t);
    let gd = haze_guide(p, a.xyz);
    return vec3<f32>(clamp(ab.x * gd + ab.y, 0.0, 1.0), haze_local(ab.z, ab.w, gd));
}

// 3×3 Gaussian high-pass of the graded luma (replicated edges; mirrors cpu blur121).
fn fine_detail(x: u32, y: u32) -> f32 {
    let w = i32(dev.v[D_SIZE].x);
    let h = i32(dev.v[D_SIZE].y);
    let f = dev.v[D_FINE];
    let k = array<f32, 5>(f.z, f.y, f.x, f.y, f.z);
    var b = 0.0;
    for (var j = 0; j < 5; j++) {
        let yy = clamp(i32(y) + j - 2, 0, h - 1);
        for (var i = 0; i < 5; i++) {
            let xx = clamp(i32(x) + i - 2, 0, w - 1);
            b += k[j] * k[i] * enc_get(graded[u32(yy * w + xx)]).w;
        }
    }
    return (enc_get(graded[y * u32(w) + x]).w - b) * dev.v[D_FINEG].x;
}

//INCLUDE_TABLES

fn coef_detail(x: u32, y: u32, v: f32, g: vec4<f32>, clarity: bool) -> f32 {
    let o = select(dev.v[D_OFF1].xy, dev.v[D_OFF0].zw, clarity);
    let up = up_pos(x + u32(o.x), y + u32(o.y), g.x, u32(g.y), u32(g.z));
    var ab: vec4<f32>;
    if (clarity) {
        ab = bilerp4(clar[up.i00], clar[up.i10], clar[up.i01], clar[up.i11], up.t);
    } else {
        ab = bilerp4(strc[up.i00], strc[up.i10], strc[up.i01], strc[up.i11], up.t);
    }
    return v - (ab.x * v + ab.y);
}

fn apply_layer(p: vec3<f32>, b_rel: f32, dc: f32, ds: f32, dt: f32, haze: vec3<f32>, fine: f32) -> vec3<f32> {
    var e = p;
    let a = lay.v[L_A];
    let b = lay.v[L_B];
    let lp = lay.v[L_P];
    let t1 = dev.v[D_TAB1];
    let en = u32(t1.y);
    if (a.y > 0.5) {
        let tn = u32(t1.x);
        let inv = u32(t1.w);
        let fwd = u32(t1.z);
        var l0 = untone_rgb(e, fwd, tn, inv, en);
        if (lp.z != 0.0) {
            l0 = dehaze3(l0, haze, dev.v[D_DHA].xyz, lp.z);
        }
        let g = exp2(a.z + hdr_gain_ev(b_rel, a.w, b.x));
        var l1 = mat_rows(lay.v[L_WB0], lay.v[L_WB1], lay.v[L_WB2], l0) * g;
        let wk = lay.v[L_WK];
        if (wk.x != 1.0) {
            let y = max(luma3(l1), 1e-9);
            if (y > wk.z) {
                l1 = l1 * pow(y / wk.z, wk.x - 1.0);
            }
        }
        e = tone_rgb(gamut(l1), fwd, tn);
        if (wk.y != 0.0) {
            e = vec3<f32>(blacks_toe(e.x, wk.y), blacks_toe(e.y, wk.y), blacks_toe(e.z, wk.y));
        }
    }
    if (b.y != 1.0) {
        let piv = dev.v[D_PIV].x;
        e = vec3<f32>(contrast1(e.x, b.y, piv), contrast1(e.y, b.y, piv), contrast1(e.z, b.y, piv));
    }
    if (b.z != 1.0) {
        e = vec3<f32>(brightness1(e.x, b.z), brightness1(e.y, b.z), brightness1(e.z, b.z));
    }
    let c = lay.v[L_C];
    if (c.x > 0.5) {
        let cur = lay.v[L_CUR];
        e = vec3<f32>(lookup_enc(u32(cur.x), en, e.x), lookup_enc(u32(cur.y), en, e.y), lookup_enc(u32(cur.z), en, e.z));
        if (cur.w >= 0.0) {
            let y = luma3(e);
            e = e + vec3<f32>(lookup_enc(u32(cur.w), en, y) - y);
        }
    }
    let bm = lay.v[L_BM];
    if (bm.w > 0.5) {
        e = balance(e, bm.xyz, lay.v[L_BS].xyz, lay.v[L_BMID].xyz, lay.v[L_BH].xyz);
    }
    if (b.w != 1.0) {
        e = saturate3(e, b.w);
    }
    e = vibrance3(e, lp.x);
    e = apply_ranges(e, u32(c.z), u32(c.y));
    let st = lay.v[L_D].x;
    if (c.w != 0.0 || st != 0.0 || lp.y != 0.0) {
        e = e + vec3<f32>(midtone_weight(luma3(e)) * (c.w * soft_detail(dc) + st * soft_detail(ds) + texture_gain(dt, lp.y, dev.v[D_FINE].w)));
    }
    if (lp.w != 0.0) {
        e = e + vec3<f32>(sharpen_delta(fine, lp.w, dev.v[D_FINE].w));
    }
    return e;
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let f = lay.v[L_F];
    var cov: f32;
    if (f.x > 1.5) {
        let up = up_pos(gid.x, gid.y, f.y, u32(f.z), u32(f.w));
        cov = bilerp1(maskp[up.i00], maskp[up.i10], maskp[up.i01], maskp[up.i11], up.t);
    } else {
        cov = maskp[i];
    }
    let g = enc_get(graded[i]);
    let d = lay.v[L_D];
    var m = select(cov, 1.0 - cov, d.y > 0.5);
    if (d.z > 0.5) {
        let lr = lay.v[L_LR];
        let y = luma3(g.xyz);
        m *= ss(lr.x - max(lr.z, 0.0), lr.x, y) * (1.0 - ss(lr.y, lr.y + max(lr.w, 0.0), y));
    }
    let cr = lay.v[L_CR];
    if (cr.w > 0.5) {
        let lch = enc_to_oklch(g.xyz);
        m *= range_weight(cr.x, cr.y, cr.z, lch.z, lch.y);
    }
    m = clamp(m, 0.0, 1.0) * lay.v[L_A].x;
    if (m <= 0.0) {
        return;
    }
    var b_rel = -dev.v[D_ISO].w;
    let hd = dev.v[D_HDRD];
    if (hd.z > 0.5) {
        let o = dev.v[D_OFF0];
        let up = up_pos(gid.x + u32(o.x), gid.y + u32(o.y), dev.v[D_HDR].z, u32(hd.x), u32(hd.y));
        let ab = bilerp4(hdr[up.i00], hdr[up.i10], hdr[up.i01], hdr[up.i11], up.t);
        b_rel = ab.x * lin[i].w + ab.y - dev.v[D_ISO].w;
    }
    var dc = 0.0;
    var ds = 0.0;
    var dt = 0.0;
    let dn = dev.v[D_DETN];
    if (dn.x > 0.5) {
        dc = coef_detail(gid.x, gid.y, g.w, dev.v[D_DETC], true);
    }
    if (dn.y > 0.5) {
        ds = coef_detail(gid.x, gid.y, g.w, dev.v[D_DETS], false);
    }
    if (dn.z > 0.5) {
        dt = texture_detail(gid.x, gid.y, g.w);
    }
    let lp = lay.v[L_P];
    var haze = vec3<f32>(0.0);
    if (lp.z != 0.0) {
        haze = haze_at(gid.x, gid.y, lin[i].xyz);
    }
    var fine = 0.0;
    if (lp.w != 0.0) {
        fine = fine_detail(gid.x, gid.y);
    }
    let p = enc_get(enc[i]).xyz;
    let o = apply_layer(p, b_rel, dc, ds, dt, haze, fine);
    let e = p + (o - p) * m;
    let y = luma3(e);
    enc[i] = enc_put(vec4<f32>(e, y));
    ypl[i] = y;
}
