// Requires `dev`, `hdr`, `dhz`, `tables` and `ranges` bindings (and the tables helpers).

// Haze estimate and neighbourhood smoothness at output pixel `gid` for linear value `p`.
fn haze_at(gid: vec3<u32>, p: vec3<f32>) -> vec3<f32> {
    let a = dev.v[D_DHA];
    let g = dev.v[D_DHG];
    let o = dev.v[D_OFF2];
    let up = up_pos(gid.x + u32(o.z), gid.y + u32(o.w), g.x, u32(g.y), u32(g.z));
    let ab = bilerp4(dhz[up.i00], dhz[up.i10], dhz[up.i01], dhz[up.i11], up.t);
    let gd = haze_guide(p, a.xyz);
    return vec3<f32>(clamp(ab.x * gd + ab.y, 0.0, 1.0), haze_local(ab.z, ab.w, gd));
}

// 3×3 matrix stored as 9 floats (rows) at table offset `o`, times `v`.
fn tab_mat(o: u32, v: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        tables[o] * v.x + tables[o + 1u] * v.y + tables[o + 2u] * v.z,
        tables[o + 3u] * v.x + tables[o + 4u] * v.y + tables[o + 5u] * v.z,
        tables[o + 6u] * v.x + tables[o + 7u] * v.y + tables[o + 8u] * v.z
    );
}

fn hsv_entry(o: u32, i: u32) -> vec3<f32> {
    return vec3<f32>(tables[o + 3u * i], tables[o + 3u * i + 1u], tables[o + 3u * i + 2u]);
}

// DNG hue/sat/value map on linear ProPhoto `rgb` (mirrors profile::HsvMap::apply).
// `m` = offset, hue, sat, value divisions; `srgb` value-index encoding; `vk` value-index scale.
fn hsv_map(rgb: vec3<f32>, m: vec4<f32>, srgb: bool, vk: f32) -> vec3<f32> {
    let v = max(rgb.x, max(rgb.y, rgb.z));
    if (!(v > 0.0)) {
        return rgb;
    }
    let d = v - min(rgb.x, min(rgb.y, rgb.z));
    var h = 0.0;
    var s = 0.0;
    if (d > 0.0) {
        if (rgb.x == v) {
            h = (rgb.y - rgb.z) / d;
        } else if (rgb.y == v) {
            h = 2.0 + (rgb.z - rgb.x) / d;
        } else {
            h = 4.0 + (rgb.x - rgb.y) / d;
        }
        if (h < 0.0) {
            h += 6.0;
        }
        s = d / v;
    }
    let o = u32(m.x);
    let hd = u32(m.y);
    let sd = u32(m.z);
    let vd = u32(m.w);
    let hs = h * (f32(hd) / 6.0);
    var h0 = u32(hs);
    let hf = hs - f32(h0);
    var h1 = h0 + 1u;
    if (h0 >= hd - 1u) {
        h0 = hd - 1u;
        h1 = 0u;
    }
    let sss = clamp(s, 0.0, 1.0) * f32(sd - 1u);
    let s0 = min(u32(sss), sd - 2u);
    let sf = sss - f32(s0);
    var v0 = 0u;
    var vf = 0.0;
    if (vd > 1u) {
        var x = clamp(v * vk, 0.0, 1.0);
        if (srgb) {
            x = srgb_enc1(x);
        }
        let vs = x * f32(vd - 1u);
        v0 = min(u32(vs), vd - 2u);
        vf = vs - f32(v0);
    }
    var e = vec3<f32>(0.0);
    for (var k = 0u; k < select(1u, 2u, vd > 1u); k++) {
        let base = (v0 + k) * hd;
        let a = mix(hsv_entry(o, (base + h0) * sd + s0), hsv_entry(o, (base + h0) * sd + s0 + 1u), sf);
        let b = mix(hsv_entry(o, (base + h1) * sd + s0), hsv_entry(o, (base + h1) * sd + s0 + 1u), sf);
        let p = mix(a, b, hf);
        if (k == 0u) {
            e = p;
        } else {
            e = mix(e, p, vf);
        }
    }
    let s2 = min(s * e.y, max(s, 1.0));
    let v2 = v * e.z;
    if (s2 <= 0.0) {
        return vec3<f32>(v2);
    }
    var h2 = (h + e.x * (6.0 / 360.0)) % 6.0;
    if (h2 < 0.0) {
        h2 += 6.0;
    }
    let i = min(u32(h2), 5u);
    let f = h2 - f32(i);
    let p = v2 * (1.0 - s2);
    let q = v2 * (1.0 - s2 * f);
    let t = v2 * (1.0 - s2 * (1.0 - f));
    switch (i) {
        case 0u: { return vec3<f32>(v2, t, p); }
        case 1u: { return vec3<f32>(q, v2, p); }
        case 2u: { return vec3<f32>(p, v2, t); }
        case 3u: { return vec3<f32>(p, q, v2); }
        case 4u: { return vec3<f32>(t, p, v2); }
        default: { return vec3<f32>(v2, p, q); }
    }
}

