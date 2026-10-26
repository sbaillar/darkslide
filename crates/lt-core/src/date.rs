//! Calendar dates and capture-date filters.
//!
//! Capture times are the camera's wall clock treated as UTC (the
//! `ImageMetaLite::capture_time_ms` convention), so a civil day is
//! `days × DAY_MS` with no time zone.

use serde::{Deserialize, Serialize};

pub const DAY_MS: i64 = 86_400_000;

/// A calendar date (proleptic Gregorian).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CivilDate {
    pub year: i32,
    /// 1–12.
    pub month: u32,
    /// 1–31.
    pub day: u32,
}

impl CivilDate {
    pub fn new(year: i32, month: u32, day: u32) -> Self {
        Self { year, month, day }
    }

    /// Days since 1970-01-01.
    pub fn days(self) -> i64 {
        days_from_civil(
            i64::from(self.year),
            i64::from(self.month),
            i64::from(self.day),
        )
    }
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> CivilDate {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    CivilDate { year, month, day }
}

/// When a photo was taken: an inclusive capture-time range (either end
/// open) and/or months of the year. The default matches every dated photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DateFilter {
    /// ms since the epoch (wall clock as UTC), inclusive.
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// Bit `m - 1` for month `m` ("in march", "summer"); 0 = any month.
    pub months: u16,
}

const ALL_MONTHS: u16 = 0x0FFF;

impl DateFilter {
    pub fn range(from_ms: i64, to_ms: i64) -> Self {
        Self {
            from_ms: Some(from_ms),
            to_ms: Some(to_ms),
            months: 0,
        }
    }

    pub fn months(mask: u16) -> Self {
        Self {
            months: mask & ALL_MONTHS,
            ..Self::default()
        }
    }

    /// Whether a capture time passes. `None` (unknown capture time) never does.
    pub fn matches(&self, capture_ms: Option<i64>) -> bool {
        let Some(t) = capture_ms else { return false };
        if self.from_ms.is_some_and(|f| t < f) || self.to_ms.is_some_and(|e| t > e) {
            return false;
        }
        self.months == 0 || {
            let month = civil_from_days(t.div_euclid(DAY_MS)).month;
            self.months & (1 << (month - 1)) != 0
        }
    }

    /// Both filters at once (two date phrases in one query, "2024 in march").
    /// `None` when nothing can match both.
    #[must_use]
    pub fn intersect(self, other: DateFilter) -> Option<DateFilter> {
        let from_ms = match (self.from_ms, other.from_ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let to_ms = match (self.to_ms, other.to_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let (Some(f), Some(t)) = (from_ms, to_ms)
            && f > t
        {
            return None;
        }
        let mask = |m: u16| if m == 0 { ALL_MONTHS } else { m };
        let months = mask(self.months) & mask(other.months);
        if months == 0 {
            return None;
        }
        Some(DateFilter {
            from_ms,
            to_ms,
            months: if months == ALL_MONTHS { 0 } else { months },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(y: i64, m: i64, d: i64) -> i64 {
        days_from_civil(y, m, d) * DAY_MS
    }

    #[test]
    fn civil_round_trip() {
        for d in [-800_000, -1, 0, 1, 10_957, 20_000, 60_000] {
            let c = civil_from_days(d);
            assert_eq!(c.days(), d, "{c:?}");
        }
        assert_eq!(civil_from_days(0), CivilDate::new(1970, 1, 1));
        assert_eq!(civil_from_days(19_782), CivilDate::new(2024, 2, 29));
    }

    #[test]
    fn range_and_month_matching() {
        let r = DateFilter::range(ms(2024, 3, 1), ms(2024, 3, 31) + 86_399_999);
        assert!(r.matches(Some(ms(2024, 3, 31) + 1000)));
        assert!(!r.matches(Some(ms(2024, 4, 1))));
        assert!(!r.matches(Some(ms(2024, 2, 29))));
        assert!(!r.matches(None));
        let m = DateFilter::months(1 << 2);
        assert!(m.matches(Some(ms(1999, 3, 15))));
        assert!(!m.matches(Some(ms(1999, 4, 15))));
        assert!(!m.matches(None));
        let since = DateFilter {
            from_ms: Some(ms(2020, 1, 1)),
            ..DateFilter::default()
        };
        assert!(since.matches(Some(ms(2030, 1, 1))));
        assert!(!since.matches(Some(ms(2019, 12, 31))));
        assert!(DateFilter::default().matches(Some(0)));
        // Before 1970 too.
        assert!(DateFilter::months(1 << 11).matches(Some(ms(1969, 12, 31))));
    }

    #[test]
    fn intersections() {
        let y = DateFilter::range(ms(2024, 1, 1), ms(2025, 1, 1) - 1);
        let y2 = DateFilter::range(ms(2023, 1, 1), ms(2024, 1, 1) - 1);
        assert_eq!(y.intersect(y2), None);
        assert_eq!(y.intersect(y), Some(y));
        let mar = DateFilter::months(1 << 2);
        let summer = DateFilter::months(0b1110_0000);
        assert_eq!(mar.intersect(summer), None);
        assert_eq!(mar.intersect(mar), Some(mar));
        let both = y.intersect(mar).unwrap();
        assert_eq!(both, mar.intersect(y).unwrap());
        assert!(both.matches(Some(ms(2024, 3, 2))));
        assert!(!both.matches(Some(ms(2023, 3, 2))));
        assert!(!both.matches(Some(ms(2024, 4, 2))));
        let open = DateFilter {
            to_ms: Some(ms(2024, 6, 1)),
            ..DateFilter::default()
        };
        assert_eq!(open.intersect(DateFilter::default()), Some(open));
        assert_eq!(DateFilter::months(0xFFFF), DateFilter::months(ALL_MONTHS));
        assert_eq!(
            DateFilter::months(ALL_MONTHS).intersect(DateFilter::default()),
            Some(DateFilter::default())
        );
    }
}
