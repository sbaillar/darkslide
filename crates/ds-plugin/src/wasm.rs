//! The WebAssembly runtime (host API 1): wasmi, an interpreter (no JIT, so it runs on
//! iPadOS too), with fuel for time, a store limit for memory, and every trap caught.
//!
//! A module sees only the imports below; there is no import for files, the network, the
//! clock, or the host's own state (grants, trust), so a plug-in can't reach them. Each
//! call gets a fresh instance: nothing leaks from one image to the next.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use serde_json::{Map, Value};
use wasmi::{
    Caller, Config, Engine, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
};

use crate::PluginError;
use crate::params::parse_color;

/// Linear memory one instance may use.
pub const MEMORY_LIMIT: usize = 64 << 20;
/// All plug-in instances together (so at most 8 run at once).
pub const TOTAL_MEMORY: usize = 512 << 20;
/// Rows handed to the module per call: at most this many bytes (plus the apron).
pub const STRIP_BYTES: usize = 4 << 20;
/// The largest canvas a layout may ask for (100 MP; the host also caps growth at twice
/// the image's area).
pub const MAX_CANVAS_PIXELS: u64 = 100_000_000;
/// Tables per instance, and elements per table (function tables of real modules are
/// small; a `table.grow` bomb is refused).
pub const MAX_TABLES: usize = 1;
pub const MAX_TABLE_ELEMENTS: usize = 100_000;
/// The largest module accepted.
pub const MAX_MODULE_BYTES: usize = 16 << 20;
/// Fuel for `start`, `ds_api`, `ds_alloc` and `ds_output_layout`.
pub const FUEL_SETUP: u64 = 10_000_000;
/// Fuel per strip: a base plus this much per pixel handed over.
pub const FUEL_BASE: u64 = 5_000_000;
pub const FUEL_PER_PIXEL: u64 = 400;
/// Failures in a row after which a contribution stops running until relaunch.
pub const MAX_FAILURES: u32 = 3;
const MAX_LOG: usize = 2000;

/// Pixel buffers a post-processor works on, in the output's encoding.
#[derive(Debug)]
pub enum Pixels<'a> {
    /// 4 bytes per pixel; alpha is the plug-in's to leave alone.
    Rgba8(&'a mut [u8]),
    /// 3 × u16 per pixel.
    Rgb16(&'a mut [u16]),
}

impl Pixels<'_> {
    fn format(&self) -> i32 {
        match self {
            Self::Rgba8(_) => 0,
            Self::Rgb16(_) => 1,
        }
    }

    fn bytes_per_pixel(&self) -> usize {
        match self {
            Self::Rgba8(_) => 4,
            Self::Rgb16(_) => 6,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Rgba8(b) => b,
            Self::Rgb16(w) => bytemuck::cast_slice(w),
        }
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Rgba8(b) => b,
            Self::Rgb16(w) => bytemuck::cast_slice_mut(w),
        }
    }
}

/// What a call's host functions read and write.
struct HostState {
    limits: StoreLimits,
    params: Map<String, Value>,
    log: Vec<String>,
    error: Option<String>,
    cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Where the photo is in the canvas: x, y, width, height.
    photo: [u32; 4],
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState")
            .field("log", &self.log)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// A compiled module, shared by every call of its contributions.
#[derive(Clone)]
pub struct WasmModule {
    engine: Engine,
    module: Module,
}

impl std::fmt::Debug for WasmModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmModule").finish_non_exhaustive()
    }
}

impl WasmModule {
    /// Compile `bytes`: a `.wasm` binary, or `.wat` text (compiled first). Checks that
    /// it imports nothing but the host API and exports what API 1 requires.
    pub fn compile(bytes: &[u8]) -> Result<Self, PluginError> {
        on_big_stack(|| Self::compile_here(bytes))
    }

