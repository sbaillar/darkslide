// RCD helpers. Requires `u: P` (BASE block) and `cfa: array<f32>`.
// The kernels run on a grid = the wanted sensor rectangle plus a margin (u.v[0].xy are the
// grid dims, u.v[8] = grid origin x, y in the sensor and the rectangle's offset in the grid,
// u.v[9].xy the rectangle size). Out-of-grid neighbours are mirrored without repeating the
// edge (CFA phase preserved); at the sensor edge that is exactly the CPU's padding, and
// elsewhere the margin keeps it away from the rectangle.

const RCD_EPS: f32 = 1e-5;
const RCD_EPSSQ: f32 = 1e-10;

fn rw() -> i32 {
    return i32(u.v[0].x);
}

fn rh() -> i32 {
    return i32(u.v[0].y);
}

fn rf(i: i32, n: i32) -> i32 {
    // Repeated mirror (triangle wave of period 2(n-1)); matches the CPU's `reflect`.
    if (n <= 1) {
        return 0;
    }
    let p = 2 * (n - 1);
    var r = i % p;
    if (r < 0) {
        r = r + p;
    }
    if (r >= n) {
        r = p - r;
    }
    return r;
}

fn ridx(x: i32, y: i32) -> u32 {
    return u32(rf(y, rh()) * rw() + rf(x, rw()));
}

fn cf(x: i32, y: i32) -> f32 {
    return cfa[ridx(x, y)];
}

fn site_color(x: i32, y: i32) -> u32 {
    // The grid origin is even, so local parity is sensor parity.
    return u32(u.v[1][u32(((y & 1) << 1u) | (x & 1))]);
}

fn sqr(v: f32) -> f32 {
    return v * v;
}
