// Glow / halation 3: screen the blurred glow over the image and add the halation's warm
// light (mirrors effects::apply_fx); updates the luma channel and plane.
// u.v[0] = w, h, glow amount, halation amount; [1] = glow f, lw, lh; [2] = glow phase x, y;
// [3] = halation f, lw, lh; [4] = halation phase x, y; [5] = halation tint rgb.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read_write> enc: array<EncT>;
@group(0) @binding(2) var<storage, read> glow: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> hal: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read_write> ypl: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    var e = enc_get(enc[i]).xyz;
    let ga = u.v[0].z;
    if (ga > 0.0) {
        let g = u.v[1];
        let o = u.v[2];
        let up = up_pos(gid.x + u32(o.x), gid.y + u32(o.y), g.x, u32(g.y), u32(g.z));
        let b = bilerp4(glow[up.i00], glow[up.i10], glow[up.i01], glow[up.i11], up.t).xyz;
        let ec = clamp(e, vec3<f32>(0.0), vec3<f32>(1.0));
        let s = 1.0 - (1.0 - ec) * (1.0 - clamp(b, vec3<f32>(0.0), vec3<f32>(1.0)));
        e = e + ga * (s - ec);
    }
    let ha = u.v[0].w;
    if (ha > 0.0) {
        let g = u.v[3];
        let o = u.v[4];
        let up = up_pos(gid.x + u32(o.x), gid.y + u32(o.y), g.x, u32(g.y), u32(g.z));
        let v = bilerp4(hal[up.i00], hal[up.i10], hal[up.i01], hal[up.i11], up.t).x * ha;
        e = e + v * u.v[5].xyz;
    }
    let y = luma3(e);
    enc[i] = enc_put(vec4<f32>(e, y));
    ypl[i] = y;
}
