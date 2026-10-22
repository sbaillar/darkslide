// RCD 2: green at R/B sites (ratio-corrected cardinal estimates); green sites copy the CFA.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> cfa: array<f32>;
@group(0) @binding(2) var<storage, read> dirs: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> green: array<f32>;

//INCLUDE_RCD

fn lpf(x: i32, y: i32) -> f32 {
    return dirs[ridx(x, y)].y;
}

fn vh(x: i32, y: i32) -> f32 {
    return dirs[ridx(x, y)].x;
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
    if (site_color(x, y) == 1u) {
        green[i] = cfa[i];
        return;
    }
    let c0 = cf(x, y);
    let n_grad = RCD_EPS + abs(cf(x, y - 1) - cf(x, y + 1)) + abs(c0 - cf(x, y - 2)) + abs(cf(x, y - 1) - cf(x, y - 3)) + abs(cf(x, y - 2) - cf(x, y - 4));
    let s_grad = RCD_EPS + abs(cf(x, y - 1) - cf(x, y + 1)) + abs(c0 - cf(x, y + 2)) + abs(cf(x, y + 1) - cf(x, y + 3)) + abs(cf(x, y + 2) - cf(x, y + 4));
    let w_grad = RCD_EPS + abs(cf(x - 1, y) - cf(x + 1, y)) + abs(c0 - cf(x - 2, y)) + abs(cf(x - 1, y) - cf(x - 3, y)) + abs(cf(x - 2, y) - cf(x - 4, y));
    let e_grad = RCD_EPS + abs(cf(x - 1, y) - cf(x + 1, y)) + abs(c0 - cf(x + 2, y)) + abs(cf(x + 1, y) - cf(x + 3, y)) + abs(cf(x + 2, y) - cf(x + 4, y));
    let l = lpf(x, y);
    let n_est = cf(x, y - 1) * (l + l) / (RCD_EPS + l + lpf(x, y - 2));
    let s_est = cf(x, y + 1) * (l + l) / (RCD_EPS + l + lpf(x, y + 2));
    let w_est = cf(x - 1, y) * (l + l) / (RCD_EPS + l + lpf(x - 2, y));
    let e_est = cf(x + 1, y) * (l + l) / (RCD_EPS + l + lpf(x + 2, y));
    let v_est = (s_grad * n_est + n_grad * s_est) / (n_grad + s_grad);
    let h_est = (w_grad * e_est + e_grad * w_est) / (e_grad + w_grad);
    let c = vh(x, y);
    let nb = 0.25 * (vh(x - 1, y - 1) + vh(x + 1, y - 1) + vh(x - 1, y + 1) + vh(x + 1, y + 1));
    let disc = select(c, nb, abs(0.5 - c) < abs(0.5 - nb));
    green[i] = disc * (h_est - v_est) + v_est;
}
