//! Packing of the compiled plan into GPU uniform blocks and storage arrays. Slot indices
//! must match `shaders/layout.wgsl`.

use ds_core::RawImage;
use ds_core::adjust::MaskOp;

use crate::color_edit::{BalanceParams, OkRange};
use crate::demosaic::{BaseParams, site_info};
use crate::geometry::Affine;
use crate::local::gaussian_kernel;
use crate::mask::{CompiledMask, Raster, ShapeKind};
use crate::plan::{LayerPlan, Plan};
use crate::tone::MID_GRAY;

pub use super::context::Block;

/// Sharpen kernels up to this half-length (radius 1, a 3×3 blur) are applied inline in the
/// output kernel; larger ones use two separable passes, which measure faster.
pub const INLINE_SHARPEN_TAPS: usize = 2;

/// A uniform block from leading rows (rest zero).
pub fn block(rows: &[[f32; 4]]) -> Block {
    let mut b = [[0.0; 4]; 64];
    b[..rows.len()].copy_from_slice(rows);
    b
}

fn row3(v: [f32; 3], w: f32) -> [f32; 4] {
    [v[0], v[1], v[2], w]
}

fn flag(b: bool) -> f32 {
    if b { 1.0 } else { 0.0 }
}

/// BASE kernel parameters: output (or RCD grid) size, CFA sites, reference WB, and for
/// RCD the grid origin in the sensor, the wanted rectangle's offset in the grid and its size.
pub fn base_block(
    raw: &RawImage,
    p: &BaseParams,
    w: u32,
    h: u32,
    grid: [u32; 4],
    rect: [u32; 4],
) -> Block {
    let si = site_info(raw);
    let mut b = block(&[
        [w as f32, h as f32, raw.size.width as f32, 0.0],
        si.color.map(|c| c as f32),
        si.black,
        si.inv_range,
        row3(p.mults, 0.0),
    ]);
    b[8] = grid.map(|v| v as f32);
    // Rectangle size, then the row offset and row stride of the output buffer (bands).
    b[9] = rect.map(|v| v as f32);
    b
}

/// Uniform for the output pass over an already developed `size` image (export resize):
/// output matrix, optional (output) sharpening from a blurred luma plane, histogram flag.
/// `mapped`: the values are output-space and gamut-mapped already (resized exports), so
/// the pass must not compress the gamut again.
pub fn out_block(
    size: ds_core::Size,
    out_matrix: &crate::color::Mat3,
    sharpen: Option<&crate::plan::SharpenPlan>,
    histogram: bool,
    mapped: bool,
) -> Block {
    let mut b = [[0.0; 4]; 64];
    b[0] = [size.width as f32, size.height as f32, 0.0, 0.0];
    b[17] = row3(out_matrix[0], 0.0);
    b[18] = row3(out_matrix[1], 0.0);
    b[19] = row3(out_matrix[2], 0.0);
    b[22] = sharpen.map_or([0.0; 4], |s| [s.gain, s.threshold, 1.0, 0.0]);
    b[48] = [0.0, 0.0, size.width as f32, size.height as f32];
    b[49] = [0.0, 0.0, 0.0, flag(histogram)];
    b[50] = [flag(mapped), 0.0, 0.0, 0.0];
    b
}

/// Heal/clone spots, 2 vec4 each: (heal, center iso x, y, radius), (feather, opacity,
/// source offset x, y in output px); then their [`crate::spots::Bins`] for a `w × h`
/// output as exact f32 words, offsets then indices (word offsets in `D_SPN`).
pub fn spots(spots: &[crate::spots::SpotPlan], w: u32, h: u32) -> Vec<[f32; 4]> {
    let bins = crate::spots::bins(spots, w, h);
    let mut words: Vec<f32> = spots
        .iter()
        .flat_map(|s| {
            [
                flag(s.heal),
                s.c[0],
                s.c[1],
                s.r,
                s.feather,
                s.opacity,
                s.d[0],
                s.d[1],
            ]
        })
        .collect();
    words.extend(bins.offsets.iter().chain(&bins.indices).map(|&v| v as f32));
    words.resize(words.len().div_ceil(4) * 4, 0.0);
    words.as_chunks::<4>().0.to_vec()
}

