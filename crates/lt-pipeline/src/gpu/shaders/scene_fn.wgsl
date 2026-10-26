// Requires `dev`, `base`, `spots` and `sstats` bindings and the BASE format helpers.
//INCLUDE_RECONSTRUCT


fn fetch(bw: u32, x: u32, y: u32) -> vec3<f32> {
    return base_get(base[y * bw + x]);
}

// Spot sources and rings read with `geo_clamp` set: positions off the level are clamped
// to its edge (never black) and flag `geo_off` (geometry::sample_point_clamped).
var<private> geo_clamp: bool = false;
var<private> geo_off: bool = false;

// Bilinear sample at E px `e` (level coordinates via D_ETB); black outside the level
// (unless `geo_clamp`).
fn sample_at(e: vec2<f32>) -> vec3<f32> {
    let lvl = dev.v[D_LVL];
    var lx = row_apply(dev.v[D_ETB0], e);
    var ly = row_apply(dev.v[D_ETB1], e);
    if (!(lx >= -0.5 && ly >= -0.5 && lx <= lvl.x + 0.5 && ly <= lvl.y + 0.5)) {
        if (!geo_clamp) {
            return vec3<f32>(0.0);
        }
        geo_off = true;
        lx = clamp(lx, -0.5, lvl.x + 0.5);
        ly = clamp(ly, -0.5, lvl.y + 0.5);
    }
    let bw = u32(dev.v[D_SIZE].z);
    let bh = u32(dev.v[D_SIZE].w);
    let fx = clamp(lx - lvl.z - 0.5, 0.0, f32(bw - 1u));
    let fy = clamp(ly - lvl.w - 0.5, 0.0, f32(bh - 1u));
    let x0 = u32(fx);
    let y0 = u32(fy);
    let x1 = min(x0 + 1u, bw - 1u);
    let y1 = min(y0 + 1u, bh - 1u);
    let t = vec2<f32>(fx - f32(x0), fy - f32(y0));
    let a = fetch(bw, x0, y0);
    let b = fetch(bw, x1, y0);
    let c = fetch(bw, x0, y1);
    let d = fetch(bw, x1, y1);
    let top = a + (b - a) * t.x;
    let bot = c + (d - c) * t.x;
    return top + (bot - top) * t.y;
}

fn tca(p: vec4<f32>, r: f32) -> f32 {
    return p.x + p.y * r + p.z * r * r;
}

// Camera RGB (BASE) at output position `p`, and the lens vignetting gain there.
fn geo_sample(p: vec2<f32>) -> vec4<f32> {
    var e = vec2<f32>(row_apply(dev.v[D_TOE0], p), row_apply(dev.v[D_TOE1], p));
    if (dev.v[D_TCAR].w > 0.5) {
        let hw = dev.v[D_KEY2].x * e.x + dev.v[D_KEY2].y * e.y + dev.v[D_KEY2].z;
        let hh = select(hw, 1e-12, abs(hw) < 1e-12);
        e = vec2<f32>(
            (dev.v[D_KEY0].x * e.x + dev.v[D_KEY0].y * e.y + dev.v[D_KEY0].z) / hh,
            (dev.v[D_KEY1].x * e.x + dev.v[D_KEY1].y * e.y + dev.v[D_KEY1].z) / hh
        );
    }
    var s: vec3<f32>;
    var gain = 1.0;
    let dist = dev.v[D_DIST];
    if (dist.w > 0.5) {
        let lens = dev.v[D_LENS];
        let d = e - lens.xy;
        let r = length(d) * lens.z;
        let f = dist.x * r * r * r + dist.y * r * r + dist.z * r + 1.0 - dist.x - dist.y - dist.z;
        let g = d * f;
        let rd = r * f;
        let rv = length(g) * lens.w;
        let r2 = rv * rv;
        let k = dev.v[D_VIGK];
        let fall = 1.0 + r2 * (k.x + r2 * (k.y + r2 * k.z));
        gain = select(20.0, 1.0 / fall, fall > 0.05);
        let kr = tca(dev.v[D_TCAR], rd);
        let kb = tca(dev.v[D_TCAB], rd);
        let sg = sample_at(lens.xy + g);
        if (kr == 1.0 && kb == 1.0) {
            s = sg;
        } else {
            s = vec3<f32>(sample_at(lens.xy + g * kr).x, sg.y, sample_at(lens.xy + g * kb).z);
        }
    } else {
        s = sample_at(e);
    }
    return vec4<f32>(s, gain);
}

fn ring_luma(v: vec3<f32>) -> f32 {
    return dot(v, vec3<f32>(0.25, 0.5, 0.25));
}

// Log2 heal gain of a destination/source statistic pair (spots::log_gain).
fn heal_lg(d: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    let yd = max(ring_luma(d), 1e-6);
    let ys = max(ring_luma(s), 1e-6);
    let l = log2(clamp(yd / ys, 0.25, 4.0));
    let chroma = (max(d, vec3<f32>(1e-6)) / yd) / (max(s, vec3<f32>(1e-6)) / ys);
    return l + log2(clamp(chroma, vec3<f32>(0.87), vec3<f32>(1.15)));
}

