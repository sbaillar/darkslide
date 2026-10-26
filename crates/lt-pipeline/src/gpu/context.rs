//! wgpu device, shader assembly, compute pipelines, a named buffer pool (LRU, counted
//! in the memory budget), size checks, device-loss detection and cancellable readback
//! that waits outside the renderer's lock.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::cache::Account;
use crate::color::{self, GAMUT_S, GAMUT_THR, Mat3, REC2020_LUMA};
use crate::lut::{LOG_MAX, LOG_MIN};
use crate::{CancelToken, PipelineError};

const COMMON: &str = include_str!("shaders/common.wgsl");
const LAYOUT: &str = include_str!("shaders/layout.wgsl");
const TABLES: &str = include_str!("shaders/tables.wgsl");
const RCD: &str = include_str!("shaders/rcd_common.wgsl");
const RECONSTRUCT: &str = include_str!("shaders/reconstruct.wgsl");
const SCENE_FN: &str = include_str!("shaders/scene_fn.wgsl");
const GRADE_FN: &str = include_str!("shaders/grade_fn.wgsl");

/// Workgroup shape of every 2-D kernel (wide rows: coalesced row-major access).
pub const WG_X: u32 = 32;
pub const WG_Y: u32 = 8;

/// Uniform block: rows of vec4 (see `shaders/layout.wgsl`).
pub type Block = [[f32; 4]; 64];

/// BASE storage formats: packed f16 (previews) or f32 (export precision).
const BASE_H: &str = "alias BaseT = vec2<u32>;
fn base_get(v: vec2<u32>) -> vec3<f32> { return unpack_rgb(v); }
fn base_put(c: vec3<f32>) -> vec2<u32> { return pack_rgb(c); }
";
const BASE_F: &str = "alias BaseT = vec4<f32>;
fn base_get(v: vec4<f32>) -> vec3<f32> { return v.xyz; }
fn base_put(c: vec3<f32>) -> vec4<f32> { return vec4<f32>(c, 0.0); }
";

/// Display-referred develop image (`enc`/`graded`: rgb + luma) formats: packed 16-bit
/// fixed point over [-0.5, 1.5] for previews (half the memory traffic of the
/// bandwidth-bound develop passes; steps of 3e-5, far below an 8-bit code) or f32 when
/// the BASE is f32 (Full quality: 16-bit export precision).
const ENC_H: &str = "alias EncT = vec2<u32>;
fn enc_get(v: vec2<u32>) -> vec4<f32> { return vec4<f32>(unpack2x16unorm(v.x), unpack2x16unorm(v.y)) * 2.0 - 0.5; }
fn enc_put(c: vec4<f32>) -> vec2<u32> { let q = (c + 0.5) * 0.5; return vec2<u32>(pack2x16unorm(q.xy), pack2x16unorm(q.zw)); }
";
const ENC_F: &str = "alias EncT = vec4<f32>;
fn enc_get(v: vec4<f32>) -> vec4<f32> { return v; }
fn enc_put(c: vec4<f32>) -> vec4<f32> { return c; }
";

fn wgsl_mat(m: &Mat3) -> String {
    format!(
        "mat3x3<f32>(vec3<f32>({:?}, {:?}, {:?}), vec3<f32>({:?}, {:?}, {:?}), vec3<f32>({:?}, {:?}, {:?}))",
        m[0][0], m[1][0], m[2][0], m[0][1], m[1][1], m[2][1], m[0][2], m[1][2], m[2][2]
    )
}