    fn compile_here(bytes: &[u8]) -> Result<Self, PluginError> {
        if bytes.len() > MAX_MODULE_BYTES {
            return Err(PluginError::Plugin(format!(
                "the module is over {} MB",
                MAX_MODULE_BYTES >> 20
            )));
        }
        let wasm = wat::parse_bytes(bytes)
            .map_err(|e| PluginError::Plugin(format!("not a WebAssembly module: {e}")))?;
        let mut config = Config::default();
        config.consume_fuel(true);
        // One linear memory per module: the memory limit is per memory, so several would
        // multiply it.
        config.wasm_multi_memory(false);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, &wasm[..])
            .map_err(|e| PluginError::Plugin(format!("invalid module: {e}")))?;
        for import in module.imports() {
            if import.module() != "darkslide" || !HOST_IMPORTS.contains(&import.name()) {
                return Err(PluginError::Plugin(format!(
                    "imports `{}.{}`, which this Darkslide doesn't provide (host API 1: {})",
                    import.module(),
                    import.name(),
                    HOST_IMPORTS.join(", ")
                )));
            }
        }
        for name in ["memory", "ds_api", "ds_alloc"] {
            if module.get_export(name).is_none() {
                return Err(PluginError::Plugin(format!(
                    "doesn't export `{name}` (host API 1)"
                )));
            }
        }
        Ok(Self { engine, module })
    }

    /// Whether the module exports `name`.
    #[must_use]
    pub fn exports(&self, name: &str) -> bool {
        self.module.get_export(name).is_some()
    }
}

/// The interpreter (and the translator before it) recurse deeper than a rayon worker's
/// 2 MiB stack allows in unoptimized builds: run on a scoped thread with room to spare.
const STACK: usize = 8 << 20;

fn on_big_stack<T: Send>(
    f: impl FnOnce() -> Result<T, PluginError> + Send,
) -> Result<T, PluginError> {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("ds-plugin wasm".into())
            .stack_size(STACK)
            .spawn_scoped(s, f)
            .map_err(PluginError::Io)?
            .join()
            .unwrap_or_else(|_| Err(PluginError::Plugin("the plug-in runtime panicked".into())))
    })
}

const HOST_IMPORTS: &[&str] = &[
    "photo_rect",
    "log",
    "error",
    "progress",
    "param_f64",
    "param_i64",
    "param_bool",
    "param_color",
];

fn read_str(caller: &Caller<'_, HostState>, mem: Memory, ptr: i32, len: i32) -> Option<String> {
    let (ptr, len) = (usize::try_from(ptr).ok()?, usize::try_from(len).ok()?);
    let data = mem.data(caller);
    let bytes = data.get(ptr..ptr.checked_add(len)?)?;
    Some(String::from_utf8_lossy(&bytes[..len.min(MAX_LOG)]).into_owned())
}

fn memory(caller: &Caller<'_, HostState>) -> Option<Memory> {
    caller
        .get_export("memory")
        .and_then(wasmi::Extern::into_memory)
}

fn param(caller: &Caller<'_, HostState>, ptr: i32, len: i32) -> Option<Value> {
    let key = read_str(caller, memory(caller)?, ptr, len)?;
    caller.data().params.get(&key).cloned()
}

