// Export tiles: the developed window with capture sharpening and the output transform
// applied (as the output pass would: matrix, gamut, clamp) stored as output-space linear
// rgb, encoded (transfer function) in 16-bit fixed point, into a full-width stripe.
// u.v[0] = destination x, y, row stride, clamp to 1.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<uniform> u: P;
@group(0) @binding(2) var<storage, read> enc: array<EncT>;
@group(0) @binding(3) var<storage, read> blur: array<f32>;
@group(0) @binding(4) var<storage, read_write> dst: array<vec2<u32>>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let win = dev.v[D_CROP];
    if (gid.x >= u32(win.z) || gid.y >= u32(win.w)) {
        return;
    }
    let w = u32(dev.v[D_SIZE].x);
    let i = (u32(win.y) + gid.y) * w + u32(win.x) + gid.x;
    let p = enc_get(enc[i]);
    var e = p.xyz;
    let sh = dev.v[D_SH];
    if (sh.z > 0.5) {
        e = e + vec3<f32>(sh.x * sharpen_thr(p.w - blur[i], sh.y));
    }
    let gr = dev.v[D_GRAIN];
    if (gr.w > 0.5) {
        let iso = dev.v[D_ISO];
        let gp = vec2<f32>(win.x + f32(gid.x) + 0.5, win.y + f32(gid.y) + 0.5);
        let fp = vec2<f32>(row_apply(dev.v[D_NORM0], gp) * iso.x * iso.z,
                           row_apply(dev.v[D_NORM1], gp) * iso.y * iso.z);
        e = grain_add(e, fp, gr);
    }
    let lin0 = mat_rows(dev.v[D_OUT0], dev.v[D_OUT1], dev.v[D_OUT2], dec3(e));
    var lin = gamut(select(lin0, vec3<f32>(0.0), lin0 != lin0));
    let at = u.v[0];
    if (at.w > 0.5) {
        lin = min(lin, vec3<f32>(1.0));
    }
    dst[(u32(at.y) + gid.y) * u32(at.z) + u32(at.x) + gid.x] = u16x4_put(vec4<f32>(enc3(lin), 0.0));
}