/// Constants shared with the Rust implementation, emitted as WGSL.
fn prelude() -> String {
    let (lms, ok2, ok2_inv, lms_inv) = color::oklab_matrices();
    let mut s = String::new();
    let _ = writeln!(
        s,
        "const LUMA_W = vec3<f32>({:?}, {:?}, {:?});",
        REC2020_LUMA[0], REC2020_LUMA[1], REC2020_LUMA[2]
    );
    let _ = writeln!(s, "const M_LMS = {};", wgsl_mat(&lms));
    let _ = writeln!(s, "const M_OK2 = {};", wgsl_mat(&ok2));
    let _ = writeln!(s, "const M_OK2_INV = {};", wgsl_mat(&ok2_inv));
    let _ = writeln!(s, "const M_LMS_INV = {};", wgsl_mat(&lms_inv));
    let _ = writeln!(
        s,
        "const M_709 = {};",
        wgsl_mat(&color::working_to_output(crate::OutputSpace::Srgb))
    );
    {
        use crate::detail as d;
        let _ = writeln!(s, "const DH_OMEGA: f32 = {:?};", d::DEHAZE_OMEGA);
        let _ = writeln!(s, "const DH_LC: f32 = {:?};", d::DEHAZE_LC);
        let _ = writeln!(s, "const DH_VEIL: f32 = {:?};", d::DEHAZE_VEIL);
        let _ = writeln!(s, "const DH_LC_EPS: f32 = {:?};", d::DEHAZE_LC_EPS);
        let _ = writeln!(s, "const DH_BRIGHT: f32 = {:?};", d::DEHAZE_BRIGHT);
        let _ = writeln!(s, "const DH_SAT: f32 = {:?};", d::DEHAZE_SAT);
        let _ = writeln!(s, "const DH_KEEP: f32 = {:?};", d::DEHAZE_KEEP);
        let v2 = |v: [f32; 2]| format!("vec2<f32>({:?}, {:?})", v[0], v[1]);
        let _ = writeln!(s, "const DH_PY = {};", v2(d::PROTECT_Y));
        let _ = writeln!(s, "const DH_PT = {};", v2(d::PROTECT_TINT));
        let _ = writeln!(s, "const DH_VIVID = {};", v2(d::DEHAZE_VIVID));
        let _ = writeln!(s, "const DH_FLOOR = {};", v2(d::DEHAZE_FLOOR));
        let _ = writeln!(s, "const DH_CV = {};", v2(d::PROTECT_CV));
        let _ = writeln!(
            s,
            "const CR_GUIDE_HUE: f32 = {:?};",
            crate::mask::CR_GUIDE_HUE
        );
        let _ = writeln!(
            s,
            "const CR_GUIDE_SMOOTH: i32 = {};\nconst CR_GUIDE_SIGMA: f32 = {:?};",
            crate::mask::CR_GUIDE_SMOOTH,
            crate::mask::CR_GUIDE_SIGMA
        );
        let _ = writeln!(s, "const TEXTURE_KNEE: f32 = {:?};", d::TEXTURE_KNEE);
        let _ = writeln!(s, "const TEXTURE_BOOST: f32 = {:?};", d::TEXTURE_BOOST);
    }
    let _ = writeln!(s, "const GAMUT_THR: f32 = {GAMUT_THR:?};");
    let _ = writeln!(s, "const GAMUT_S: f32 = {GAMUT_S:?};");
    let _ = writeln!(s, "const MIN_LIN: f32 = 1.5258789e-5;");
    let _ = writeln!(s, "const LOG_MIN: f32 = {LOG_MIN:?};");
    let _ = writeln!(s, "const LOG_MAX: f32 = {LOG_MAX:?};");
    let _ = writeln!(s, "const WG_N: u32 = {}u;", WG_X * WG_Y);
    let _ = writeln!(
        s,
        "const DETAIL_KNEE: f32 = {:?};",
        crate::detail::DETAIL_KNEE
    );
    s
}

/// Kernel name, WGSL source, and placeholder substitutions.
type Kernel = (
    &'static str,
    &'static str,
    Vec<(&'static str, &'static str)>,
);

const PACK8: &str = "q";

/// The output kernel with (`*_hist`) or without the histogram: the workgroup-local bins
/// (4 KB of zero-initialized shared memory per workgroup) cost ~2 ms at 5120 px, so
/// frames that don't ask for a histogram use a variant without them.
const HIST_WG: &str = "var<workgroup> local_hist: array<atomic<u32>, 1033>;";
const HIST_BODY: &str = include_str!("shaders/output_hist.wgsl.part");
// Keeps the `hist` binding in the auto-derived layout; never taken (the flag is only set
// for `*_hist` kernels).
const HIST_OFF: &str = "if (ups.w > 0.5 && inside) { atomicAdd(&hist[codes.x], 1u); }";
const PACK16: &str = "vec2<u32>(pack2x16unorm(enc3(min(g, vec3<f32>(1.0))).xy), pack2x16unorm(vec2<f32>(enc3(min(g, vec3<f32>(1.0))).z, 1.0)))";
const PACKF: &str = "vec2<u32>(pack2x16float(g.xy), pack2x16float(vec2<f32>(g.z, 1.0)))";