/// Separable convolution parameters (half kernel ≤ 20 taps).
pub fn sep_block(w: u32, h: u32, kernel: &[f32], step: u32, vertical: bool) -> Block {
    let mut b = block(&[
        [w as f32, h as f32, (kernel.len() - 1) as f32, step as f32],
        [flag(vertical), 0.0, 0.0, 0.0],
    ]);
    for (i, k) in kernel.iter().take(20).enumerate() {
        b[2 + i / 4][i % 4] = *k;
    }
    b
}

/// Grid sizes the develop uniform needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dims {
    pub base: (u32, u32),
    pub hdr: (u32, u32),
    pub clarity: (u32, u32),
    pub structure: (u32, u32),
    pub nr: (u32, u32),
    pub texture: (u32, u32),
    pub dehaze: (u32, u32),
}

/// All lookup tables and color ranges for one render, with offsets.
#[derive(Debug, Clone, PartialEq)]
pub struct Tables {
    pub data: Vec<f32>,
    /// Base tone table (log domain).
    pub base: u32,
    /// First of three consecutive per-channel encoded tables, if any.
    pub chan: Option<u32>,
    pub luma: Option<u32>,
    pub fwd: u32,
    pub inv: u32,
    /// Profile maps (hue/sat map, look table; entries of 3 floats) and the working ↔
    /// ROMM matrices (18 floats, rows).
    pub hue_sat: Option<u32>,
    pub look: Option<u32>,
    pub pp: u32,
    /// Per layer: (chan offsets, luma offset).
    pub layer_curves: Vec<Option<([u32; 3], Option<u32>)>>,
    pub ranges: Vec<[f32; 4]>,
    /// Per layer range offset (vec4 index).
    pub layer_ranges: Vec<u32>,
}

fn push(data: &mut Vec<f32>, t: &[f32]) -> u32 {
    let off = data.len() as u32;
    data.extend_from_slice(t);
    off
}

fn push_ranges(out: &mut Vec<[f32; 4]>, rs: &[OkRange]) -> u32 {
    let off = out.len() as u32;
    for r in rs {
        out.push([r.center, r.half, r.smooth, r.hue_shift]);
        out.push([r.sat, r.light, r.hue_pull, 0.0]);
    }
    off
}

impl Tables {
    pub fn build(plan: &Plan) -> Self {
        let base_luts = &*plan.profile.luts;
        let mut data = Vec::new();
        let base = push(&mut data, &plan.luts.base);
        let chan = plan.luts.chan.as_ref().map(|c| {
            let o = push(&mut data, &c[0]);
            push(&mut data, &c[1]);
            push(&mut data, &c[2]);
            o
        });
        let luma = plan.luts.luma.as_ref().map(|t| push(&mut data, t));
        let fwd = push(&mut data, &base_luts.fwd);
        let inv = push(&mut data, &base_luts.inv);
        let mut map = |m: &Option<std::sync::Arc<crate::profile::HsvMap>>| {
            m.as_ref().map(|m| push(&mut data, m.data().as_flattened()))
        };
        let pr = &plan.profile;
        let (hue_sat, look) = (map(&pr.hue_sat), map(&pr.look));
        let pp = push(&mut data, pr.to_pp.as_flattened());
        push(&mut data, pr.from_pp.as_flattened());
        let layer_curves = plan
            .layers
            .iter()
            .map(|lp| {
                lp.curves.as_ref().map(|c| {
                    let ch = [0, 1, 2].map(|k| push(&mut data, &c.chan[k]));
                    (ch, c.luma.as_ref().map(|t| push(&mut data, t)))
                })
            })
            .collect();
        let mut ranges = Vec::new();
        push_ranges(&mut ranges, &plan.ranges);
        let layer_ranges = plan
            .layers
            .iter()
            .map(|lp| push_ranges(&mut ranges, &lp.ranges))
            .collect();
        Self {
            data,
            base,
            chan,
            luma,
            fwd,
            inv,
            hue_sat,
            look,
            pp,
            layer_curves,
            ranges,
            layer_ranges,
        }
    }
}

