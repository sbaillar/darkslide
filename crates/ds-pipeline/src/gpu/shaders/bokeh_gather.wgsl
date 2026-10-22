// Lens blur 3: occlusion-aware gather on the grid (mirrors effects::gather). Every cell
// whose aperture covers this one contributes, weighted by 1/r² and its highlight gain; a
// cell behind this one spreads no wider than this cell's own blur.
// u.v[0] = lw, lh, f, rlow; [1] = phase x, y, shape (0 circle, 1 blades, 2 cat's eye).
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<uniform> u: P;
@group(0) @binding(2) var<storage, read> lo: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> loz: array<f32>;
@group(0) @binding(4) var<storage, read_write> outp: array<vec4<f32>>;

fn disc(x: f32, y: f32, r: f32) -> f32 {
    return clamp(r - sqrt(x * x + y * y) + 0.5, 0.0, 1.0);
}

fn cover(shape: u32, dx: f32, dy: f32, r: f32, vig: vec2<f32>) -> f32 {
    if (shape == 1u) {
        let m = max(max(abs(dx * 0.9659258 + dy * 0.2588190), abs(dx * 0.2588190 + dy * 0.9659258)),
                    abs(dx * -0.7071068 + dy * 0.7071068));
        return clamp(r - m / 0.8660254 + 0.5, 0.0, 1.0);
    }
    if (shape == 2u) {
        let s = 0.7 * r;
        return min(disc(dx, dy, r), disc(dx + s * vig.x, dy + s * vig.y, r));
    }
    return disc(dx, dy, r);
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let lw = i32(u.v[0].x);
    let lh = i32(u.v[0].y);
    let px = i32(gid.x);
    let py = i32(gid.y);
    if (px >= lw || py >= lh) {
        return;
    }
    let f = u.v[0].z;
    let reach = i32(u.v[0].w) + 1;
    let reach2 = (f32(reach) + 0.5) * (f32(reach) + 0.5);
    let shape = u32(u.v[1].z);
    let p = py * lw + px;
    let zp = loz[p];
    let cp = vec2<f32>((f32(px) + 0.5) * f - u.v[1].x, (f32(py) + 0.5) * f - u.v[1].y);
    let vig = vec2<f32>(row_apply(dev.v[D_VIG0], cp), row_apply(dev.v[D_VIG1], cp));
    var acc = vec3<f32>(0.0);
    var wsum = 0.0;
    for (var dy = -reach; dy <= reach; dy++) {
        let sy = py + dy;
        if (sy < 0 || sy >= lh) {
            continue;
        }
        for (var dx = -reach; dx <= reach; dx++) {
            let sx = px + dx;
            if (sx < 0 || sx >= lw || f32(dx * dx + dy * dy) > reach2) {
                continue;
            }
            let s = sy * lw + sx;
            let c = lo[s];
            if (c.w <= 0.0) {
                continue;
            }
            let zs = loz[s];
            let r = select(min(abs(zs), abs(zp)), abs(zs), zs < zp);
            let cv = cover(shape, -f32(dx), -f32(dy), r, vig);
            if (cv <= 0.0) {
                continue;
            }
            let wt = cv * c.w / max(r * r, 0.25);
            acc += wt * c.xyz;
            wsum += wt;
        }
    }
    if (wsum > 0.0) {
        outp[p] = vec4<f32>(acc / wsum, 1.0);
    } else {
        outp[p] = vec4<f32>(0.0);
    }
}