/// Kernel name → (source, substitutions). `_h`/`_f` variants read/write f16/f32 BASE.
fn kernels() -> Vec<Kernel> {
    let mut k: Vec<Kernel> = Vec::new();
    for (names, fmt) in [
        (
            [
                "base_super_h",
                "base_down_h",
                "rcd_final_h",
                "scene_h",
                "scene_grade_h",
                "spot_stats_h",
            ],
            "BASE_H",
        ),
        (
            [
                "base_super_f",
                "base_down_f",
                "rcd_final_f",
                "scene_f",
                "scene_grade_f",
                "spot_stats_f",
            ],
            "BASE_F",
        ),
    ] {
        let srcs = [
            include_str!("shaders/base_super.wgsl"),
            include_str!("shaders/base_down.wgsl"),
            include_str!("shaders/rcd_final.wgsl"),
            include_str!("shaders/scene.wgsl"),
            include_str!("shaders/scene_grade.wgsl"),
            include_str!("shaders/spot_stats.wgsl"),
        ];
        // The develop image is f16 exactly when the BASE is.
        let enc = if fmt == "BASE_H" { "ENC_H" } else { "ENC_F" };
        for (name, src) in names.into_iter().zip(srcs) {
            k.push((
                name,
                src,
                vec![("//BASE_FORMAT", fmt), ("//ENC_FORMAT", enc)],
            ));
        }
    }
    let out = |t: &'static str, p: &'static str| {
        vec![
            ("OUT_T", t),
            ("OUT_PACK", p),
            ("//HIST_WG", ""),
            ("//HIST_BODY", HIST_OFF),
        ]
    };
    let out_hist = |t: &'static str, p: &'static str| {
        vec![
            ("OUT_T", t),
            ("OUT_PACK", p),
            ("//HIST_WG", HIST_WG),
            ("//HIST_BODY", HIST_BODY),
        ]
    };
    // (f32 name, f16 name, source, substitutions)
    let enc_kernels = [
        (
            "grade",
            "grade_h",
            include_str!("shaders/grade.wgsl"),
            vec![],
        ),
        (
            "local_contrast",
            "local_contrast_h",
            include_str!("shaders/local_contrast.wgsl"),
            vec![],
        ),
        (
            "mask_eval",
            "mask_eval_h",
            include_str!("shaders/mask_eval.wgsl"),
            vec![],
        ),
        (
            "layer_apply",
            "layer_apply_h",
            include_str!("shaders/layer_apply.wgsl"),
            vec![],
        ),
        (
            "nr_apply",
            "nr_apply_h",
            include_str!("shaders/nr_apply.wgsl"),
            vec![],
        ),
        (
            "gdown_nr",
            "gdown_nr_h",
            include_str!("shaders/gdown_nr.wgsl"),
            vec![],
        ),
        (
            "gdown_w",
            "gdown_w_h",
            include_str!("shaders/gdown_w.wgsl"),
            vec![],
        ),
        (
            "cr_plane",
            "cr_plane_h",
            include_str!("shaders/cr_plane.wgsl"),
            vec![],
        ),
        (
            "gdown_x",
            "gdown_x_h",
            include_str!("shaders/gdown_x.wgsl"),
            vec![],
        ),
        (
            "auto_apply",
            "auto_apply_h",
            include_str!("shaders/auto_apply.wgsl"),
            vec![],
        ),
        (
            "store_enc",
            "store_enc_h",
            include_str!("shaders/store_enc.wgsl"),
            vec![],
        ),
        (
            "fx_down",
            "fx_down_h",
            include_str!("shaders/fx_down.wgsl"),
            vec![],
        ),
        (
            "fx_apply",
            "fx_apply_h",
            include_str!("shaders/fx_apply.wgsl"),
            vec![],
        ),
        (
            "output8",
            "output8_h",
            include_str!("shaders/output.wgsl"),
            out("u32", PACK8),
        ),
        (
            "output16",
            "output16_h",
            include_str!("shaders/output.wgsl"),
            out("vec2<u32>", PACK16),
        ),
        (
            "outputf16",
            "outputf16_h",
            include_str!("shaders/output.wgsl"),
            out("vec2<u32>", PACKF),
        ),
        (
            "output8_hist",
            "output8_hist_h",
            include_str!("shaders/output.wgsl"),
            out_hist("u32", PACK8),
        ),
        (
            "output16_hist",
            "output16_hist_h",
            include_str!("shaders/output.wgsl"),
            out_hist("vec2<u32>", PACK16),
        ),
        (
            "outputf16_hist",
            "outputf16_hist_h",
            include_str!("shaders/output.wgsl"),
            out_hist("vec2<u32>", PACKF),
        ),
    ];
    for (f, h, src, subs) in enc_kernels {
        let mut sf = subs.clone();
        sf.push(("//ENC_FORMAT", "ENC_F"));
        let mut sh = subs;
        sh.push(("//ENC_FORMAT", "ENC_H"));
        k.push((f, src, sf));
        k.push((h, src, sh));
    }
    k.extend([
        ("blocks", include_str!("shaders/blocks.wgsl"), vec![]),
        (
            "gdown_haze",
            include_str!("shaders/gdown_haze.wgsl"),
            vec![],
        ),
        (
            "haze_patch",
            include_str!("shaders/haze_patch.wgsl"),
            vec![],
        ),
        (
            "gcoef_haze",
            include_str!("shaders/gcoef_haze.wgsl"),
            vec![],
        ),
        ("auto_src", include_str!("shaders/auto_src.wgsl"), vec![]),
        ("gdown_cr", include_str!("shaders/gdown_cr.wgsl"), vec![]),
        ("cr_apply", include_str!("shaders/cr_apply.wgsl"), vec![]),
        ("plane_up", include_str!("shaders/plane_up.wgsl"), vec![]),
        ("resize_h", include_str!("shaders/resize_h.wgsl"), vec![]),
        ("pack16", include_str!("shaders/pack16.wgsl"), vec![]),
        (
            "brush_stamp",
            include_str!("shaders/brush_stamp.wgsl"),
            vec![],
        ),
        (
            "brush_fold",
            include_str!("shaders/brush_fold.wgsl"),
            vec![],
        ),
        ("resize_v", include_str!("shaders/resize_v.wgsl"), vec![]),
        ("rcd_cfa", include_str!("shaders/rcd_cfa.wgsl"), vec![]),
        ("rcd_dir", include_str!("shaders/rcd_dir.wgsl"), vec![]),
        ("rcd_green", include_str!("shaders/rcd_green.wgsl"), vec![]),
        ("rcd_rb", include_str!("shaders/rcd_rb.wgsl"), vec![]),
        ("gbox", include_str!("shaders/gbox.wgsl"), vec![]),
        (
            "gcoef_self",
            include_str!("shaders/gcoef_self.wgsl"),
            vec![],
        ),
        (
            "gcoef_cross",
            include_str!("shaders/gcoef_cross.wgsl"),
            vec![],
        ),
        ("pdown", include_str!("shaders/pdown.wgsl"), vec![]),
        ("sep", include_str!("shaders/sep.wgsl"), vec![]),
        ("blur3", include_str!("shaders/blur3.wgsl"), vec![]),
        ("bokeh_coc", include_str!("shaders/bokeh_coc.wgsl"), vec![]),
        (
            "bokeh_down",
            include_str!("shaders/bokeh_down.wgsl"),
            vec![],
        ),
        (
            "bokeh_gather",
            include_str!("shaders/bokeh_gather.wgsl"),
            vec![],
        ),
        (
            "bokeh_apply",
            include_str!("shaders/bokeh_apply.wgsl"),
            vec![],
        ),
        ("fx_blur", include_str!("shaders/fx_blur.wgsl"), vec![]),
    ]);
    k
}

