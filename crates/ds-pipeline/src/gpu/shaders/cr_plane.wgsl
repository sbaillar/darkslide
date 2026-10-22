// Mask plane: a colour range's weight per pixel, and its refinement guide
// (mirrors mask::cr_guide). u.v[0] = OK hue center, half width, smoothness.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<uniform> u: P;
@group(0) @binding(2) var<storage, read> graded: array<EncT>;
@group(0) @binding(3) var<storage, read_write> outp: array<f32>;
@group(0) @binding(4) var<storage, read_write> guide: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let e = enc_get(graded[i]);
    let lch = enc_to_oklch(e.xyz);
    let r = u.v[0];
    outp[i] = range_weight(r.x, r.y, r.z, lch.z, lch.y);
    guide[i] = luma3(e.xyz) + CR_GUIDE_HUE * range_weight(r.x, r.y, 1.0, lch.z, lch.y);
}
