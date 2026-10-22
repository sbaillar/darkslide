// Auto-mask: the auto strokes' footprint and core coverage (E-frame rasters) sampled at
// each output pixel. u.v[0] = footprint raster w, h, offset (u32 bits);
// u.v[1] = core raster w, h, offset (u32 bits).
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<uniform> u: P;
@group(0) @binding(2) var<storage, read> rasters: array<f32>;
@group(0) @binding(3) var<storage, read_write> cov: array<f32>;
@group(0) @binding(4) var<storage, read_write> core: array<f32>;

fn raster_at(r: vec4<f32>, uv: vec2<f32>) -> f32 {
    let rw = u32(r.x);
    let rh = u32(r.y);
    let off = bitcast<u32>(r.z);
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

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let px = vec2<f32>(f32(gid.x) + 0.5, f32(gid.y) + 0.5);
    let uv = vec2<f32>(row_apply(dev.v[D_NORM0], px), row_apply(dev.v[D_NORM1], px));
    let i = gid.y * w + gid.x;
    cov[i] = raster_at(u.v[0], uv);
    core[i] = raster_at(u.v[1], uv);
}
