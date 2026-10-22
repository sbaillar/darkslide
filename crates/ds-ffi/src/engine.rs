//! The `Engine` object: the single long-lived handle Swift holds.
//!
//! Today it renders the test pattern into caller-owned memory; it will own the
//! renderer, caches, the job scheduler, and open sessions as those crates land
//! (see docs/FFI.md). All async methods go through [`crate::jobs::run_job`]:
//! off the calling thread, cancellable via [`CancelToken`], panic-safe.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::error::DsError;
use crate::events::{Dispatcher, ProgressEvent, ProgressListener, TaskPhase};
use crate::frame::{self, FrameInfo, RenderRequest};
use crate::jobs::{CancelToken, guard, run_job};
use crate::render::{self, TestPatternParams};

/// Shared engine state. Thread-safe; Swift may call it from any thread.
#[derive(Debug, uniffi::Object)]
pub struct Engine {
    events: Arc<Dispatcher>,
    next_task: AtomicU64,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            events: Arc::new(Dispatcher::new()),
            next_task: AtomicU64::new(1),
        }
    }
}

/// A caller-owned destination buffer, passed as raw address + length.
#[derive(Debug, Clone, Copy)]
struct Dest {
    addr: usize,
    len: usize,
}

impl Dest {
    fn new(ptr: u64, len: u64, needed: u64) -> Result<Self, DsError> {
        if ptr == 0 {
            return Err(DsError::invalid("destination pointer is null"));
        }
        if len < needed {
            return Err(DsError::invalid(format!(
                "destination holds {len} bytes, frame needs {needed}"
            )));
        }
        let addr = usize::try_from(ptr).map_err(|_| DsError::invalid("pointer out of range"))?;
        let len = usize::try_from(len)
            .ok()
            .filter(|&l| isize::try_from(l).is_ok())
            .ok_or_else(|| DsError::invalid("destination length out of range"))?;
        Ok(Self { addr, len })
    }

    /// The destination as a mutable byte slice.
    ///
    /// # Safety
    /// Caller contract of `Engine::render_test_pattern_into` (documented there
    /// and in docs/FFI.md): `addr..addr+len` is valid, writable memory that
    /// nothing else reads or writes until the call's future completes (or is
    /// dropped — `run_job` waits for the worker before either happens).
    #[allow(unsafe_code)]
    unsafe fn slice<'a>(self) -> &'a mut [u8] {
        // SAFETY: non-null and len ≤ isize::MAX were checked in `new`; u8 has
        // no alignment requirement; validity and exclusivity for the duration
        // of the job are the caller's documented contract (see above).
        unsafe { std::slice::from_raw_parts_mut(self.addr as *mut u8, self.len) }
    }
}

impl Engine {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Install (or with `None`, remove) the progress listener.
    pub fn set_progress_listener(&self, listener: Option<Arc<dyn ProgressListener>>) {
        self.events.set_listener(listener);
    }

    /// Wait (up to 5 s) until every progress event emitted so far was delivered.
    pub fn flush_progress(&self) -> bool {
        self.events.flush(Duration::from_secs(5))
    }
}

#[uniffi::export]
impl Engine {
    #[uniffi::constructor(name = "new")]
    pub fn ffi_new() -> Result<Arc<Self>, DsError> {
        guard(|| Ok(Self::new()))
    }

    /// Install (or with `None`, remove) the progress listener.
    ///
    /// The engine keeps a strong reference: Swift implementations must hold
    /// their own owner *weakly* to avoid a cycle (Engine → listener → owner → Engine).
    #[uniffi::method(name = "set_progress_listener")]
    pub fn ffi_set_progress_listener(
        &self,
        listener: Option<Arc<dyn ProgressListener>>,
    ) -> Result<(), DsError> {
        guard(|| {
            self.set_progress_listener(listener);
            Ok(())
        })
    }

    /// Wait (up to 5 s) until every progress event emitted so far was delivered.
    #[uniffi::method(name = "flush_progress")]
    pub fn ffi_flush_progress(&self) -> Result<bool, DsError> {
        guard(|| Ok(self.flush_progress()))
    }

    /// `Running` events dropped because the listener fell behind.
    pub fn dropped_progress_events(&self) -> Result<u64, DsError> {
        guard(|| Ok(self.events.dropped()))
    }

