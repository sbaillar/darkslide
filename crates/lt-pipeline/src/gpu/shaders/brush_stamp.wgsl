// GPU brush stamping (mirrors mask::BrushRaster::stamp): over the op's box, the
// stroke plane takes the max of its value and the capsule coverage of the nearest
// segment, then the combined plane is recombined with the base. Segments are binned
// into 32×32 tiles of the box on the CPU.
// u.v[0] = raster w, box x0, y0, box w; [1] = box h, tiles across, radius, hardness;
// [2] = flow, erase, combined offset (u32 bits), base offset (bits); [3].x = stroke offset.
@group(0) @binding(0) var<uniform> u: P;
@group(0) @binding(1) var<storage, read> segs: array<vec4<f32>>;
@group(0) @binding(2) var<storage, read> tiles: array<vec2<u32>>;
@group(0) @binding(3) var<storage, read> idx: array<u32>;
@group(0) @binding(4) var<storage, read_write> planes: array<f32>;

fn seg_dist(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let d = b - a;
    let len2 = dot(d, d);
    var t = 0.0;
    if (len2 > 0.0) {
        t = clamp(dot(p - a, d) / len2, 0.0, 1.0);
    }
    return length(p - a - t * d);
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let bw = u32(u.v[0].w);
    let bh = u32(u.v[1].x);
    if (gid.x >= bw || gid.y >= bh) {
        return;
    }
    let w = u32(u.v[0].x);
    let x = u32(u.v[0].y) + gid.x;
    let y = u32(u.v[0].z) + gid.y;
    let radius = u.v[1].z;
    let hard = u.v[1].w;
    let flow = u.v[2].x;
    let t = tiles[(gid.y / 32u) * u32(u.v[1].y) + gid.x / 32u];
    let p = vec2<f32>(f32(x) + 0.5, f32(y) + 0.5);
    let i = y * w + x;
    let so = bitcast<u32>(u.v[3].x);
    var m = planes[so + i];
    for (var k = 0u; k < t.y; k++) {
        let s = segs[idx[t.x + k]];
        let d = seg_dist(p, s.xy, s.zw);
        if (d < radius) {
            m = max(m, flow * (1.0 - ss(hard, 1.0, d / radius)));
        }
    }
    planes[so + i] = m;
    let base = planes[bitcast<u32>(u.v[2].w) + i];
    var c: f32;
    if (u.v[2].y > 0.5) {
        c = base * (1.0 - m);
    } else {
        c = base + m * (1.0 - base);
    }
    planes[bitcast<u32>(u.v[2].z) + i] = c;
}
