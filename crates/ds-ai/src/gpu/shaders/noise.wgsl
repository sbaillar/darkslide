// One workgroup per noise job (image, flat tile, tap spacing): the
// histogram of |Immerkær residual| with taps `step` (1..=4) apart
// (`noise::residual_hist`), four pixels per `vec4<i32>`; with step <= 4 all
// their taps lie in the three words around them.
// Residuals below LOCAL_BINS go through a workgroup histogram; the rare
// larger ones (edges inside a "flat" tile) go straight to the global one.

//INCLUDE_COMMON

struct Job {
    img: u32,
    tile: u32,
    step: u32,
};

const WG: u32 = 128u;
const BINS: u32 = 4081u; // 16 * 255 + 1
const LOCAL_BINS: u32 = 256u;

@group(0) @binding(3) var<storage, read> jobs: array<Job>;
@group(0) @binding(4) var<storage, read_write> hist: array<atomic<u32>>;

var<workgroup> lh: array<atomic<u32>, 256>;

fn bump(base: u32, v: i32) {
    let a = u32(abs(v));
    if (a < LOCAL_BINS) {
        atomicAdd(&lh[a], 1u);
    } else {
        atomicAdd(&hist[base + a], 1u);
    }
}

// Second difference across the row: left - 2 centre + right.
fn d2(r: Win, s: u32) -> vec4<i32> {
    return left(r, s) - 2 * r.c + right(r, s);
}

@compute @workgroup_size(128, 1, 1)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let item = linear(wg);
    if (item >= params.n_items) {
        return;
    }
    for (var k = li; k < LOCAL_BINS; k += WG) {
        atomicStore(&lh[k], 0u);
    }
    workgroupBarrier();

    let job = jobs[item];
    let m = metas[job.img];
    let s = job.step;
    let base = (job.img * 4u + s - 1u) * BINS;
    let tx0 = (job.tile % m.cols) * m.tile;
    let ty0 = (job.tile / m.cols) * m.tile;
    // `noise::residual_bounds`: the kernel must fit inside the frame.
    let x0 = max(tx0, s);
    let y0 = max(ty0, s);
    let x1 = min(min(tx0 + m.tile, m.w), select(0u, m.w - s, m.w >= s));
    let y1 = min(min(ty0 + m.tile, m.h), select(0u, m.h - s, m.h >= s));
    if (x0 < x1 && y0 < y1) {
        let k0 = x0 / 4u;
        let kw = (x1 + 3u) / 4u - k0;
        let count = kw * (y1 - y0);
        for (var i = li; i < count; i += WG) {
            let k = k0 + i % kw;
            let y = y0 + i / kw;
            let r = d2(win(m, y - s, k), s) - 2 * d2(win(m, y, k), s) + d2(win(m, y + s, k), s);
            let v = lanes(k, x0, x1);
            if (v.x != 0) { bump(base, r.x); }
            if (v.y != 0) { bump(base, r.y); }
            if (v.z != 0) { bump(base, r.z); }
            if (v.w != 0) { bump(base, r.w); }
        }
    }
    workgroupBarrier();
    for (var k = li; k < LOCAL_BINS; k += WG) {
        let c = atomicLoad(&lh[k]);
        if (c != 0u) {
            atomicAdd(&hist[base + k], c);
        }
    }
}