// The profile's colour maps on scene-linear working colour (mirrors ProfilePlan::apply).
fn profile_maps(c: vec3<f32>) -> vec3<f32> {
    let hsm = dev.v[D_HSM];
    let look = dev.v[D_LOOK];
    if (hsm.x < 0.0 && look.x < 0.0) {
        return c;
    }
    let pm = dev.v[D_PMAP];
    let mo = u32(pm.x);
    var pp = tab_mat(mo, c);
    if (hsm.x >= 0.0) {
        pp = hsv_map(pp, hsm, pm.y > 0.5, pm.w);
    }
    if (look.x >= 0.0) {
        pp = hsv_map(pp, look, pm.z > 0.5, 1.0);
    }
    return tab_mat(mo + 9u, pp);
}

// Graded, display-encoded Rec.2020 for output pixel `gid` with scene value `l`.
fn grade_px(gid: vec3<u32>, l: vec4<f32>) -> vec3<f32> {
    var c = l.xyz;
    let pres = dev.v[D_PRES];
    if (pres.z != 0.0) {
        c = dehaze3(c, haze_at(gid, c), dev.v[D_DHA].xyz, pres.z);
    }
    let hp = dev.v[D_HDR];
    if (hp.w > 0.5) {
        let hd = dev.v[D_HDRD];
        let o = dev.v[D_OFF0];
        let up = up_pos(gid.x + u32(o.x), gid.y + u32(o.y), hp.z, u32(hd.x), u32(hd.y));
        let ab = bilerp4(hdr[up.i00], hdr[up.i10], hdr[up.i01], hdr[up.i11], up.t);
        let b = ab.x * l.w + ab.y;
        c = c * exp2(hdr_gain_ev(b - dev.v[D_ISO].w, hp.x, hp.y));
    }
    let t0 = dev.v[D_TAB0];
    let tn = u32(dev.v[D_TAB1].x);
    var e = tone_rgb(profile_maps(c), u32(t0.x), tn);
    if (t0.y >= 0.0) {
        let en = u32(dev.v[D_TAB1].y);
        let o = u32(t0.y);
        e = vec3<f32>(lookup_enc(o, en, e.x), lookup_enc(o + en, en, e.y), lookup_enc(o + 2u * en, en, e.z));
    }
    if (t0.w >= 0.0) {
        let y = luma3(e);
        e = e + vec3<f32>(lookup_enc(u32(t0.w), u32(dev.v[D_TAB1].y), y) - y);
    }
    let bm = dev.v[D_BM];
    if (dev.v[D_BH].w > 0.5) {
        e = balance(e, bm.xyz, dev.v[D_BS].xyz, dev.v[D_BMID].xyz, dev.v[D_BH].xyz);
    }
    if (bm.w != 1.0) {
        e = saturate3(e, bm.w);
    }
    e = vibrance3(e, pres.x);
    e = apply_ranges(e, u32(dev.v[D_BMID].w), u32(dev.v[D_BS].w));
    return e;
}
