//! Deterministic find-query parser: the offline [`FindBackend`].
//!
//! Text is split into lower-case words (accents folded, possessive "'s"
//! dropped, "-" its own word, "4+" kept), then read left to right; each word
//! is used by at most one part:
//!
//! 1. **People after "of" / "with" / "featuring"**, and after "and" or ","
//!    following a person: a known person (any match kind, typos included), else
//!    a capitalized word or a common first name ("pics of alex"), which
//!    becomes a person nobody is called yet. Dates and places win there:
//!    "pics of 2024", "photos of Iceland".
//! 2. **Dates** ("last summer", "in march", "2019 to 2021", see `dates.rs`).
//!    Two date phrases are intersected.
//! 3. **Ratings and flags**: "5 stars", "4+ stars", "at least 3 stars",
//!    "3 stars or better", "unrated", "top rated", "picked", "rejects".
//! 4. **Places after "in" / "at" / "near" / "around" / "from" / "to"**: a known
//!    or built-in place, else up to two words that aren't an article, a
//!    description ("in snow", "at night", "at the beach" stay text) or a
//!    connective ("in sicily" is a place the caller may not know).
//! 5. **Known people anywhere** (whole word or nickname: "alex at the beach"),
//!    then **places anywhere** (the caller's known places, then built-in ones),
//!    then **common first names anywhere** (an unknown person).
//! 6. **Filler** ("let's find pics of", "show me", "photos") is dropped.
//!
//! What remains, minus articles and prepositions at its ends, is the
//! descriptive text ("beach at sunset").

use super::dates::parse_date;
use super::names::{
    fold, is_combining, is_descriptive, is_first_name, longest_at, match_people, place_at,
};
use super::{
    CivilDate, DateFilter, DateTerm, FindBackend, FindQuery, FlagQuery, PersonMatchKind,
    PersonTerm, QueryContext, RatingQuery,
};

/// Characters of a query that are read, and words kept from them. A real
/// query is a few words; the cap keeps a pasted blob from making the
/// word-by-word scan quadratic.
const MAX_QUERY_CHARS: usize = 2048;
const MAX_WORDS: usize = 128;

/// The offline rule-based find parser. Stateless.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleBasedFindParser;

impl FindBackend for RuleBasedFindParser {
    fn name(&self) -> &'static str {
        "rules"
    }

    fn parse(&self, text: &str, ctx: &QueryContext) -> FindQuery {
        Parser::new(text, ctx).run()
    }
}

const FILLER: &[&str] = &[
    "all",
    "any",
    "can",
    "could",
    "display",
    "every",
    "find",
    "for",
    "get",
    "give",
    "i",
    "id",
    "im",
    "image",
    "images",
    "just",
    "let",
    "lets",
    "like",
    "look",
    "looking",
    "me",
    "my",
    "need",
    "ones",
    "our",
    "photo",
    "photograph",
    "photographs",
    "photos",
    "pic",
    "pics",
    "picture",
    "pictures",
    "please",
    "search",
    "see",
    "shot",
    "shots",
    "show",
    "snap",
    "snaps",
    "snapshot",
    "snapshots",
    "some",
    "taken",
    "that",
    "the",
    "us",
    "want",
    "were",
    "where",
    "which",
    "would",
    "you",
];

/// Trimmed from the ends of the descriptive text, and never part of a place.
const CONNECTIVES: &[&str] = &[
    "-", "a", "about", "an", "and", "around", "at", "by", "during", "for", "from", "her", "his",
    "in", "into", "is", "near", "of", "on", "or", "our", "plus", "taken", "that", "the", "their",
    "this", "to", "was", "were", "with",
];

const ARTICLES: &[&str] = &[
    "a", "an", "her", "his", "my", "our", "some", "that", "the", "their", "these", "this", "those",
];

const PERSON_INTRO: &[&str] = &["of", "with", "featuring", "feat", "starring", "including"];
const PLACE_INTRO: &[&str] = &["in", "at", "near", "around", "from", "to", "visiting"];

