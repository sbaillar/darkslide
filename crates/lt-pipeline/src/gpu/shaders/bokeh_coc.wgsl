// Lens blur 1: signed circle of confusion per developed pixel (negative in front of the
// focus), from the depth map (nearest of the four bilinear taps) or the distance to the
// tilt-shift band (mirrors effects::LensBlurPlan::coc).
// u.v[0] = rmax, focus depth, focal range, mode (0 depth, 1 plane);
// [1] = depth w, depth h, depth ramp; [2] = band centre (iso) x, y, normal x, y;
// [3] = band width, feather.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<uniform> u: P;
@group(0) @binding(2) var<storage, read> depth: array<u32>;
@group(0) @binding(3) var<storage, read_write> coc: array<f32>;

fn depth_at(x: u32, y: u32, dw: u32) -> u32 {
    let i = y * dw + x;
    return (depth[i >> 2u] >> ((i & 3u) * 8u)) & 255u;
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let p = vec2<f32>(f32(gid.x) + 0.5, f32(gid.y) + 0.5);
    let nu = row_apply(dev.v[D_NORM0], p);
    let nv = row_apply(dev.v[D_NORM1], p);
    let a = u.v[0];
    let rmax = a.x;
    var c = 0.0;
    if (a.w < 0.5) {
        let dw = u32(u.v[1].x);
        let dh = u32(u.v[1].y);
        let fx = clamp(nu * f32(dw) - 0.5, 0.0, f32(dw - 1u));
        let fy = clamp(nv * f32(dh) - 0.5, 0.0, f32(dh - 1u));
        let x0 = u32(fx);
        let y0 = u32(fy);
        let x1 = min(x0 + 1u, dw - 1u);
        let y1 = min(y0 + 1u, dh - 1u);
        let m = max(max(depth_at(x0, y0, dw), depth_at(x1, y0, dw)),
                    max(depth_at(x0, y1, dw), depth_at(x1, y1, dw)));
        let delta = f32(m) / 255.0 - a.y;
        let k = clamp((abs(delta) - a.z) / u.v[1].z, 0.0, 1.0);
        c = select(rmax * k, -rmax * k, delta > 0.0);
    } else {
        let iso = dev.v[D_ISO];
        let b = u.v[2];
        let q = vec2<f32>(nu * iso.x - b.x, nv * iso.y - b.y);
        let dist = abs(q.x * b.z + q.y * b.w);
        c = rmax * clamp((dist - u.v[3].x) / u.v[3].y, 0.0, 1.0);
    }
    coc[gid.y * w + gid.x] = c;
}