fn linker(engine: &Engine) -> Result<Linker<HostState>, PluginError> {
    let mut l = Linker::<HostState>::new(engine);
    let err = |e: wasmi::errors::LinkerError| PluginError::Plugin(e.to_string());
    l.func_wrap(
        "darkslide",
        "log",
        |caller: Caller<'_, HostState>, _level: i32, ptr: i32, len: i32| {
            let msg = memory(&caller).and_then(|m| read_str(&caller, m, ptr, len));
            let mut caller = caller;
            let log = &mut caller.data_mut().log;
            if let Some(m) = msg
                && log.len() < 100
            {
                log.push(m);
            }
        },
    )
    .map_err(err)?;
    l.func_wrap(
        "darkslide",
        "error",
        |caller: Caller<'_, HostState>, ptr: i32, len: i32| {
            let msg = memory(&caller).and_then(|m| read_str(&caller, m, ptr, len));
            let mut caller = caller;
            caller.data_mut().error = msg;
        },
    )
    .map_err(err)?;
    l.func_wrap(
        "darkslide",
        "progress",
        |caller: Caller<'_, HostState>, _permille: i32| -> i32 {
            i32::from((caller.data().cancelled)())
        },
    )
    .map_err(err)?;
    l.func_wrap(
        "darkslide",
        "param_f64",
        |caller: Caller<'_, HostState>, ptr: i32, len: i32| -> f64 {
            param(&caller, ptr, len)
                .and_then(|v| v.as_f64())
                .unwrap_or(f64::NAN)
        },
    )
    .map_err(err)?;
    l.func_wrap(
        "darkslide",
        "param_i64",
        |caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
            param(&caller, ptr, len)
                .and_then(|v| v.as_i64())
                .unwrap_or(i64::MIN)
        },
    )
    .map_err(err)?;
    l.func_wrap(
        "darkslide",
        "param_bool",
        |caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i32 {
            match param(&caller, ptr, len) {
                Some(Value::Bool(b)) => i32::from(b),
                _ => -1,
            }
        },
    )
    .map_err(err)?;
    // Where the photo is in the canvas, as four u32 (x, y, width, height) at `ptr`.
    l.func_wrap(
        "darkslide",
        "photo_rect",
        |mut caller: Caller<'_, HostState>, ptr: i32| -> i32 {
            let rect = caller.data().photo;
            let Some(mem) = memory(&caller) else {
                return -1;
            };
            let bytes: Vec<u8> = rect.iter().flat_map(|v| v.to_le_bytes()).collect();
            match usize::try_from(ptr) {
                Ok(at) if mem.write(&mut caller, at, &bytes).is_ok() => 0,
                _ => -1,
            }
        },
    )
    .map_err(err)?;
    // A colour as 0xRRGGBBAA in the low 32 bits; −1 when there is no such param.
    l.func_wrap(
        "darkslide",
        "param_color",
        |caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
            param(&caller, ptr, len)
                .as_ref()
                .and_then(Value::as_str)
                .and_then(parse_color)
                .map_or(-1, |c| i64::from(u32::from_be_bytes(c)))
        },
    )
    .map_err(err)?;
    Ok(l)
}

/// The canvas a post-processor draws on: its size and where the photo sits in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// The photo's top-left corner in the canvas.
    pub x: u32,
    pub y: u32,
}

/// One `export_post_process` contribution, ready to run.
#[derive(Debug)]
pub struct PostProcessor {
    /// "Film Border" (for messages).
    pub name: String,
    module: WasmModule,
    entry: String,
    apron_rows: u32,
    failures: AtomicU32,
}

/// One instance for one image: its store, its buffers, its entry points.
struct Session<'a> {
    pp: &'a PostProcessor,
    store: Store<HostState>,
    instance: wasmi::Instance,
    mem: Memory,
    _slot: MemorySlot,
}

impl PostProcessor {
    pub fn new(
        name: &str,
        module: WasmModule,
        entry: &str,
        apron_rows: u32,
    ) -> Result<Self, PluginError> {
        if !module.exports(entry) {
            return Err(PluginError::Plugin(format!(
                "{name}: the module doesn't export `{entry}`"
            )));
        }
        Ok(Self {
            name: name.to_owned(),
            module,
            entry: entry.to_owned(),
            apron_rows: apron_rows.min(256),
            failures: AtomicU32::new(0),
        })
    }

    /// Whether it stopped after [`MAX_FAILURES`] failures in a row.
    #[must_use]
    pub fn is_suspended(&self) -> bool {
        self.failures.load(Ordering::Relaxed) >= MAX_FAILURES
    }

    fn fail(&self, m: impl std::fmt::Display) -> PluginError {
        PluginError::Plugin(format!("{}: {m}", self.name))
    }

    fn check_suspended(&self) -> Result<(), PluginError> {
        if self.is_suspended() {
            return Err(PluginError::Plugin(format!(
                "{}: stopped after {MAX_FAILURES} failures in a row (it runs again after a relaunch)",
                self.name
            )));
        }
        Ok(())
    }

