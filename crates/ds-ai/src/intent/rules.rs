//! Deterministic, offline phrase parser: the fallback [`IntentBackend`].
//!
//! **Principle: when unsure, don't apply — report.** Every clause that is not
//! understood, is ambiguous, or asks for something this seam cannot express
//! ends up in [`ParseReport::unrecognized`], never silently dropped.
//!
//! Text is lower-cased (Unicode minus → `-`, decimal comma → point), a few
//! phrases are canonicalized ("black and white" → `bw`, "get rid of" →
//! `remove`, "too little" / "not enough" → `notenough`, "twice as" → `1 stop`,
//! "two thirds" → `2/3`), then split into clauses on punctuation and on
//! "and"/"then"/"also"/"plus"/"but".
//!
//! **Carrying modifiers across "and".** Intensity words ("a bit", "much")
//! carry to the next clause when it has none of its own ("a bit warmer and
//! brighter"). Direction words ("less", "lift", "darken") carry only to a
//! bare control noun ("lift shadows and highlights", "less yellow and
//! green") — never to a clause with its own direction ("reduce contrast and
//! brighten" brightens).
//!
//! **Amounts.** A keyword alone gives a default step (warmth 25, tint 10,
//! exposure 0.5 EV, contrast/saturation 20, shadows/highlights 25, band 20);
//! "a bit"/"slightly" halve it, "much"/"a lot" double it. Numbers are read
//! with their unit: `%` of brightness in stops (20 % brighter = log2 1.2 EV),
//! `k` Kelvin — absolute with "set", a shift with a direction or "by" ("make
//! it 2500k warmer") — `°` degrees. Named white-balance presets map to
//! Kelvin (daylight/flash 5500, cloudy 6500, shade 7500, tungsten 3200,
//! fluorescent 4000). Out-of-range numbers reject the clause. A bare number
//! counts only next to "by" or a control word ("by 2", "exposure 2"); "the
//! 3 people are too dark" is the default step, not +3 EV.
//!
//! **Direction.** Negators ("less", "no", "not", "reduce", "undo", "kill",
//! "remove") flip a keyword's natural direction; "too" flips it again (it
//! names the problem: "too dark" → brighter), and so does a description
//! ("the sky looks dark", "it's yellow") unless it asks ("make it look
//! darker"). Words that only ever name a problem ("washed out", "dull",
//! "faded", "underexposed", "oversaturated") are always corrected.
//! "not … enough" / "too little" keep the direction. "down" only negates
//! nouns ("contrast down"), never adjectives ("cool it down"). Shadows and highlights follow Capture One's sliders
//! (shadows + = brighter, highlights + = recovered/darker), and negators bind
//! to their adjective ("highlights less bright" recovers them).
//!
//! **Refused and reported**: "don't …", "not too …", split toning ("more blue
//! in the shadows"), conflicting directions (in one clause, or two clauses
//! pushing one control opposite ways), absolute "set X to N" for anything but
//! white balance, rotations without a degree unit or direction word.

use super::{BandEdit, ColorBand, CropAspect, EditIntent, IntentBackend, IntentError, Straighten};

/// The offline rule-based parser. Stateless and cheap to construct.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleBasedParser;

/// What a backend understood, and what it did not.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParseReport {
    pub intent: EditIntent,
    /// Clauses that were not understood or were refused, for "didn't
    /// understand: …" feedback.
    pub unrecognized: Vec<String>,
}

impl IntentBackend for RuleBasedParser {
    fn name(&self) -> &'static str {
        "rules"
    }

    fn parse(&self, text: &str) -> Result<ParseReport, IntentError> {
        let r = self.parse_detailed(text);
        if r.intent.is_empty() {
            Err(IntentError::NotUnderstood(text.trim().to_string()))
        } else {
            Ok(r)
        }
    }
}

/// Characters of a request that are read; a real one is a sentence or two.
/// The cap keeps a pasted blob from costing more than a long request.
const MAX_REQUEST_CHARS: usize = 2048;

const PHRASES: &[(&str, &str)] = &[
    ("black and white", " bw "),
    ("black & white", " bw "),
    ("black-and-white", " bw "),
    ("b&w", " bw "),
    ("b & w", " bw "),
    ("b/w", " bw "),
    ("counter-clockwise", " ccw "),
    ("counter clockwise", " ccw "),
    ("counterclockwise", " ccw "),
    ("anti-clockwise", " ccw "),
    ("anticlockwise", " ccw "),
    ("clockwise", " cw "),
    ("auto white balance", " autowb "),
    ("auto wb", " autowb "),
    ("color cast", " colorcast "),
    ("colour cast", " colorcast "),
    ("auto-", "auto "),
    ("color temperature", " temperature "),
    ("colour temperature", " temperature "),
    ("color temp", " temperature "),
    ("colour temp", " temperature "),
    ("white balance", " temperature "),
    ("do not", " dont "),
    ("don't", " dont "),
    ("too little", " notenough "),
    ("too few", " notenough "),
    ("not enough", " notenough "),
    ("too much", " too "),
    ("tone down", " reduce "),
    ("bring down", " reduce "),
    ("pull back", " reduce "),
    ("pull down", " reduce "),
    ("dial back", " reduce "),
    ("turn down", " reduce "),
    ("turn up", " increase "),
    ("take out", " remove "),
    ("get rid of", " remove "),
    ("open up", " lift "),
    ("fix the horizon", " straighten "),
    ("level the horizon", " straighten "),
    ("twice as", " 1 stop "),
    ("half as", " -1 stop "),
    ("two thirds", " 2/3 "),
    ("one third", " 1/3 "),
    ("a third", " 1/3 "),
    ("a lot", " lot "),
];