#[derive(Debug, Clone)]
struct Word {
    w: String,
    /// Capitalized as typed.
    cap: bool,
}

fn tokenize(text: &str) -> Vec<Word> {
    let chars: Vec<char> = text
        .chars()
        .take(MAX_QUERY_CHARS)
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '`' => '\'',
            '\u{2212}' | '\u{2013}' | '\u{2014}' => '-',
            '★' | '⭐' => '*',
            c => c,
        })
        .collect();
    // Possessive "'s" goes ("alex's" → "alex"); other apostrophes vanish.
    let mut clean = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\'' {
            let possessive = i > 0
                && chars[i - 1].is_alphanumeric()
                && matches!(chars.get(i + 1), Some('s' | 'S'))
                && chars.get(i + 2).is_none_or(|c| !c.is_alphanumeric());
            i += if possessive { 2 } else { 1 };
            continue;
        }
        clean.push(chars[i]);
        i += 1;
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cap = false;
    let flush = |cur: &mut String, cap: bool, out: &mut Vec<Word>| {
        if !cur.is_empty() {
            let w = if cur.ends_with('+') {
                cur.clone()
            } else {
                fold(cur).replace(' ', "")
            };
            if !w.is_empty() {
                out.push(Word { w, cap });
            }
            cur.clear();
        }
    };
    for c in clean {
        match c {
            c if c.is_alphanumeric() => {
                if cur.is_empty() {
                    cap = c.is_uppercase();
                }
                cur.push(c);
            }
            // A decomposed accent stays with its letter ("Mu\u{308}ller").
            c if is_combining(c) && !cur.is_empty() => cur.push(c),
            '+' if !cur.is_empty() && cur.chars().all(|c| c.is_ascii_digit()) => {
                cur.push('+');
                flush(&mut cur, cap, &mut out);
            }
            '-' | '*' | '&' => {
                flush(&mut cur, cap, &mut out);
                let w = match c {
                    '*' => "star",
                    '&' => "and",
                    _ => "-",
                };
                out.push(Word {
                    w: w.to_owned(),
                    cap: false,
                });
            }
            _ => flush(&mut cur, cap, &mut out),
        }
    }
    flush(&mut cur, cap, &mut out);
    out.truncate(MAX_WORDS);
    out
}

/// A usable "today": a real calendar day in the years the date parser knows.
fn valid_today(d: CivilDate) -> bool {
    let leap = d.year % 4 == 0 && (d.year % 100 != 0 || d.year % 400 == 0);
    let len = match d.month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + u32::from(leap),
        _ => return false,
    };
    (1900..=2100).contains(&d.year) && (1..=len).contains(&d.day)
}

fn star_count(w: &str) -> Option<u8> {
    Some(match w {
        "0" | "zero" | "no" => 0,
        "1" | "one" | "a" => 1,
        "2" | "two" => 2,
        "3" | "three" => 3,
        "4" | "four" => 4,
        "5" | "five" => 5,
        _ => return None,
    })
}

fn is_star(w: &str) -> bool {
    matches!(w, "star" | "stars" | "starred")
}