    fn count<T>(&self, r: Result<T, PluginError>) -> Result<T, PluginError> {
        match &r {
            Ok(_) => self.failures.store(0, Ordering::Relaxed),
            Err(PluginError::Cancelled) => {}
            Err(_) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        r
    }

    /// The canvas for a `width` × `height` image: the module's `ds_output_layout` when
    /// it exports one (a border that adds a margin), else the image itself.
    pub fn layout(
        &self,
        width: u32,
        height: u32,
        params: &Map<String, Value>,
        context: &Value,
    ) -> Result<Layout, PluginError> {
        let same = Layout {
            width,
            height,
            x: 0,
            y: 0,
        };
        if !self.module.exports("ds_output_layout") {
            return Ok(same);
        }
        self.check_suspended()?;
        let r = on_big_stack(|| {
            let mut s = Session::start(
                self,
                params.clone(),
                Arc::new(|| false),
                [0, 0, width, height],
            )?;
            s.layout(width, height, context)
        });
        self.count(r)
    }

    /// Run on `pixels` (`width` × `height`, rows packed) in strips of at most
    /// [`STRIP_BYTES`] (plus the declared apron rows of context above and below; only a
    /// strip's own rows are copied back). `params` are already resolved; `context` is
    /// the rest of the call's JSON. On a failure the image may be partly processed: the
    /// export of that file fails (nothing is written).
    // The call's inputs, moved to the interpreter's own thread.
    #[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
    pub fn run(
        &self,
        pixels: Pixels<'_>,
        width: u32,
        height: u32,
        photo: [u32; 4],
        params: Map<String, Value>,
        context: &Value,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<String>, PluginError> {
        self.check_suspended()?;
        // The export's Cancel reaches the module through `progress()`: a watcher copies it
        // into a flag the interpreter's thread can own.
        let flag = Arc::new(AtomicBool::new(cancelled()));
        let done = AtomicBool::new(false);
        let r = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    if cancelled() {
                        flag.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            });
            let f2 = flag.clone();
            let r = on_big_stack(|| {
                let probe: Arc<dyn Fn() -> bool + Send + Sync> =
                    Arc::new(move || f2.load(Ordering::Relaxed));
                let mut s = Session::start(self, params, probe.clone(), photo)?;
                s.process(pixels, width, height, context, &*probe)
            });
            done.store(true, Ordering::Relaxed);
            r
        });
        self.count(r)
    }
}

