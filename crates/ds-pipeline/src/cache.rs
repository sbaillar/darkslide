//! BASE caches and the process-wide memory budget they share.
//!
//! One entry per (image, base level). A white-balance change never touches it (BASE is
//! camera RGB at the reference WB). A 1:1 region render may cache only a sensor
//! sub-rectangle; a later view outside it replaces the entry instead of adding one.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ImageKey;
use crate::plan::{BaseKey, DemosaicMode};

/// Physical RAM in bytes (macOS `hw.memsize`; 8 GiB when unknown).
pub fn physical_memory() -> u64 {
    static MEM: OnceLock<u64> = OnceLock::new();
    *MEM.get_or_init(|| sysctl_memsize().unwrap_or(8 << 30))
}

#[cfg(target_os = "macos")]
fn sysctl_memsize() -> Option<u64> {
    std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
}

/// iOS can't start processes: budget for 8 GiB (the smallest M-series iPad), which keeps
/// the BASE caches well inside what iPadOS lets one app use.
#[cfg(not(target_os = "macos"))]
fn sysctl_memsize() -> Option<u64> {
    None
}

/// Memory counted against a budget, and promised to renders in flight.
///
/// A render is admitted with a [`Reservation`] for its estimated working set. Buffers it
/// then creates are paid from that reservation, and buffers it frees go back into it
/// (one it drops to replace, or evicts to make room, stays its own), so memory promised
/// to one render is never handed to another in between. Checks compare `committed` (what
/// is counted plus what reservations have not spent yet) with the budget: as long as
/// renders allocate within their reservations, or make room first, `used` stays under it.
#[derive(Debug)]
struct Ledger {
    /// Bytes counted: BASE caches (CPU and GPU; unified memory) plus [`Account`]ed
    /// buffers.
    used: AtomicU64,
    /// Highest `used` seen (for budget tests and diagnostics).
    peak: AtomicU64,
    /// `used` plus the unspent part of every reservation.
    committed: AtomicU64,
    /// Bytes allocated beyond the allocating render's reservation (diagnostics).
    overdrawn: AtomicU64,
}

/// The process's ledger (the budget is process-wide).
static LEDGER: Ledger = Ledger::new();

thread_local! {
    /// Unspent bytes of the [`Reservation`] held by the render on this thread, if any.
    static RESERVED_HERE: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Unspent bytes of this thread's reservation.
fn reserved_here() -> u64 {
    RESERVED_HERE.with(Cell::get).unwrap_or(0)
}

impl Ledger {
    const fn new() -> Self {
        Self {
            used: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            committed: AtomicU64::new(0),
            overdrawn: AtomicU64::new(0),
        }
    }

    /// Count `bytes` newly allocated: paid from this thread's reservation as far as it
    /// goes, the rest is new commitment.
    fn add(&self, bytes: u64) {
        let now = self.used.fetch_add(bytes, Ordering::SeqCst) + bytes;
        self.peak.fetch_max(now, Ordering::Relaxed);
        let paid = RESERVED_HERE.with(|r| {
            let left = r.get()?;
            let paid = left.min(bytes);
            r.set(Some(left - paid));
            if paid < bytes {
                self.overdrawn.fetch_add(bytes - paid, Ordering::Relaxed);
            }
            Some(paid)
        });
        self.committed
            .fetch_add(bytes - paid.unwrap_or(0), Ordering::SeqCst);
    }

    /// Count `bytes` freed: back into this thread's reservation if it holds one.
    fn sub(&self, bytes: u64) {
        let kept = RESERVED_HERE.with(|r| r.get().map(|left| r.set(Some(left + bytes))));
        if kept.is_none() {
            self.committed.fetch_sub(bytes, Ordering::SeqCst);
        }
        self.used.fetch_sub(bytes, Ordering::SeqCst);
    }

    /// Would allocating `bytes` more exceed `budget`? Only the part this thread's
    /// reservation doesn't cover counts, next to everything committed.
    fn over_with(&self, bytes: u64, budget: u64) -> bool {
        self.committed
            .load(Ordering::SeqCst)
            .saturating_add(bytes.saturating_sub(reserved_here()))
            > budget
    }

    /// Reserve `bytes` for the render on this thread if they fit under `limit` next to
    /// everything committed (`None`: unconditionally, for a render that runs alone).
    fn reserve(&self, bytes: u64, limit: Option<u64>) -> Option<Reservation<'_>> {
        let fits = |c: u64| match limit {
            Some(l) => c.checked_add(bytes).filter(|t| *t <= l),
            None => Some(c.saturating_add(bytes)),
        };
        let mut c = self.committed.load(Ordering::SeqCst);
        while let Err(now) =
            self.committed
                .compare_exchange_weak(c, fits(c)?, Ordering::SeqCst, Ordering::SeqCst)
        {
            c = now;
        }
        let outer = RESERVED_HERE.with(|r| r.replace(Some(bytes)));
        Some(Reservation {
            ledger: self,
            outer,
            _thread: PhantomData,
        })
    }
}