fn affine_rows(m: [f32; 6]) -> ([f32; 4], [f32; 4]) {
    ([m[0], m[1], m[2], 0.0], [m[3], m[4], m[5], 0.0])
}

fn balance_rows(b: &BalanceParams) -> [[f32; 4]; 4] {
    [
        row3(b.master, flag(!b.is_identity())),
        row3(b.shadows, 0.0),
        row3(b.midtones, 0.0),
        row3(b.highlights, 0.0),
    ]
}

/// Where the resident BASE sits in its level grid.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BaseView {
    pub scale: f32,
    pub level: [u32; 2],
    pub origin: [u32; 2],
}

/// Ask the develop block's output pass for the display histogram (the layout lives here,
/// next to [`dev_block`]).
pub fn set_histogram(b: &mut Block) {
    b[49][3] = 1.0;
}

/// The develop uniform block.
pub fn dev_block(
    plan: &Plan,
    d: &Dims,
    t: &Tables,
    bv: &BaseView,
    out_matrix: &crate::color::Mat3,
) -> Block {
    let g = &plan.geo;
    let (vig0, vig1) = affine_rows(g.to_vignette.to_f32());
    let (n0, n1) = affine_rows(g.to_norm.to_f32());
    let (amount, mid) = plan.vignette.unwrap_or((0.0, 0.5));
    let bal = balance_rows(&plan.balance);
    let m = out_matrix;
    let iso = g.iso();
    let hdr_global = plan.highlights != 0.0 || plan.shadows != 0.0;
    let det_global = plan.clarity != 0.0 || plan.structure != 0.0 || plan.texture != 0.0;
    let sh = plan.sharpen.as_ref();
    let mut b = block(&[
        [
            g.out.width as f32,
            g.out.height as f32,
            d.base.0 as f32,
            d.base.1 as f32,
        ],
        // Slots 1–2: the profile maps (filled below).
        [0.0; 4],
        [0.0; 4],
        vig0,
        vig1,
        [
            plan.exposure_gain,
            amount,
            mid,
            flag(plan.vignette.is_some()),
        ],
        [
            plan.highlights,
            plan.shadows,
            plan.hdr_gp.s as f32,
            flag(hdr_global),
        ],
        [d.hdr.0 as f32, d.hdr.1 as f32, flag(plan.needs_hdr()), 0.0],
        [
            t.base as f32,
            t.chan.map_or(-1.0, |o| o as f32),
            0.0,
            t.luma.map_or(-1.0, |o| o as f32),
        ],
        [
            crate::lut::TONE_N as f32,
            crate::lut::ENC_N as f32,
            t.fwd as f32,
            t.inv as f32,
        ],
        [bal[0][0], bal[0][1], bal[0][2], plan.saturation],
        [bal[1][0], bal[1][1], bal[1][2], plan.ranges.len() as f32],
        [bal[2][0], bal[2][1], bal[2][2], 0.0],
        [bal[3][0], bal[3][1], bal[3][2], bal[0][3]],
        [
            plan.clarity,
            plan.structure,
            flag(plan.needs_detail()),
            flag(det_global),
        ],
        [
            plan.clarity_gp.s as f32,
            d.clarity.0 as f32,
            d.clarity.1 as f32,
            0.0,
        ],
        [
            plan.structure_gp.s as f32,
            d.structure.0 as f32,
            d.structure.1 as f32,
            0.0,
        ],
        row3(m[0], 0.0),
        row3(m[1], 0.0),
        row3(m[2], 0.0),
        [plan.nr.t1, plan.nr.t2, plan.nr.chroma, plan.nr.gp.s as f32],
        [
            d.nr.0 as f32,
            d.nr.1 as f32,
            flag(plan.nr.luma_active()),
            flag(plan.nr.chroma_active()),
        ],
        [
            sh.map_or(0.0, |s| s.gain),
            sh.map_or(0.0, |s| s.threshold),
            flag(sh.is_some()),
            sh.filter(|s| s.kernel.len() <= INLINE_SHARPEN_TAPS)
                .map_or(0.0, |s| (s.kernel.len() - 1) as f32),
        ],
        n0,
        n1,
        [iso[0], iso[1], g.long_edge_out(), MID_GRAY.log2()],
        [plan.profile.pivot, 0.0, 0.0, 0.0],
    ]);
    // Profile maps: offset (−1 none), hue, saturation, value divisions; then the
    // ProPhoto matrices' offset, the maps' sRGB value flags, the hue/sat value scale.
    let pr = &plan.profile;
    let map_row = |m: &Option<std::sync::Arc<crate::profile::HsvMap>>, off: Option<u32>| {
        m.as_ref().zip(off).map_or([-1.0, 0.0, 0.0, 0.0], |(m, o)| {
            let d = m.dims();
            [o as f32, d[0] as f32, d[1] as f32, d[2] as f32]
        })
    };
    b[1] = map_row(&pr.hue_sat, t.hue_sat);
    b[2] = map_row(&pr.look, t.look);
    b[62] = [
        t.pp as f32,
        flag(pr.hue_sat.as_ref().is_some_and(|m| m.srgb_value())),
        flag(pr.look.as_ref().is_some_and(|m| m.srgb_value())),
        pr.hue_sat_v,
    ];
    let o = g.optics_f32();
    let (mut e0, e1) = affine_rows(o.to_e);
    // D_TOE0.w: average 2×2 geometry samples (Plan::supersample).
    e0[3] = if plan.supersample { 1.0 } else { 0.0 };
    let sc = f64::from(bv.scale);
    let etb = g.e_to_sensor.then(&Affine::scale(sc, sc)).to_f32();
    let (b0, b1) = affine_rows(etb);
    let k = o.keystone;
    let lm = o.model;
    let rows: [[f32; 4]; 18] = [
        e0,
        e1,
        [k[0], k[1], k[2], 0.0],
        [k[3], k[4], k[5], 0.0],
        [k[6], k[7], k[8], 0.0],
        [o.center[0], o.center[1], o.r_dist, o.r_vig],
        row3(lm.dist, flag(o.has_lens)),
        row3(lm.tca_red, flag(o.has_keystone)),
        row3(lm.tca_blue, 0.0),
        row3(lm.vig, 0.0),
        b0,
        b1,
        [
            bv.level[0] as f32,
            bv.level[1] as f32,
            bv.origin[0] as f32,
            bv.origin[1] as f32,
        ],
        row3(plan.wb_ratio, 0.0),
        row3(plan.clip, 0.0),
        row3(plan.cam_matrix[0], 0.0),
        row3(plan.cam_matrix[1], 0.0),
        row3(plan.cam_matrix[2], 0.0),
    ];
    b[27..45].copy_from_slice(&rows);
    let off = |gp: &crate::local::GuidedParams| [gp.off[0] as f32, gp.off[1] as f32];
    let (ho, co, so, no) = (
        off(&plan.hdr_gp),
        off(&plan.clarity_gp),
        off(&plan.structure_gp),
        off(&plan.nr.gp),
    );
    b[46] = [ho[0], ho[1], co[0], co[1]];
    b[47] = [so[0], so[1], no[0], no[1]];
    b[51] = [
        plan.vibrance,
        plan.texture,
        plan.dehaze,
        flag(plan.texture != 0.0),
    ];
    b[52] = [
        plan.texture_gp.s as f32,
        d.texture.0 as f32,
        d.texture.1 as f32,
        0.0,
    ];
    b[53] = row3(plan.airlight, flag(plan.needs_dehaze()));
    b[54] = [
        plan.dehaze_gp.s as f32,
        d.dehaze.0 as f32,
        d.dehaze.1 as f32,
        0.0,
    ];
    let (to, dz) = (off(&plan.texture_gp), off(&plan.dehaze_gp));
    b[55] = [to[0], to[1], dz[0], dz[1]];
    if let Some(inv) = g.to_norm.inverse() {
        let (i0, i1) = affine_rows(inv.to_f32());
        b[56] = i0;
        b[57] = i1;
    }
    if !plan.spots.is_empty() {
        // Word offsets of the spot bins in the `spots` buffer (see [`spots`]).
        let bin = crate::spots::BIN;
        let (tx, ty) = (
            g.out.width.div_ceil(bin).max(1),
            g.out.height.div_ceil(bin).max(1),
        );
        let offs = 8 * plan.spots.len() as u32;
        b[58] = [
            plan.spots.len() as f32,
            tx as f32,
            offs as f32,
            (offs + tx * ty + 1) as f32,
        ];
    }
    b[59] = [
        flag(plan.needs_clarity_plane()),
        flag(plan.needs_structure_plane()),
        flag(plan.needs_texture_plane()),
        0.0,
    ];
    let (fk, fg) = plan.fine;
    b[60] = [fk[0], fk[1], fk[2], plan.detail_noise];
    b[61] = [fg, 0.0, 0.0, 0.0];
    if let Some(gr) = &plan.grain {
        b[63] = [gr.amp, gr.cell, gr.rough, 1.0];
    }
    let win = plan.window.unwrap_or([0, 0, g.out.width, g.out.height]);
    b[48] = win.map(|v| v as f32);

    if let Some(to) = plan.upscale {
        b[49] = [to.width as f32, to.height as f32, 1.0, 0.0];
    }
    if let Some(s) = sh.filter(|s| s.kernel.len() <= INLINE_SHARPEN_TAPS) {
        for (i, k) in s.kernel.iter().enumerate() {
            b[45][i] = *k;
        }
    }
    b
}

