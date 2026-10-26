// One workgroup per rectangle (a tile, or a ≤ 256 × 256 block of a subject
// region): the fused stencil sums of `tiles.rs` (Laplacian, Sobel structure
// tensor, luma moments, clip count) over the rectangle's pixels, which the
// host has already trimmed so every 3×3 stencil lies inside the frame.
// Four pixels (one word) per `vec4<i32>`.
//
// Threads split the rectangle into word columns × row groups and slide a
// 3-row window down their group, so each step loads one new row (3 words).
//
// Per-lane and per-thread sums are i32. A rectangle is at most 256 px (65
// words) wide and 256 rows tall, so a thread sees at most two word columns
// of 256 rows: 2048 px, and 2048 × 1020² < 2³¹. The workgroup reduction is
// 64-bit (u32 lo/hi pairs), so the results are exact and identical to the
// CPU's i64 sums.

//INCLUDE_COMMON

const WG: u32 = 64u;

struct Rect {
    img: u32,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
};

@group(0) @binding(3) var<storage, read> rects: array<Rect>;
@group(0) @binding(4) var<storage, read_write> stats: array<u32>;

var<workgroup> red: array<vec2<u32>, 576>; // 9 sums × WG

fn ext(v: i32) -> vec2<u32> {
    return vec2<u32>(bitcast<u32>(v), select(0u, 0xffffffffu, v < 0));
}

fn add64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
    let lo = a.x + b.x;
    return vec2<u32>(lo, a.y + b.y + select(0u, 1u, lo < a.x));
}

fn hsum(v: vec4<i32>) -> i32 {
    return v.x + v.y + v.z + v.w;
}

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let item = linear(wg);
    if (item >= params.n_items) {
        return;
    }
    let rect = rects[item];
    let m = metas[rect.img];
    let xa = rect.x0;
    let xb = rect.x1;
    let ya = rect.y0;
    let yb = rect.y1;

    var n = vec4<i32>(0);
    var lap_s = vec4<i32>(0);
    var lap2_s = vec4<i32>(0);
    var gxx_s = vec4<i32>(0);
    var gyy_s = vec4<i32>(0);
    var gxy_s = vec4<i32>(0);
    var luma_s = vec4<i32>(0);
    var luma2_s = vec4<i32>(0);
    var clip_s = vec4<i32>(0);
    if (xa < xb && ya < yb) {
        let k0 = xa / 4u;
        let kw = (xb + 3u) / 4u - k0;
        let rows = yb - ya;
        let groups = max(1u, WG / kw);
        let per = (rows + groups - 1u) / groups;
        for (var it = li; it < kw * groups; it += WG) {
            let k = k0 + it % kw;
            let y0 = ya + (it / kw) * per;
            let y1 = min(y0 + per, yb);
            let x = vec4<u32>(4u * k) + vec4<u32>(0u, 1u, 2u, 3u);
            let inside = x >= vec4<u32>(xa) & x < vec4<u32>(xb);
            let zero = vec4<i32>(0);
            n += select(zero, vec4<i32>(i32(max(y1, y0) - y0)), inside);
            var up = win(m, y0 - 1u, k);
            var mid = win(m, y0, k);
            for (var y = y0; y < y1; y++) {
                let dn = win(m, y + 1u, k);
                let c = mid.c;
                let l = left(mid, 1u);
                let r = right(mid, 1u);
                let u = up.c;
                let d = dn.c;
                let a = left(up, 1u);
                let b = right(up, 1u);
                let e = left(dn, 1u);
                let f = right(dn, 1u);
                // Lanes outside the rectangle contribute zeros.
                let lap = select(zero, 4 * c - l - r - u - d, inside);
                let gx = select(zero, (b + 2 * r + f) - (a + 2 * l + e), inside);
                let gy = select(zero, (e + 2 * d + f) - (a + 2 * u + b), inside);
                let cv = select(zero, c, inside);
                lap_s += lap;
                lap2_s += lap * lap;
                gxx_s += gx * gx;
                gyy_s += gy * gy;
                gxy_s += gx * gy;
                luma_s += cv;
                luma2_s += cv * cv;
                clip_s += select(zero, vec4<i32>(1), inside & (c == zero | c == vec4<i32>(255)));
                up = mid;
                mid = dn;
            }
        }
    }

    red[li] = ext(hsum(n));
    red[WG + li] = ext(hsum(lap_s));
    red[2u * WG + li] = ext(hsum(lap2_s));
    red[3u * WG + li] = ext(hsum(gxx_s));
    red[4u * WG + li] = ext(hsum(gyy_s));
    red[5u * WG + li] = ext(hsum(gxy_s));
    red[6u * WG + li] = ext(hsum(luma_s));
    red[7u * WG + li] = ext(hsum(luma2_s));
    red[8u * WG + li] = ext(hsum(clip_s));
    workgroupBarrier();
    for (var h = WG / 2u; h > 0u; h >>= 1u) {
        if (li < h) {
            for (var s = 0u; s < 9u; s++) {
                let i = s * WG + li;
                red[i] = add64(red[i], red[i + h]);
            }
        }
        workgroupBarrier();
    }
    if (li < 9u) {
        let v = red[li * WG];
        stats[item * 18u + li * 2u] = v.x;
        stats[item * 18u + li * 2u + 1u] = v.y;
    }
}