impl<'a> Session<'a> {
    fn start(
        pp: &'a PostProcessor,
        params: Map<String, Value>,
        cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
        photo: [u32; 4],
    ) -> Result<Self, PluginError> {
        let slot = MemorySlot::take(&*cancelled).ok_or(PluginError::Cancelled)?;
        let state = HostState {
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_LIMIT)
                .memories(1)
                .tables(MAX_TABLES)
                .table_elements(MAX_TABLE_ELEMENTS)
                .instances(1)
                .build(),
            params,
            log: Vec::new(),
            error: None,
            cancelled,
            photo,
        };
        let mut store = Store::new(&pp.module.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_fuel(FUEL_SETUP).map_err(|e| pp.fail(e))?;
        let instance = linker(&pp.module.engine)?
            .instantiate_and_start(&mut store, &pp.module.module)
            .map_err(|e| pp.fail(format!("couldn't start: {}", trap_text(&store, &e))))?;
        let mem = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| pp.fail("no memory"))?;
        let mut s = Self {
            pp,
            store,
            instance,
            mem,
            _slot: slot,
        };
        s.store.set_fuel(FUEL_SETUP).map_err(|e| pp.fail(e))?;
        let api = s.call::<(), i32>("ds_api", ())?;
        if api != crate::manifest::HOST_API as i32 {
            return Err(pp.fail(format!(
                "built for host API {api}; this Darkslide provides 1"
            )));
        }
        Ok(s)
    }

    fn call<P: wasmi::WasmParams, R: wasmi::WasmResults>(
        &mut self,
        name: &str,
        args: P,
    ) -> Result<R, PluginError> {
        let f = self
            .instance
            .get_typed_func::<P, R>(&self.store, name)
            .map_err(|e| {
                self.pp
                    .fail(format!("`{name}` has the wrong signature: {e}"))
            })?;
        f.call(&mut self.store, args)
            .map_err(|e| self.pp.fail(trap_text(&self.store, &e)))
    }

    /// `ds_alloc(len)` with its own small fuel budget, bounds-checked.
    fn alloc(&mut self, len: usize) -> Result<(i32, usize), PluginError> {
        let n = i32::try_from(len).map_err(|_| self.pp.fail("a buffer is too large"))?;
        self.store
            .set_fuel(FUEL_SETUP)
            .map_err(|e| self.pp.fail(e))?;
        let ptr = self.call::<i32, i32>("ds_alloc", n)?;
        let at = usize::try_from(ptr)
            .ok()
            .filter(|&p| {
                p.checked_add(len)
                    .is_some_and(|end| end <= self.mem.data_size(&self.store))
            })
            .ok_or_else(|| {
                self.pp
                    .fail("ds_alloc returned a buffer outside its memory")
            })?;
        Ok((ptr, at))
    }

    fn context(&mut self, context: &Value) -> Result<(i32, i32), PluginError> {
        let mut ctx = context.clone();
        if let Value::Object(o) = &mut ctx {
            o.insert(
                "params".into(),
                Value::Object(self.store.data().params.clone()),
            );
        }
        let ctx = serde_json::to_vec(&ctx).map_err(|e| self.pp.fail(e))?;
        let (ptr, at) = self.alloc(ctx.len())?;
        self.mem.data_mut(&mut self.store)[at..at + ctx.len()].copy_from_slice(&ctx);
        Ok((ptr, i32::try_from(ctx.len()).unwrap_or(i32::MAX)))
    }

    fn layout(&mut self, width: u32, height: u32, context: &Value) -> Result<Layout, PluginError> {
        let (ctx_ptr, ctx_len) = self.context(context)?;
        let (out_ptr, out_at) = self.alloc(16)?;
        let (w, h) = (to_i32(width)?, to_i32(height)?);
        self.store
            .set_fuel(FUEL_SETUP)
            .map_err(|e| self.pp.fail(e))?;
        let rc = self.call::<(i32, i32, i32, i32, i32), i32>(
            "ds_output_layout",
            (ctx_ptr, ctx_len, w, h, out_ptr),
        )?;
        if rc != 0 {
            return Err(self.error_or(rc));
        }
        let b = &self.mem.data(&self.store)[out_at..out_at + 16];
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let l = Layout {
            width: word(0),
            height: word(4),
            x: word(8),
            y: word(12),
        };
        let fits = l.x.checked_add(width).is_some_and(|r| r <= l.width)
            && l.y.checked_add(height).is_some_and(|b| b <= l.height);
        if !fits || u64::from(l.width) * u64::from(l.height) > MAX_CANVAS_PIXELS {
            return Err(self.pp.fail(format!(
                "its canvas {}×{} doesn't hold the photo or is too large",
                l.width, l.height
            )));
        }
        Ok(l)
    }

    fn error_or(&self, rc: i32) -> PluginError {
        self.pp.fail(
            self.store
                .data()
                .error
                .clone()
                .unwrap_or_else(|| format!("failed (code {rc})")),
        )
    }

    fn process(
        &mut self,
        mut pixels: Pixels<'_>,
        width: u32,
        height: u32,
        context: &Value,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<String>, PluginError> {
        let bpp = pixels.bytes_per_pixel();
        let row = (width as usize)
            .checked_mul(bpp)
            .ok_or_else(|| self.pp.fail("the image is too wide"))?;
        if row == 0
            || height == 0
            || row.checked_mul(height as usize) != Some(pixels.as_bytes().len())
        {
            return Err(PluginError::Invalid(format!(
                "{width}×{height} doesn't match the pixel buffer"
            )));
        }
        let (w, h, stride, fmt) = (
            to_i32(width)?,
            to_i32(height)?,
            to_i32(row)?,
            pixels.format(),
        );
        let core_rows = (STRIP_BYTES / row).clamp(1, height as usize);
        let apron = self.pp.apron_rows as usize;
        let max_rows = (core_rows + 2 * apron).min(height as usize);
        let (ctx_ptr, ctx_len) = self.context(context)?;
        let (px_ptr, px_at) = self.alloc(max_rows * row)?;
        // The rows above a strip were already written back: its top apron comes from this
        // rolling copy of the last `apron` input rows (across as many strips as that
        // spans), so every strip sees only unprocessed rows and the result doesn't depend
        // on the strip height.
        let mut above: Vec<u8> = Vec::with_capacity(apron * row);
        let mut y0 = 0usize;
        while y0 < height as usize {
            if cancelled() {
                return Err(PluginError::Cancelled);
            }
            let rows = core_rows.min(height as usize - y0);
            let buf_y0 = y0.saturating_sub(apron);
            let buf_end = (y0 + rows + apron).min(height as usize);
            let buf_rows = buf_end - buf_y0;
            let top = (y0 - buf_y0) * row; // == above.len()
            let rest = &pixels.as_bytes()[y0 * row..buf_end * row];
            let mem = self.mem.data_mut(&mut self.store);
            mem[px_at..px_at + top].copy_from_slice(&above[above.len() - top..]);
            mem[px_at + top..px_at + top + rest.len()].copy_from_slice(rest);
            let fuel = FUEL_BASE
                .saturating_add(FUEL_PER_PIXEL.saturating_mul((buf_rows * width as usize) as u64));
            self.store.set_fuel(fuel).map_err(|e| self.pp.fail(e))?;
            let args = (
                ctx_ptr,
                ctx_len,
                px_ptr,
                w,
                h,
                stride,
                fmt,
                to_i32(buf_y0)?,
                to_i32(buf_rows)?,
                to_i32(y0)?,
                to_i32(rows)?,
            );
            let rc = self.call::<(i32, i32, i32, i32, i32, i32, i32, i32, i32, i32, i32), i32>(
                &self.pp.entry.clone(),
                args,
            )?;
            if rc == 1 && cancelled() {
                return Err(PluginError::Cancelled);
            }
            if rc != 0 {
                return Err(self.error_or(rc));
            }
            // Keep the input of the next strips' top apron before writing this one back.
            above.extend_from_slice(&pixels.as_bytes()[y0 * row..(y0 + rows) * row]);
            above.drain(..above.len().saturating_sub(apron * row));
            let off = px_at + (y0 - buf_y0) * row;
            let core = &self.mem.data(&self.store)[off..off + rows * row];
            pixels.as_bytes_mut()[y0 * row..(y0 + rows) * row].copy_from_slice(core);
            y0 += rows;
        }
        Ok(std::mem::take(&mut self.store.data_mut().log))
    }
}

