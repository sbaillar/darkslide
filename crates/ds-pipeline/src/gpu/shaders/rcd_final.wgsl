// RCD 4: R and B at green sites → BASE (camera RGB at the reference WB) for the rectangle.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> cfa: array<f32>;
@group(0) @binding(2) var<storage, read> dirs: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> green: array<f32>;
@group(0) @binding(4) var<storage, read> rb: array<vec2<f32>>;
@group(0) @binding(5) var<storage, read_write> outb: array<BaseT>;

//INCLUDE_RCD

fn vh(x: i32, y: i32) -> f32 {
    return dirs[ridx(x, y)].x;
}

fn g(x: i32, y: i32) -> f32 {
    return green[ridx(x, y)];
}

fn chroma(x: i32, y: i32, c: u32) -> f32 {
    let v = rb[ridx(x, y)];
    return select(v.y, v.x, c == 0u);
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(u.v[0].x);
    let rw = u32(u.v[9].x);
    let rh = u32(u.v[9].y);
    if (gid.x >= rw || gid.y >= rh) {
        return;
    }
    let gx = gid.x + u32(u.v[8].z);
    let gy = gid.y + u32(u.v[8].w);
    let x = i32(gx);
    let y = i32(gy);
    let i = gy * w + gx;
    var rgb: vec3<f32>;
    let own = site_color(x, y);
    if (own != 1u) {
        // Own channel straight from the CFA, the other from RCD 3.
        let v = rb[i];
        rgb = select(vec3<f32>(v.x, green[i], cfa[i]), vec3<f32>(cfa[i], green[i], v.y), own == 0u);
    } else {
        let c0 = vh(x, y);
        let nb = 0.25 * (vh(x - 1, y - 1) + vh(x + 1, y - 1) + vh(x - 1, y + 1) + vh(x + 1, y + 1));
        let disc = select(c0, nb, abs(0.5 - c0) < abs(0.5 - nb));
        let g0 = green[i];
        let n1 = RCD_EPS + abs(g0 - g(x, y - 2));
        let s1 = RCD_EPS + abs(g0 - g(x, y + 2));
        let w1 = RCD_EPS + abs(g0 - g(x - 2, y));
        let e1 = RCD_EPS + abs(g0 - g(x + 2, y));
        let gn = g(x, y - 1);
        let gs = g(x, y + 1);
        let gw = g(x - 1, y);
        let ge = g(x + 1, y);
        var out = vec2<f32>(0.0);
        for (var k = 0u; k < 2u; k++) {
            let c = k * 2u;
            let rn = chroma(x, y - 1, c);
            let rs = chroma(x, y + 1, c);
            let rw_ = chroma(x - 1, y, c);
            let re = chroma(x + 1, y, c);
            let sn = abs(rn - rs);
            let ew = abs(rw_ - re);
            let n_grad = n1 + sn + abs(rn - chroma(x, y - 3, c));
            let s_grad = s1 + sn + abs(rs - chroma(x, y + 3, c));
            let w_grad = w1 + ew + abs(rw_ - chroma(x - 3, y, c));
            let e_grad = e1 + ew + abs(re - chroma(x + 3, y, c));
            let n_est = rn - gn;
            let s_est = rs - gs;
            let w_est = rw_ - gw;
            let e_est = re - ge;
            let v_est = (n_grad * s_est + s_grad * n_est) / (n_grad + s_grad);
            let h_est = (e_grad * w_est + w_grad * e_est) / (e_grad + w_grad);
            out[k] = g0 + disc * (h_est - v_est) + v_est;
        }
        rgb = vec3<f32>(out.x, g0, out.y);
    }
    // Bands write at their row offset (u.v[9].z) with the full rectangle's stride (.w).
    outb[(gid.y + u32(u.v[9].z)) * u32(u.v[9].w) + gid.x] = base_put(max(rgb, vec3<f32>(0.0)));
}