/// Full WGSL source for a kernel.
pub fn assemble(src: &str, subs: &[(&str, &str)]) -> String {
    let mut body = src
        .replace("//INCLUDE_SCENE_FN", SCENE_FN)
        .replace("//INCLUDE_GRADE_FN", GRADE_FN)
        .replace("//INCLUDE_TABLES", TABLES)
        .replace("//INCLUDE_RCD", RCD)
        .replace("//INCLUDE_RECONSTRUCT", RECONSTRUCT);
    let mut format = String::new();
    for (k, v) in subs {
        match *k {
            "//BASE_FORMAT" => format.push_str(if *v == "BASE_H" { BASE_H } else { BASE_F }),
            "//ENC_FORMAT" => format.push_str(if *v == "ENC_H" { ENC_H } else { ENC_F }),
            _ => body = body.replace(k, v),
        }
    }
    let body = body.replace(
        "@workgroup_size(16, 16)",
        &format!("@workgroup_size({WG_X}, {WG_Y})"),
    );
    format!("{}\n{LAYOUT}\n{COMMON}\n{format}\n{body}", prelude())
}

pub const STORAGE: wgpu::BufferUsages = wgpu::BufferUsages::STORAGE
    .union(wgpu::BufferUsages::COPY_SRC)
    .union(wgpu::BufferUsages::COPY_DST);

/// A pooled frame buffer, counted in the process-wide memory budget.
#[derive(Debug)]
struct PoolEntry {
    buf: wgpu::Buffer,
    /// Frame (render) that last used it, for LRU trimming.
    used: u64,
    _acct: Account,
}

/// Size of a pooled buffer asked for `bytes`.
fn pool_size(bytes: u64) -> u64 {
    bytes.max(16).next_multiple_of(256)
}

/// Can a pooled buffer of `have` bytes serve a request for `bytes`? Not when far larger
/// (e.g. left over from a full-resolution export): previews don't pin export-sized memory.
fn reusable(have: u64, bytes: u64) -> bool {
    have >= bytes && (have <= 4 * bytes || have <= 64 << 20)
}

/// Frame-buffer pool lanes: one per priority class plus one for interactive tiled renders.
pub const LANES: usize = 5;