/// A rating or flag at `words[i..]`: how many words, and what.
fn rating_at(words: &[&str], i: usize) -> Option<(usize, Result<RatingQuery, FlagQuery>)> {
    let w = |k: usize| words.get(i + k).copied().unwrap_or("");
    let exact = |n: u8| RatingQuery {
        min: Some(n),
        max: Some(n),
    };
    let at_least = |n: u8| RatingQuery {
        min: Some(n),
        max: None,
    };
    let flag = |f| Some((1, Err(f)));
    match w(0) {
        "unrated" => return Some((1, Ok(exact(0)))),
        "starred" | "rated" => {
            return Some((
                1,
                Ok(RatingQuery {
                    min: Some(1),
                    max: None,
                }),
            ));
        }
        "top" | "highly" | "best" if w(1) == "rated" => return Some((2, Ok(at_least(4)))),
        "picked" | "picks" | "pick" | "keepers" | "keeper" | "flagged" => {
            return flag(FlagQuery::Picked);
        }
        "rejected" | "rejects" | "reject" => return flag(FlagQuery::Rejected),
        "unflagged" | "unpicked" => return flag(FlagQuery::Unflagged),
        "at" if w(1) == "least" => {
            let n = star_count(w(2))?;
            return is_star(w(3)).then_some((4, Ok(at_least(n))));
        }
        _ => {}
    }
    if let Some(n) = w(0).strip_suffix('+').and_then(star_count) {
        return is_star(w(1)).then_some((2, Ok(at_least(n))));
    }
    let n = star_count(w(0))?;
    // "4 or more stars".
    if w(1) == "or" && matches!(w(2), "more" | "better" | "higher" | "above") && is_star(w(3)) {
        return Some((4, Ok(at_least(n))));
    }
    // "five-star" arrives as "five - star".
    let dash = usize::from(w(1) == "-");
    if !is_star(w(1 + dash)) {
        return None;
    }
    let k = 2 + dash;
    match (w(k), w(k + 1)) {
        ("or", "more" | "better" | "higher" | "above" | "up")
        | ("and", "up" | "above" | "better") => Some((k + 2, Ok(at_least(n)))),
        ("plus", _) => Some((k + 1, Ok(at_least(n)))),
        _ if n == 0 => Some((k, Ok(exact(0)))),
        // "a star" alone isn't a rating ("a star in the sky").
        _ if w(0) == "a" => None,
        _ => Some((k, Ok(exact(n)))),
    }
}

struct Parser<'a> {
    words: Vec<Word>,
    used: Vec<bool>,
    ctx: &'a QueryContext,
    /// `ctx.today` when it is a real date; relative dates need it.
    today: Option<CivilDate>,
    places: Vec<String>,
    q: FindQuery,
    date_phrases: Vec<String>,
}

impl<'a> Parser<'a> {
    fn new(text: &str, ctx: &'a QueryContext) -> Self {
        let words = tokenize(text);
        let used = vec![false; words.len()];
        let mut places: Vec<String> = ctx
            .known_places
            .iter()
            .map(|p| fold(p))
            .filter(|p| !p.is_empty())
            .collect();
        places.sort();
        places.dedup();
        Self {
            words,
            used,
            ctx,
            today: ctx.today.filter(|d| valid_today(*d)),
            places,
            q: FindQuery::default(),
            date_phrases: Vec::new(),
        }
    }

    fn strs(&self) -> Vec<&str> {
        self.words.iter().map(|w| w.w.as_str()).collect()
    }

