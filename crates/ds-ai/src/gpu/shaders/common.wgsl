// Shared by every ds-ai kernel: the batch layout and 4-pixel word access.
//
// All images of a batch live in one buffer of u32 words. Every image row
// starts on a word boundary (`stride` words per row, the tail of the last
// word zero), so a thread works on four horizontally adjacent pixels as one
// `vec4<i32>`, with its left/right neighbours taken from the adjacent words
// by swizzles (no dynamically indexed arrays, which Metal spills to memory).

struct Meta {
    w: u32,
    h: u32,
    // Word index of row 0.
    off: u32,
    // Words per row: ceil(w / 4).
    stride: u32,
    tile: u32,
    cols: u32,
    // First stencil rectangle / box / box row of this image in the
    // batch-wide arrays.
    rect_base: u32,
    box_base: u32,
    box_row_base: u32,
    gw: u32,
    gh: u32,
    pad: u32,
};

struct Params {
    n_images: u32,
    // Work items (tiles, box rows or noise jobs) in this dispatch.
    n_items: u32,
    // Workgroups per dispatch row (2-D dispatch past the 65 535 limit).
    row_stride: u32,
    pad: u32,
};

@group(0) @binding(0) var<storage, read> luma: array<u32>;
@group(0) @binding(1) var<storage, read> metas: array<Meta>;
@group(0) @binding(2) var<uniform> params: Params;

// Words k-1, k, k+1 of a row, unpacked (zero past either end of the row).
struct Win {
    p: vec4<i32>,
    c: vec4<i32>,
    q: vec4<i32>,
};

fn unpack(v: u32) -> vec4<i32> {
    return vec4<i32>((vec4<u32>(v) >> vec4<u32>(0u, 8u, 16u, 24u)) & vec4<u32>(0xffu));
}

fn win(m: Meta, y: u32, k: u32) -> Win {
    let base = m.off + y * m.stride;
    var p = 0u;
    if (k > 0u) {
        p = luma[base + k - 1u];
    }
    var q = 0u;
    if (k + 1u < m.stride) {
        q = luma[base + k + 1u];
    }
    return Win(unpack(p), unpack(luma[base + k]), unpack(q));
}

// The four pixels shifted by `s` (1..=4) to the left / right.
fn left(r: Win, s: u32) -> vec4<i32> {
    switch s {
        case 1u: { return vec4<i32>(r.p.w, r.c.xyz); }
        case 2u: { return vec4<i32>(r.p.zw, r.c.xy); }
        case 3u: { return vec4<i32>(r.p.yzw, r.c.x); }
        default: { return r.p; }
    }
}

fn right(r: Win, s: u32) -> vec4<i32> {
    switch s {
        case 1u: { return vec4<i32>(r.c.yzw, r.q.x); }
        case 2u: { return vec4<i32>(r.c.zw, r.q.xy); }
        case 3u: { return vec4<i32>(r.c.w, r.q.xyz); }
        default: { return r.q; }
    }
}

// 1 for the lanes of word `k` whose x lies in `x0..x1`, else 0.
fn lanes(k: u32, x0: u32, x1: u32) -> vec4<i32> {
    let x = vec4<u32>(4u * k) + vec4<u32>(0u, 1u, 2u, 3u);
    return select(vec4<i32>(0), vec4<i32>(1), x >= vec4<u32>(x0) & x < vec4<u32>(x1));
}

// 2-D dispatch → linear workgroup index.
fn linear(wg: vec3<u32>) -> u32 {
    return wg.y * params.row_stride + wg.x;
}
