// Layer mask: combine shapes (before feather/invert/luma refinement).
// Shapes: 3 vec4 each — (kind, op, raster offset as u32 bits, _), params A, params B.
// Raster shapes sample an E-frame plane of size A.xy at the pixel's E-normalized (u, v).
// Kinds: 0 fill, 1 linear, 2 radial, 3 color range, 4 raster. Ops: 0 add, 1 subtract, 2 intersect.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<uniform> lay: P;
@group(0) @binding(2) var<storage, read> shapes: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> rasters: array<f32>;
@group(0) @binding(4) var<storage, read> graded: array<EncT>;
@group(0) @binding(5) var<storage, read_write> maskp: array<f32>;
// Output-space planes of this layer's mask (feathered colour ranges, auto-masks): n each.
@group(0) @binding(6) var<storage, read> planes: array<f32>;

fn raster_at(off: u32, rw: u32, rh: u32, uv: vec2<f32>) -> f32 {
    let fx = clamp(uv.x * f32(rw) - 0.5, 0.0, f32(rw - 1u));
    let fy = clamp(uv.y * f32(rh) - 0.5, 0.0, f32(rh - 1u));
    let x0 = u32(fx);
    let y0 = u32(fy);
    let x1 = min(x0 + 1u, rw - 1u);
    let y1 = min(y0 + 1u, rh - 1u);
    let t = vec2<f32>(fx - f32(x0), fy - f32(y0));
    return bilerp1(
        rasters[off + y0 * rw + x0],
        rasters[off + y0 * rw + x1],
        rasters[off + y1 * rw + x0],
        rasters[off + y1 * rw + x1],
        t
    );
}

fn shape_value(kind: u32, a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, p: vec2<f32>, rgb: vec3<f32>, off: u32, i: u32, n: u32) -> f32 {
    switch kind {
        case 0u: {
            return 1.0;
        }
        case 1u: {
            let d = a.zw - a.xy;
            let len2 = dot(d, d);
            if (len2 < 1e-12) {
                return 1.0;
            }
            let t = dot(p - a.xy, d) / len2;
            return 1.0 - ss(0.0, 1.0, t);
        }
        case 2u: {
            if (a.z <= 0.0 || a.w <= 0.0) {
                return 0.0;
            }
            let dx = p.x - a.x;
            let dy = p.y - a.y;
            let qx = b.x * dx + b.y * dy;
            let qy = -b.y * dx + b.x * dy;
            let d = length(vec2<f32>(qx / a.z, qy / a.w));
            return 1.0 - ss(1.0 - b.z, 1.0, d);
        }
        case 3u: {
            let lch = enc_to_oklch(rgb);
            return range_weight(a.x, a.y, a.z, lch.z, lch.y);
        }
        case 5u: {
            let v = planes[u32(a.x) * n + i];
            if (a.y > 0.5) {
                return max(v, raster_at(off, u32(a.z), u32(a.w), uv));
            }
            return v;
        }
        default: {
            return raster_at(off, u32(a.x), u32(a.y), uv);
        }
    }
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let px = vec2<f32>(f32(gid.x) + 0.5, f32(gid.y) + 0.5);
    let iso = dev.v[D_ISO];
    let uv = vec2<f32>(row_apply(dev.v[D_NORM0], px), row_apply(dev.v[D_NORM1], px));
    let p = uv * iso.xy;
    let rgb = enc_get(graded[i]).xyz;
    let count = u32(lay.v[L_D].w);
    let off = u32(lay.v[L_E].x);
    var acc = 0.0;
    for (var k = 0u; k < count; k++) {
        let h0 = shapes[off + 3u * k];
        let v = shape_value(u32(h0.x), shapes[off + 3u * k + 1u], shapes[off + 3u * k + 2u], uv, p, rgb, bitcast<u32>(h0.z), i, w * h);
        let op = u32(h0.y);
        if (op == 0u) {
            acc = max(acc, v);
        } else if (op == 1u) {
            acc = acc * (1.0 - v);
        } else {
            acc = acc * v;
        }
    }
    maskp[i] = acc;
}