/// How a layer mask is feathered on the GPU (mirrors `mask::feather`).
#[derive(Debug, Clone, PartialEq)]
pub enum Feather {
    None,
    Full {
        kernel: Vec<f32>,
    },
    Low {
        f: u32,
        lw: u32,
        lh: u32,
        kernel: Vec<f32>,
    },
}

impl Feather {
    pub fn new(sigma: f32, w: u32, h: u32) -> Self {
        if sigma < 0.5 {
            return Self::None;
        }
        let f = ((sigma / 4.0).floor() as u32).max(1);
        let kernel = gaussian_kernel(sigma / f as f32);
        if f == 1 {
            Self::Full { kernel }
        } else {
            Self::Low {
                f,
                lw: w.div_ceil(f),
                lh: h.div_ceil(f),
                kernel,
            }
        }
    }

    fn row(&self) -> [f32; 4] {
        match self {
            Self::None => [0.0; 4],
            Self::Full { .. } => [1.0, 1.0, 0.0, 0.0],
            Self::Low { f, lw, lh, .. } => [2.0, *f as f32, *lw as f32, *lh as f32],
        }
    }
}

/// Packed mask shapes for all layers, and each layer's raster layout (offsets in floats
/// into that layer's own raster buffer).
#[derive(Debug, Clone)]
pub struct Shapes {
    pub data: Vec<[f32; 4]>,
    pub offsets: Vec<u32>,
    pub layouts: Vec<Vec<RasterSlot>>,
}