const SMALL: &[&str] = &[
    "bit", "slightly", "slight", "little", "touch", "tad", "subtle", "subtly", "somewhat", "hint",
    "gently", "mild", "mildly", "smidge",
];
const BIG: &[&str] = &[
    "much",
    "lot",
    "lots",
    "very",
    "way",
    "really",
    "significantly",
    "strongly",
    "heavily",
    "considerably",
    "massively",
    "greatly",
    "substantially",
    "super",
    "extremely",
    "far",
];
const NEG: &[&str] = &[
    "less",
    "reduce",
    "decrease",
    "lower",
    "lessen",
    "drop",
    "cut",
    "remove",
    "weaken",
    "minus",
    "fewer",
    "diminish",
    "reduced",
    "decreased",
    "not",
    "no",
    "undo",
    "kill",
    "eliminate",
];
/// Direction words (besides [`NEG`]); a clause with none may borrow them.
const DIRECTION: &[&str] = &[
    "too",
    "notenough",
    "enough",
    "more",
    "up",
    "down",
    "lift",
    "raise",
    "boost",
    "increase",
    "add",
    "crush",
    "deepen",
    "recover",
    "tame",
    "brighten",
    "darken",
    "lighten",
];
/// Control nouns: a clause made only of these (plus filler) is "bare".
const NOUNS: &[&str] = &[
    "contrast",
    "saturation",
    "vibrance",
    "exposure",
    "brightness",
    "shadows",
    "shadow",
    "highlights",
    "highlight",
    "temperature",
    "temp",
    "tint",
    "warmth",
    "whites",
    "blacks",
    "midtones",
];
const COLORS: &[(&str, ColorBand)] = &[
    ("red", ColorBand::Red),
    ("orange", ColorBand::Orange),
    ("yellow", ColorBand::Yellow),
    ("green", ColorBand::Green),
    ("cyan", ColorBand::Cyan),
    ("aqua", ColorBand::Cyan),
    ("blue", ColorBand::Blue),
    ("purple", ColorBand::Purple),
    ("magenta", ColorBand::Magenta),
    ("pink", ColorBand::Magenta),
];
const FILLER: &[&str] = &[
    "the", "a", "an", "some", "of", "it", "its", "in", "on", "to", "by", "please", "make", "is",
    "are",
];
/// "looks / is / are …": the clause describes the photo (a complaint), it
/// doesn't ask for the adjective ("the sky looks dark" → brighter).
const LINKING: &[&str] = &[
    "looks", "look", "looking", "seems", "seem", "is", "are", "was", "were", "feels", "feel",
    "it's", "its", "that's",
];
/// Words that make a linking clause a request after all ("make it look darker").
const REQUEST: &[&str] = &[
    "make", "want", "should", "could", "can", "need", "needs", "let",
];
/// Adjectives that only ever name a problem: always corrected, whatever the
/// direction words ("washed out", "less dull", "too dull" all add colour).
const WASHED: &[&str] = &["washed", "dull", "faded", "drab", "lifeless"];
const GARISH: &[&str] = &["oversaturated", "garish", "overcooked"];
/// Words a bare number may follow or precede to belong to a control
/// ("exposure 2", "by 2", "rotate right 1"); "the 3 people" has none.
const ANCHORS: &[&str] = &[
    "by",
    "rotate",
    "rotation",
    "tilt",
    "straighten",
    "turn",
    "left",
    "right",
    "cw",
    "ccw",
];
const DEGREE_UNITS: &[&str] = &["degrees", "degree", "deg", "°"];
const DIRECTIONS: &[&str] = &["cw", "ccw", "left", "right"];
const PRESETS: &[(&str, f32)] = &[
    ("daylight", 5500.0),
    ("sunny", 5500.0),
    ("flash", 5500.0),
    ("cloudy", 6500.0),
    ("overcast", 6500.0),
    ("shade", 7500.0),
    ("shady", 7500.0),
    ("tungsten", 3200.0),
    ("incandescent", 3200.0),
    ("fluorescent", 4000.0),
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Unit {
    None,
    Percent,
    Kelvin,
    Degrees,
    Stops,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Num {
    value: f32,
    signed: bool,
    unit: Unit,
}

/// A clause was matched but its request is invalid, ambiguous or refused.
#[derive(Debug)]
struct Reject;

struct Clause {
    toks: Vec<String>,
    /// Numbers already consumed (by a crop ratio or rotation).
    numbers_used: bool,
}

impl Clause {
    fn any(&self, set: &[&str]) -> bool {
        self.toks.iter().any(|t| set.contains(&t.as_str()))
    }

    fn any_prefix(&self, prefixes: &[&str]) -> bool {
        self.toks
            .iter()
            .any(|t| prefixes.iter().any(|p| t.starts_with(p)))
    }

    /// A colour/temperature word or its inflection ("greener", "warmth").
    fn any_word(&self, stems: &[&str]) -> bool {
        self.toks
            .iter()
            .any(|t| stems.iter().any(|s| is_word(t, s)))
    }

    fn scale(&self) -> f32 {
        if self.any(SMALL) {
            0.5
        } else if self.any(BIG) {
            2.0
        } else {
            1.0
        }
    }

    fn wants_more(&self) -> bool {
        self.any(&["notenough"]) || (self.any(&["not"]) && self.any(&["enough"]))
    }

    /// The clause describes the photo ("the sky looks dark", "it's too
    /// warm") rather than asking for the adjective: the adjective flips.
    fn complains(&self) -> bool {
        self.any(&["too"]) || (self.any(LINKING) && !self.any(REQUEST))
    }

    /// Direction multiplier for a keyword whose natural sign is `base`.
    /// `noun`: the keyword names a control, so "down" lowers it (and "the
    /// exposure is …" is not a complaint about an adjective).
    fn dir(&self, base: f32, noun: bool) -> f32 {
        if self.wants_more() {
            return base;
        }
        let mut s = base;
        if self.any(NEG) || (noun && self.any(&["down"])) {
            s = -s;
        }
        if self.any(&["too"]) || (!noun && self.complains()) {
            s = -s;
        }
        s
    }

    /// The first number that belongs to a control: with a unit ("2 stops",
    /// "400k", "3°", "20%"), a sign ("+2"), or next to "by" / a control word
    /// ("by 2", "exposure 2"). "the 3 people" and "the 2nd shot" have none.
    fn number(&self) -> Option<Num> {
        if self.numbers_used {
            return None;
        }
        let count = [("one", 1.0), ("two", 2.0), ("three", 3.0)];
        self.toks.iter().enumerate().find_map(|(k, t)| {
            let count_word = count.iter().find(|(w, _)| w == t);
            let numeric = t.starts_with(|c: char| c.is_ascii_digit() || "+-./".contains(c))
                || matches!(t.as_str(), "half" | "third" | "quarter");
            if count_word.is_none() && !numeric {
                return None;
            }
            // "two thirds of a stop", "half a stop": the unit after "of a".
            let unit_at = self.toks[k + 1..]
                .iter()
                .find(|w| !matches!(w.as_str(), "of" | "a" | "an"))
                .map_or("", String::as_str);
            if let Some(&(_, v)) = count_word {
                let unit = unit_word(unit_at);
                return (unit != Unit::None).then_some(Num {
                    value: v,
                    signed: false,
                    unit,
                });
            }
            let n = parse_number(t, unit_at)?;
            let near = |w: Option<&String>| {
                w.is_some_and(|w| NOUNS.contains(&w.as_str()) || ANCHORS.contains(&w.as_str()))
            };
            let anchored = n.unit != Unit::None
                || n.signed
                || near(k.checked_sub(1).and_then(|j| self.toks.get(j)))
                || near(self.toks.get(k + 1));
            anchored.then_some(n)
        })
    }

    /// `base · step · scale`, or the explicit number, for a slider bounded by `limit`.
    fn slider(&self, base: f32, noun: bool, step: f32, limit: f32) -> Result<f32, Reject> {
        self.amount(self.dir(base, noun), step, limit)
    }

    /// [`Self::slider`] with the direction `d` already decided.
    fn amount(&self, d: f32, step: f32, limit: f32) -> Result<f32, Reject> {
        let v = match self.number() {
            None => d * step * self.scale(),
            Some(Num {
                unit: Unit::Kelvin | Unit::Degrees | Unit::Stops,
                ..
            }) => return Err(Reject),
            Some(n) if n.signed => n.value,
            Some(n) => d * n.value.abs(),
        };
        if v.abs() > limit { Err(Reject) } else { Ok(v) }
    }

    /// Both directions of one quality in a single clause.
    fn conflicting(&self) -> bool {
        let brighter = self.any_prefix(&["bright", "lighte"]);
        let darker = self.any_prefix(&["dark", "dim"]);
        let warm = self.any_word(&["warm"]);
        let cool = self.any_word(&["cool", "cold"]);
        (brighter && darker) || (warm && cool) || (self.any(&["more"]) && self.any(&["less"]))
    }
}

/// `tok` is `stem` or an inflection of it ("greener", "reds", "reddish",
/// "warmth") — not an unrelated word that merely starts the same ("reduce").
fn is_word(tok: &str, stem: &str) -> bool {
    tok.strip_prefix(stem).is_some_and(|rest| {
        [
            "", "s", "r", "er", "ish", "ness", "th", "ing", "der", "dish",
        ]
        .contains(&rest)
    })
}

fn unit_word(w: &str) -> Unit {
    match w {
        "%" | "percent" | "pct" => Unit::Percent,
        "k" | "kelvin" => Unit::Kelvin,
        "degrees" | "degree" | "deg" | "°" => Unit::Degrees,
        "stop" | "stops" | "ev" => Unit::Stops,
        _ => Unit::None,
    }
}

/// A number token with an optional attached or following unit.
fn parse_number(tok: &str, next: &str) -> Option<Num> {
    let fractions = [("half", 0.5), ("third", 1.0 / 3.0), ("quarter", 0.25)];
    if let Some(&(_, v)) = fractions.iter().find(|(w, _)| *w == tok) {
        return Some(Num {
            value: v,
            signed: false,
            unit: unit_word(next),
        });
    }
    let split = tok
        .find(|c: char| !(c.is_ascii_digit() || "+-./".contains(c)))
        .unwrap_or(tok.len());
    let (body, suffix) = tok.split_at(split);
    let unit = match unit_word(suffix) {
        Unit::None if suffix.is_empty() => unit_word(next),
        Unit::None => return None,
        u => u,
    };
    let signed = body.starts_with(['+', '-']);
    let digits = body.trim_start_matches(['+', '-']);
    if !digits.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let v = match digits.split_once('/') {
        Some((a, b)) => a.parse::<f32>().ok()? / b.parse::<f32>().ok().filter(|b| *b != 0.0)?,
        None => digits.parse::<f32>().ok()?,
    };
    let value = if body.starts_with('-') { -v } else { v };
    value.is_finite().then_some(Num {
        value,
        signed,
        unit,
    })
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// `"4:5"`, `"16x9"`, `"4×5"`, `"1.91:1"` → integer `(w, h)` in lowest terms.
fn parse_ratio(tok: &str) -> Option<(u32, u32)> {
    let (a, b) = tok.split_once([':', 'x', '×'])?;
    let decimals = a.contains('.') || b.contains('.');
    let (fa, fb) = (a.parse::<f32>().ok()?, b.parse::<f32>().ok()?);
    let k = if decimals { 100.0 } else { 1.0 };
    let (w, h) = ((fa * k).round(), (fb * k).round());
    if !(1.0..=100_000.0).contains(&w) || !(1.0..=100_000.0).contains(&h) {
        return Some((0, 0));
    }
    let (w, h) = (w as u32, h as u32);
    let g = gcd(w, h);
    Some((w / g, h / g))
}

fn crop(c: &Clause) -> Result<Option<CropAspect>, Reject> {
    let t = &c.toks;
    let ratio = t.iter().find_map(|s| parse_ratio(s)).or_else(|| {
        t.windows(3).find_map(|w| {
            (w[1] == "by" || w[1] == "x")
                .then(|| Some((w[0].parse().ok()?, w[2].parse().ok()?)))
                .flatten()
        })
    });
    let ratio = ratio.or(if c.any(&["square"]) {
        Some((1, 1))
    } else if c.any(&["widescreen"]) {
        Some((16, 9))
    } else if c.any(&["instagram"]) {
        Some((4, 5))
    } else {
        None
    });
    let Some((w, h)) = ratio else { return Ok(None) };
    if !(1..=1000).contains(&w) || !(1..=1000).contains(&h) {
        return Err(Reject);
    }
    let (w, h) = if c.any(&["portrait", "vertical", "tall"]) {
        (w.min(h), w.max(h))
    } else if c.any(&["landscape", "horizontal", "wide"]) {
        (w.max(h), w.min(h))
    } else {
        (w, h)
    };
    Ok(Some(CropAspect {
        width: w,
        height: h,
    }))
}

fn straighten(c: &Clause) -> Result<Option<Straighten>, Reject> {
    let has_unit = c.any(DEGREE_UNITS)
        || c.toks
            .iter()
            .any(|t| t.ends_with('°') || t.ends_with("deg"));
    let has_dir = c.any(DIRECTIONS);
    let level = c.any(&["straighten", "horizon"])
        || (c.any(&["level"]) && c.any(&["it", "horizon", "out", "image", "photo", "picture"]));
    let explicit = c.any(&["rotate", "rotation", "tilt"]);
    let loose = c.any(&["turn", "level"]) && (has_unit || has_dir);
    if !(level || explicit || loose) {
        return Ok(None);
    }
    match c.number() {
        None if level => Ok(Some(Straighten::Auto)),
        Some(n) if (has_unit || has_dir) && n.value.abs() <= 45.0 => {
            let cw = !n.signed && c.any(&["cw", "right"]);
            Ok(Some(Straighten::Degrees(if cw {
                -n.value
            } else {
                n.value
            })))
        }
        _ => Err(Reject),
    }
}

/// The colour a token names, including comparatives ("greener", "bluer").
fn color_of(tok: &str) -> Option<ColorBand> {
    COLORS
        .iter()
        .find(|(w, _)| is_word(tok, w))
        .map(|&(_, b)| b)
}

/// Band edits: a colour with an object ("blue sky", "purple flowers",
/// "greener grass"), an object alone ("more colour in the sky"), or a plural
/// colour ("brighter blues").
fn band(c: &Clause) -> Option<ColorBand> {
    const OBJECTS: &[(&str, ColorBand)] = &[
        ("sky", ColorBand::Blue),
        ("skies", ColorBand::Blue),
        ("grass", ColorBand::Green),
        ("foliage", ColorBand::Green),
        ("trees", ColorBand::Green),
        ("leaves", ColorBand::Green),
        ("greenery", ColorBand::Green),
        ("water", ColorBand::Cyan),
        ("sea", ColorBand::Cyan),
        ("ocean", ColorBand::Cyan),
        ("skin", ColorBand::Orange),
        ("sunset", ColorBand::Orange),
    ];
    const PLURALS: &[&str] = &[
        "reds", "oranges", "yellows", "greens", "cyans", "aquas", "blues", "purples", "magentas",
    ];
    let object = OBJECTS.iter().find(|(w, _)| c.any(&[w])).map(|&(_, b)| b);
    let not_object = |t: &str| {
        NOUNS.contains(&t)
            || FILLER.contains(&t)
            || NEG.contains(&t)
            || DIRECTION.contains(&t)
            || SMALL.contains(&t)
            || BIG.contains(&t)
            || [
                "tint",
                "tone",
                "tones",
                "hue",
                "cast",
                "colorcast",
                "color",
                "colour",
                "and",
                "please",
            ]
            .contains(&t)
            || !t.chars().all(char::is_alphabetic)
    };
    for (k, t) in c.toks.iter().enumerate() {
        if let Some(b) = color_of(t) {
            let plural = PLURALS.contains(&t.as_str());
            let before_noun = c
                .toks
                .get(k + 1)
                .is_some_and(|n| !not_object(n) && color_of(n).is_none());
            if plural || object.is_some() || before_noun {
                return Some(b);
            }
        }
    }
    object
}

fn band_edit(c: &Clause, band: ColorBand) -> Result<BandEdit, Reject> {
    let mut e = BandEdit {
        band,
        saturation: 0.0,
        lightness: 0.0,
    };
    if c.any_prefix(&["bright", "light"]) {
        e.lightness = c.slider(1.0, false, 20.0, 100.0)?;
    } else if c.any_prefix(&["dark", "deep"]) {
        e.lightness = c.slider(-1.0, false, 20.0, 100.0)?;
    } else if c.any(WASHED) {
        // "the sky looks washed out": a complaint, corrected with colour.
        e.saturation = c.amount(1.0, 20.0, 100.0)?;
    } else if c.any(GARISH) {
        e.saturation = c.amount(-1.0, 20.0, 100.0)?;
    } else if c.any_prefix(&["desaturat", "mute"]) {
        e.saturation = c.slider(-1.0, false, 20.0, 100.0)?;
    } else {
        // "more colour in the sky", "greener grass", "purple flowers pop".
        e.saturation = c.slider(1.0, false, 20.0, 100.0)?;
    }
    Ok(e)
}

fn black_and_white(c: &Clause) -> Option<bool> {
    // "more colour in the sky" is a band edit, not "back in colour".
    let color = c.any(&["color", "colour"]) && band(c).is_none();
    if c.any(&["bw", "monochrome", "mono", "grayscale", "greyscale", "noir"]) {
        Some(!c.any(&["undo", "remove", "no", "not", "without", "cancel", "revert"]))
    } else if color && c.any(&["no", "without"]) {
        Some(true)
    } else if color && c.any(&["back", "restore", "in", "full", "into"]) {
        Some(false)
    } else {
        None
    }
}

fn temperature(c: &Clause, i: &mut EditIntent) -> Result<bool, Reject> {
    if let Some(&(_, k)) = PRESETS.iter().find(|(w, _)| c.any(&[w])) {
        i.temperature_k = Some(k);
        return Ok(true);
    }
    let (base, noun) = if c.any_word(&["warm", "yellow", "orange", "amber"]) {
        (1.0, false)
    } else if c.any_word(&["cool", "cold", "blue"]) {
        (-1.0, false)
    } else if c.any(&["temperature", "temp", "wb"]) {
        (1.0, true)
    } else {
        return Ok(false);
    };
    match c.number() {
        Some(Num {
            unit: Unit::Kelvin,
            value,
            signed,
        }) => {
            let set = c.any(&["set"]);
            let shift = !set && (signed || !noun || c.any(&["by"]));
            if shift {
                let v = if signed {
                    value
                } else {
                    c.dir(base, noun) * value
                };
                if v.abs() > 20_000.0 {
                    return Err(Reject);
                }
                i.temperature_shift_k = v;
            } else if (2000.0..=50_000.0).contains(&value) {
                i.temperature_k = Some(value);
            } else {
                return Err(Reject);
            }
        }
        _ => i.warmth = c.slider(base, noun, 25.0, 100.0)?,
    }
    Ok(true)
}

fn tint(c: &Clause) -> Result<Option<f32>, Reject> {
    let (base, noun) = if c.any_word(&["green"]) {
        (-1.0, false)
    } else if c.any_word(&["magenta", "pink", "purple"]) {
        (1.0, false)
    } else if c.any(&["tint"]) {
        (1.0, true)
    } else {
        return Ok(None);
    };
    c.slider(base, noun, 10.0, 150.0).map(Some)
}

/// Shadows follow the slider: + brighter shadows.
fn shadows(c: &Clause) -> Result<Option<f32>, Reject> {
    if !c.any_prefix(&["shadow"]) {
        return Ok(None);
    }
    tonal(
        c,
        &[
            "lift", "raise", "open", "brighten", "lighten", "boost", "increase", "more", "up",
            "recover",
        ],
        &[
            "crush", "deepen", "darken", "lower", "decrease", "reduce", "down", "drop", "cut",
            "block",
        ],
        &["bright", "brighter", "light", "lighter"],
        &["dark", "darker", "deep", "deeper"],
        -1.0,
    )
}

/// Highlights follow Capture One: + recovers (darker highlights).
fn highlights(c: &Clause) -> Result<Option<f32>, Reject> {
    if !c.any_prefix(&["highlight", "blown"]) {
        return Ok(None);
    }
    tonal(
        c,
        &[
            "recover", "tame", "reduce", "lower", "decrease", "darken", "down", "pull", "save",
            "rescue", "fix", "drop", "cut",
        ],
        &[
            "boost", "brighten", "raise", "increase", "more", "lift", "up", "lighten",
        ],
        &["dark", "darker"],
        &["bright", "brighter", "light", "lighter"],
        1.0,
    )
}

/// Shared by shadows/highlights. Verbs (`plus`/`minus`) set the direction;
/// otherwise adjectives do, and a negator binds to the adjective ("less
/// dark" = lighter); otherwise a bare negator gives `bare_neg`, and nothing
/// at all gives +. "too" flips (it names the problem).
fn tonal(
    c: &Clause,
    plus: &[&str],
    minus: &[&str],
    adj_plus: &[&str],
    adj_minus: &[&str],
    bare_neg: f32,
) -> Result<Option<f32>, Reject> {
    let negated = c.any(&["less", "not", "no", "fewer"]);
    let adjective = !c.any(plus) && !c.any(minus) && (c.any(adj_plus) || c.any(adj_minus));
    let mut d = if c.any(plus) {
        1.0
    } else if c.any(minus) {
        -1.0
    } else if adjective {
        let a = if c.any(adj_plus) { 1.0 } else { -1.0 };
        if negated && !c.wants_more() { -a } else { a }
    } else if negated {
        bare_neg
    } else {
        1.0
    };
    // "too" names the problem, and so does "the shadows are dark".
    if !c.wants_more() && (c.any(&["too"]) || (adjective && c.complains())) {
        d = -d;
    }
    let v = match c.number() {
        None => d * 25.0 * c.scale(),
        Some(Num {
            unit: Unit::None | Unit::Percent,
            value,
            signed,
        }) => {
            if signed {
                value
            } else {
                d * value.abs()
            }
        }
        Some(_) => return Err(Reject),
    };
    if v.abs() > 100.0 {
        Err(Reject)
    } else {
        Ok(Some(v))
    }
}

fn contrast(c: &Clause, highlights: bool) -> Result<Option<f32>, Reject> {
    if c.any(&["washed"]) {
        return c.amount(1.0, 20.0, 100.0).map(Some); // washed out: flat and pale
    }
    let (base, noun) = if c.any(&["contrast"]) {
        (1.0, true)
    } else if c.any_prefix(&["contrasty", "punch", "dramatic"]) || c.any(&["pop"]) {
        (1.0, false)
    } else if c.any_prefix(&["flat"]) || (c.any(&["soft", "softer"]) && !highlights) {
        (-1.0, false)
    } else {
        return Ok(None);
    };
    c.slider(base, noun, 20.0, 100.0).map(Some)
}

fn saturation(c: &Clause) -> Result<Option<f32>, Reject> {
    // Complaint-only words are corrected whatever else the clause says.
    if c.any(WASHED) {
        return c.amount(1.0, 20.0, 100.0).map(Some);
    }
    if c.any(GARISH) {
        return c.amount(-1.0, 20.0, 100.0).map(Some);
    }
    let (base, noun) = if c.any_prefix(&["desaturat", "mute", "pastel"]) {
        (-1.0, false)
    } else if c.any(&["saturation", "vibrance"]) {
        (1.0, true)
    } else if c.any_prefix(&["saturat", "vibran", "vivid", "colorful", "colourful"])
        || c.any(&["colors", "colours", "color", "colour"])
    {
        (1.0, false)
    } else {
        return Ok(None);
    };
    c.slider(base, noun, 20.0, 100.0).map(Some)
}

fn exposure(c: &Clause) -> Result<Option<f32>, Reject> {
    // "underexposed" names the problem: corrected, not flipped by "is"/"too".
    let corrective = c.any(&["underexposed", "overexposed"]);
    let (base, noun) = if c.any(&["underexposed", "overexpose"]) {
        (1.0, false)
    } else if c.any(&["overexposed", "underexpose"]) {
        (-1.0, false)
    } else if c.any(&["exposure", "brightness"]) {
        (1.0, true)
    } else if c.any_prefix(&["bright", "lighte"]) {
        (1.0, false)
    } else if c.any_prefix(&["dark", "dim"]) {
        (-1.0, false)
    } else if c.number().is_some_and(|n| n.unit == Unit::Stops) {
        (1.0, true)
    } else {
        return Ok(None);
    };
    let d = if corrective { base } else { c.dir(base, noun) };
    let ev = match c.number() {
        None => d * 0.5 * c.scale(),
        Some(Num {
            unit: Unit::Percent,
            value,
            signed,
        }) => {
            let p = if signed { value } else { d * value.abs() } / 100.0;
            if p <= -1.0 {
                return Err(Reject);
            }
            (1.0 + p).log2()
        }
        Some(Num {
            unit: Unit::None | Unit::Stops,
            value,
            signed,
        }) => {
            if signed {
                value
            } else {
                d * value.abs()
            }
        }
        Some(_) => return Err(Reject),
    };
    if ev.abs() > 5.0 {
        Err(Reject)
    } else {
        Ok(Some(ev))
    }
}

const COLOR_WORDS: &[&str] = &[
    "warm", "cool", "cold", "blue", "yellow", "orange", "amber", "green", "magenta", "pink",
    "purple", "red", "cyan",
];

/// One clause → intent; `Ok(None)` when nothing matched.
fn clause(c: &mut Clause) -> Result<Option<EditIntent>, Reject> {
    // Refusals: explicit no-ops, double negation, split toning, conflicts.
    let split_toning = c.any_word(COLOR_WORDS) && c.any_prefix(&["shadow", "highlight", "midtone"]);
    let not_too = c.any(&["not"]) && c.any(&["too"]) && !c.wants_more();
    if c.any(&["dont"]) || not_too || split_toning || c.conflicting() {
        return Err(Reject);
    }
    let mut i = EditIntent::default();
    let mut hit = false;
    if c.any(&["colorcast", "autowb"]) || (c.any(&["auto"]) && c.any(&["temperature", "wb"])) {
        i.auto_white_balance = true;
        return Ok(Some(i));
    }
    let bw = black_and_white(c);
    if bw.is_some() {
        i.black_and_white = bw;
        hit = true;
    }
    if let Some(a) = crop(c)? {
        i.crop_aspect = Some(a);
        c.numbers_used = true; // the clause's numbers belong to the ratio
        hit = true;
    }
    if let Some(s) = straighten(c)? {
        i.straighten = Some(s);
        c.numbers_used = true;
        hit = true;
    } else if c.any(&[
        "auto",
        "automatic",
        "autotone",
        "autofix",
        "enhance",
        "autoenhance",
    ]) {
        i.auto = true;
        hit = true;
    }
    let wb_words =
        c.any(&["temperature", "temp", "wb"]) || PRESETS.iter().any(|(w, _)| c.any(&[w]));
    if c.any(&["set"]) && !wb_words {
        return Err(Reject); // absolute values only exist for white balance
    }
    if bw.is_none()
        && let Some(b) = band(c)
    {
        i.color_bands.push(band_edit(c, b)?);
        return Ok(Some(i));
    }
    hit |= temperature(c, &mut i)?;
    let sh = shadows(c)?;
    let hl = highlights(c)?;
    let fields = [
        (&mut i.tint, tint(c)?),
        (&mut i.contrast, contrast(c, hl.is_some())?),
        (
            &mut i.saturation,
            if bw.is_some() { None } else { saturation(c)? },
        ),
        (
            &mut i.exposure_ev,
            if sh.is_some() || hl.is_some() {
                None
            } else {
                exposure(c)?
            },
        ),
        (&mut i.shadows, sh),
        (&mut i.highlights, hl),
    ];
    for (slot, v) in fields {
        if let Some(v) = v {
            *slot = v;
            hit = true;
        }
    }
    Ok(hit.then_some(i))
}

/// The signed controls of an intent, for conflict detection across clauses.
fn directions(i: &EditIntent) -> [f32; 8] {
    [
        // (`f32::signum(0.0)` is 1, hence the explicit zero test.)
        i.warmth
            + if i.temperature_shift_k == 0.0 {
                0.0
            } else {
                i.temperature_shift_k.signum()
            },
        i.tint,
        i.exposure_ev,
        i.contrast,
        i.saturation,
        i.shadows,
        i.highlights,
        i.color_bands
            .iter()
            .map(|b| b.saturation + b.lightness)
            .sum(),
    ]
}

impl RuleBasedParser {
    /// Parse and also report the clauses that were not understood or refused.
    pub fn parse_detailed(&self, text: &str) -> ParseReport {
        let end = text
            .char_indices()
            .nth(MAX_REQUEST_CHARS)
            .map_or(text.len(), |(i, _)| i);
        let mut s = format!(" {} ", normalize(&text[..end]));
        for (from, to) in PHRASES {
            s = replace_phrase(&s, from, to);
        }
        let mut report = ParseReport::default();
        let mut parsed: Vec<(String, EditIntent)> = Vec::new();
        for sentence in split_sentences(&s) {
            let (mut intensity, mut direction): (Vec<String>, Vec<String>) =
                (Vec::new(), Vec::new());
            for part in split_words(&sentence) {
                let mut toks: Vec<String> = part
                    .split_whitespace()
                    .map(|t| {
                        t.trim_matches(|c: char| !(c.is_alphanumeric() || "+-.:/%°×".contains(c)))
                    })
                    .map(|t| t.trim_end_matches('.').to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
                if toks.is_empty() {
                    continue;
                }
                let is = |t: &String, set: &[&str]| set.contains(&t.as_str());
                let own_int: Vec<String> = toks
                    .iter()
                    .filter(|t| is(t, SMALL) || is(t, BIG))
                    .cloned()
                    .collect();
                let own_dir: Vec<String> = toks
                    .iter()
                    .filter(|t| is(t, NEG) || is(t, DIRECTION))
                    .cloned()
                    .collect();
                let bare = toks.iter().all(|t| {
                    is(t, NOUNS)
                        || is(t, FILLER)
                        || is(t, SMALL)
                        || is(t, BIG)
                        || COLORS.iter().any(|(c, _)| c == t)
                });
                // Size words carry unless the clause states its own size or
                // its own direction ("a bit less contrast and more exposure").
                if !own_int.is_empty() {
                    intensity = own_int;
                } else if own_dir.is_empty() {
                    toks.extend(intensity.iter().cloned());
                }
                if !own_dir.is_empty() {
                    direction = own_dir;
                } else if bare {
                    toks.extend(direction.iter().cloned());
                }
                match clause(&mut Clause {
                    toks,
                    numbers_used: false,
                }) {
                    Ok(Some(i)) => parsed.push((part.trim().to_string(), i)),
                    Ok(None) | Err(Reject) => report.unrecognized.push(part.trim().to_string()),
                }
            }
        }
        // Two clauses pushing one control opposite ways: refuse both.
        let dirs: Vec<[f32; 8]> = parsed.iter().map(|(_, i)| directions(i)).collect();
        // A clause is refused when another clause pushes one of its controls
        // the other way: per control, note whether anyone pushes each way.
        let mut up = [false; 8];
        let mut down = [false; 8];
        for d in &dirs {
            for (f, v) in d.iter().enumerate() {
                up[f] |= *v > 0.0;
                down[f] |= *v < 0.0;
            }
        }
        let keep: Vec<bool> = dirs
            .iter()
            .map(|d| {
                !d.iter()
                    .enumerate()
                    .any(|(f, v)| (*v > 0.0 && down[f]) || (*v < 0.0 && up[f]))
            })
            .collect();
        for ((text, i), keep) in parsed.into_iter().zip(keep) {
            if keep {
                report.intent = report.intent.then(i);
            } else {
                report.unrecognized.push(text);
            }
        }
        report
    }
}

/// `s` with each whole-word occurrence of `from` replaced by `to`: "a lot"
/// is a phrase, "a lottery" is not. A side of the phrase that ends in a
/// symbol ("auto-") needs no boundary.
fn replace_phrase(s: &str, from: &str, to: &str) -> String {
    let word = |c: char| c.is_alphanumeric();
    let (starts, ends) = (from.starts_with(word), from.ends_with(word));
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (at, m) in s.match_indices(from) {
        let end = at + m.len();
        let before_ok = !starts || !s[..at].ends_with(word);
        let after_ok = !ends || !s[end..].starts_with(word);
        if at >= last && before_ok && after_ok {
            out.push_str(&s[last..at]);
            out.push_str(to);
            last = end;
        }
    }
    out.push_str(&s[last..]);
    out
}

/// Lower-case; Unicode minus/en dash → `-`, em dash → `,`, curly apostrophe
/// → `'`, decimal comma between digits → `.`.
fn normalize(text: &str) -> String {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let digit = |k: usize| chars.get(k).is_some_and(char::is_ascii_digit);
    chars
        .iter()
        .enumerate()
        .map(|(k, &ch)| match ch {
            '\u{2212}' | '\u{2013}' => '-',
            '\u{2014}' => ',',
            '\u{2019}' => '\'',
            ',' if k > 0 && digit(k - 1) && digit(k + 1) => '.',
            c => c,
        })
        .collect()
}

/// Split on `, ; ! ? newline` and on `.` unless it is a decimal point.
fn split_sentences(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = vec![String::new()];
    for (k, &ch) in chars.iter().enumerate() {
        let decimal = ch == '.' && chars.get(k + 1).is_some_and(char::is_ascii_digit);
        if matches!(ch, ',' | ';' | '!' | '?' | '\n') || (ch == '.' && !decimal) {
            out.push(String::new());
        } else {
            out.last_mut().expect("never empty").push(ch);
        }
    }
    out
}

/// Split a sentence on connective words.
fn split_words(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    for w in s.split_whitespace() {
        if matches!(w, "and" | "then" | "also" | "plus" | "but" | "&") {
            out.push(String::new());
        } else {
            let cur = out.last_mut().expect("never empty");
            cur.push(' ');
            cur.push_str(w);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> EditIntent {
        RuleBasedParser
            .parse(text)
            .unwrap_or_else(|e| panic!("{text:?}: {e}"))
            .intent
    }

    /// Nothing applied from `text`, and all of it reported.
    fn refused(text: &str) -> bool {
        let r = RuleBasedParser.parse_detailed(text);
        r.intent.is_empty() && !r.unrecognized.is_empty()
    }

    fn rejected(text: &str) -> bool {
        let r = RuleBasedParser.parse_detailed(text);
        !r.unrecognized.is_empty()
    }

    #[test]
    fn warmth_phrasings() {
        assert_eq!(p("warmer").warmth, 25.0);
        assert_eq!(p("Make it a bit warmer.").warmth, 12.5);
        assert_eq!(p("much warmer please").warmth, 50.0);
        assert_eq!(p("warm it up").warmth, 25.0);
        assert_eq!(p("cooler").warmth, -25.0);
        assert_eq!(p("cool it down a bit").warmth, -12.5);
        assert_eq!(p("too warm").warmth, -25.0);
        assert_eq!(p("less blue").warmth, 25.0);
        assert_eq!(p("reduce the temperature").warmth, -25.0);
        assert_eq!(p("temperature down").warmth, -25.0);
        assert_eq!(p("warmer by 40").warmth, 40.0);
        assert_eq!(p("not so warm").warmth, -25.0);
        assert_eq!(p("cooler temperature").warmth, -25.0);
        assert_eq!(p("cooler color temp").warmth, -25.0);
        for t in [
            "undo the warmth",
            "take out the warmth",
            "get rid of the yellow",
        ] {
            assert_eq!(p(t).warmth, -25.0, "{t}");
        }
        assert_eq!(p("kill the blue").warmth, 25.0);
        assert!(rejected("warmer by 400"));
    }

    #[test]
    fn kelvin() {
        assert_eq!(p("set temperature to 3200k").temperature_k, Some(3200.0));
        assert_eq!(p("white balance 5600 kelvin").temperature_k, Some(5600.0));
        assert_eq!(p("warmer by 500k").temperature_shift_k, 500.0);
        assert_eq!(p("cooler by 300k").temperature_shift_k, -300.0);
        assert_eq!(p("temperature -200k").temperature_shift_k, -200.0);
        assert!(rejected("set temperature to 90000k"));
        assert!(rejected("warmer by 30000k"));
        assert!(rejected("set contrast to 20"));
    }

    #[test]
    fn tint_phrasings() {
        assert_eq!(p("more magenta").tint, 10.0);
        assert_eq!(p("less green").tint, 10.0);
        assert_eq!(p("a bit greener").tint, -5.0);
        assert_eq!(p("tint -15").tint, -15.0);
        assert_eq!(p("more green tint").tint, -10.0);
        assert_eq!(p("add a green tint").tint, -10.0);
        assert_eq!(p("too much green tint").tint, 10.0);
    }

    #[test]
    fn exposure_phrasings() {
        assert_eq!(p("brighter").exposure_ev, 0.5);
        assert_eq!(p("a bit brighter").exposure_ev, 0.25);
        assert_eq!(p("much brighter").exposure_ev, 1.0);
        assert_eq!(p("brighten it up").exposure_ev, 0.5);
        assert_eq!(p("darken a touch").exposure_ev, -0.25);
        assert_eq!(p("it's too dark").exposure_ev, 0.5);
        assert_eq!(p("too bright").exposure_ev, -0.5);
        assert_eq!(p("less exposure").exposure_ev, -0.5);
        assert_eq!(p("exposure down").exposure_ev, -0.5);
        assert_eq!(p("exposure +0.7").exposure_ev, 0.7);
        assert_eq!(p("exposure -1/3").exposure_ev, -1.0 / 3.0);
        assert_eq!(p("+1 stop").exposure_ev, 1.0);
        assert_eq!(p("+0.3ev").exposure_ev, 0.3);
        assert_eq!(p("\u{2212}1 stop").exposure_ev, -1.0);
        assert_eq!(p("darker by half a stop").exposure_ev, -0.5);
        assert_eq!(p("one stop brighter").exposure_ev, 1.0);
        assert_eq!(p("make this one brighter").exposure_ev, 0.5);
        assert!((p("20% brighter").exposure_ev - 1.2f32.log2()).abs() < 1e-6);
        assert!((p("brighter by 50%").exposure_ev - 1.5f32.log2()).abs() < 1e-6);
        assert_eq!(p("50 percent darker").exposure_ev, -1.0);
        assert_eq!(p("it's not bright enough").exposure_ev, 0.5);
        assert_eq!(p("not dark enough").exposure_ev, -0.5);
        assert_eq!(p("underexposed").exposure_ev, 0.5);
        assert_eq!(p("overexposed").exposure_ev, -0.5);
        assert!(rejected("brighter by 7 stops"));
        assert!(rejected("100% darker"));
        assert!(rejected("brighter by 3 degrees"));
    }

    #[test]
    fn contrast_and_saturation_phrasings() {
        assert_eq!(p("more contrast").contrast, 20.0);
        assert_eq!(p("punchier").contrast, 20.0);
        assert_eq!(p("make it pop").contrast, 20.0);
        assert_eq!(p("reduce contrast by 30%").contrast, -30.0);
        assert_eq!(p("too flat").contrast, 20.0);
        assert_eq!(p("contrast down a lot").contrast, -40.0);
        assert_eq!(p("not enough contrast").contrast, 20.0);
        assert_eq!(p("too little contrast").contrast, 20.0);
        assert_eq!(p("more saturated").saturation, 20.0);
        assert_eq!(p("desaturate slightly").saturation, -10.0);
        assert_eq!(p("too saturated").saturation, -20.0);
        assert_eq!(p("not saturated enough").saturation, 20.0);
        assert_eq!(p("turn the saturation up by 20").saturation, 20.0);
        assert_eq!(p("turn the saturation up by 20").straighten, None);
        assert!(rejected("contrast +150"));
    }

    #[test]
    fn shadows_and_highlights_phrasings() {
        assert_eq!(p("lift the shadows").shadows, 25.0);
        assert_eq!(p("open up the shadows a bit").shadows, 12.5);
        assert_eq!(p("brighter shadows").exposure_ev, 0.0);
        assert_eq!(p("crush the shadows").shadows, -25.0);
        assert_eq!(p("shadows too dark").shadows, 25.0);
        assert_eq!(p("shadows +40").shadows, 40.0);
        for t in ["shadows down", "lower the shadows", "decrease shadows"] {
            assert_eq!(p(t).shadows, -25.0, "{t}");
        }
        assert_eq!(p("recover the highlights").highlights, 25.0);
        assert_eq!(p("tone down the highlights").highlights, 25.0);
        assert_eq!(p("brighter highlights").highlights, -25.0);
        assert_eq!(p("highlights too dark").highlights, -25.0);
        assert_eq!(p("highlights too bright").highlights, 25.0);
        assert_eq!(p("highlights -20").highlights, -20.0);
        assert!(rejected("shadows 2 stops"));
    }

    #[test]
    fn modifiers_distribute_across_and() {
        let i = p("lift shadows and highlights");
        assert_eq!((i.shadows, i.highlights), (25.0, -25.0));
        let i = p("darken highlights and shadows");
        assert_eq!((i.highlights, i.shadows), (25.0, -25.0));
        let i = p("less contrast and saturation");
        assert_eq!((i.contrast, i.saturation), (-20.0, -20.0));
        let i = p("less yellow and green");
        assert_eq!((i.warmth, i.tint), (-25.0, 10.0));
        let i = p("a bit warmer and brighter");
        assert_eq!((i.warmth, i.exposure_ev), (12.5, 0.25));
        // Commas start fresh.
        assert_eq!(p("a bit warmer, brighter").exposure_ev, 0.5);
    }

    #[test]
    fn color_bands() {
        let b = |t: &str| p(t).color_bands;
        let sat = |band, s| {
            vec![BandEdit {
                band,
                saturation: s,
                lightness: 0.0,
            }]
        };
        assert_eq!(b("blue sky more saturated"), sat(ColorBand::Blue, 20.0));
        assert_eq!(b("more colour in the sky"), sat(ColorBand::Blue, 20.0));
        assert_eq!(b("greener grass"), sat(ColorBand::Green, 20.0));
        assert_eq!(b("mute the greens"), sat(ColorBand::Green, -20.0));
        assert_eq!(
            b("brighter blues"),
            vec![BandEdit {
                band: ColorBand::Blue,
                saturation: 0.0,
                lightness: 20.0
            }]
        );
        assert_eq!(
            b("darker sky"),
            vec![BandEdit {
                band: ColorBand::Blue,
                saturation: 0.0,
                lightness: -20.0
            }]
        );
        let i = p("greener grass");
        assert_eq!((i.tint, i.saturation), (0.0, 0.0));
    }

    #[test]
    fn crop_phrasings() {
        let c = |t: &str| p(t).crop_aspect.map(|a| (a.width, a.height));
        assert_eq!(c("crop to 4:5"), Some((4, 5)));
        assert_eq!(c("Crop 16x9"), Some((16, 9)));
        assert_eq!(c("crop 4 by 5"), Some((4, 5)));
        assert_eq!(c("make it square"), Some((1, 1)));
        assert_eq!(c("crop for instagram"), Some((4, 5)));
        assert_eq!(c("widescreen"), Some((16, 9)));
        assert_eq!(c("crop 3:2 portrait"), Some((2, 3)));
        assert_eq!(c("crop 2:3 landscape"), Some((3, 2)));
        // The rest of the clause still counts, and numbers stay with the ratio.
        let i = p("crop 4:5 brighter");
        assert_eq!(
            (i.crop_aspect, i.exposure_ev),
            (
                Some(CropAspect {
                    width: 4,
                    height: 5
                }),
                0.5
            )
        );
        assert_eq!(c("crop to 1.91:1"), Some((191, 100)));
        assert!(rejected("crop 0:5") && rejected("crop 5000:1"));
    }

    #[test]
    fn straighten_phrasings() {
        let s = |t: &str| p(t).straighten;
        assert_eq!(s("straighten"), Some(Straighten::Auto));
        assert_eq!(s("fix the horizon"), Some(Straighten::Auto));
        assert_eq!(s("level it"), Some(Straighten::Auto));
        assert!(!p("auto straighten").auto);
        assert_eq!(s("rotate 2 degrees"), Some(Straighten::Degrees(2.0)));
        assert_eq!(s("rotate 1.5° clockwise"), Some(Straighten::Degrees(-1.5)));
        assert_eq!(
            s("tilt 3 deg counter-clockwise"),
            Some(Straighten::Degrees(3.0))
        );
        assert_eq!(s("turn it 2° left"), Some(Straighten::Degrees(2.0)));
        assert_eq!(s("rotate right 1"), Some(Straighten::Degrees(-1.0)));
        assert_eq!(s("straighten by -2°"), Some(Straighten::Degrees(-2.0)));
        // No unit or direction → not a rotation; "same level" is not straighten.
        assert!(rejected("rotate 2") && rejected("rotate") && rejected("rotate 90°"));
        assert!(RuleBasedParser.parse("same level").is_err());
    }

    #[test]
    fn black_and_white_and_auto() {
        assert_eq!(p("black and white").black_and_white, Some(true));
        assert_eq!(p("B&W").black_and_white, Some(true));
        assert_eq!(p("make it greyscale").black_and_white, Some(true));
        assert_eq!(p("no color").black_and_white, Some(true));
        assert_eq!(p("no color").saturation, 0.0);
        assert_eq!(p("undo black and white").black_and_white, Some(false));
        assert_eq!(p("back to color").black_and_white, Some(false));
        let bw = p("black and white with more contrast");
        assert_eq!((bw.black_and_white, bw.contrast), (Some(true), 20.0));
        assert!(p("auto").auto && p("Auto-tone").auto && p("enhance").auto);
    }

    #[test]
    fn unrecognized_text_is_reported() {
        let r = RuleBasedParser.parse_detailed("brighter and add a unicorn");
        assert_eq!(r.intent.exposure_ev, 0.5);
        assert_eq!(r.unrecognized, vec!["add a unicorn".to_string()]);
        assert_eq!(
            RuleBasedParser.parse("  hello there "),
            Err(IntentError::NotUnderstood("hello there".into()))
        );
        assert!(RuleBasedParser.parse(",,, ... !!").is_err());
        assert_eq!(RuleBasedParser.name(), "rules");
        // The trait returns the report: the refused half is visible.
        let r = RuleBasedParser
            .parse("brighter and rotate 90 degrees")
            .unwrap();
        assert_eq!(
            (r.intent.exposure_ev, r.unrecognized),
            (0.5, vec!["rotate 90 degrees".to_string()])
        );
    }

    /// Phrasings that once parsed wrongly.
    #[test]
    fn review_round_two() {
        // Direction words never carry into a clause with its own direction.
        let i = p("reduce contrast and brighten");
        assert_eq!((i.contrast, i.exposure_ev), (-20.0, 0.5));
        let i = p("less saturation and warmer");
        assert_eq!((i.saturation, i.warmth), (-20.0, 25.0));
        let i = p("a bit less contrast and more exposure");
        assert_eq!((i.contrast, i.exposure_ev), (-10.0, 0.5));
        let i = p("exposure +1 and contrast");
        assert_eq!((i.exposure_ev, i.contrast), (1.0, 20.0));
        let i = p("add some warmth and a little contrast");
        assert_eq!((i.warmth, i.contrast), (25.0, 10.0));
        // Negation binds inside shadows/highlights.
        assert_eq!(p("highlights less bright").highlights, 25.0);
        assert_eq!(p("make the shadows less dark").shadows, 25.0);
        assert_eq!(p("increase highlights").highlights, -25.0);
        assert_eq!(p("reduce the highlights").highlights, 25.0);
        assert_eq!(p("the whites are blown").highlights, 25.0);
        assert_eq!(p("shadows are crushed").shadows, 25.0);
        // White balance presets, Kelvin shifts vs absolutes, text order.
        assert_eq!(
            p("set white balance to daylight").temperature_k,
            Some(5500.0)
        );
        assert_eq!(
            p("set the temperature to tungsten").temperature_k,
            Some(3200.0)
        );
        assert_eq!(p("cloudy white balance").temperature_k, Some(6500.0));
        let i = p("make it 2500k warmer");
        assert_eq!((i.temperature_k, i.temperature_shift_k), (None, 2500.0));
        assert_eq!(p("warmer by 2500k").temperature_shift_k, 2500.0);
        let i = p("warmer by 500k, set temperature to 3200k");
        assert_eq!(
            (i.temperature_k, i.temperature_shift_k),
            (Some(3200.0), 0.0)
        );
        let i = p("set temperature to 3200k, then warmer by 500k");
        assert_eq!(
            (i.temperature_k, i.temperature_shift_k),
            (Some(3200.0), 500.0)
        );
        assert!(refused("set the temperature to 500k"));
        assert!(p("remove the color cast").auto_white_balance);
        assert!(p("auto white balance").auto_white_balance);
        // "no" negates.
        assert_eq!(p("no more blue").warmth, 25.0);
        let i = p("less blue, no green");
        assert_eq!((i.warmth, i.tint), (25.0, 10.0));
        // Numbers.
        assert_eq!(p("brighter by 1,5 stops").exposure_ev, 1.5);
        assert!((p("two thirds of a stop brighter").exposure_ev - 2.0 / 3.0).abs() < 1e-6);
        assert!((p("darker by 2/3 stop").exposure_ev + 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(p("twice as bright").exposure_ev, 1.0);
        assert_eq!(p("half as bright").exposure_ev, -1.0);
        assert!((p("increase exposure by 50%").exposure_ev - 1.5f32.log2()).abs() < 1e-6);
        assert_eq!(p("lighten it up").exposure_ev, 0.5);
        assert_eq!(p("tint toward magenta by 5").tint, 5.0);
        // Rotation.
        assert_eq!(
            p("straighten it 2 degrees to the right").straighten,
            Some(Straighten::Degrees(-2.0))
        );
        assert_eq!(
            p("rotate left by 1.5 degrees").straighten,
            Some(Straighten::Degrees(1.5))
        );
        // Colour bands, not global tint.
        let i = p("make the purple flowers pop");
        assert_eq!((i.tint, i.contrast), (0.0, 0.0));
        assert_eq!(
            i.color_bands,
            vec![BandEdit {
                band: ColorBand::Purple,
                saturation: 20.0,
                lightness: 0.0
            }]
        );
        assert_eq!(
            p("less yellow in the skin").color_bands,
            vec![BandEdit {
                band: ColorBand::Yellow,
                saturation: -20.0,
                lightness: 0.0
            }]
        );
        // Refused and reported, never applied.
        for t in [
            "don't make it brighter",
            "not too bright",
            "more blue in the shadows",
            "warm highlights, cool shadows",
            "rotate 90 degrees",
            "brightness +10",
            "bring up the blacks",
            "brighter and darker",
            "warmer but cooler",
            "a touch more vibrance but less saturation",
        ] {
            assert!(refused(t), "{t}: {:?}", RuleBasedParser.parse_detailed(t));
        }
        let r = RuleBasedParser.parse_detailed("a touch more vibrance but less saturation");
        assert_eq!(r.unrecognized.len(), 2);
        // Typographic input: curly apostrophe, em dash between clauses.
        assert!(refused("don\u{2019}t make it brighter"));
        let i = p("warmer \u{2014} a bit brighter");
        assert_eq!((i.warmth, i.exposure_ev), (25.0, 0.25));
        // "reduce" is not red.
        assert_eq!(p("reduce the shadows").color_bands, vec![]);
    }

    /// Complaints are corrected, never applied as asked.
    #[test]
    fn complaints_map_to_the_correction() {
        let band = |t: &str| p(t).color_bands;
        let sat = |band, s| {
            vec![BandEdit {
                band,
                saturation: s,
                lightness: 0.0,
            }]
        };
        assert_eq!(band("the sky looks washed out"), sat(ColorBand::Blue, 20.0));
        assert_eq!(band("the greens look dull"), sat(ColorBand::Green, 20.0));
        assert_eq!(band("the reds look garish"), sat(ColorBand::Red, -20.0));
        assert_eq!(band("mute the greens"), sat(ColorBand::Green, -20.0));
        assert_eq!(
            band("the sky is dark"),
            vec![BandEdit {
                band: ColorBand::Blue,
                saturation: 0.0,
                lightness: 20.0
            }]
        );
        let i = p("it looks washed out");
        assert_eq!((i.saturation, i.contrast), (20.0, 20.0));
        for t in [
            "looks dull",
            "less dull",
            "too dull",
            "the colours are muted",
            "the photo is faded",
        ] {
            assert_eq!(p(t).saturation, 20.0, "{t}");
        }
        for t in [
            "too saturated",
            "it's oversaturated",
            "the colours are too saturated",
        ] {
            assert_eq!(p(t).saturation, -20.0, "{t}");
        }
        assert_eq!(p("make it more muted").saturation, -20.0);
        assert_eq!(p("it looks flat").contrast, 20.0);
        assert_eq!(p("flatter").contrast, -20.0);
        for t in [
            "the image is dark",
            "it's underexposed",
            "it is underexposed",
            "looks dim",
        ] {
            assert_eq!(p(t).exposure_ev, 0.5, "{t}");
        }
        for t in [
            "it looks too bright",
            "it's overexposed",
            "the photo is bright",
        ] {
            assert_eq!(p(t).exposure_ev, -0.5, "{t}");
        }
        assert_eq!(p("make it look darker").exposure_ev, -0.5);
        assert_eq!(p("the shadows are dark").shadows, 25.0);
        assert_eq!(p("the highlights are too bright").highlights, 25.0);
        assert_eq!(p("the highlights are blown").highlights, 25.0);
        assert_eq!(p("it looks yellow").warmth, -25.0);
        assert_eq!(p("it's cold").warmth, 25.0);
        // A noun is not a complaint: "the exposure is down" isn't about an adjective.
        assert_eq!(p("exposure down").exposure_ev, -0.5);
    }

    /// A number counts only with a unit, a sign, "by" or a control word.
    #[test]
    fn numbers_need_a_unit_or_a_control() {
        assert_eq!(p("the 3 people are too dark").exposure_ev, 0.5);
        assert_eq!(p("brighter like the 2nd shot").exposure_ev, 0.5);
        assert_eq!(p("the 2 kids look too warm").warmth, -25.0);
        assert_eq!(p("exposure 2").exposure_ev, 2.0);
        assert_eq!(p("brighter by 2").exposure_ev, 2.0);
        assert_eq!(p("brighter +2").exposure_ev, 2.0);
        assert_eq!(p("2 stops brighter").exposure_ev, 2.0);
        assert_eq!(p("half a stop brighter").exposure_ev, 0.5);
        assert_eq!(p("contrast 30").contrast, 30.0);
        assert_eq!(p("30 contrast").contrast, 30.0);
        assert_eq!(p("warmer by 400k").temperature_shift_k, 400.0);
        assert_eq!(
            p("rotate right 1").straighten,
            Some(Straighten::Degrees(-1.0))
        );
        // A stray number in a rotation still has no unit or direction.
        assert!(rejected("rotate the 2 photos"));
    }

    #[test]
    fn number_parsing() {
        let n = |t: &str| parse_number(t, "");
        assert_eq!(
            n("+20"),
            Some(Num {
                value: 20.0,
                signed: true,
                unit: Unit::None
            })
        );
        assert_eq!(
            n("-0.5ev"),
            Some(Num {
                value: -0.5,
                signed: true,
                unit: Unit::Stops
            })
        );
        assert_eq!(
            n("30%"),
            Some(Num {
                value: 30.0,
                signed: false,
                unit: Unit::Percent
            })
        );
        assert_eq!(n("3200k").map(|n| n.unit), Some(Unit::Kelvin));
        assert_eq!(parse_number("2", "°").map(|n| n.unit), Some(Unit::Degrees));
        for bad in ["1/0", "x/2", "+", "warm", "1e39", "20zz"] {
            assert_eq!(n(bad), None, "{bad}");
        }
        assert_eq!(parse_ratio("4×5"), Some((4, 5)));
        assert_eq!(parse_ratio("1.5:1"), Some((3, 2)));
        assert_eq!(parse_ratio("a:5"), None);
    }

    #[test]
    fn phrases_replace_whole_words_only() {
        let r = |s: &str, from: &str, to: &str| replace_phrase(s, from, to);
        assert_eq!(r(" a lot more ", "a lot", " lot "), "  lot  more ");
        assert_eq!(r(" a lottery ", "a lot", " lot "), " a lottery ");
        assert_eq!(r(" open upstairs ", "open up", " lift "), " open upstairs ");
        assert_eq!(r(" half asleep ", "half as", " -1 stop "), " half asleep ");
        assert_eq!(r(" pull backs ", "pull back", " reduce "), " pull backs ");
        // A symbol edge needs no boundary: "auto-tone" is "auto tone".
        assert_eq!(r(" auto-tone ", "auto-", "auto "), " auto tone ");
        assert_eq!(r(" xauto-tone ", "auto-", "auto "), " xauto-tone ");
        // Repeats all go.
        assert_eq!(r(" b/w b/w ", "b/w", " bw "), "  bw   bw  ");
        // Through the parser: the phrase still works, the lookalike is not it.
        assert_eq!(p("make it black and white").black_and_white, Some(true));
        assert_eq!(p("brighter, like a lottery ticket").exposure_ev, 0.5);
        assert_eq!(p("a lot brighter").exposure_ev, 1.0);
    }

    #[test]
    fn a_huge_request_is_bounded() {
        let start = std::time::Instant::now();
        // Many clauses (conflict check) and many tokens (number scan).
        let many = "brighter, darker, ".repeat(200_000);
        let r = RuleBasedParser.parse_detailed(&many);
        let one_clause = format!("brighter {}", "the ".repeat(500_000));
        let _ = RuleBasedParser.parse_detailed(&one_clause);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        // Only the first MAX_REQUEST_CHARS are read: 2048 / 18 characters per
        // pair is at most 114 pairs.
        assert!(r.unrecognized.len() <= 2 * (MAX_REQUEST_CHARS / 18 + 1));
    }

    #[test]
    fn opposite_clauses_are_refused_in_one_pass() {
        // Three pushing up and one down on exposure: all four are refused;
        // contrast, pushed one way only, is kept.
        let r =
            RuleBasedParser.parse_detailed("brighter, brighter, brighter, darker, more contrast");
        assert_eq!(r.intent.exposure_ev, 0.0);
        assert_eq!(r.intent.contrast, 20.0);
        assert_eq!(r.unrecognized.len(), 4);
    }
}
