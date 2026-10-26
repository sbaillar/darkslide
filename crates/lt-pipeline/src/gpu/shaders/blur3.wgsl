// 3×3 binomial ([1 2 1]⊗[1 2 1]/16) blur of a scalar plane with dilation `step`
// (à-trous) and replicated edges. u.v[0] = w, h, step.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;

fn row(y: u32, x0: u32, x: u32, x1: u32, w: u32) -> f32 {
    return 0.25 * (src[y * w + x0] + 2.0 * src[y * w + x] + src[y * w + x1]);
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    let step = u32(u.v[0].z);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let x0 = select(0u, gid.x - step, gid.x >= step);
    let x1 = min(gid.x + step, w - 1u);
    let y0 = select(0u, gid.y - step, gid.y >= step);
    let y1 = min(gid.y + step, h - 1u);
    let a = row(y0, x0, gid.x, x1, w);
    let b = row(gid.y, x0, gid.x, x1, w);
    let c = row(y1, x0, gid.x, x1, w);
    dst[gid.y * w + gid.x] = 0.25 * (a + 2.0 * b + c);
}
