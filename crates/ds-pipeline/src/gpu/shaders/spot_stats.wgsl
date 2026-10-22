// Heal ring statistics: invocation k·16 + side·8 + sector takes the robust flat-field
// statistic (camera × vignetting gain; trimmed mean of the middle half by luminance of
// the on-image samples; w = 1 when any) over the sector's ring samples around the
// destination (side 0) or source (side 1) of spot k (mirrors spots::ring_points /
// sector_stats).
@group(0) @binding(0) var<uniform> dev: P;
@group(0) @binding(1) var<storage, read> base: array<BaseT>;
@group(0) @binding(2) var<storage, read> spots: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> sout: array<vec4<f32>>;
// Declared for the shared scene code (heal gains); unused here, so not in the layout.
@group(0) @binding(4) var<storage, read> sstats: array<vec4<f32>>;

//INCLUDE_SCENE_FN

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let n = u32(dev.v[D_SPN].x);
    if (gid.y > 0u || gid.x >= n * 16u) {
        return;
    }
    let k = gid.x / 16u;
    let side = (gid.x % 16u) / 8u;
    let j = gid.x % 8u;
    let a = spots[2u * k];
    let b = spots[2u * k + 1u];
    if (a.x < 0.5) {
        sout[gid.x] = vec4<f32>(0.0);
        return;
    }
    let iso = dev.v[D_ISO].xy;
    let rings = array<f32, 3>(1.1, 1.25, 1.4);
    var vals: array<vec3<f32>, 12>;
    var lum: array<f32, 12>;
    var m = 0u;
    geo_clamp = true;
    for (var ai = 0u; ai < 4u; ai++) {
        let t = (f32(j) + (f32(ai) + 0.5) / 4.0) * 6.2831855 / 8.0;
        for (var ri = 0u; ri < 3u; ri++) {
            let rho = rings[ri];
            let u = (a.y + a.w * rho * cos(t)) / iso.x;
            let v = (a.z + a.w * rho * sin(t)) / iso.y;
            var p = vec2<f32>(row_apply(dev.v[D_SPI0], vec2<f32>(u, v)), row_apply(dev.v[D_SPI1], vec2<f32>(u, v)));
            if (side == 1u) {
                p += b.zw;
            }
            geo_off = false;
            let sc = geo_sample(p);
            if (!geo_off) {
                // Stable insertion by luminance.
                let x = sc.xyz * sc.w;
                let y = ring_luma(x);
                var i = m;
                while (i > 0u && lum[i - 1u] > y) {
                    vals[i] = vals[i - 1u];
                    lum[i] = lum[i - 1u];
                    i -= 1u;
                }
                vals[i] = x;
                lum[i] = y;
                m += 1u;
            }
        }
    }
    if (m == 0u) {
        sout[gid.x] = vec4<f32>(0.0);
        return;
    }
    let cut = m / 4u;
    var acc = vec3<f32>(0.0);
    for (var i = cut; i < m - cut; i++) {
        acc += vals[i];
    }
    sout[gid.x] = vec4<f32>(acc / f32(m - 2u * cut), 1.0);
}