/// Budget held for the render on this thread (see [`Ledger`]); what is left is given
/// back on drop. Not `Send`: the reservation is the thread's.
#[derive(Debug)]
pub(crate) struct Reservation<'a> {
    ledger: &'a Ledger,
    /// The reservation this one stands in for while alive (a nested one).
    outer: Option<u64>,
    _thread: PhantomData<*const ()>,
}

impl Reservation<'static> {
    /// Reserve `bytes` of the process budget if they fit under `limit` next to
    /// everything committed (`None`: unconditionally, for a render that runs alone).
    pub fn try_new(bytes: u64, limit: Option<u64>) -> Option<Self> {
        LEDGER.reserve(bytes, limit)
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let left = RESERVED_HERE.with(|r| r.replace(self.outer)).unwrap_or(0);
        self.ledger.committed.fetch_sub(left, Ordering::SeqCst);
    }
}

/// Bytes counted against the budget plus what renders in flight reserved and have not
/// allocated yet.
pub fn committed_bytes() -> u64 {
    LEDGER.committed.load(Ordering::SeqCst)
}

/// Bytes renders allocated beyond their reservations so far (an estimate fell short;
/// the allocation made room first where it could).
pub fn overdrawn_bytes() -> u64 {
    LEDGER.overdrawn.load(Ordering::Relaxed)
}

/// What memory counted against the budget is used for (diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    Base = 0,
    FramePool = 1,
    Output = 2,
    Raw = 3,
    MaskRaster = 4,
    GpuRaster = 5,
    Export = 6,
}

static BY_USE: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Bytes counted per [`Use`], in `Use` order.
pub fn used_by_use() -> [u64; 7] {
    std::array::from_fn(|i| BY_USE[i].load(Ordering::Relaxed))
}

/// Highest number of bytes counted against the budget since the last [`reset_peak`].
pub fn peak_bytes() -> u64 {
    LEDGER.peak.load(Ordering::Relaxed)
}

/// Restart peak tracking from the current use.
pub fn reset_peak() {
    LEDGER.peak.store(used_bytes(), Ordering::Relaxed);
}

/// Would allocating `bytes` more exceed the budget? Only the part this thread's
/// reservation doesn't cover counts, next to everything committed.
pub fn over_budget_with(bytes: u64) -> bool {
    LEDGER.over_with(bytes, budget_bytes())
}

/// 0 = derive from physical memory.
static LIMIT_OVERRIDE: AtomicU64 = AtomicU64::new(0);

/// The shared budget: a quarter of physical memory unless overridden.
pub fn budget_bytes() -> u64 {
    match LIMIT_OVERRIDE.load(Ordering::Relaxed) {
        0 => physical_memory() / 4,
        v => v,
    }
}

/// Override the shared budget (0 restores the default). For hosts that know better.
pub fn set_budget_bytes(bytes: u64) {
    LIMIT_OVERRIDE.store(bytes, Ordering::Relaxed);
}

/// Bytes currently counted against the budget.
pub fn used_bytes() -> u64 {
    LEDGER.used.load(Ordering::SeqCst)
}

/// A share of the budget for memory outside the BASE caches (GPU frame buffers, raw
/// uploads, output slots, mask rasters): counted while alive.
#[derive(Debug)]
pub(crate) struct Account(u64, Use);

impl Account {
    pub fn new(what: Use, bytes: u64) -> Self {
        BY_USE[what as usize].fetch_add(bytes, Ordering::Relaxed);
        LEDGER.add(bytes);
        Self(bytes, what)
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        BY_USE[self.1 as usize].fetch_sub(self.0, Ordering::Relaxed);
        LEDGER.sub(self.0);
    }
}

/// Something a cache can hold.
pub(crate) trait Resident {
    fn bytes(&self) -> u64;
    /// Covered rectangle (x, y, w, h) in the level's pixel grid.
    fn rect(&self) -> [u32; 4];
}

/// Accounting wrapper: adds to the global counter on creation, subtracts on drop.
#[derive(Debug)]
pub(crate) struct Tracked<V: Resident> {
    v: V,
    /// What was added to the counters; the drop gives back exactly this, whatever
    /// `bytes()` reports by then.
    counted: u64,
}