    /// Render the test pattern for a `source_width`×`source_height` image into
    /// caller memory at `dest_ptr` (`dest_len` bytes), laid out per
    /// [`frame::plan_frame`] for the same arguments (call it first to size the
    /// buffer). Returns that layout.
    ///
    /// **Contract:** the memory stays valid and untouched by the caller until
    /// this call returns. Cancel with `token`; the call then returns
    /// `DsError::Cancelled` after at most one more band of rows.
    #[allow(clippy::too_many_arguments)]
    pub async fn render_test_pattern_into(
        &self,
        source_width: u32,
        source_height: u32,
        params: TestPatternParams,
        request: RenderRequest,
        dest_ptr: u64,
        dest_len: u64,
        token: Arc<CancelToken>,
    ) -> Result<FrameInfo, DsError> {
        let info = frame::plan(source_width, source_height, &request)?;
        params.validate()?;
        let dest = Dest::new(dest_ptr, dest_len, info.byte_len())?;
        let id = self.next_task.fetch_add(1, Ordering::Relaxed);
        let closed = Arc::new(AtomicBool::new(false));
        let mut task = TaskReporter::new(id, "render", self.events.clone(), closed.clone());
        let result = run_job(token, move |cancelled| {
            // SAFETY: see `Dest::slice`; `run_job` does not return (or drop)
            // before this closure has finished with the slice.
            #[allow(unsafe_code)]
            let buf = unsafe { dest.slice() };
            let r = render::render_pattern_into(&info, params, buf, cancelled, &mut |d, t| {
                task.progress(d, t);
            });
            task.finish(&r);
            r.map(|()| info)
        })
        .await;
        if let Err(e @ (DsError::Cancelled | DsError::Internal { .. })) = &result {
            // The worker never ran (pre-cancelled) or panicked: close the task here.
            if !closed.load(Ordering::Acquire) {
                TaskReporter::close(&self.events, id, "render", e);
            }
        }
        result
    }
}

/// Emits the progress events of one task.
struct TaskReporter {
    id: u64,
    kind: &'static str,
    events: Arc<Dispatcher>,
    last: (u64, u64),
    /// Set once the terminal event was emitted.
    closed: Arc<AtomicBool>,
}

impl TaskReporter {
    fn new(id: u64, kind: &'static str, events: Arc<Dispatcher>, closed: Arc<AtomicBool>) -> Self {
        let t = Self {
            id,
            kind,
            events,
            last: (0, 0),
            closed,
        };
        t.send(TaskPhase::Started, 0, 0);
        t
    }

    fn progress(&mut self, completed: u64, total: u64) {
        self.last = (completed, total);
        self.send(TaskPhase::Running, completed, total);
    }

    fn finish(&self, r: &Result<(), DsError>) {
        self.send(phase_for(r.as_ref().err()), self.last.0, self.last.1);
        self.closed.store(true, Ordering::Release);
    }

    /// Terminal event for a task whose worker never reported (pre-cancelled, panicked).
    fn close(events: &Arc<Dispatcher>, id: u64, kind: &str, e: &DsError) {
        events.emit(ProgressEvent {
            task_id: id,
            kind: kind.into(),
            phase: phase_for(Some(e)),
            completed: 0,
            total: 0,
        });
    }

    fn send(&self, phase: TaskPhase, completed: u64, total: u64) {
        self.events.emit(ProgressEvent {
            task_id: self.id,
            kind: self.kind.to_owned(),
            phase,
            completed,
            total,
        });
    }
}

