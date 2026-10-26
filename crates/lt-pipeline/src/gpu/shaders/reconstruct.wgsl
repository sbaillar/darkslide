// Highlight reconstruction (mirrors demosaic::reconstruct_highlights).
fn reconstruct(v: vec3<f32>, clip: vec3<f32>) -> vec3<f32> {
    let t = vec3<f32>(
        ss(0.95 * clip.x, 0.995 * clip.x, v.x),
        ss(0.95 * clip.y, 0.995 * clip.y, v.y),
        ss(0.95 * clip.z, 0.995 * clip.z, v.z)
    );
    if (t.x + t.y + t.z <= 0.0) {
        return v;
    }
    let sum = v.x + v.y + v.z;
    var o = v;
    for (var c = 0u; c < 3u; c++) {
        if (t[c] > 0.0) {
            let est = max(v[c], 0.5 * (sum - v[c]));
            o[c] = v[c] + (est - v[c]) * t[c];
        }
    }
    let all_t = min(t.x, min(t.y, t.z));
    if (all_t > 0.0) {
        let mx = max(o.x, max(o.y, o.z));
        o = o + (vec3<f32>(mx) - o) * all_t;
    }
    return o;
}
