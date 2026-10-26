// Lens blur 4: composite (mirrors effects::composite). Pixels within half a pixel of
// focus keep their exact value; the rest blend toward the upsampled gather by their blur.
// The log-luma channel (w) is recomputed for the HDR filter downstream.
// u.v[0] = w, h, f; [1] = lw, lh, phase x, y.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read_write> lin: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> coc: array<f32>;
@group(0) @binding(3) var<storage, read> gathered: array<vec4<f32>>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let c = abs(coc[i]);
    if (c < 0.5) {
        return;
    }
    let f = u.v[0].z;
    let lw = u32(u.v[1].x);
    let lh = u32(u.v[1].y);
    let up = up_pos(gid.x + u32(u.v[1].z), gid.y + u32(u.v[1].w), f, lw, lh);
    let a = gathered[up.i00];
    let b = gathered[up.i10];
    let cc = gathered[up.i01];
    let d = gathered[up.i11];
    let t = up.t;
    let w00 = (1.0 - t.x) * (1.0 - t.y) * a.w;
    let w10 = t.x * (1.0 - t.y) * b.w;
    let w01 = (1.0 - t.x) * t.y * cc.w;
    let w11 = t.x * t.y * d.w;
    let alpha = w00 + w10 + w01 + w11;
    if (alpha <= 1e-6) {
        return;
    }
    let blurred = (w00 * a.xyz + w10 * b.xyz + w01 * cc.xyz + w11 * d.xyz) / alpha;
    let k = clamp((c - 0.5) / max(0.5 * f, 1.0), 0.0, 1.0) * min(alpha, 1.0);
    let p = lin[i].xyz;
    let o = p + (blurred - p) * k;
    lin[i] = vec4<f32>(o, log2(max(luma3(o), MIN_LIN)));
}