fn op_code(op: MaskOp) -> f32 {
    match op {
        MaskOp::Add => 0.0,
        MaskOp::Subtract => 1.0,
        MaskOp::Intersect => 2.0,
    }
}

/// Where a layer raster lives in the layer's raster buffer (offsets in floats): the
/// uploaded plane at `src`, and what shapes read at `read` (a GPU-feathered copy right
/// after it when the raster has a feather, else `src`).
#[derive(Debug, Clone)]
pub struct RasterSlot {
    pub raster: Raster,
    pub src: u32,
    pub read: u32,
}

pub fn shapes(masks: &[CompiledMask]) -> Shapes {
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    let mut layouts = Vec::new();
    for cm in masks {
        offsets.push(data.len() as u32);
        let mut layout = Vec::new();
        let mut off = 0u32;
        for r in &cm.rasters {
            let n = (r.plane.w * r.plane.h) as u32;
            // A GPU brush keeps combined, base and stroke planes (combined first).
            let own = if r.gpu_brush { 3 * n } else { n };
            let read = if r.feather > 0.0 { off + own } else { off };
            layout.push(RasterSlot {
                raster: r.clone(),
                src: off,
                read,
            });
            off += own + if r.feather > 0.0 { n } else { 0 };
        }
        for s in &cm.shapes {
            let op = op_code(s.op);
            let (kind, a, b, raster) = match s.kind {
                ShapeKind::Fill => (0.0, [0.0; 4], [0.0; 4], 0),
                ShapeKind::Linear { s, e } => (1.0, [s[0], s[1], e[0], e[1]], [0.0; 4], 0),
                ShapeKind::Radial { c, r, cs, feather } => (
                    2.0,
                    [c[0], c[1], r[0], r[1]],
                    [cs[0], cs[1], feather, 0.0],
                    0,
                ),
                ShapeKind::ColorRange(r) => (3.0, [r.center, r.half, r.smooth, 0.0], [0.0; 4], 0),
                ShapeKind::Raster(i) => {
                    let sl: &RasterSlot = &layout[i];
                    (
                        4.0,
                        [sl.raster.plane.w as f32, sl.raster.plane.h as f32, 0.0, 0.0],
                        [0.0; 4],
                        sl.read,
                    )
                }
                ShapeKind::Plane { plane, raster } => match raster {
                    Some(i) => {
                        let sl: &RasterSlot = &layout[i];
                        (
                            5.0,
                            [
                                plane as f32,
                                1.0,
                                sl.raster.plane.w as f32,
                                sl.raster.plane.h as f32,
                            ],
                            [0.0; 4],
                            sl.read,
                        )
                    }
                    None => (5.0, [plane as f32, 0.0, 0.0, 0.0], [0.0; 4], 0),
                },
            };
            data.push([kind, op, f32::from_bits(raster), 0.0]);
            data.push(a);
            data.push(b);
        }
        layouts.push(layout);
    }
    Shapes {
        data,
        offsets,
        layouts,
    }
}

