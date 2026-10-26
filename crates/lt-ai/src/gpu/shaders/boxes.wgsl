// One workgroup per perceptual-hash box row (`phash::box_sums`): the luma
// sums of that row's ≤ 64 boxes, plus the frame's 256-bin luma histogram
// (exposure) over the same pixels, both in workgroup memory. Every pixel
// lies in exactly one box, so the histogram counts each pixel once.
// Box `b` of `n` spans `b * len / n .. (b + 1) * len / n`.

//INCLUDE_COMMON

const WG: u32 = 256u;

@group(0) @binding(3) var<storage, read_write> boxes: array<u32>;
@group(0) @binding(4) var<storage, read_write> hist: array<atomic<u32>>;

var<workgroup> lh: array<atomic<u32>, 256>;
var<workgroup> lb: array<atomic<u32>, 64>;

// Image owning batch-wide box row `item`.
fn image_of(item: u32) -> u32 {
    var lo = 0u;
    var hi = params.n_images;
    while (hi - lo > 1u) {
        let mid = (lo + hi) / 2u;
        if (metas[mid].box_row_base <= item) { lo = mid; } else { hi = mid; }
    }
    return lo;
}

@compute @workgroup_size(256, 1, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let item = linear(wg);
    if (item >= params.n_items) {
        return;
    }
    atomicStore(&lh[li], 0u);
    if (li < 64u) {
        atomicStore(&lb[li], 0u);
    }
    workgroupBarrier();

    let img = image_of(item);
    let m = metas[img];
    let by = item - m.box_row_base;
    let ya = by * m.h / m.gh;
    let yb = (by + 1u) * m.h / m.gh;
    // Threads walk (row, box) pairs, neighbours on adjacent runs of a row.
    let count = m.gw * (yb - ya);
    for (var it = li; it < count; it += WG) {
        let bx = it % m.gw;
        let y = ya + it / m.gw;
        let xa = bx * m.w / m.gw;
        let xb = (bx + 1u) * m.w / m.gw;
        let base = m.off + y * m.stride;
        var s = 0;
        for (var k = xa / 4u; 4u * k < xb; k++) {
            let px = unpack(luma[base + k]);
            let v = lanes(k, xa, xb);
            s += dot(px, v);
            if (v.x != 0) { atomicAdd(&lh[px.x], 1u); }
            if (v.y != 0) { atomicAdd(&lh[px.y], 1u); }
            if (v.z != 0) { atomicAdd(&lh[px.z], 1u); }
            if (v.w != 0) { atomicAdd(&lh[px.w], 1u); }
        }
        atomicAdd(&lb[bx], bitcast<u32>(s));
    }
    workgroupBarrier();

    if (li < m.gw) {
        boxes[m.box_base + by * m.gw + li] = atomicLoad(&lb[li]);
    }
    let c = atomicLoad(&lh[li]);
    if (c != 0u) {
        atomicAdd(&hist[img * 256u + li], c);
    }
}
