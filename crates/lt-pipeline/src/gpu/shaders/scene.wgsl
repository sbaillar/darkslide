// DEVELOP 1: geometry (crop/straighten/turns → keystone → lens distortion + lateral CA),
// resample from BASE, develop-time WB, highlight reconstruction, lens vignetting, camera
// matrix, exposure, vignette, gamut pre-compression. Writes linear Rec.2020 + log2 Y.
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<storage, read> base: array<BaseT>;
@group(0) @binding(2) var<storage, read_write> lin: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> spots: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read> sstats: array<vec4<f32>>;

//INCLUDE_SCENE_FN

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = u32(dev.v[D_SIZE].x);
    let h = u32(dev.v[D_SIZE].y);
    if (gid.x >= w || gid.y >= h) {
        return;
    }
    lin[gid.y * w + gid.x] = scene_px(gid);
}