/// Per-layer uniform block.
pub fn layer_block(
    lp: &LayerPlan,
    cm: &CompiledMask,
    t: &Tables,
    li: usize,
    shape_off: u32,
    f: &Feather,
) -> Block {
    let curves = t.layer_curves.get(li).copied().flatten();
    let cur = match curves {
        Some((c, l)) => [
            c[0] as f32,
            c[1] as f32,
            c[2] as f32,
            l.map_or(-1.0, |o| o as f32),
        ],
        None => [-1.0; 4],
    };
    let bal = balance_rows(&lp.balance);
    let lr = cm
        .luma
        .map_or([0.0; 4], |l| [l.low, l.high, l.low_falloff, l.high_falloff]);
    block(&[
        [lp.opacity, flag(lp.retone), lp.ev, lp.highlights],
        [lp.shadows, lp.contrast_k, lp.brightness_g, lp.saturation],
        row3(lp.wb[0], 0.0),
        row3(lp.wb[1], 0.0),
        row3(lp.wb[2], 0.0),
        cur,
        [
            flag(curves.is_some()),
            lp.ranges.len() as f32,
            t.layer_ranges.get(li).copied().unwrap_or(0) as f32,
            lp.clarity,
        ],
        [
            lp.structure,
            flag(cm.invert),
            flag(cm.luma.is_some()),
            cm.shapes.len() as f32,
        ],
        lr,
        f.row(),
        bal[0],
        bal[1],
        bal[2],
        bal[3],
        [shape_off as f32, 0.0, 0.0, 0.0],
        [lp.vibrance, lp.texture_amount(), lp.dehaze, lp.sharpening],
        [lp.whites_f, lp.blacks, crate::tone::MID_GRAY, 0.0],
        cm.color
            .map_or([0.0; 4], |r| [r.center, r.half, r.smooth, 1.0]),
    ])
}