    fn take(&mut self, i: usize, n: usize) -> String {
        for u in &mut self.used[i..i + n] {
            *u = true;
        }
        self.words[i..i + n]
            .iter()
            .map(|w| w.w.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn free(&self, i: usize, n: usize) -> bool {
        i + n <= self.words.len() && self.used[i..i + n].iter().all(|u| !u)
    }

    /// A known person at `i`: `(words, term, matches)`, longest phrase first.
    /// `any`: typos and prefixes count too (a person slot).
    fn known_person(&self, i: usize, any: bool) -> Option<(usize, Vec<super::PersonMatch>)> {
        for n in (1..=3).rev() {
            if !self.free(i, n) {
                continue;
            }
            let term = self.words[i..i + n]
                .iter()
                .map(|w| w.w.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let mut m = match_people(&term, &self.ctx.known_people);
            if !any {
                m.retain(|m| m.kind <= PersonMatchKind::Nickname);
            }
            if !m.is_empty() {
                return Some((n, m));
            }
        }
        None
    }

    fn place(&self, i: usize) -> Option<(usize, String)> {
        let words = self.strs();
        longest_at(&words, i, self.places.iter().map(String::as_str))
            .or_else(|| place_at(&words, i))
            .filter(|(n, _)| self.free(i, *n))
    }

    fn date(&self, i: usize) -> Option<(usize, DateFilter)> {
        parse_date(&self.strs(), i, self.today).filter(|(n, _)| self.free(i, *n))
    }

    fn add_person(&mut self, i: usize, n: usize, matches: Vec<super::PersonMatch>) {
        let term = self.take(i, n);
        if !self.q.people.iter().any(|p| p.term == term) {
            self.q.people.push(PersonTerm { term, matches });
        }
    }

    fn add_place(&mut self, i: usize, n: usize) {
        let p = self.take(i, n);
        if !self.q.places.contains(&p) {
            self.q.places.push(p);
        }
    }

    /// A person after "of"/"with"/"and": known (any match), else a name.
    fn person_slot(&mut self, i: usize) -> bool {
        if !self.free(i, 1) || self.date(i).is_some() || self.place(i).is_some() {
            return false;
        }
        if let Some((n, m)) = self.known_person(i, true) {
            self.add_person(i, n, m);
            return true;
        }
        let w = &self.words[i];
        let plain = |w: &Word| {
            !FILLER.contains(&w.w.as_str())
                && !CONNECTIVES.contains(&w.w.as_str())
                && !ARTICLES.contains(&w.w.as_str())
                && !is_descriptive(&w.w)
                && w.w.chars().all(char::is_alphabetic)
        };
        if plain(w) && (w.cap || is_first_name(&w.w)) {
            // "Zephyr Jones": a capitalized surname comes along.
            let n = if w.cap
                && self.words.get(i + 1).is_some_and(|x| x.cap && plain(x))
                && self.free(i + 1, 1)
            {
                2
            } else {
                1
            };
            self.add_person(i, n, Vec::new());
            return true;
        }
        false
    }

    /// A place after "in"/"at"/…: known, built-in, or up to two plain words.
    fn place_slot(&mut self, i: usize) -> bool {
        if let Some((n, _)) = self.place(i) {
            self.add_place(i, n);
            return true;
        }
        let words = self.strs();
        let stop = |j: usize| {
            let w = words[j];
            !self.free(j, 1)
                || FILLER.contains(&w)
                || CONNECTIVES.contains(&w)
                || ARTICLES.contains(&w)
                || is_descriptive(w)
                || !w.chars().all(char::is_alphabetic)
                || parse_date(&words, j, self.today).is_some()
                || rating_at(&words, j).is_some()
                || self.known_person(j, false).is_some()
        };
        let mut n = 0;
        while i + n < words.len() && n < 2 && !stop(i + n) {
            n += 1;
        }
        if n == 0 {
            return false;
        }
        self.add_place(i, n);
        true
    }

    fn add_date(&mut self, i: usize, n: usize, f: DateFilter) {
        let phrase = self.take(i, n);
        self.date_phrases.push(phrase);
        let filter = match self.q.date.take() {
            // Contradictory dates: a range nothing is in, said honestly.
            Some(d) => d.filter.intersect(f).unwrap_or(DateFilter::range(1, 0)),
            None => f,
        };
        self.q.date = Some(DateTerm {
            phrase: self.date_phrases.join(" "),
            filter,
        });
    }

    fn run(mut self) -> FindQuery {
        let n = self.words.len();
        let mut last_was_person = false;
        let mut i = 0;
        while i < n {
            if self.used[i] {
                i += 1;
                continue;
            }
            let w = self.words[i].w.clone();
            let person_link = PERSON_INTRO.contains(&w.as_str())
                || (last_was_person && matches!(w.as_str(), "and" | "plus"));
            if person_link && self.person_slot(i + 1) {
                self.used[i] = true;
                last_was_person = true;
                i += 1;
                continue;
            }
            last_was_person = false;
            if let Some((k, f)) = self.date(i) {
                self.add_date(i, k, f);
                i += k;
                continue;
            }
            if let Some((k, r)) = rating_at(&self.strs(), i).filter(|(k, _)| self.free(i, *k)) {
                self.take(i, k);
                match r {
                    Ok(r) => {
                        let prev = self.q.rating.unwrap_or_default();
                        self.q.rating = Some(RatingQuery {
                            min: r.min.or(prev.min),
                            max: r.max.or(prev.max),
                        });
                    }
                    Err(f) => self.q.flag = Some(f),
                }
                i += k;
                continue;
            }
            if PLACE_INTRO.contains(&w.as_str()) && self.free(i + 1, 1) && self.place_slot(i + 1) {
                self.used[i] = true;
                i += 1;
                continue;
            }
            if let Some((k, m)) = self.known_person(i, false) {
                self.add_person(i, k, m);
                last_was_person = true;
                i += k;
                continue;
            }
            if let Some((k, _)) = self.place(i) {
                self.add_place(i, k);
                i += k;
                continue;
            }
            if is_first_name(&w) {
                self.add_person(i, 1, Vec::new());
                last_was_person = true;
                i += 1;
                continue;
            }
            if FILLER.contains(&w.as_str()) && w != "the" {
                self.used[i] = true;
            }
            i += 1;
        }
        self.q.text = self.text();
        self.q
    }

    /// Unused words, with connectives and articles trimmed from both ends.
    fn text(&self) -> String {
        let rest: Vec<&str> = self
            .words
            .iter()
            .zip(&self.used)
            .filter(|(_, u)| !**u)
            .map(|(w, _)| w.w.as_str())
            .collect();
        let edge = |w: &&str| CONNECTIVES.contains(w) || ARTICLES.contains(w) || FILLER.contains(w);
        let start = rest.iter().position(|w| !edge(w)).unwrap_or(rest.len());
        let end = rest.iter().rposition(|w| !edge(w)).map_or(start, |e| e + 1);
        rest[start..end.max(start)].join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CivilDate, PersonMatch, days_from_civil};
    use super::*;

    fn ctx(people: &[&str], places: &[&str]) -> QueryContext {
        QueryContext {
            known_people: people.iter().map(|s| (*s).to_owned()).collect(),
            known_places: places.iter().map(|s| (*s).to_owned()).collect(),
            today: Some(CivilDate::new(2026, 10, 4)),
        }
    }

    fn parse(text: &str, c: &QueryContext) -> FindQuery {
        RuleBasedFindParser.parse(text, c)
    }

    fn p(text: &str) -> FindQuery {
        parse(text, &ctx(&[], &[]))
    }

    fn people(q: &FindQuery) -> Vec<(&str, Vec<&str>)> {
        q.people
            .iter()
            .map(|p| {
                (
                    p.term.as_str(),
                    p.matches.iter().map(|m| m.name.as_str()).collect(),
                )
            })
            .collect()
    }

    fn day(y: i64, m: i64, d: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400_000
    }

    #[test]
    fn the_example() {
        let c = ctx(&["Alex", "Alexander", "Sam"], &[]);
        let q = parse("let's find pics of alex in iceland", &c);
        assert_eq!(people(&q), [("alex", vec!["Alex", "Alexander"])]);
        assert_eq!(q.places, ["iceland"]);
        assert_eq!(q.text, "");
        assert_eq!(q.date, None);
        // Nobody named yet: still a person, with no matches.
        let q = p("Let’s find pics of Alex in Iceland");
        assert_eq!(people(&q), [("alex", vec![])]);
        assert_eq!(q.places, ["iceland"]);
        assert!(!q.is_empty());
        assert_eq!(RuleBasedFindParser.name(), "rules");
    }

    #[test]
    fn everything_at_once() {
        let c = ctx(&["Alex", "Sam Lee"], &[]);
        let q = parse(
            "show me 4+ star photos of alex and sam on the beach at sunset in crete last summer, picked",
            &c,
        );
        assert_eq!(
            people(&q),
            [("alex", vec!["Alex"]), ("sam", vec!["Sam Lee"])]
        );
        assert_eq!(q.places, ["crete"]);
        assert_eq!(q.text, "beach at sunset");
        assert_eq!(
            q.rating,
            Some(RatingQuery {
                min: Some(4),
                max: None
            })
        );
        assert_eq!(q.flag, Some(FlagQuery::Picked));
        let d = q.date.unwrap();
        assert_eq!(d.phrase, "last summer");
        assert_eq!(d.filter.from_ms, Some(day(2026, 6, 1)));
    }

    #[test]
    fn descriptions_stay_text() {
        assert_eq!(p("red car").text, "red car");
        assert_eq!(
            p("kids on the beach at sunset").text,
            "kids on the beach at sunset"
        );
        let q = p("dogs in the snow");
        assert_eq!((q.text.as_str(), q.places.len()), ("dogs in the snow", 0));
        let q = p("waterfall in black and white");
        assert_eq!(
            (q.text.as_str(), q.places.len()),
            ("waterfall in black and white", 0)
        );
        let q = p("city at night");
        assert_eq!((q.text.as_str(), q.places.len()), ("city at night", 0));
        let q = p("pictures of dogs");
        assert_eq!((q.text.as_str(), q.people.len()), ("dogs", 0));
        let q = p("a photo of the ocean");
        assert_eq!(q.text, "ocean");
        let q = p("alex with a dog");
        assert_eq!(people(&q), [("alex", vec![])]);
        assert_eq!(q.text, "dog");
        assert!(p("").is_empty());
        assert!(p("show me all the photos").is_empty());
        assert!(p("  ,,, !! ").is_empty());
    }

    #[test]
    fn place_terms() {
        let q = p("sunset in sicily");
        assert_eq!(
            (q.places.clone(), q.text.as_str()),
            (vec!["sicily".to_owned()], "sunset")
        );
        let q = p("photos of Iceland");
        assert_eq!(
            (q.places.clone(), q.people.len()),
            (vec!["iceland".to_owned()], 0)
        );
        let q = p("iceland waterfall");
        assert_eq!(
            (q.places.clone(), q.text.as_str()),
            (vec!["iceland".to_owned()], "waterfall")
        );
        let q = p("trip to new york city");
        assert_eq!(
            (q.places.clone(), q.text.as_str()),
            (vec!["new york".to_owned()], "trip city")
        );
        // Unknown places after "in"/"at": up to two plain words.
        let q = p("hiking at Askja Crater summit");
        assert_eq!(q.places, ["askja crater"]);
        assert_eq!(q.text, "hiking summit");
        // Caller's places as whole phrases, before the built-in list.
        let c = ctx(&[], &["Lake Tahoe Cabin", "Grandma's House"]);
        let q = parse("lake tahoe cabin sunsets", &c);
        assert_eq!(
            (q.places.clone(), q.text.as_str()),
            (vec!["lake tahoe cabin".to_owned()], "sunsets")
        );
        let q = parse("at grandma's house", &c);
        assert_eq!(q.places, ["grandma house"]);
        let q = p("photos from crete and sicily");
        assert_eq!(q.places, ["crete", "sicily"]);
        let q = p("in iceland in iceland");
        assert_eq!(q.places, ["iceland"]);
        // Not places: numbers, people, dates.
        let q = p("at 5");
        assert_eq!((q.places.len(), q.text.as_str()), (0, "5"));
        assert_eq!(p("in").text, "");
    }

    #[test]
    fn person_terms() {
        let c = ctx(&["Alexander Lee", "Robert", "Mom"], &[]);
        let q = parse("bob and mom at the lake", &c);
        assert_eq!(people(&q), [("bob", vec!["Robert"]), ("mom", vec!["Mom"])]);
        assert_eq!(q.text, "lake");
        let q = parse("pics of alexnder", &c);
        assert_eq!(people(&q), [("alexnder", vec!["Alexander Lee"])]);
        // Typos only in a person slot.
        let q = parse("alexnder", &c);
        assert_eq!((q.people.len(), q.text.as_str()), (0, "alexnder"));
        let q = parse("photos with alex lee", &c);
        assert_eq!(people(&q), [("alex lee", vec!["Alexander Lee"])]);
        // Capitalized unknown names after "of", with a surname.
        let q = p("Pics of Zephyr Jones skiing");
        assert_eq!(people(&q), [("zephyr jones", vec![])]);
        assert_eq!(q.text, "skiing");
        let q = p("pics of zephyr");
        assert_eq!((q.people.len(), q.text.as_str()), (0, "zephyr"));
        // A first name anywhere.
        let q = p("sarah surfing");
        assert_eq!(people(&q), [("sarah", vec![])]);
        let q = p("alex's birthday");
        assert_eq!(people(&q), [("alex", vec![])]);
        assert_eq!(q.text, "birthday");
        let q = p("alex and alex");
        assert_eq!(q.people.len(), 1);
        // A date or place after "of" isn't a person.
        let q = p("photos of 2024");
        assert_eq!((q.people.len(), q.date.is_some()), (0, true));
        let q = p("featuring alice & bob");
        assert_eq!(people(&q), [("alice", vec![]), ("bob", vec![])]);
        // "and" links people only after a person.
        let q = p("sunset and sam");
        assert_eq!(people(&q), [("sam", vec![])]);
        assert_eq!(q.text, "sunset");
        let q = p("with the dog");
        assert_eq!((q.people.len(), q.text.as_str()), (0, "dog"));
        let q = p("of Sunset");
        assert_eq!((q.people.len(), q.text.as_str()), (0, "sunset"));
        let m: Vec<PersonMatch> = parse("of rob", &c).people[0].matches.clone();
        assert_eq!(m[0].kind, PersonMatchKind::Nickname);
    }

    #[test]
    fn dates() {
        let q = p("iceland 2024");
        assert_eq!(
            q.date.as_ref().unwrap().filter.from_ms,
            Some(day(2024, 1, 1))
        );
        assert_eq!(q.places, ["iceland"]);
        let q = p("beach in march");
        assert_eq!(q.date.as_ref().unwrap().filter, DateFilter::months(1 << 2));
        assert_eq!(q.text, "beach");
        assert_eq!(q.places.len(), 0);
        // Two phrases: intersected.
        let q = p("2024 in march");
        let d = q.date.unwrap();
        assert_eq!(d.phrase, "2024 in march");
        assert!(d.filter.matches(Some(day(2024, 3, 5))));
        assert!(!d.filter.matches(Some(day(2025, 3, 5))));
        // Contradictory: matches nothing.
        let d = p("2023 2024").date.unwrap();
        assert!(!d.filter.matches(Some(day(2023, 6, 1))));
        assert!(!d.filter.matches(Some(day(2024, 6, 1))));
        // No "today": relative dates stay words.
        let q = parse("last summer", &QueryContext::default());
        assert_eq!((q.date.is_none(), q.text.as_str()), (true, "last summer"));
        let q = p("may i see beach photos");
        assert_eq!((q.date.is_none(), q.text.as_str()), (true, "may beach"));
        let q = p("hiking from 2019 to 2021");
        assert_eq!(q.date.unwrap().filter.to_ms, Some(day(2022, 1, 1) - 1));
    }

    #[test]
    fn ratings_and_flags() {
        let r = |s: &str| p(s).rating;
        let ex = |n| {
            Some(RatingQuery {
                min: Some(n),
                max: Some(n),
            })
        };
        let al = |n| {
            Some(RatingQuery {
                min: Some(n),
                max: None,
            })
        };
        assert_eq!(r("5 stars"), ex(5));
        assert_eq!(r("five-star sunsets"), ex(5));
        assert_eq!(r("3★"), ex(3));
        assert_eq!(r("4+ stars"), al(4));
        assert_eq!(r("at least 3 stars"), al(3));
        assert_eq!(r("3 stars or better"), al(3));
        assert_eq!(r("3 stars and up"), al(3));
        assert_eq!(r("2 stars plus"), al(2));
        assert_eq!(r("4 or more stars"), al(4));
        assert_eq!(r("one star"), ex(1));
        assert_eq!(r("unrated"), ex(0));
        assert_eq!(r("no stars"), ex(0));
        assert_eq!(r("top rated"), al(4));
        assert_eq!(r("rated"), al(1));
        assert_eq!(r("a star in the sky"), None);
        assert_eq!(r("at least 3"), None);
        assert_eq!(r("4+ cats"), None);
        assert_eq!(r("4 or more cats"), None);
        assert_eq!(p("a star in the sky").text, "star in the sky");
        let f = |s: &str| p(s).flag;
        assert_eq!(f("picked waterfalls"), Some(FlagQuery::Picked));
        assert_eq!(f("keepers"), Some(FlagQuery::Picked));
        assert_eq!(f("rejects"), Some(FlagQuery::Rejected));
        assert_eq!(f("unflagged"), Some(FlagQuery::Unflagged));
        // Two conditions combine.
        let q = p("unrated rejects");
        assert_eq!((q.rating, q.flag), (ex(0), Some(FlagQuery::Rejected)));
        let q = p("rated 3 stars");
        assert_eq!(q.rating, ex(3));
    }

    #[test]
    fn a_huge_query_is_cut_not_quadratic() {
        let words = tokenize(&"beach ".repeat(100_000));
        assert_eq!(words.len(), MAX_WORDS);
        let start = std::time::Instant::now();
        let q = p(&"sunset at the beach in iceland ".repeat(50_000));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(q.places, ["iceland"]);
        // Text past the character cap is ignored.
        let head = "x".repeat(MAX_QUERY_CHARS);
        assert_eq!(p(&format!("{head} iceland")), p(&head));
    }

    #[test]
    fn an_impossible_today_means_no_relative_dates() {
        for today in [
            CivilDate::new(2026, 13, 1),
            CivilDate::new(2026, 0, 5),
            CivilDate::new(2026, 2, 30),
            CivilDate::new(2025, 2, 29),
            CivilDate::new(2026, 10, 0),
            CivilDate::new(1000, 6, 1),
        ] {
            let c = QueryContext {
                today: Some(today),
                ..QueryContext::default()
            };
            let q = parse("beach last summer", &c);
            assert!(q.date.is_none(), "{today:?}");
            assert_eq!(q.text, "beach last summer");
        }
        let leap = QueryContext {
            today: Some(CivilDate::new(2024, 2, 29)),
            ..QueryContext::default()
        };
        assert!(parse("yesterday", &leap).date.is_some());
    }

    #[test]
    fn decomposed_accents_stay_in_the_word() {
        let c = ctx(&["Müller"], &[]);
        let q = parse("pics of Mu\u{308}ller", &c);
        assert_eq!(people(&q), [("muller", vec!["Müller"])]);
        let w: Vec<String> = tokenize("Jose\u{301} \u{301}x")
            .into_iter()
            .map(|w| w.w)
            .collect();
        assert_eq!(w, ["jose", "x"]);
    }

    #[test]
    fn tokenizer() {
        let w: Vec<String> = tokenize("Let’s go—to José's 4+ ★ b/w 2024-03 & 'x")
            .into_iter()
            .map(|w| w.w)
            .collect();
        assert_eq!(
            w,
            [
                "let", "go", "-", "to", "jose", "4+", "star", "b", "w", "2024", "-", "03", "and",
                "x"
            ]
        );
        let caps: Vec<bool> = tokenize("Alex and Sam's")
            .into_iter()
            .map(|w| w.cap)
            .collect();
        assert_eq!(caps, [true, false, true]);
        assert_eq!(tokenize("it's").len(), 1);
        assert_eq!(tokenize("sam's.").len(), 1);
        assert_eq!(tokenize("x'sy")[0].w, "xsy");
        assert_eq!(tokenize("'s")[0].w, "s");
        assert_eq!(tokenize("+5")[0].w, "5");
    }
}