/// Device generation counter: resources of a lost device are never reused.
static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug)]
pub struct Context {
    pub epoch: u64,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pipes: HashMap<&'static str, wgpu::ComputePipeline>,
    /// Keyed by (name, lane): interactive and background/export work keep separate
    /// buffers, so an export never forces the viewer to reallocate.
    pool: HashMap<(&'static str, u8), PoolEntry>,
    frame: u64,
    /// Pool lane of the current turn: the priority class, or [`LANES`] - 1 for interactive
    /// full-quality (tiled) renders.
    pub lane: u8,
    /// First kernel name a dispatch asked for that does not exist (a programming error);
    /// the turn reports it as an error without treating the device as lost.
    pub unknown_kernel: std::sync::Mutex<Option<String>>,
    dummy: wgpu::Buffer,
    /// A second placeholder for read-write bindings (a buffer can't be bound read-only
    /// and read-write in one dispatch).
    dummy_rw: wgpu::Buffer,
    pub adapter_name: String,
    /// Largest buffer this device accepts (min of max buffer and storage binding size).
    pub max_buffer: u64,
    /// Set when the device may be lost (callback, or a scope/map error); the renderer
    /// checks and rebuilds the context when it sees it.
    pub lost: Arc<AtomicBool>,
    /// Set only by the device-lost callback: certainly lost.
    lost_for_sure: Arc<AtomicBool>,
    /// Held (read) while a mapped output buffer is copied; `true` once the device was
    /// destroyed. Destroying frees mapped memory, so it takes the write side.
    pub maps: Arc<std::sync::RwLock<bool>>,
    /// Storage buffers can be mapped directly (unified memory): the output kernel writes
    /// into memory the CPU reads, with no GPU-side copy.
    pub mappable: bool,
}

impl Context {
    /// Create the device (Metal on macOS) and compile every kernel.
    pub fn new() -> Result<Self, PipelineError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|_| PipelineError::NoAdapter)?;
        let limits = adapter.limits();
        let features = adapter.features() & wgpu::Features::MAPPABLE_PRIMARY_BUFFERS;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("lt-pipeline"),
            required_features: features,
            required_limits: limits.clone(),
            ..Default::default()
        }))
        .map_err(|e| PipelineError::Gpu(e.to_string()))?;
        let lost = Arc::new(AtomicBool::new(false));
        let lost_for_sure = Arc::new(AtomicBool::new(false));
        let (flag, sure) = (Arc::clone(&lost), Arc::clone(&lost_for_sure));
        device.set_device_lost_callback(move |_, _| {
            sure.store(true, Ordering::SeqCst);
            flag.store(true, Ordering::SeqCst);
        });
        // Anything outside an error scope is reported here instead of panicking; renders run
        // inside scopes and turn errors into `PipelineError::Gpu`.
        device.on_uncaptured_error(Arc::new(|e: wgpu::Error| {
            eprintln!("lt-pipeline: uncaptured wgpu error: {e}");
        }));
        let mut pipes = HashMap::new();
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        for (name, src, subs) in kernels() {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(name),
                source: wgpu::ShaderSource::Wgsl(assemble(src, &subs).into()),
            });
            let pipe = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(name),
                layout: None,
                module: &module,
                entry_point: Some("main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            });
            pipes.insert(name, pipe);
        }
        if let Some(err) = pollster::block_on(scope.pop()) {
            return Err(PipelineError::Gpu(format!(
                "shader compilation failed: {err}"
            )));
        }
        let dummy = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy"),
            size: 256,
            usage: STORAGE,
            mapped_at_creation: false,
        });
        let dummy_rw = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy_rw"),
            size: 256,
            usage: STORAGE,
            mapped_at_creation: false,
        });
        Ok(Self {
            epoch: EPOCH.fetch_add(1, Ordering::Relaxed) + 1,
            device,
            queue,
            pipes,
            pool: HashMap::new(),
            frame: 0,
            lane: 0,
            unknown_kernel: std::sync::Mutex::new(None),
            dummy,
            dummy_rw,
            adapter_name: adapter.get_info().name,
            max_buffer: limits
                .max_buffer_size
                .min(limits.max_storage_buffer_binding_size),
            lost,
            lost_for_sure,
            maps: Arc::new(std::sync::RwLock::new(false)),
            mappable: features.contains(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS),
        })
    }

    /// Poll without blocking (fires the device-lost callback on a destroyed device).
    pub fn poll_lost(&self) -> bool {
        let _ = self.device.poll(wgpu::PollType::Poll);
        if self.destroyed() {
            self.lost.store(true, Ordering::SeqCst);
        }
        self.lost.load(Ordering::SeqCst)
    }

    /// Destroyed through [`Self::destroy`]: certainly lost, whether or not the callback
    /// has run yet.
    fn destroyed(&self) -> bool {
        *self
            .maps
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Declare the device lost (tests).
    pub fn mark_lost(&self) {
        self.lost_for_sure.store(true, Ordering::SeqCst);
        self.lost.store(true, Ordering::SeqCst);
    }

    /// Is a suspected loss real? The callback says so for sure; otherwise a tiny buffer
    /// round trip must validate. (A spurious suspicion must not cost a rebuild.)
    pub fn really_lost(&self) -> bool {
        if self.lost_for_sure.load(Ordering::SeqCst) || self.destroyed() {
            return true;
        }
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let b = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("probe"),
            size: 16,
            usage: STORAGE,
            mapped_at_creation: false,
        });
        // A destroyed device still hands out (invalid) buffers; using one is what fails.
        self.queue.write_buffer(&b, 0, &[0; 16]);
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        enc.clear_buffer(&b, 0, None);
        self.queue.submit([enc.finish()]);
        let error = pollster::block_on(scope.pop()).is_some();
        let _ = self.device.poll(wgpu::PollType::Poll);
        error || self.lost_for_sure.load(Ordering::SeqCst)
    }

    /// Start a render: pooled buffers used from now on are "fresh" for LRU trimming.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Error unless a buffer of `bytes` fits this device.
    pub fn check_size(&self, what: &str, bytes: u64) -> Result<(), PipelineError> {
        if bytes > self.max_buffer {
            return Err(PipelineError::Gpu(format!(
                "{what} needs a {bytes}-byte buffer; this GPU allows {}",
                self.max_buffer
            )));
        }
        Ok(())
    }

    /// A pooled storage buffer of at least `bytes`, reused across renders. A pooled
    /// buffer far larger than needed (e.g. left over from a full-resolution export) is
    /// replaced so previews don't pin export-sized memory.
    pub fn buf(&mut self, name: &'static str, bytes: u64) -> Result<wgpu::Buffer, PipelineError> {
        let bytes = pool_size(bytes);
        self.check_size(name, bytes)?;
        let frame = self.frame;
        let key = (name, self.lane);
        if let Some(e) = self.pool.get_mut(&key)
            && reusable(e.buf.size(), bytes)
        {
            e.used = frame;
            return Ok(e.buf.clone());
        }
        // Release the old one before allocating its replacement, and make room first:
        // buffers the current render hasn't used go before the budget is exceeded.
        self.pool.remove(&key);
        if crate::cache::over_budget_with(bytes) {
            self.trim_stale(0);
        }
        let b = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(name),
            size: bytes,
            usage: STORAGE,
            mapped_at_creation: false,
        });
        self.pool.insert(
            key,
            PoolEntry {
                buf: b.clone(),
                used: frame,
                _acct: Account::new(crate::cache::Use::FramePool, bytes),
            },
        );
        Ok(b)
    }

    /// Drop pooled buffers larger than `keep` bytes (after export-sized work).
    pub fn trim_over(&mut self, keep: u64) {
        self.pool.retain(|_, e| e.buf.size() <= keep);
    }

    /// Drop pooled buffers whose name starts with `prefix`.
    pub fn drop_pooled(&mut self, prefix: &str) {
        self.pool.retain(|(name, _), _| !name.starts_with(prefix));
    }

    /// Drop least recently used buffers not used by the current render while the pool
    /// holds more than `cap` bytes.
    pub fn trim_stale(&mut self, cap: u64) {
        while self.pool_bytes() > cap {
            let Some(key) = self
                .pool
                .iter()
                .filter(|(_, e)| e.used < self.frame)
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| *k)
            else {
                return;
            };
            self.pool.remove(&key);
        }
    }

    /// Bytes held by `lane`'s DEVELOP frame buffers (not the demosaic's work buffers).
    pub fn lane_pool_bytes(&self, lane: u8) -> u64 {
        self.pool
            .iter()
            .filter(|((name, l), _)| *l == lane && !name.starts_with("rcd_"))
            .map(|(_, e)| e.buf.size())
            .sum()
    }

    /// New bytes that [`Self::buf`] calls for `bufs` (name, bytes) in the current lane
    /// will allocate: pooled buffers they reuse cost nothing, ones they replace are
    /// released first.
    pub fn shortfall(&self, bufs: &[(&'static str, u64)]) -> u64 {
        bufs.iter()
            .map(|&(name, bytes)| {
                let bytes = pool_size(bytes);
                match self.pool.get(&(name, self.lane)) {
                    Some(e) if reusable(e.buf.size(), bytes) => 0,
                    Some(e) => bytes.saturating_sub(e.buf.size()),
                    None => bytes,
                }
            })
            .sum()
    }

    /// Bytes held by the frame-buffer pool.
    pub fn pool_bytes(&self) -> u64 {
        self.pool.values().map(|e| e.buf.size()).sum()
    }

    pub fn dummy(&self) -> wgpu::Buffer {
        self.dummy.clone()
    }

    pub fn dummy_rw(&self) -> wgpu::Buffer {
        self.dummy_rw.clone()
    }

    /// A buffer written through the queue (never mapped at creation: a lost device then
    /// yields an invalid buffer and a scoped error, not a panic).
    fn written(&self, label: &str, usage: wgpu::BufferUsages, bytes: &[u8]) -> wgpu::Buffer {
        let size = (bytes.len() as u64).max(16).next_multiple_of(4);
        let b = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if bytes.len().is_multiple_of(4) {
            self.queue.write_buffer(&b, 0, bytes);
        } else {
            let mut data = bytes.to_vec();
            data.resize(bytes.len().next_multiple_of(4), 0);
            self.queue.write_buffer(&b, 0, &data);
        }
        b
    }

    pub fn uniform(&self, block: &Block) -> wgpu::Buffer {
        self.written(
            "uniform",
            wgpu::BufferUsages::UNIFORM,
            bytemuck::cast_slice(block),
        )
    }

    pub fn storage_init(&self, label: &str, bytes: &[u8]) -> Result<wgpu::Buffer, PipelineError> {
        self.check_size(label, bytes.len() as u64)?;
        Ok(self.written(label, STORAGE, bytes))
    }

    /// A dedicated buffer that lives across GPU turns (export stripes), counted in the
    /// budget while alive.
    pub fn owned_buffer(
        &self,
        label: &str,
        bytes: u64,
    ) -> Result<(wgpu::Buffer, Account), PipelineError> {
        let b = self.new_buffer(label, bytes)?;
        let size = b.size();
        Ok((b, Account::new(crate::cache::Use::Export, size)))
    }

    /// A fresh (unpooled) storage buffer.
    pub fn new_buffer(&self, label: &str, bytes: u64) -> Result<wgpu::Buffer, PipelineError> {
        let bytes = bytes.max(16).next_multiple_of(4);
        self.check_size(label, bytes)?;
        Ok(self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes,
            usage: STORAGE,
            mapped_at_creation: false,
        }))
    }

    /// Record one compute dispatch over a `w × h` grid.
    pub fn dispatch(
        &self,
        enc: &mut wgpu::CommandEncoder,
        kernel: &str,
        bindings: &[&wgpu::Buffer],
        w: u32,
        h: u32,
    ) {
        let Some(pipe) = self.pipes.get(kernel) else {
            self.unknown_kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert_with(|| kernel.to_string());
            return;
        };
        let layout = pipe.get_bind_group_layout(0);
        let entries: Vec<wgpu::BindGroupEntry<'_>> = bindings
            .iter()
            .enumerate()
            .map(|(i, b)| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: b.as_entire_binding(),
            })
            .collect();
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(kernel),
            layout: &layout,
            entries: &entries,
        });
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(kernel),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipe);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(w.div_ceil(WG_X), h.div_ceil(WG_Y), 1);
    }

    /// A new output slot of at least `bytes` (the output kernel writes `gpu`; the CPU
    /// maps `gpu` directly on unified memory, else a staging copy).
    pub fn out_slot(&self, bytes: u64) -> Result<OutSlot, PipelineError> {
        let size = bytes.max(16).next_multiple_of(256);
        self.check_size("output", size)?;
        let make = |label, usage| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let (gpu, staging) = if self.mappable {
            (
                make(
                    "out",
                    wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::MAP_READ
                        | wgpu::BufferUsages::COPY_DST,
                ),
                None,
            )
        } else {
            (
                make("out", STORAGE),
                Some(make(
                    "readback",
                    wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                )),
            )
        };
        let acct = Account::new(
            crate::cache::Use::Output,
            size * if staging.is_some() { 2 } else { 1 },
        );
        Ok(OutSlot {
            gpu,
            staging,
            epoch: self.epoch,
            _acct: acct,
        })
    }

    /// Submit `enc` (plus the staging copy) and start mapping the first `bytes` of the
    /// slot. The wait happens in [`Readback::finish`], outside any lock.
    pub fn submit_readback(
        &self,
        mut enc: wgpu::CommandEncoder,
        slot: OutSlot,
        bytes: u64,
    ) -> Readback {
        let size = bytes.next_multiple_of(4);
        if let Some(st) = &slot.staging {
            enc.copy_buffer_to_buffer(&slot.gpu, 0, st, 0, size);
        }
        self.queue.submit([enc.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        slot.target()
            .map_async(wgpu::MapMode::Read, ..size, move |r| {
                let _ = tx.send(r);
            });
        Readback {
            slot,
            bytes,
            size,
            rx,
            device: self.device.clone(),
            lost: Arc::clone(&self.lost),
            maps: Arc::clone(&self.maps),
        }
    }

    /// Destroy the device (tests: a real loss) once no mapped output is being copied.
    pub fn destroy(&self) {
        let mut destroyed = self
            .maps
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *destroyed = true;
        self.device.destroy();
    }
}

/// Map `buf` (`MAP_WRITE`) for writing and wait for it (non-blocking polls with backoff),
/// failing on a device loss.
pub fn map_write(
    device: &wgpu::Device,
    lost: &AtomicBool,
    buf: &wgpu::Buffer,
    size: u64,
) -> Result<(), PipelineError> {
    let (tx, rx) = std::sync::mpsc::channel();
    buf.map_async(wgpu::MapMode::Write, ..size, move |r| {
        let _ = tx.send(r);
    });
    let mut backoff = Backoff::new();
    loop {
        let _ = device.poll(wgpu::PollType::Poll);
        match rx.try_recv() {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => return Err(PipelineError::Gpu(e.to_string())),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(PipelineError::Gpu("buffer mapping abandoned".into()));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if lost.load(Ordering::SeqCst) {
            return Err(PipelineError::Gpu("device lost".into()));
        }
        backoff.sleep();
    }
}

/// How long a waiting thread sleeps between non-blocking polls. Blocking waits on a
/// submission index from several threads at once trip an internal wgpu assertion, so
/// waits are non-blocking polls plus completion callbacks.
const POLL_NAP: Duration = Duration::from_micros(100);

/// Naps between polls: 100 µs for the first millisecond (frames finish fast), then
/// doubling to 2 ms, so long waits (exports, queued work) cost next to no CPU.
struct Backoff {
    start: std::time::Instant,
    nap: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            nap: POLL_NAP,
        }
    }

    fn sleep(&mut self) {
        std::thread::sleep(self.nap);
        if self.start.elapsed() > Duration::from_millis(1) {
            self.nap = (self.nap * 2).min(Duration::from_millis(2));
        }
    }
}