fn phase_for(e: Option<&DsError>) -> TaskPhase {
    match e {
        None => TaskPhase::Finished,
        Some(DsError::Cancelled) => TaskPhase::Cancelled,
        Some(_) => TaskPhase::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::FrameFormat;
    use crate::render::tests::fit;
    use futures_executor::block_on;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<ProgressEvent>>);

    impl ProgressListener for Recorder {
        fn on_progress(&self, event: ProgressEvent) -> Result<(), crate::error::DsError> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    impl Recorder {
        fn phases(&self) -> Vec<TaskPhase> {
            self.0.lock().unwrap().iter().map(|e| e.phase).collect()
        }
    }

    fn render(
        engine: &Engine,
        w: u32,
        h: u32,
        params: TestPatternParams,
        token: Arc<CancelToken>,
    ) -> (Result<FrameInfo, DsError>, Vec<u8>) {
        let req = fit(w.max(h), FrameFormat::Rgba8Srgb);
        let n = frame::plan(w, h, &req).map_or(16, |i| i.byte_len() as usize);
        let mut buf = vec![0u8; n];
        let r = block_on(engine.render_test_pattern_into(
            w,
            h,
            params,
            req,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            token,
        ));
        (r, buf)
    }

    #[test]
    fn renders_into_caller_memory_and_reports_in_order() {
        let engine = Engine::new();
        let rec = Arc::new(Recorder::default());
        engine.set_progress_listener(Some(rec.clone()));
        let (r, buf) = render(
            &engine,
            600,
            1000,
            TestPatternParams::default(),
            CancelToken::new(),
        );
        let info = r.unwrap();
        assert_eq!((info.width, info.height), (600, 1000));
        // White chart patch (row 3, col 0).
        let (x, y) = (
            (0.5 / 6.0 * 600.0) as u32,
            ((0.45 + 3.5 / 4.0 * 0.40) * 1000.0) as u32,
        );
        let i = (y * info.bytes_per_row + x * 4) as usize;
        assert_eq!(&buf[i..i + 4], &[243, 243, 242, 255]);

        assert!(engine.flush_progress());
        let events = rec.0.lock().unwrap().clone();
        let bands = u64::from(1000_u32.div_ceil(render::BAND_ROWS));
        assert_eq!(events.first().unwrap().phase, TaskPhase::Started);
        assert_eq!(events.last().unwrap().phase, TaskPhase::Finished);
        assert_eq!(events.last().unwrap().completed, bands);
        assert_eq!(events.len() as u64, bands + 2);
        assert!(
            events
                .iter()
                .all(|e| e.task_id == events[0].task_id && e.kind == "render")
        );
        assert_eq!(engine.dropped_progress_events().unwrap(), 0);
    }

    #[test]
    fn task_ids_increase_and_listener_can_be_removed() {
        let engine = Engine::new();
        let rec = Arc::new(Recorder::default());
        engine.set_progress_listener(Some(rec.clone()));
        for _ in 0..2 {
            render(
                &engine,
                4,
                4,
                TestPatternParams::default(),
                CancelToken::new(),
            )
            .0
            .unwrap();
        }
        assert!(engine.flush_progress());
        let ids: Vec<_> = rec.0.lock().unwrap().iter().map(|e| e.task_id).collect();
        assert!(ids.first() < ids.last());

        engine.set_progress_listener(None);
        let before = rec.0.lock().unwrap().len();
        render(
            &engine,
            4,
            4,
            TestPatternParams::default(),
            CancelToken::new(),
        )
        .0
        .unwrap();
        assert!(engine.flush_progress());
        assert_eq!(rec.0.lock().unwrap().len(), before);
    }

    #[test]
    fn invalid_arguments_fail_fast_without_events() {
        let engine = Engine::new();
        let rec = Arc::new(Recorder::default());
        engine.set_progress_listener(Some(rec.clone()));
        let (r, _) = render(
            &engine,
            0,
            4,
            TestPatternParams::default(),
            CancelToken::new(),
        );
        assert!(matches!(r, Err(DsError::InvalidArgument { .. })));
        let bad = TestPatternParams {
            exposure_ev: 9.0,
            ..Default::default()
        };
        assert!(matches!(
            render(&engine, 4, 4, bad, CancelToken::new()).0,
            Err(DsError::InvalidArgument { .. })
        ));
        let req = fit(4, FrameFormat::Rgba8Srgb);
        let mut small = vec![0u8; 8];
        for (ptr, len) in [(0_u64, 4096_u64), (small.as_mut_ptr() as u64, 8)] {
            let r = block_on(engine.render_test_pattern_into(
                4,
                4,
                TestPatternParams::default(),
                req,
                ptr,
                len,
                CancelToken::new(),
            ));
            assert!(matches!(r, Err(DsError::InvalidArgument { .. })), "{r:?}");
        }
        assert!(engine.flush_progress());
        assert_eq!(rec.phases().len(), 0);
    }

    #[test]
    fn dest_rejects_out_of_range_lengths() {
        assert!(Dest::new(8, u64::MAX, 1).is_err());
        assert!(Dest::new(8, 16, 8).is_ok());
    }

    #[test]
    fn cancelled_token_stops_the_render_and_reports_cancelled() {
        let engine = Engine::new();
        let rec = Arc::new(Recorder::default());
        engine.set_progress_listener(Some(rec.clone()));
        let token = CancelToken::new();
        token.cancel();
        let (r, buf) = render(&engine, 800, 800, TestPatternParams::default(), token);
        assert_eq!(r, Err(DsError::Cancelled));
        assert!(buf.iter().all(|&b| b == 0), "nothing was written");
        assert!(engine.flush_progress());
        assert_eq!(rec.phases(), vec![TaskPhase::Started, TaskPhase::Cancelled]);
    }

    #[test]
    fn cancelling_mid_render_returns_early() {
        let engine = Engine::new();
        let (w, h) = (4000_u32, 16_000_u32);
        let req = crate::frame::RenderRequest {
            scale: Some(1.0),
            ..fit(1, FrameFormat::Rgba8Srgb)
        };
        let info = frame::plan(w, h, &req).unwrap();
        let mut buf = vec![0u8; info.byte_len() as usize];
        let token = CancelToken::new();
        let t2 = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            t2.cancel();
        });
        let r = block_on(engine.render_test_pattern_into(
            w,
            h,
            TestPatternParams::default(),
            req,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            token,
        ));
        assert_eq!(r, Err(DsError::Cancelled));
        let last_row = &buf[buf.len() - info.bytes_per_row as usize..];
        assert!(
            last_row.iter().all(|&b| b == 0),
            "the end was never rendered"
        );
    }
}
