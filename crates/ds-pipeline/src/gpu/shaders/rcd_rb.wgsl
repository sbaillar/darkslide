// RCD 3: the missing chroma at R/B sites (R at B, B at R) along the P/Q diagonals.
// rb = (R, B); green sites are left for RCD 4.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> cfa: array<f32>;
@group(0) @binding(2) var<storage, read> dirs: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> green: array<f32>;
@group(0) @binding(4) var<storage, read_write> rb: array<vec2<f32>>;

//INCLUDE_RCD

fn pq(x: i32, y: i32) -> f32 {
    return dirs[ridx(x, y)].z;
}

fn g(x: i32, y: i32) -> f32 {
    return green[ridx(x, y)];
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
    let i = gid.y * w + gid.x;
    let own = site_color(x, y);
    if (own == 1u) {
        return;
    }
    let pc = pq(x, y);
    let nb = 0.25 * (pq(x - 1, y - 1) + pq(x + 1, y - 1) + pq(x - 1, y + 1) + pq(x + 1, y + 1));
    let disc = select(pc, nb, abs(0.5 - pc) < abs(0.5 - nb));
    // The diagonal neighbours carry the other chroma directly in the CFA.
    let g0 = g(x, y);
    let nw = RCD_EPS + abs(cf(x - 1, y - 1) - cf(x + 1, y + 1)) + abs(cf(x - 1, y - 1) - cf(x - 3, y - 3)) + abs(g0 - g(x - 2, y - 2));
    let ne = RCD_EPS + abs(cf(x + 1, y - 1) - cf(x - 1, y + 1)) + abs(cf(x + 1, y - 1) - cf(x + 3, y - 3)) + abs(g0 - g(x + 2, y - 2));
    let sw = RCD_EPS + abs(cf(x + 1, y - 1) - cf(x - 1, y + 1)) + abs(cf(x - 1, y + 1) - cf(x - 3, y + 3)) + abs(g0 - g(x - 2, y + 2));
    let se = RCD_EPS + abs(cf(x - 1, y - 1) - cf(x + 1, y + 1)) + abs(cf(x + 1, y + 1) - cf(x + 3, y + 3)) + abs(g0 - g(x + 2, y + 2));
    let nw_e = cf(x - 1, y - 1) - g(x - 1, y - 1);
    let ne_e = cf(x + 1, y - 1) - g(x + 1, y - 1);
    let sw_e = cf(x - 1, y + 1) - g(x - 1, y + 1);
    let se_e = cf(x + 1, y + 1) - g(x + 1, y + 1);
    let p_est = (nw * se_e + se * nw_e) / (nw + se);
    let q_est = (ne * sw_e + sw * ne_e) / (ne + sw);
    let other = g0 + disc * (q_est - p_est) + p_est;
    let mine = cfa[i];
    if (own == 0u) {
        rb[i] = vec2<f32>(mine, other);
    } else {
        rb[i] = vec2<f32>(other, mine);
    }
}
