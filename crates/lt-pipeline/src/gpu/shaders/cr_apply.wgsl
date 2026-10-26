// Colour-range refinement, last step (mirrors mask::build_planes): the guided filter's
// upsampled coefficients applied to the guide smoothed like mask::cr_smooth_guide
// (bilateral, radius CR_GUIDE_SMOOTH, range σ CR_GUIDE_SIGMA, replicated edges),
// `clamp(a·g + b, 0, 1)`.
// u.v[0] = w, h, s; [1] = lw, lh, phase x, y.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> guide: array<f32>;
@group(0) @binding(2) var<storage, read> coef: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> outp: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let up = up_pos(gid.x + u32(u.v[1].z), gid.y + u32(u.v[1].w), u.v[0].z, u32(u.v[1].x), u32(u.v[1].y));
    let ab = bilerp4(coef[up.i00], coef[up.i10], coef[up.i01], coef[up.i11], up.t);
    let c = guide[i];
    let k = -0.5 / (CR_GUIDE_SIGMA * CR_GUIDE_SIGMA);
    var sum = 0.0;
    var wsum = 0.0;
    for (var dy = -CR_GUIDE_SMOOTH; dy <= CR_GUIDE_SMOOTH; dy++) {
        let y = u32(clamp(i32(gid.y) + dy, 0, i32(h) - 1));
        for (var dx = -CR_GUIDE_SMOOTH; dx <= CR_GUIDE_SMOOTH; dx++) {
            let x = u32(clamp(i32(gid.x) + dx, 0, i32(w) - 1));
            let g = guide[y * w + x];
            let wt = exp(k * (g - c) * (g - c));
            sum += wt * g;
            wsum += wt;
        }
    }
    outp[i] = clamp(ab.x * sum / wsum + ab.y, 0.0, 1.0);
}