fn to_i32(n: impl TryInto<i32>) -> Result<i32, PluginError> {
    n.try_into()
        .map_err(|_| PluginError::Invalid("the image is too large for a plug-in".into()))
}

/// A trap in words: out of fuel, the module's own `error`, or the trap.
fn trap_text(store: &Store<HostState>, e: &wasmi::Error) -> String {
    if e.as_trap_code() == Some(wasmi::TrapCode::OutOfFuel) {
        return "took too long and was stopped".into();
    }
    store
        .data()
        .error
        .clone()
        .unwrap_or_else(|| format!("crashed ({e})"))
}

/// A share of the global plug-in memory budget, held while an instance lives: at most
/// `TOTAL_MEMORY / MEMORY_LIMIT` instances run at once, whatever the thread count.
struct MemorySlot;

static SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

impl MemorySlot {
    /// A slot, waiting for one to free up; `None` when the job is cancelled meanwhile.
    fn take(cancelled: &dyn Fn() -> bool) -> Option<Self> {
        let max = TOTAL_MEMORY / MEMORY_LIMIT;
        let mut used = SLOTS.0.lock().unwrap_or_else(PoisonError::into_inner);
        while *used >= max {
            if cancelled() {
                return None;
            }
            used = SLOTS
                .1
                .wait_timeout(used, std::time::Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *used += 1;
        Some(Self)
    }
}

impl Drop for MemorySlot {
    fn drop(&mut self) {
        let mut used = SLOTS.0.lock().unwrap_or_else(PoisonError::into_inner);
        *used = used.saturating_sub(1);
        SLOTS.1.notify_one();
    }
}

#[cfg(test)]
#[path = "wasm_tests.rs"]
mod tests;
