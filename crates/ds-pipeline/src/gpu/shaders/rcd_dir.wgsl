// RCD 1: V/H discrimination (all sites), low-pass (R/B sites) and P/Q discrimination
// (R/B sites). out = (vh, lpf, pq, 0).
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> cfa: array<f32>;
@group(0) @binding(2) var<storage, read_write> dirs: array<vec4<f32>>;

//INCLUDE_RCD

fn bv(x: i32, y: i32) -> f32 {
    return sqr((cf(x, y - 3) - cf(x, y - 1) - cf(x, y + 1) + cf(x, y + 3)) - 3.0 * (cf(x, y - 2) + cf(x, y + 2)) + 6.0 * cf(x, y));
}

fn bh(x: i32, y: i32) -> f32 {
    return sqr((cf(x - 3, y) - cf(x - 1, y) - cf(x + 1, y) + cf(x + 3, y)) - 3.0 * (cf(x - 2, y) + cf(x + 2, y)) + 6.0 * cf(x, y));
}

fn bp(x: i32, y: i32) -> f32 {
    return sqr((cf(x - 3, y - 3) - cf(x - 1, y - 1) - cf(x + 1, y + 1) + cf(x + 3, y + 3)) - 3.0 * (cf(x - 2, y - 2) + cf(x + 2, y + 2)) + 6.0 * cf(x, y));
}

fn bq(x: i32, y: i32) -> f32 {
    return sqr((cf(x + 3, y - 3) - cf(x + 1, y - 1) - cf(x - 1, y + 1) + cf(x - 3, y + 3)) - 3.0 * (cf(x + 2, y - 2) + cf(x - 2, y + 2)) + 6.0 * cf(x, y));
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let h = u32(u.v[0].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let x = i32(gid.x);
    let y = i32(gid.y);
    let v = max(bv(x, y - 1) + bv(x, y) + bv(x, y + 1), RCD_EPSSQ);
    let hh = max(bh(x - 1, y) + bh(x, y) + bh(x + 1, y), RCD_EPSSQ);
    var o = vec4<f32>(v / (v + hh), 0.0, 0.0, 0.0);
    if (site_color(x, y) != 1u) {
        o.y = cf(x, y) + 0.5 * (cf(x, y - 1) + cf(x, y + 1) + cf(x - 1, y) + cf(x + 1, y))
            + 0.25 * (cf(x - 1, y - 1) + cf(x + 1, y - 1) + cf(x - 1, y + 1) + cf(x + 1, y + 1));
        let p = max(bp(x - 1, y - 1) + bp(x, y) + bp(x + 1, y + 1), RCD_EPSSQ);
        let q = max(bq(x + 1, y - 1) + bq(x, y) + bq(x - 1, y + 1), RCD_EPSSQ);
        o.z = p / (p + q);
    }
    dirs[gid.y * w + gid.x] = o;
}