/// Wait (outside any lock, cancellable) until the work submitted before `done` was
/// registered (`Queue::on_submitted_work_done`) has executed.
pub fn wait_for(
    device: &wgpu::Device,
    lost: &AtomicBool,
    done: &std::sync::mpsc::Receiver<()>,
    cancel: Option<&CancelToken>,
) -> Result<(), PipelineError> {
    let mut backoff = Backoff::new();
    let fired = |done: &std::sync::mpsc::Receiver<()>| match done.try_recv() {
        Ok(()) => Ok(true),
        Err(std::sync::mpsc::TryRecvError::Empty) => Ok(false),
        // The callback was dropped without running: the work was abandoned (device lost).
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(PipelineError::Gpu(
            "GPU work abandoned (device lost?)".into(),
        )),
    };
    loop {
        if fired(done)? {
            break;
        }
        if let Err(e) = device.poll(wgpu::PollType::Poll) {
            return Err(PipelineError::Gpu(e.to_string()));
        }
        if fired(done)? {
            break;
        }
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(PipelineError::Cancelled);
        }
        if lost.load(Ordering::SeqCst) {
            break;
        }
        backoff.sleep();
    }
    // Work dropped by a device loss also "completes": make sure the loss surfaces.
    let _ = device.poll(wgpu::PollType::Poll);
    if lost.load(Ordering::SeqCst) {
        return Err(PipelineError::Gpu(
            "GPU device lost during the stage".into(),
        ));
    }
    Ok(())
}

