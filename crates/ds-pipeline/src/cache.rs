//! BASE caches and the process-wide memory budget they share.
//!
//! One entry per (image, base level). A white-balance change never touches it (BASE is
//! camera RGB at the reference WB). A 1:1 region render may cache only a sensor
//! sub-rectangle; a later view outside it replaces the entry instead of adding one.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ImageKey;
use crate::plan::{BaseKey, DemosaicMode};

/// Physical RAM in bytes (macOS `hw.memsize`; 8 GiB when unknown).
pub fn physical_memory() -> u64 {
    static MEM: OnceLock<u64> = OnceLock::new();
    *MEM.get_or_init(|| {
        std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(8 << 30)
    })
}

/// Bytes counted against the budget in the process: BASE caches (CPU and GPU; unified
/// memory) plus [`Account`]ed buffers.
static USED: AtomicU64 = AtomicU64::new(0);
/// Highest `USED` seen (for budget tests and diagnostics).
static PEAK: AtomicU64 = AtomicU64::new(0);

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

fn add_used(bytes: u64) {
    let now = USED.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

/// Highest number of bytes counted against the budget since the last [`reset_peak`].
pub fn peak_bytes() -> u64 {
    PEAK.load(Ordering::Relaxed)
}

/// Restart peak tracking from the current use.
pub fn reset_peak() {
    PEAK.store(used_bytes(), Ordering::Relaxed);
}

/// Would `bytes` more exceed the budget?
pub fn over_budget_with(bytes: u64) -> bool {
    used_bytes() + bytes > budget_bytes()
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
    USED.load(Ordering::Relaxed)
}

/// A share of the budget for memory outside the BASE caches (GPU frame buffers, raw
/// uploads, output slots, mask rasters): counted while alive.
#[derive(Debug)]
pub(crate) struct Account(u64, Use);

impl Account {
    pub fn new(what: Use, bytes: u64) -> Self {
        BY_USE[what as usize].fetch_add(bytes, Ordering::Relaxed);
        add_used(bytes);
        Self(bytes, what)
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        BY_USE[self.1 as usize].fetch_sub(self.0, Ordering::Relaxed);
        USED.fetch_sub(self.0, Ordering::Relaxed);
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
        add_used(counted);
        Self { v, counted }
    }
    pub fn get(&self) -> &V {
        &self.v
    }
}

impl<V: Resident> Drop for Tracked<V> {
    fn drop(&mut self) {
        BY_USE[Use::Base as usize].fetch_sub(self.counted, Ordering::Relaxed);
        USED.fetch_sub(self.counted, Ordering::Relaxed);
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
}
