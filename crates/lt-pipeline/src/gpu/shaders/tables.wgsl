// Requires `tables: array<f32>` and `ranges: array<vec4<f32>>` bindings.

fn interp_t(off: u32, n: u32, pos: f32) -> f32 {
    let i = min(u32(pos), n - 2u);
    let f = pos - f32(i);
    let a = tables[off + i];
    return a + (tables[off + i + 1u] - a) * f;
}

fn lookup_log(off: u32, n: u32, x: f32) -> f32 {
    if (!(x > MIN_LIN)) {
        return tables[off];
    }
    let pos = min((log2(x) - LOG_MIN) * (f32(n - 1u) / (LOG_MAX - LOG_MIN)), f32(n - 1u));
    return interp_t(off, n, pos);
}

fn lookup_enc(off: u32, n: u32, y: f32) -> f32 {
    var pos = 0.0;
    if (y == y) {
        pos = clamp(y, 0.0, 1.0) * f32(n - 1u);
    }
    return interp_t(off, n, pos);
}

// Hue-preserving tone (mirrors lut::tone_rgb).
fn tone_rgb(c: vec3<f32>, off: u32, n: u32) -> vec3<f32> {
    let pc = vec3<f32>(lookup_log(off, n, c.x), lookup_log(off, n, c.y), lookup_log(off, n, c.z));
    let m = max(c.x, max(c.y, c.z));
    if (!(m > MIN_LIN)) {
        return pc;
    }
    let k = srgb_dec1(lookup_log(off, n, m)) / m;
    let rp = oklab_lin(max(c, vec3<f32>(0.0)) * k);
    let lab = oklab_lin(dec3(pc));
    let c_pc = length(lab.yz);
    let c_rp = length(rp.yz);
    if (c_pc < 1e-4 || c_rp < 1e-4) {
        return pc;
    }
    let f = c_pc / c_rp;
    let q = M_OK2_INV * vec3<f32>(lab.x, rp.y * f, rp.z * f);
    return enc3(max(M_LMS_INV * (q * q * q), vec3<f32>(0.0)));
}

// Inverse with one correction step (mirrors lut::untone_rgb).
fn untone_rgb(e: vec3<f32>, fwd: u32, tn: u32, inv: u32, n: u32) -> vec3<f32> {
    let x0 = vec3<f32>(lookup_enc(inv, n, e.x), lookup_enc(inv, n, e.y), lookup_enc(inv, n, e.z));
    let t = tone_rgb(x0, fwd, tn);
    let b = vec3<f32>(lookup_enc(inv, n, t.x), lookup_enc(inv, n, t.y), lookup_enc(inv, n, t.z));
    return select(x0, x0 * x0 / b, b > vec3<f32>(1e-9));
}

fn apply_ranges(e: vec3<f32>, off: u32, count: u32) -> vec3<f32> {
    if (count == 0u) {
        return e;
    }
    var lch = enc_to_oklch(e);
    var touched = false;
    for (var k = 0u; k < count; k++) {
        let a = ranges[off + 2u * k];
        let b = ranges[off + 2u * k + 1u];
        let w = range_weight(a.x, a.y, a.z, lch.z, lch.y);
        if (w <= 0.0) {
            continue;
        }
        touched = true;
        var dh = wrap360(a.x - lch.z);
        if (dh > 180.0) {
            dh -= 360.0;
        }
        lch.z += w * (a.w + dh * b.z);
        lch.y *= max(1.0 + w * b.x, 0.0);
        lch.x *= 1.0 + 0.4 * w * b.y;
    }
    if (!touched) {
        return e;
    }
    return oklch_to_enc(lch);
}
