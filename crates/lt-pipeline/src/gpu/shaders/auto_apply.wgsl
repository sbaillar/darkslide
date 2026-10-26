// Auto-mask plane: footprint × similarity of luma and OKLab (a, b) to the core-weighted
// local reference (mirrors mask::auto_value). u.v[0] = w, h, s, luma tolerance;
// [1] = lw, lh, phase x, y; [2] = colour tolerance.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> graded: array<EncT>;
@group(0) @binding(2) var<storage, read> cov: array<f32>;
@group(0) @binding(3) var<storage, read> m: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read_write> outp: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let up = up_pos(gid.x + u32(u.v[1].z), gid.y + u32(u.v[1].w), u.v[0].z, u32(u.v[1].x), u32(u.v[1].y));
    let r = bilerp4(m[up.i00], m[up.i10], m[up.i01], m[up.i11], up.t);
    var v = 0.0;
    if (r.x > 1e-4) {
        let g = enc_get(graded[i]);
        let d = (g.w - r.y / r.x) / u.v[0].w;
        let dab = (auto_ab(g.xyz) - r.zw / r.x) / u.v[2].x;
        v = cov[i] * exp(-(d * d + dot(dab, dab)));
    }
    outp[i] = v;
}
