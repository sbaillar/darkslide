// DEVELOP 1+2 fused (used when no local tone needs the scene image): geometry/WB/matrix
// straight into grading, so the scene-linear image never round-trips through memory.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<storage, read> base: array<BaseT>;
@group(0) @binding(2) var<storage, read> hdr: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> tables: array<f32>;
@group(0) @binding(4) var<storage, read> ranges: array<vec4<f32>>;
@group(0) @binding(5) var<storage, read_write> graded: array<EncT>;
@group(0) @binding(6) var<storage, read_write> ypl: array<f32>;
@group(0) @binding(7) var<storage, read> dhz: array<vec4<f32>>;
@group(0) @binding(8) var<storage, read> spots: array<vec4<f32>>;
@group(0) @binding(9) var<storage, read> sstats: array<vec4<f32>>;

//INCLUDE_SCENE_FN
//INCLUDE_TABLES
//INCLUDE_GRADE_FN

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    let i = gid.y * w + gid.x;
    let e = grade_px(gid, scene_px(gid));
    let y = luma3(e);
    graded[i] = enc_put(vec4<f32>(e, y));
    ypl[i] = y;
}