impl<V: Resident> Tracked<V> {
    fn new(v: V) -> Self {
        let counted = v.bytes();
        BY_USE[Use::Base as usize].fetch_add(counted, Ordering::Relaxed);
        LEDGER.add(counted);
        Self { v, counted }
    }
    pub fn get(&self) -> &V {
        &self.v
    }
}

impl<V: Resident> Drop for Tracked<V> {
    fn drop(&mut self) {
        BY_USE[Use::Base as usize].fetch_sub(self.counted, Ordering::Relaxed);
        LEDGER.sub(self.counted);
    }
}

fn covers(have: [u32; 4], need: [u32; 4]) -> bool {
    let end = |r: [u32; 4], i: usize| u64::from(r[i]) + u64::from(r[i + 2]);
    have[0] <= need[0]
        && have[1] <= need[1]
        && end(have, 0) >= end(need, 0)
        && end(have, 1) >= end(need, 1)
}

/// The rectangle to demosaic for a view needing `need` on a `full`-sized level:
/// full frame unless the view is a small part of a full-resolution RCD level, in which
/// case the need plus 25 % slack on each side (so small pans stay cached).
pub(crate) fn build_rect(bk: BaseKey, need: [u32; 4], full: [u32; 2]) -> [u32; 4] {
    let whole = [0, 0, full[0], full[1]];
    let area = u64::from(need[2]) * u64::from(need[3]);
    let full_area = u64::from(full[0]) * u64::from(full[1]);
    if bk.mode != DemosaicMode::Rcd || bk.down != 0 || area * 10 > full_area * 6 {
        return whole;
    }
    let (sx, sy) = (need[2] / 4, need[3] / 4);
    let x0 = need[0].saturating_sub(sx) & !1;
    let y0 = need[1].saturating_sub(sy) & !1;
    let x1 = (need[0] + need[2] + sx).min(full[0]);
    let y1 = (need[1] + need[3] + sy).min(full[1]);
    [x0, y0, x1 - x0, y1 - y0]
}

/// LRU cache of BASE levels.
#[derive(Debug)]
pub(crate) struct BaseCache<V: Resident> {
    entries: Vec<(ImageKey, BaseKey, Arc<Tracked<V>>)>,
    pub builds: u64,
    /// Per-cache budget instead of the shared one (tests).
    local_budget: Option<u64>,
}

impl<V: Resident> Default for BaseCache<V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            builds: 0,
            local_budget: None,
        }
    }
}

impl<V: Resident> BaseCache<V> {
    #[cfg(test)]
    pub fn with_budget(bytes: u64) -> Self {
        Self {
            local_budget: Some(bytes),
            ..Self::default()
        }
    }

    fn over_budget(&self) -> bool {
        match self.local_budget {
            Some(b) => self.bytes() > b,
            None => used_bytes() > budget_bytes(),
        }
    }

    /// The cached level if it covers `need` (marked most recently used).
    pub fn get(&mut self, key: ImageKey, bk: BaseKey, need: [u32; 4]) -> Option<Arc<Tracked<V>>> {
        let pos = self
            .entries
            .iter()
            .position(|(k, b, _)| *k == key && *b == bk)?;
        if !covers(self.entries[pos].2.get().rect(), need) {
            return None;
        }
        let e = self.entries.remove(pos);
        let img = Arc::clone(&e.2);
        self.entries.push(e);
        Some(img)
    }

    /// Insert a freshly built level, replacing any entry for the same image and level and
    /// evicting least recently used entries while over budget.
    pub fn insert(&mut self, key: ImageKey, bk: BaseKey, v: V) -> Arc<Tracked<V>> {
        self.entries.retain(|(k, b, _)| !(*k == key && *b == bk));
        self.builds += 1;
        let img = Arc::new(Tracked::new(v));
        self.entries.push((key, bk, Arc::clone(&img)));
        while self.over_budget() && self.entries.len() > 1 {
            self.entries.remove(0);
        }
        img
    }

    #[cfg(test)]
    /// The cached level if it covers `need`; otherwise build (replacing any entry for the
    /// same image and level) the rectangle chosen by [`build_rect`].
    pub fn get_or_build(
        &mut self,
        key: ImageKey,
        bk: BaseKey,
        need: [u32; 4],
        full: [u32; 2],
        build: impl FnOnce([u32; 4]) -> V,
    ) -> Arc<Tracked<V>> {
        if let Some(img) = self.get(key, bk, need) {
            return img;
        }
        self.insert(key, bk, build(build_rect(bk, need, full)))
    }