fn heal_sector(k: u32, j: u32, avg: vec3<f32>) -> vec3<f32> {
    let d = sstats[k * 16u + j];
    let s = sstats[k * 16u + 8u + j];
    if (d.w > 0.0 && s.w > 0.0) {
        return heal_lg(d.xyz, s.xyz);
    }
    return avg;
}

// Heal gain for spot `k` (mirrors spots::heal_gain).
fn heal_gain(k: u32, q: vec2<f32>, rho: f32) -> vec3<f32> {
    var ds = vec3<f32>(0.0);
    var ss_ = vec3<f32>(0.0);
    var n = 0u;
    for (var j = 0u; j < 8u; j++) {
        let d = sstats[k * 16u + j];
        let s = sstats[k * 16u + 8u + j];
        if (d.w > 0.0 && s.w > 0.0) {
            ds += d.xyz;
            ss_ += s.xyz;
            n += 1u;
        }
    }
    if (n == 0u) {
        return vec3<f32>(1.0);
    }
    let avg = heal_lg(ds, ss_);
    var a = atan2(q.y, q.x);
    if (a < 0.0) {
        a += 6.2831855;
    }
    let t = a / 6.2831855 * 8.0 - 0.5;
    let t0 = floor(t);
    let f = t - t0;
    let j0 = u32(((i32(t0) % 8) + 8) % 8);
    let j1 = (j0 + 1u) % 8u;
    let r0 = heal_sector(k, j0, avg);
    let r1 = heal_sector(k, j1, avg);
    let local = r0 + (r1 - r0) * f;
    return exp2(avg + (local - avg) * (rho * rho));
}

// Word `i` of the spot bins (exact f32 integers after the spot data, params::spots).
fn spot_word(i: u32) -> u32 {
    return u32(spots[i >> 2u][i & 3u]);
}

// Heal/clone spots at output position `p` on camera value `s` (vignetting gain `g`):
// the spots binned to p's 32-pixel tile, in order (mirrors cpu::apply_spots).
fn apply_spots(p: vec2<f32>, s: vec3<f32>, g: f32) -> vec3<f32> {
    let sp = dev.v[D_SPN];
    let t = (u32(p.y) / 32u) * u32(sp.y) + u32(p.x) / 32u;
    let i0 = spot_word(u32(sp.z) + t);
    let i1 = spot_word(u32(sp.z) + t + 1u);
    if (i0 == i1) {
        return s;
    }
    let iso = dev.v[D_ISO].xy;
    let pi = vec2<f32>(row_apply(dev.v[D_NORM0], p), row_apply(dev.v[D_NORM1], p)) * iso;
    var v = s;
    for (var i = i0; i < i1; i++) {
        let k = spot_word(u32(sp.w) + i);
        let a = spots[2u * k];
        let b = spots[2u * k + 1u];
        let q = (pi - a.yz) / a.w;
        let rho = length(q);
        if (rho >= 1.0) {
            continue;
        }
        var edge = 1.0;
        if (b.x > 0.0) {
            edge = 1.0 - ss(1.0 - b.x, 1.0, rho);
        }
        let wt = b.y * edge;
        if (wt <= 0.0) {
            continue;
        }
        geo_clamp = true;
        let sc = geo_sample(p + b.zw);
        geo_clamp = false;
        var corr = vec3<f32>(1.0);
        if (a.x > 0.5) {
            corr = heal_gain(k, q, rho);
        }
        let val = sc.xyz * (sc.w / max(g, 1e-6)) * corr;
        v = v + (val - v) * wt;
    }
    return v;
}

// Linear Rec.2020 + log2 Y for output pixel `gid`.
fn scene_px(gid: vec3<u32>) -> vec4<f32> {
    let p = vec2<f32>(f32(gid.x) + 0.5, f32(gid.y) + 0.5);
    let c = geo_sample(p);
    var s = c.xyz;
    let gain = c.w;
    if (dev.v[D_TOE0].w > 0.5) {
        // Supersampled (Plan::supersample): the vignetting gain stays the center's.
        s = 0.25 * (geo_sample(p + vec2<f32>(-0.25, -0.25)).xyz
            + geo_sample(p + vec2<f32>(0.25, -0.25)).xyz
            + geo_sample(p + vec2<f32>(-0.25, 0.25)).xyz
            + geo_sample(p + vec2<f32>(0.25, 0.25)).xyz);
    }
    if (dev.v[D_SPN].x > 0.5) {
        s = apply_spots(p, s, gain);
    }
    let cam = reconstruct(s * dev.v[D_WBR].xyz, dev.v[D_CLIP].xyz) * gain;
    let rgb = mat_rows(dev.v[D_CAM0], dev.v[D_CAM1], dev.v[D_CAM2], cam);
    let ex = dev.v[D_EXPO];
    var g = ex.x;
    if (ex.w > 0.5) {
        let r = length(vec2<f32>(row_apply(dev.v[D_VIG0], p), row_apply(dev.v[D_VIG1], p)));
        let tt = ss(clamp(ex.z, 0.0, 0.99), 1.0, r);
        g *= exp2(ex.y * tt * tt);
    }
    let o = gamut(rgb * g);
    return vec4<f32>(o, log2(max(luma3(o), MIN_LIN)));
}
