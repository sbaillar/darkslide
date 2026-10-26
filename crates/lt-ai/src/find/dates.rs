//! Date phrases: "2024", "march 2024", "in march", "last summer", "since
//! 2020", "from 2019 to 2021", "the 90s", "last 3 months", "yesterday".
//!
//! Seasons are northern-hemisphere meteorological ones (spring = March–May,
//! summer = June–August, fall = September–November, winter = December–
//! February, and "winter 2024" starts in December 2024). "may", "march",
//! "spring" and "fall" are also ordinary words, so alone they count as dates
//! only after "in", "during", "since", "last", "this", "of"… or before a year.

use lt_core::date::{CivilDate, DAY_MS, DateFilter, days_from_civil};

/// `[first day 00:00, last day 23:59:59.999]` in ms.
fn days_range(first: i64, last: i64) -> DateFilter {
    DateFilter::range(first * DAY_MS, (last + 1) * DAY_MS - 1)
}

/// Months `first..=last` (12 + n spills into the next year).
fn month_span(year: i64, first: i64, last: i64) -> DateFilter {
    let start = days_from_civil(year + (first - 1) / 12, (first - 1) % 12 + 1, 1);
    let end = days_from_civil(year + last / 12, last % 12 + 1, 1) - 1;
    days_range(start, end)
}

fn year_span(first: i64, last: i64) -> DateFilter {
    days_range(
        days_from_civil(first, 1, 1),
        days_from_civil(last + 1, 1, 1) - 1,
    )
}

const MONTHS: &[(&str, u32)] = &[
    ("january", 1),
    ("jan", 1),
    ("february", 2),
    ("feb", 2),
    ("march", 3),
    ("mar", 3),
    ("april", 4),
    ("apr", 4),
    ("may", 5),
    ("june", 6),
    ("jun", 6),
    ("july", 7),
    ("jul", 7),
    ("august", 8),
    ("aug", 8),
    ("september", 9),
    ("sept", 9),
    ("sep", 9),
    ("october", 10),
    ("oct", 10),
    ("november", 11),
    ("nov", 11),
    ("december", 12),
    ("dec", 12),
];

/// Words that are dates only next to a trigger or a year.
const AMBIGUOUS: &[&str] = &["may", "march", "mar", "spring", "fall", "jan", "sep", "dec"];

