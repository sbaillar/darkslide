// GPU brush fold (mirrors mask::BrushRaster::fold): base = combined, stroke = 0 over
// the box. u.v[0] = raster w, box x0, y0, box w; [1] = box h, combined, base, stroke
// offsets (u32 bits).
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read_write> planes: array<f32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let bw = u32(u.v[0].w);
    let bh = u32(u.v[1].x);
    if (gid.x >= bw || gid.y >= bh) {
        return;
    }
    let i = (u32(u.v[0].z) + gid.y) * u32(u.v[0].x) + u32(u.v[0].y) + gid.x;
    planes[bitcast<u32>(u.v[1].z) + i] = planes[bitcast<u32>(u.v[1].y) + i];
    planes[bitcast<u32>(u.v[1].w) + i] = 0.0;
}