    /// The rectangle cached for (`key`, `bk`), if any (not marked as used).
    pub fn rect_of(&self, key: ImageKey, bk: BaseKey) -> Option<[u32; 4]> {
        self.entries
            .iter()
            .find(|(k, b, _)| *k == key && *b == bk)
            .map(|e| e.2.get().rect())
    }

    /// Drop `key`'s entry for level `bk` (before building its replacement).
    pub fn evict_level(&mut self, key: ImageKey, bk: BaseKey) {
        self.entries.retain(|(k, b, _)| !(*k == key && *b == bk));
    }

    /// Drop the least recently used level of another image than `keep`; false if none.
    pub fn evict_oldest_except(&mut self, keep: ImageKey) -> bool {
        match self.entries.iter().position(|(k, _, _)| *k != keep) {
            Some(i) => {
                self.entries.remove(i);
                true
            }
            None => false,
        }
    }

    /// Drop every level of `key`.
    pub fn evict(&mut self, key: ImageKey) {
        self.entries.retain(|(k, _, _)| *k != key);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.2.get().bytes()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Blob(u64, [u32; 4]);
    impl Resident for Blob {
        fn bytes(&self) -> u64 {
            self.0
        }
        fn rect(&self) -> [u32; 4] {
            self.1
        }
    }

    const RCD: BaseKey = BaseKey {
        mode: DemosaicMode::Rcd,
        down: 0,
        precise: false,
    };

    #[test]
    fn rect_choice() {
        let full = [6000, 4000];
        assert_eq!(
            build_rect(RCD, [0, 0, 6000, 4000], full),
            [0, 0, 6000, 4000]
        );
        let r = build_rect(RCD, [1001, 1001, 1000, 800], full);
        assert_eq!(r, [750, 800, 1501, 1201]);
        assert!(covers(r, [1001, 1001, 1000, 800]));
        let sp = BaseKey {
            mode: DemosaicMode::Superpixel,
            down: 0,
            precise: false,
        };
        assert_eq!(build_rect(sp, [10, 10, 10, 10], full), [0, 0, 6000, 4000]);
        let down = BaseKey {
            mode: DemosaicMode::Rcd,
            down: 1,
            precise: false,
        };
        assert_eq!(
            build_rect(down, [10, 10, 10, 10], [3000, 2000]),
            [0, 0, 3000, 2000]
        );
    }

    #[test]
    fn one_entry_per_level_replaced_not_accumulated() {
        let mut c: BaseCache<Blob> = BaseCache::default();
        let full = [6000, 4000];
        let need = [1000, 1000, 500, 500];
        c.get_or_build(ImageKey(1), RCD, need, full, |r| Blob(100, r));
        // Inside the slack: hit.
        c.get_or_build(ImageKey(1), RCD, [1010, 1010, 500, 500], full, |_| {
            panic!("cached")
        });
        // A pan outside it: rebuilt, still one entry.
        c.get_or_build(ImageKey(1), RCD, [4000, 3000, 500, 500], full, |r| {
            Blob(100, r)
        });
        assert_eq!((c.len(), c.builds), (1, 2));
        assert_eq!(c.bytes(), 100);
        c.evict(ImageKey(1));
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn budget_evicts_oldest() {
        assert!(physical_memory() >= 1 << 30);
        assert_eq!(budget_bytes(), physical_memory() / 4);
        let mut c: BaseCache<Blob> = BaseCache::with_budget(150);
        let full = [10, 10];
        let sp = BaseKey {
            mode: DemosaicMode::Superpixel,
            down: 0,
            precise: false,
        };
        c.get_or_build(ImageKey(1), sp, [0, 0, 10, 10], full, |r| Blob(100, r));
        c.get_or_build(ImageKey(2), sp, [0, 0, 10, 10], full, |r| Blob(100, r));
        assert_eq!(c.len(), 1, "oldest dropped once the budget is exceeded");
        c.get_or_build(ImageKey(2), sp, [0, 0, 10, 10], full, |_| {
            panic!("newest kept")
        });
    }

    #[test]
    fn tracked_gives_back_what_it_added() {
        use std::sync::atomic::AtomicU64;
        #[derive(Debug)]
        struct Shrinking(Arc<AtomicU64>);
        impl Resident for Shrinking {
            fn bytes(&self) -> u64 {
                self.0.load(Ordering::Relaxed)
            }
            fn rect(&self) -> [u32; 4] {
                [0; 4]
            }
        }
        let size = Arc::new(AtomicU64::new(1000));
        let t = Tracked::new(Shrinking(size.clone()));
        // The resident is trimmed while cached; the counters must not under-flow later.
        size.store(10, Ordering::Relaxed);
        assert_eq!(t.counted, 1000);
    }

    #[test]
    fn covers_does_not_overflow_on_huge_rects() {
        assert!(!covers([0, 0, u32::MAX, 10], [u32::MAX, 0, u32::MAX, 10]));
        assert!(covers([0, 0, u32::MAX, 10], [1, 0, u32::MAX - 1, 10]));
    }

    fn used(l: &Ledger) -> u64 {
        l.used.load(Ordering::SeqCst)
    }

    fn committed(l: &Ledger) -> u64 {
        l.committed.load(Ordering::SeqCst)
    }

    /// The CI failure, forced: render A's estimate counted on its pooled frame buffers,
    /// its make-room then trimmed them before it developed (reallocating them). Admission
    /// used to compare only what was counted (plus reservations) with the budget, so it
    /// handed A's trimmed buffers to render B in that window, and A's reallocation pushed
    /// the count past the budget. The trimmed bytes now stay A's until A ends.
    #[test]
    fn what_a_render_frees_stays_its_own_so_the_ceiling_holds() {
        let l = Ledger::new();
        let budget = 100;
        // A's pooled frame buffers, allocated by an earlier render.
        l.add(50);
        let (to_b, a_trimmed) = std::sync::mpsc::channel();
        let (to_a, b_done) = std::sync::mpsc::channel();
        let l = &l;
        std::thread::scope(|s| {
            s.spawn(move || {
                // A: the pool is resident, so 10 more cover its working set.
                let r = l.reserve(10, Some(budget)).expect("A fits");
                l.sub(50);
                to_b.send(()).unwrap();
                b_done.recv().unwrap();
                // A develops: its frame buffers again, plus what it reserved.
                l.add(50);
                l.add(10);
                drop(r);
            });
            a_trimmed.recv().unwrap();
            // B, in the window: only 40 are really free (the old check saw 90).
            assert_eq!(used(l), 0);
            assert!(l.over_with(45, budget));
            assert!(l.reserve(45, Some(budget)).is_none(), "A's buffers are A's");
            let b = l.reserve(40, Some(budget)).expect("what is free");
            l.add(40);
            drop(b);
            to_a.send(()).unwrap();
        });
        assert_eq!(used(l), 100);
        assert!(l.peak.load(Ordering::SeqCst) <= budget, "a hard ceiling");
        assert_eq!(l.overdrawn.load(Ordering::SeqCst), 0);
        assert_eq!(committed(l), used(l), "reservations given back");
    }

    #[test]
    fn allocations_are_paid_from_the_reservation_and_the_rest_is_overdrawn() {
        let l = Ledger::new();
        l.add(30);
        let r = l.reserve(20, Some(100)).expect("fits");
        assert_eq!(committed(&l), 50);
        // Only what the reservation doesn't cover counts against the budget.
        assert!(!l.over_with(70, 100));
        assert!(l.over_with(71, 100));
        l.add(15);
        assert_eq!((used(&l), committed(&l)), (45, 50));
        assert!(!l.over_with(55, 100));
        assert!(l.over_with(56, 100));
        // Past the reservation: overdrawn and committed anew.
        l.add(10);
        assert_eq!((used(&l), committed(&l)), (55, 55));
        assert_eq!(l.overdrawn.load(Ordering::SeqCst), 5);
        // A free goes back into the reservation, not to everyone.
        l.sub(20);
        assert_eq!((used(&l), committed(&l)), (35, 55));
        drop(r);
        assert_eq!(committed(&l), 35);
        // Without a reservation a free is everyone's at once.
        l.sub(35);
        assert_eq!((used(&l), committed(&l)), (0, 0));
    }

    #[test]
    fn a_render_alone_is_reserved_past_the_budget_and_nested_ones_restore() {
        let l = Ledger::new();
        let alone = l.reserve(500, None).expect("alone: always");
        assert!(l.reserve(1, Some(100)).is_none(), "nothing else fits");
        assert!(l.reserve(u64::MAX, Some(u64::MAX)).is_none(), "no overflow");
        {
            // A nested reservation stands in for the outer one while alive.
            let inner = l.reserve(10, None).expect("nested");
            l.add(4);
            assert_eq!(reserved_here(), 6);
            drop(inner);
        }
        assert_eq!(reserved_here(), 500);
        assert_eq!(committed(&l), 504);
        drop(alone);
        assert_eq!((reserved_here(), committed(&l)), (0, 4));
    }
}