/// A per-render output buffer (reused through a free list once unmapped).
#[derive(Debug)]
pub struct OutSlot {
    pub gpu: wgpu::Buffer,
    staging: Option<wgpu::Buffer>,
    /// The device generation that created it.
    pub epoch: u64,
    _acct: Account,
}

impl OutSlot {
    fn target(&self) -> &wgpu::Buffer {
        self.staging.as_ref().unwrap_or(&self.gpu)
    }

    /// Big enough for `bytes`, and not wastefully big.
    pub fn fits(&self, bytes: u64) -> bool {
        let s = self.gpu.size();
        s >= bytes && (s <= 4 * bytes || s <= 64 << 20)
    }
}

/// Submitted work whose output is being mapped.
pub struct Readback {
    slot: OutSlot,
    bytes: u64,
    size: u64,
    rx: std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    device: wgpu::Device,
    lost: Arc<AtomicBool>,
    maps: Arc<std::sync::RwLock<bool>>,
}

impl Readback {
    /// Wait (cancellable) for the output and hand its bytes to `sink`; the slot comes
    /// back for reuse. On error the slot (possibly still mapping) is dropped.
    pub fn finish(
        self,
        cancel: Option<&CancelToken>,
        sink: impl FnOnce(&[u8]),
    ) -> Result<OutSlot, PipelineError> {
        let mut backoff = Backoff::new();
        loop {
            match self.rx.try_recv() {
                Ok(r) => {
                    if let Err(e) = r {
                        // Mapping only fails on a broken device: rebuild it next time.
                        self.lost.store(true, Ordering::SeqCst);
                        return Err(PipelineError::Gpu(e.to_string()));
                    }
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(PipelineError::Gpu("readback abandoned".into()));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if let Err(e) = self.device.poll(wgpu::PollType::Poll) {
                return Err(PipelineError::Gpu(e.to_string()));
            }
            match self.rx.try_recv() {
                Ok(Ok(())) => break,
                Ok(Err(e)) => {
                    self.lost.store(true, Ordering::SeqCst);
                    return Err(PipelineError::Gpu(e.to_string()));
                }
                Err(_) => {}
            }
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(PipelineError::Cancelled);
            }
            if self.lost.load(Ordering::SeqCst) {
                return Err(PipelineError::Gpu("device lost".into()));
            }
            backoff.sleep();
        }
        let _ = self.device.poll(wgpu::PollType::Poll);
        if self.lost.load(Ordering::SeqCst) {
            return Err(PipelineError::Gpu(
                "GPU device lost during the frame".into(),
            ));
        }
        let target = self.slot.target();
        // The device can't be destroyed (freeing the mapping) while the bytes are copied.
        let destroyed = self
            .maps
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *destroyed {
            self.lost.store(true, Ordering::SeqCst);
            return Err(PipelineError::Gpu(
                "GPU device lost during the frame".into(),
            ));
        }
        {
            let view = target.get_mapped_range(..self.size).map_err(|e| {
                self.lost.store(true, Ordering::SeqCst);
                PipelineError::Gpu(format!("{e:?}"))
            })?;
            sink(&view[..self.bytes as usize]);
        }
        target.unmap();
        drop(destroyed);
        Ok(self.slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loss probe must see a destroyed device on its own (the lost callback may come
    /// late): such a device still hands out buffers, which only fail when used.
    /// A4: a stage wait ends on completion, on an abandoned callback (never "still
    /// pending"), on a loss, and on cancel.
    #[test]
    fn stage_waits_end_on_every_outcome() {
        let Ok(ctx) = Context::new() else { return };
        let lost = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(()).unwrap();
        assert!(wait_for(&ctx.device, &lost, &rx, None).is_ok());
        drop(tx);
        let err = wait_for(&ctx.device, &lost, &rx, None).unwrap_err();
        assert!(err.to_string().contains("abandoned"), "{err}");
        let (_tx, rx) = std::sync::mpsc::channel::<()>();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            wait_for(&ctx.device, &lost, &rx, Some(&cancel)),
            Err(PipelineError::Cancelled)
        ));
        lost.store(true, Ordering::SeqCst);
        let err = wait_for(&ctx.device, &lost, &rx, None).unwrap_err();
        assert!(err.to_string().contains("lost"), "{err}");
    }

    #[test]
    fn the_loss_probe_sees_a_destroyed_device() {
        let Ok(ctx) = Context::new() else { return };
        assert!(!ctx.really_lost(), "a healthy device is not lost");
        ctx.destroy();
        ctx.lost_for_sure.store(false, Ordering::SeqCst);
        assert!(ctx.really_lost());
    }
}