/// Lens blur kernels' uniform (see `shaders/bokeh_*.wgsl`) for a `w × h` render.
pub fn bokeh_block(lb: &crate::effects::LensBlurPlan, w: u32, h: u32) -> Block {
    use crate::effects::CocSource;
    let (lw, lh) = lb.grid(w as usize, h as usize);
    let mut b = block(&[
        [lb.rmax, 0.0, 0.0, 0.0],
        [0.0, 0.0, crate::effects::DEPTH_RAMP, 0.0],
        [0.0; 4],
        [0.0, 1.0, 0.0, 0.0],
        // Grid: size, block, gather radius; phase, shape, boost.
        [lw as f32, lh as f32, lb.f as f32, lb.rlow as f32],
        [
            lb.off[0] as f32,
            lb.off[1] as f32,
            lb.shape as f32,
            lb.boost,
        ],
        [w as f32, h as f32, lb.f as f32, lb.boost],
        [lw as f32, lh as f32, lb.off[0] as f32, lb.off[1] as f32],
    ]);
    match &lb.source {
        CocSource::Depth { map, focus, range } => {
            b[0] = [lb.rmax, *focus, *range, 0.0];
            b[1][0] = map.size.width as f32;
            b[1][1] = map.size.height as f32;
        }
        CocSource::Plane {
            center,
            normal,
            width,
            feather,
        } => {
            b[0][3] = 1.0;
            b[2] = [center[0], center[1], normal[0], normal[1]];
            b[3] = [*width, *feather, 0.0, 0.0];
        }
    }
    b
}

/// Rows `at..` of a [`bokeh_block`] as the leading rows of a kernel's own block.
pub fn rows_from(b: &Block, at: usize) -> Block {
    let mut o = [[0.0; 4]; 64];
    o[..64 - at].copy_from_slice(&b[at..]);
    o
}

/// The depth map as little-endian packed bytes (4 per `u32` word).
pub fn depth_words(map: &crate::mask::MaskBitmap) -> Vec<u8> {
    let mut d = map.data.clone();
    d.resize(d.len().next_multiple_of(4).max(4), 0);
    d
}

/// Glow/halation grid pass parameters: `[w, h, f, mode]`, `[lw, lh, phase x, y]`.
pub fn overlay_block(ov: &crate::effects::BlurOverlay, w: u32, h: u32, mode: f32) -> Block {
    let (lw, lh) = ov.grid(w as usize, h as usize);
    block(&[
        [w as f32, h as f32, ov.f as f32, mode],
        [lw as f32, lh as f32, ov.off[0] as f32, ov.off[1] as f32],
    ])
}

/// A vec4 Gaussian pass over a `lw × lh` grid (half kernel ≤ 248 taps).
pub fn blur4_block(lw: usize, lh: usize, kernel: &[f32], vertical: bool) -> Block {
    let mut b = block(&[[
        lw as f32,
        lh as f32,
        (kernel.len() - 1) as f32,
        flag(vertical),
    ]]);
    for (i, k) in kernel.iter().take(248).enumerate() {
        b[2 + i / 4][i % 4] = *k;
    }
    b
}

/// The glow/halation composite's parameters (see `shaders/fx_apply.wgsl`).
pub fn fx_apply_block(fx: &crate::effects::FxPlan, w: u32, h: u32) -> Block {
    let grid = |ov: &crate::effects::BlurOverlay| {
        let (lw, lh) = ov.grid(w as usize, h as usize);
        (
            [ov.f as f32, lw as f32, lh as f32, 0.0],
            [ov.off[0] as f32, ov.off[1] as f32, 0.0, 0.0],
        )
    };
    let mut b = block(&[[
        w as f32,
        h as f32,
        fx.glow.as_ref().map_or(0.0, |g| g.amount),
        fx.halation.as_ref().map_or(0.0, |g| g.amount),
    ]]);
    if let Some(g) = &fx.glow {
        (b[1], b[2]) = grid(g);
    }
    if let Some(g) = &fx.halation {
        (b[3], b[4]) = grid(g);
    }
    b[5] = row3(crate::effects::HALATION_TINT, 0.0);
    b
}
