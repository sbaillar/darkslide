// DEVELOP 5: noise reduction. Luma: 2-level à-trous garrote shrinkage; chroma: blend toward
// a self-guided estimate per channel (ε = measured chroma noise²). In place on enc.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<storage, read_write> enc: array<EncT>;
@group(0) @binding(2) var<storage, read> b1: array<f32>;
@group(0) @binding(3) var<storage, read> b2: array<f32>;
@group(0) @binding(4) var<storage, read> coef: array<vec4<f32>>;
@group(0) @binding(5) var<storage, read_write> ypl: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let p = enc_get(enc[i]);
    let l = p.w;
    let nr = dev.v[D_NR];
    let nd = dev.v[D_NRD];
    var nl = l;
    if (nd.z > 0.5) {
        let v1 = b1[i];
        let v2 = b2[i];
        nl = v2 + garrote(v1 - v2, nr.y) + garrote(l - v1, nr.x);
    }
    var cb = p.z - l;
    var cr = p.x - l;
    if (nd.w > 0.5) {
        let o = dev.v[D_OFF1];
        let up = up_pos(gid.x + u32(o.z), gid.y + u32(o.w), nr.w, u32(nd.x), u32(nd.y));
        let c = bilerp4(coef[up.i00], coef[up.i10], coef[up.i01], coef[up.i11], up.t);
        let qb = c.x * cb + c.y;
        let qr = c.z * cr + c.w;
        cb += (qb - cb) * nr.z;
        cr += (qr - cr) * nr.z;
    }
    let e = from_luma_chroma(nl, cb, cr);
    let y = luma3(e);
    enc[i] = enc_put(vec4<f32>(e, y));
    ypl[i] = y;
}