/// Words before a date that make an ambiguous word one ("in may").
const TRIGGERS: &[&str] = &[
    "in", "during", "since", "last", "this", "of", "from", "before", "after", "until", "early",
    "late", "past",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Season {
    Spring,
    Summer,
    Fall,
    Winter,
}

impl Season {
    fn of(w: &str) -> Option<Season> {
        Some(match w {
            "spring" => Season::Spring,
            "summer" | "summertime" => Season::Summer,
            "fall" | "autumn" => Season::Fall,
            "winter" | "wintertime" => Season::Winter,
            _ => return None,
        })
    }

    /// First month (winter starts in December).
    fn first(self) -> i64 {
        match self {
            Season::Spring => 3,
            Season::Summer => 6,
            Season::Fall => 9,
            Season::Winter => 12,
        }
    }

    fn mask(self) -> u16 {
        match self {
            Season::Winter => 1 << 11 | 0b11,
            s => 0b111 << (s.first() - 1),
        }
    }

    /// The season starting in `year`.
    fn span(self, year: i64) -> DateFilter {
        month_span(year, self.first(), self.first() + 2)
    }

    /// Year of the season containing `today`, or of the last one that ended.
    fn year_of(self, today: CivilDate, last: bool) -> i64 {
        let y = i64::from(today.year);
        let t = today.days();
        // The season starting this year, else (winter in Jan/Feb) last year.
        let mut year = if self == Season::Winter && today.month <= 2 {
            y - 1
        } else {
            y
        };
        if last {
            // The most recent one that is over; a season that hasn't started
            // yet this year (or is running now) means the one a year before.
            while self.span(year).to_ms.is_some_and(|e| e >= t * DAY_MS) {
                year -= 1;
            }
        }
        year
    }
}

fn month_of(w: &str) -> Option<u32> {
    MONTHS.iter().find(|(n, _)| *n == w).map(|m| m.1)
}

/// "2024" → 2024 (1900–2100 only).
fn year_of(w: &str) -> Option<i64> {
    if w.len() != 4 || !w.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i64 = w.parse().ok()?;
    (1900..=2100).contains(&y).then_some(y)
}

/// "2010s" / "90s" / "'90s" → first year of the decade.
fn decade_of(w: &str) -> Option<i64> {
    let d = w.trim_start_matches('\'').strip_suffix('s')?;
    if !d.bytes().all(|b| b.is_ascii_digit()) || !d.ends_with('0') {
        return None;
    }
    match d.len() {
        4 => year_of(d),
        2 => {
            let n: i64 = d.parse().ok()?;
            Some(if n >= 30 { 1900 + n } else { 2000 + n })
        }
        _ => None,
    }
}

fn small_number(w: &str) -> Option<i64> {
    const WORDS: &[&str] = &[
        "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
        "twelve",
    ];
    if let Some(i) = WORDS.iter().position(|n| *n == w) {
        return Some(i as i64 + 1);
    }
    w.parse().ok().filter(|n| (1..=1000).contains(n))
}

/// The date at `words[i..]`: how many words it uses and what it means.
/// A leading "in"/"during"/"on"/"from"/"of" is taken with it.
pub(super) fn parse_date(
    words: &[&str],
    i: usize,
    today: Option<CivilDate>,
) -> Option<(usize, DateFilter)> {
    let w = |k: usize| words.get(i + k).copied().unwrap_or("");
    if matches!(w(0), "in" | "during" | "on" | "of" | "taken" | "around") {
        let (n, f) = parse_core(words, i + 1, today, true)?;
        return Some((n + 1, f));
    }
    let before = if i > 0 { words[i - 1] } else { "" };
    // "last summer" is read from "last" (needs today); not as "summer" alone.
    if matches!(before, "last" | "this" | "past") {
        return None;
    }
    parse_core(words, i, today, TRIGGERS.contains(&before))
}

/// "since …", "after …", "before …", "until …", "from … to …", "between
/// … and …": `Some(result)` when `words[i]` is one of those words.
#[allow(clippy::option_option)]
fn bounded(
    words: &[&str],
    i: usize,
    today: Option<CivilDate>,
) -> Option<Option<(usize, DateFilter)>> {
    let w = |k: usize| words.get(i + k).copied().unwrap_or("");
    let open = |n: usize, from_ms: Option<i64>, to_ms: Option<i64>| {
        (from_ms.is_some() || to_ms.is_some()).then_some((
            n + 1,
            DateFilter {
                from_ms,
                to_ms,
                ..DateFilter::default()
            },
        ))
    };
    let next = || parse_core(words, i + 1, today, true);
    Some(match w(0) {
        "since" => next().and_then(|(n, f)| open(n, f.from_ms, None)),
        "after" => next().and_then(|(n, f)| open(n, f.to_ms.map(|t| t + 1), None)),
        "before" => next().and_then(|(n, f)| open(n, None, f.from_ms.map(|t| t - 1))),
        "until" => next().and_then(|(n, f)| open(n, None, f.to_ms)),
        "from" | "between" => {
            let (n, a) = next()?;
            if matches!(w(n + 1), "to" | "and" | "through" | "thru" | "until" | "-")
                && let Some((m, b)) = parse_core(words, i + n + 2, today, true)
            {
                if let (Some(from_ms), Some(to_ms)) = (a.from_ms, b.to_ms)
                    && from_ms <= to_ms
                {
                    return Some(Some((n + m + 2, DateFilter::range(from_ms, to_ms))));
                }
                // "from march to may": months of any year, wrapping the new
                // year ("november to february").
                if let Some(months) = month_run(a, b) {
                    return Some(Some((n + m + 2, DateFilter::months(months))));
                }
            }
            // "from 2023" = taken in 2023 ("between" alone means nothing).
            (w(0) == "from").then_some((n + 1, a))
        }
        _ => return None,
    })
}

/// The months from `a` through `b` (inclusive, wrapping December to January)
/// when each is a single month of any year.
fn month_run(a: DateFilter, b: DateFilter) -> Option<u16> {
    let single = |f: DateFilter| {
        let open = f.from_ms.is_none() && f.to_ms.is_none();
        (open && f.months.is_power_of_two()).then(|| f.months.trailing_zeros())
    };
    let (from, to) = (single(a)?, single(b)?);
    let len = (to + 12 - from) % 12 + 1;
    Some((0..len).fold(0, |mask, k| mask | 1 << ((from + k) % 12)))
}

fn parse_core(
    words: &[&str],
    i: usize,
    today: Option<CivilDate>,
    triggered: bool,
) -> Option<(usize, DateFilter)> {
    let w = |k: usize| words.get(i + k).copied().unwrap_or("");
    let today_days = today.map(CivilDate::days);
    if let Some(r) = bounded(words, i, today) {
        return r;
    }
    match w(0) {
        "today" => return today_days.map(|t| (1, days_range(t, t))),
        "yesterday" => return today_days.map(|t| (1, days_range(t - 1, t - 1))),
        "recently" | "recent" => return today_days.map(|t| (1, days_range(t - 30, t))),
        _ => {}
    }
    if let Some(y) = year_of(w(0)) {
        // "2019 - 2021", "2019 to 2021".
        if matches!(w(1), "-" | "to" | "through" | "thru")
            && let Some(y2) = year_of(w(2))
            && y2 >= y
        {
            return Some((3, year_span(y, y2)));
        }
        // "2024-03" (the tokenizer splits "-").
        if w(1) == "-"
            && w(2).len() == 2
            && let Ok(m) = w(2).parse::<i64>()
            && (1..=12).contains(&m)
        {
            return Some((3, month_span(y, m, m)));
        }
        return Some((1, year_span(y, y)));
    }
    if w(0) == "the" {
        let d = decade_of(w(1))?;
        return Some((2, year_span(d, d + 9)));
    }
    if let Some(d) = decade_of(w(0)) {
        return Some((1, year_span(d, d + 9)));
    }
    if matches!(w(0), "last" | "this" | "past") {
        return relative(words, i, today?);
    }
    let year_after = |k: usize| {
        let of = usize::from(w(k) == "of");
        year_of(w(k + of)).map(|y| (k + of + 1, y))
    };
    if let Some(m) = month_of(w(0)) {
        if let Some((n, y)) = year_after(1) {
            return Some((n, month_span(y, i64::from(m), i64::from(m))));
        }
        if AMBIGUOUS.contains(&w(0)) && !triggered {
            return None;
        }
        return Some((1, DateFilter::months(1 << (m - 1))));
    }
    if let Some(s) = Season::of(w(0)) {
        if let Some((n, y)) = year_after(1) {
            return Some((n, s.span(y)));
        }
        if AMBIGUOUS.contains(&w(0)) && !triggered {
            return None;
        }
        return Some((1, DateFilter::months(s.mask())));
    }
    None
}

/// "last summer", "this year", "last week", "past 3 months", "last march".
fn relative(words: &[&str], i: usize, today: CivilDate) -> Option<(usize, DateFilter)> {
    let w = |k: usize| words.get(i + k).copied().unwrap_or("");
    let last = w(0) != "this";
    let t = today.days();
    let (y, m) = (i64::from(today.year), i64::from(today.month));
    // Monday of this week (1970-01-01 was a Thursday).
    let monday = t - (t + 3).rem_euclid(7);
    let f = match (last, w(1)) {
        (_, "year") if w(0) == "past" => days_range(back_months(today, 12), t),
        (true, "year") => year_span(y - 1, y - 1),
        (false, "year") => year_span(y, y),
        (_, "month") if w(0) == "past" => days_range(back_months(today, 1), t),
        (true, "month") => month_span(
            if m == 1 { y - 1 } else { y },
            (m + 10) % 12 + 1,
            (m + 10) % 12 + 1,
        ),
        (false, "month") => month_span(y, m, m),
        (_, "week") if w(0) == "past" => days_range(t - 7, t),
        (true, "week") => days_range(monday - 7, monday - 1),
        (false, "week") => days_range(monday, t),
        (_, word) => {
            if let Some(s) = Season::of(word) {
                s.span(s.year_of(today, last))
            } else if let Some(mo) = month_of(word) {
                let mo = i64::from(mo);
                // The most recent one before this month ("last march").
                let year = if last && mo >= m { y - 1 } else { y };
                month_span(year, mo, mo)
            } else if last || w(0) == "past" {
                // "last 3 months", "past two weeks".
                let n = small_number(word)?;
                let from = match w(2).trim_end_matches('s') {
                    "day" => t - n,
                    "week" => t - 7 * n,
                    "month" => back_months(today, n),
                    "year" => back_months(today, 12 * n),
                    _ => return None,
                };
                return Some((3, days_range(from, t)));
            } else {
                return None;
            }
        }
    };
    Some((2, f))
}

/// The day `n` months before `today` (clamped to the month's length).
fn back_months(today: CivilDate, n: i64) -> i64 {
    let total = i64::from(today.year) * 12 + i64::from(today.month) - 1 - n;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) + 1);
    let next = if m == 12 {
        days_from_civil(y + 1, 1, 1)
    } else {
        days_from_civil(y, m + 1, 1)
    };
    let len = next - days_from_civil(y, m, 1);
    days_from_civil(y, m, i64::from(today.day).min(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODAY: CivilDate = CivilDate {
        year: 2026,
        month: 10,
        day: 4,
    };

    fn day(y: i64, m: i64, d: i64) -> i64 {
        days_from_civil(y, m, d) * DAY_MS
    }

    fn end(y: i64, m: i64, d: i64) -> i64 {
        day(y, m, d) + DAY_MS - 1
    }

    fn p(s: &str) -> Option<(usize, DateFilter)> {
        p_at(s, TODAY)
    }

    fn p_at(s: &str, today: CivilDate) -> Option<(usize, DateFilter)> {
        let w: Vec<&str> = s.split_whitespace().collect();
        parse_date(&w, 0, Some(today))
    }

    fn range(s: &str) -> (i64, i64) {
        let (_, f) = p(s).unwrap_or_else(|| panic!("{s} not a date"));
        (f.from_ms.unwrap(), f.to_ms.unwrap())
    }

    #[test]
    fn years_months_and_decades() {
        assert_eq!(range("2024"), (day(2024, 1, 1), end(2024, 12, 31)));
        assert_eq!(range("in 2024"), (day(2024, 1, 1), end(2024, 12, 31)));
        assert_eq!(range("march 2024"), (day(2024, 3, 1), end(2024, 3, 31)));
        assert_eq!(range("feb of 2024"), (day(2024, 2, 1), end(2024, 2, 29)));
        assert_eq!(range("2024 - 03"), (day(2024, 3, 1), end(2024, 3, 31)));
        assert_eq!(range("2019 to 2021"), (day(2019, 1, 1), end(2021, 12, 31)));
        assert_eq!(
            range("from 2019 - 2021"),
            (day(2019, 1, 1), end(2021, 12, 31))
        );
        assert_eq!(
            range("between 2019 and 2021"),
            (day(2019, 1, 1), end(2021, 12, 31))
        );
        assert_eq!(range("from 2023"), (day(2023, 1, 1), end(2023, 12, 31)));
        assert_eq!(range("the 90s"), (day(1990, 1, 1), end(1999, 12, 31)));
        assert_eq!(range("2010s"), (day(2010, 1, 1), end(2019, 12, 31)));
        assert_eq!(range("'20s"), (day(2020, 1, 1), end(2029, 12, 31)));
        assert_eq!(
            range("december 2024"),
            (day(2024, 12, 1), end(2024, 12, 31))
        );
        assert_eq!(p("2024").unwrap().0, 1);
        assert_eq!(p("march 2024").unwrap().0, 2);
        assert_eq!(p("in 2024").unwrap().0, 2);
        // Not dates.
        for s in [
            "3000",
            "12",
            "beach",
            "the beach",
            "from beach",
            "between 2019",
            "195s",
            "2015s",
            "x5s",
        ] {
            assert!(p(s).is_none(), "{s}");
        }
        // A backwards range stays the first year.
        assert_eq!(
            range("from 2021 to 2019"),
            (day(2021, 1, 1), end(2021, 12, 31))
        );
    }

    #[test]
    fn open_ranges() {
        let (n, f) = p("since 2020").unwrap();
        assert_eq!((n, f.from_ms, f.to_ms), (2, Some(day(2020, 1, 1)), None));
        let (_, f) = p("after 2020").unwrap();
        assert_eq!(f.from_ms, Some(day(2021, 1, 1)));
        let (_, f) = p("before 2020").unwrap();
        assert_eq!((f.from_ms, f.to_ms), (None, Some(day(2020, 1, 1) - 1)));
        let (_, f) = p("until march 2020").unwrap();
        assert_eq!(f.to_ms, Some(end(2020, 3, 31)));
        // "since may" has no start without a year.
        assert!(p("since may").is_none());
        assert!(p("since beach").is_none());
        assert!(p("before").is_none());
    }

    #[test]
    fn month_to_month() {
        let months = |s: &str| p(s).map(|(n, f)| (n, f.months, f.from_ms, f.to_ms));
        // March, April, May.
        assert_eq!(months("from march to may"), Some((4, 0b1_1100, None, None)));
        assert_eq!(
            months("between june and august"),
            Some((4, 0b1110_0000, None, None))
        );
        // Wraps the new year.
        assert_eq!(
            months("from november to february"),
            Some((4, 0b1100_0000_0011, None, None))
        );
        // The same month twice is that month.
        assert_eq!(months("from may to may"), Some((4, 1 << 4, None, None)));
        // A year on either side keeps the exact range.
        assert_eq!(
            range("from march 2020 to may 2020"),
            (day(2020, 3, 1), end(2020, 5, 31))
        );
        // A season is not a single month: only the first date counts.
        assert_eq!(p("from march to summer").unwrap().0, 2);
    }

    #[test]
    fn months_of_any_year() {
        assert_eq!(p("in june").unwrap(), (2, DateFilter::months(1 << 5)));
        assert_eq!(p("july").unwrap(), (1, DateFilter::months(1 << 6)));
        assert_eq!(p("in may").unwrap(), (2, DateFilter::months(1 << 4)));
        assert_eq!(p("in march").unwrap(), (2, DateFilter::months(1 << 2)));
        // Ordinary words unless triggered.
        assert!(p("may").is_none());
        assert!(p("march").is_none());
        assert!(p("spring").is_none());
        assert!(p("fall").is_none());
        let w = ["of", "march"];
        assert!(parse_date(&w, 1, None).is_some());
        assert_eq!(p("summer").unwrap(), (1, DateFilter::months(0b1110_0000)));
        assert_eq!(
            p("in fall").unwrap(),
            (2, DateFilter::months(0b111_0000_0000))
        );
        assert_eq!(
            p("winter").unwrap(),
            (1, DateFilter::months(0b1000_0000_0011))
        );
        assert_eq!(p("in spring").unwrap(), (2, DateFilter::months(0b11100)));
    }

    #[test]
    fn seasons_with_years() {
        assert_eq!(range("summer 2023"), (day(2023, 6, 1), end(2023, 8, 31)));
        assert_eq!(range("winter 2023"), (day(2023, 12, 1), end(2024, 2, 29)));
        assert_eq!(range("spring of 2022"), (day(2022, 3, 1), end(2022, 5, 31)));
        assert_eq!(range("autumn 2021"), (day(2021, 9, 1), end(2021, 11, 30)));
    }

    #[test]
    fn relative_dates() {
        assert_eq!(range("last summer"), (day(2026, 6, 1), end(2026, 8, 31)));
        assert_eq!(range("this summer"), (day(2026, 6, 1), end(2026, 8, 31)));
        // In July the running summer is "this", last year's is "last".
        let july = CivilDate::new(2026, 7, 10);
        let (_, f) = p_at("last summer", july).unwrap();
        assert_eq!(f.from_ms, Some(day(2025, 6, 1)));
        assert_eq!(range("last winter"), (day(2025, 12, 1), end(2026, 2, 28)));
        let jan = CivilDate::new(2026, 1, 15);
        let (_, f) = p_at("this winter", jan).unwrap();
        assert_eq!(f.from_ms, Some(day(2025, 12, 1)));
        let (_, f) = p_at("last winter", jan).unwrap();
        assert_eq!(f.from_ms, Some(day(2024, 12, 1)));
        assert_eq!(range("last fall"), (day(2025, 9, 1), end(2025, 11, 30)));
        assert_eq!(range("last year"), (day(2025, 1, 1), end(2025, 12, 31)));
        assert_eq!(range("this year"), (day(2026, 1, 1), end(2026, 12, 31)));
        assert_eq!(range("last month"), (day(2026, 9, 1), end(2026, 9, 30)));
        assert_eq!(range("this month"), (day(2026, 10, 1), end(2026, 10, 31)));
        let (_, f) = p_at("last month", jan).unwrap();
        assert_eq!(f.from_ms, Some(day(2025, 12, 1)));
        // 2026-10-04 is a Sunday.
        assert_eq!(range("this week"), (day(2026, 9, 28), end(2026, 10, 4)));
        assert_eq!(range("last week"), (day(2026, 9, 21), end(2026, 9, 27)));
        assert_eq!(range("past week"), (day(2026, 9, 27), end(2026, 10, 4)));
        assert_eq!(range("past month"), (day(2026, 9, 4), end(2026, 10, 4)));
        assert_eq!(range("past year"), (day(2025, 10, 4), end(2026, 10, 4)));
        assert_eq!(range("last march"), (day(2026, 3, 1), end(2026, 3, 31)));
        assert_eq!(range("last october"), (day(2025, 10, 1), end(2025, 10, 31)));
        assert_eq!(
            range("this december"),
            (day(2026, 12, 1), end(2026, 12, 31))
        );
        assert_eq!(range("last 3 months"), (day(2026, 7, 4), end(2026, 10, 4)));
        assert_eq!(
            range("past two weeks"),
            (day(2026, 9, 20), end(2026, 10, 4))
        );
        assert_eq!(range("last 10 days"), (day(2026, 9, 24), end(2026, 10, 4)));
        assert_eq!(range("last 2 years"), (day(2024, 10, 4), end(2026, 10, 4)));
        assert_eq!(p("last 3 months").unwrap().0, 3);
        assert_eq!(range("today"), (day(2026, 10, 4), end(2026, 10, 4)));
        assert_eq!(range("yesterday"), (day(2026, 10, 3), end(2026, 10, 3)));
        assert_eq!(range("recently"), (day(2026, 9, 4), end(2026, 10, 4)));
        // Month-end clamping: 31 March minus one month is 28 February.
        let mar31 = CivilDate::new(2026, 3, 31);
        let (_, f) = p_at("past month", mar31).unwrap();
        assert_eq!(f.from_ms, Some(day(2026, 2, 28)));
        let (_, f) = p_at("past 3 months", CivilDate::new(2026, 1, 31)).unwrap();
        assert_eq!(f.from_ms, Some(day(2025, 10, 31)));
        for s in [
            "last beach",
            "last 3 beaches",
            "this 3 days",
            "last",
            "past zero days",
        ] {
            assert!(p(s).is_none(), "{s}");
        }
        // Without today, relative dates aren't dates.
        let w = ["last", "summer"];
        assert!(parse_date(&w, 0, None).is_none());
        assert!(parse_date(&["today"], 0, None).is_none());
    }
}
