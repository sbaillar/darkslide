// DEVELOP 3: global clarity/structure. Copies graded → enc (rgb, luma) with the local
// contrast delta when active.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<storage, read> graded: array<EncT>;
@group(0) @binding(2) var<storage, read> clar: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> strc: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read_write> enc: array<EncT>;
@group(0) @binding(5) var<storage, read_write> ypl: array<f32>;
@group(0) @binding(6) var<storage, read> tex: array<vec4<f32>>;

fn texture_detail(x: u32, y: u32, v: f32) -> f32 {
    let t = dev.v[D_TEX];
    let o = dev.v[D_OFF2];
    let up = up_pos(x + u32(o.x), y + u32(o.y), t.x, u32(t.y), u32(t.z));
    let ab = bilerp4(tex[up.i00], tex[up.i10], tex[up.i01], tex[up.i11], up.t);
    return v - (ab.x * v + ab.y);
}

fn detail_at(x: u32, y: u32, v: f32, s: f32, lw: f32, lh: f32, clarity: bool) -> f32 {
    let o = select(dev.v[D_OFF1].xy, dev.v[D_OFF0].zw, clarity);
    let up = up_pos(x + u32(o.x), y + u32(o.y), s, u32(lw), u32(lh));
    var ab: vec4<f32>;
    if (clarity) {
        ab = bilerp4(clar[up.i00], clar[up.i10], clar[up.i01], clar[up.i11], up.t);
    } else {
        ab = bilerp4(strc[up.i00], strc[up.i10], strc[up.i01], strc[up.i11], up.t);
    }
    return v - (ab.x * v + ab.y);
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let g = enc_get(graded[i]);
    var e = g.xyz;
    let d = dev.v[D_DET];
    if (d.w > 0.5) {
        let cc = dev.v[D_DETC];
        let cs = dev.v[D_DETS];
        let dn = dev.v[D_DETN];
        var dc = 0.0;
        var ds = 0.0;
        if (dn.x > 0.5) {
            dc = detail_at(gid.x, gid.y, g.w, cc.x, cc.y, cc.z, true);
        }
        if (dn.y > 0.5) {
            ds = detail_at(gid.x, gid.y, g.w, cs.x, cs.y, cs.z, false);
        }
        var dt = 0.0;
        let pres = dev.v[D_PRES];
        if (pres.w > 0.5 && dn.z > 0.5) {
            dt = texture_gain(texture_detail(gid.x, gid.y, g.w), pres.y, dev.v[D_FINE].w);
        }
        e = e + vec3<f32>(midtone_weight(g.w) * (d.x * soft_detail(dc) + d.y * soft_detail(ds) + dt));
    }
    let y = luma3(e);
    enc[i] = enc_put(vec4<f32>(e, y));
    ypl[i] = y;
}
