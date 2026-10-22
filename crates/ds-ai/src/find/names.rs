//! People and place vocabulary: fuzzy person matching against the known
//! people, and the small built-in word lists the parser uses to tell a name
//! or a place from a description.

use serde::{Deserialize, Serialize};

/// How a query term matched a known person (best first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonMatchKind {
    /// The whole name, or one of its words ("alex" → "Alex", "lee" → "Alex Lee").
    Exact,
    /// A nickname of the first name ("bob" → "Robert", "alex" ↔ "Alexander").
    Nickname,
    /// The start of the first name, three letters or more ("alexan" → "Alexandra").
    Prefix,
    /// One typo (two in long names): "alexnder" → "Alexander".
    Typo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonMatch {
    /// The known person's name as stored.
    pub name: String,
    pub kind: PersonMatchKind,
}

/// A combining mark (the accent in a decomposed "e" + U+0301). Folding drops
/// it, so "Mu\u{308}ller" and "Müller" read the same.
pub(super) fn is_combining(c: char) -> bool {
    matches!(
        c,
        '\u{300}'..='\u{36f}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}' | '\u{fe20}'..='\u{fe2f}'
    )
}

/// Lower case, common Latin accents removed, anything but letters and digits
/// a single space ("José-María" → "jose maria").
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        if is_combining(c) {
            continue;
        }
        let base = match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => "a",
            'æ' => "ae",
            'ç' | 'č' | 'ć' => "c",
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' => "e",
            'ì' | 'í' | 'î' | 'ï' | 'ī' => "i",
            'ñ' | 'ń' => "n",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => "o",
            'ù' | 'ú' | 'û' | 'ü' | 'ū' => "u",
            'ý' | 'ÿ' => "y",
            'ß' => "ss",
            'ł' => "l",
            'š' | 'ś' => "s",
            'ž' | 'ź' | 'ż' => "z",
            c if c.is_alphanumeric() => {
                out.push(c);
                continue;
            }
            _ => " ",
        };
        if base == " " {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push_str(base);
        }
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out
}

/// Nickname families: any member matches any other.
const NICKNAMES: &[&[&str]] = &[
    &["christopher", "chris", "topher", "kit"],
    &[
        "christine",
        "christina",
        "chris",
        "chrissy",
        "tina",
        "kristin",
        "kristen",
    ],
    &["robert", "rob", "bob", "bobby", "robbie", "bert"],
    &["william", "will", "bill", "billy", "liam", "willy"],
    &["michael", "mike", "mikey", "mick", "mickey"],
    &[
        "elizabeth",
        "liz",
        "lizzie",
        "beth",
        "betty",
        "betsy",
        "eliza",
        "lisa",
        "libby",
    ],
    &[
        "katherine",
        "catherine",
        "kathryn",
        "kate",
        "katie",
        "kathy",
        "cathy",
        "kat",
        "kay",
    ],
    &["james", "jim", "jimmy", "jamie"],
    &["thomas", "tom", "tommy"],
    &["david", "dave", "davey"],
    &["daniel", "dan", "danny"],
    &["joseph", "joe", "joey"],
    &["matthew", "matt", "matty"],
    &["nicholas", "nick", "nicky", "nico"],
    &["alexander", "alex", "al", "xander", "sasha"],
    &["alexandra", "alex", "alexa", "lexi", "sandra", "sasha"],
    &["samuel", "sam", "sammy"],
    &["samantha", "sam", "sammy"],
    &["benjamin", "ben", "benny"],
    &["anthony", "tony"],
    &["steven", "stephen", "steve", "stevie"],
    &["andrew", "andy", "drew"],
    &["jennifer", "jen", "jenny", "jenn"],
    &["rebecca", "becky", "becca"],
    &["margaret", "maggie", "meg", "peggy", "marge", "greta"],
    &["richard", "rich", "rick", "ricky", "dick"],
    &["edward", "ed", "eddie", "ted", "teddy", "ned"],
    &["charles", "charlie", "chuck", "chas"],
    &["charlotte", "charlie", "lottie"],
    &["henry", "hank", "harry"],
    &["john", "jack", "johnny", "jon"],
    &["jonathan", "jon", "jonny"],
    &["patrick", "pat", "paddy"],
    &["patricia", "pat", "patty", "trish"],
    &["susan", "sue", "susie", "suzy"],
    &["nathan", "nathaniel", "nate", "nat"],
    &["zachary", "zach", "zack"],
    &["joshua", "josh"],
    &["gregory", "greg"],
    &["jeffrey", "geoffrey", "jeff", "geoff"],
    &["kenneth", "ken", "kenny"],
    &["lawrence", "larry", "laurence"],
    &["ronald", "ron", "ronnie"],
    &["donald", "don", "donnie"],
    &["timothy", "tim", "timmy"],
    &["victoria", "vicky", "tori"],
    &["abigail", "abby", "abbie"],
    &["amanda", "mandy"],
    &["deborah", "debra", "debbie", "deb"],
    &["cynthia", "cindy"],
    &["frederick", "fred", "freddie"],
    &["gabriel", "gabe"],
    &["gabriella", "gabrielle", "gabby", "gabi"],
    &["isabella", "isabel", "izzy", "bella"],
    &["jacob", "jake"],
    &["jessica", "jess", "jessie"],
    &["joanna", "joanne", "jo"],
    &["leonard", "leo", "len", "lenny"],
    &["madeline", "madeleine", "maddie", "maddy"],
    &["nicole", "nikki", "nicky"],
    &["oliver", "ollie"],
    &["olivia", "liv", "livvy"],
    &["peter", "pete"],
    &["philip", "phillip", "phil"],
    &["raymond", "ray"],
    &["theodore", "theo", "ted", "teddy"],
    &["valerie", "val"],
    &["vincent", "vince", "vinny"],
    &["walter", "walt", "wally"],
    &["eleanor", "ellie", "nell", "nora"],
    &["emily", "em", "emmy"],
    &["sophia", "sophie"],
    &["stephanie", "steph"],
    &["mother", "mom", "mum", "mommy", "mummy", "mama"],
    &["father", "dad", "daddy", "papa"],
    &["grandmother", "grandma", "granny", "nana", "gran"],
    &["grandfather", "grandpa", "gramps", "grandad", "granddad"],
];

fn nicknames_match(a: &str, b: &str) -> bool {
    a != b && NICKNAMES.iter().any(|g| g.contains(&a) && g.contains(&b))
}

/// Optimal-string-alignment distance (a swap counts as one edit), or `max + 1`
/// once it exceeds `max`.
fn edit_distance(a: &str, b: &str, max: usize) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max {
        return max + 1;
    }
    let w = b.len() + 1;
    let mut d = vec![0usize; (a.len() + 1) * w];
    for (j, v) in d.iter_mut().take(w).enumerate() {
        *v = j;
    }
    for i in 1..=a.len() {
        d[i * w] = i;
        let mut row_min = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[(i - 1) * w + j] + 1)
                .min(d[i * w + j - 1] + 1)
                .min(d[(i - 1) * w + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[(i - 2) * w + j - 2] + 1);
            }
            d[i * w + j] = v;
            row_min = row_min.min(v);
        }
        if row_min > max {
            return max + 1;
        }
    }
    d[a.len() * w + b.len()].min(max + 1)
}

/// How `term` (one or more words, already folded) matches the folded `name`.
fn match_kind(term: &str, name: &str) -> Option<PersonMatchKind> {
    if term.is_empty() || name.is_empty() {
        return None;
    }
    if term == name {
        return Some(PersonMatchKind::Exact);
    }
    let words: Vec<&str> = name.split(' ').collect();
    let first = words[0];
    let tw: Vec<&str> = term.split(' ').collect();
    if tw.len() > 1 {
        // "alex lee" → "Alexander Lee": every word, in order, from the start.
        if tw.len() > words.len() {
            return None;
        }
        return tw
            .iter()
            .zip(&words)
            .map(|(t, w)| word_kind(t, w))
            .try_fold(PersonMatchKind::Exact, |acc, k| k.map(|k| acc.max(k)));
    }
    if words.contains(&term) {
        return Some(PersonMatchKind::Exact);
    }
    word_kind(term, first)
}

fn word_kind(t: &str, w: &str) -> Option<PersonMatchKind> {
    if t == w {
        return Some(PersonMatchKind::Exact);
    }
    if nicknames_match(t, w) {
        return Some(PersonMatchKind::Nickname);
    }
    if t.chars().count() >= 3 && w.starts_with(t) {
        return Some(PersonMatchKind::Prefix);
    }
    let (lt, lw) = (t.chars().count(), w.chars().count());
    let max = if lt >= 8 && lw >= 8 { 2 } else { 1 };
    (lt >= 5 && lw >= 5 && edit_distance(t, w, max) <= max).then_some(PersonMatchKind::Typo)
}

/// The known people `term` may mean, best match first (then by name). Case,
/// accents and punctuation are ignored.
pub fn match_people(term: &str, known: &[String]) -> Vec<PersonMatch> {
    let t = fold(term);
    let mut out: Vec<PersonMatch> = Vec::new();
    for name in known {
        if out.iter().any(|m| m.name == *name) {
            continue;
        }
        if let Some(kind) = match_kind(&t, &fold(name)) {
            out.push(PersonMatch {
                name: name.clone(),
                kind,
            });
        }
    }
    out.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    out
}

/// First names (and family words) taken as a person even when nobody by
/// that name is known yet. Words that are also common nouns, months or places
/// ("may", "rose", "georgia") are deliberately left out.
const FIRST_NAMES: &[&str] = &[
    "aaron",
    "abby",
    "adam",
    "adrian",
    "aidan",
    "alan",
    "albert",
    "alex",
    "alexander",
    "alexandra",
    "alice",
    "alicia",
    "allison",
    "amanda",
    "amber",
    "amelia",
    "amy",
    "ana",
    "andrea",
    "andrew",
    "andy",
    "angela",
    "anna",
    "anne",
    "anthony",
    "ava",
    "barbara",
    "ben",
    "benjamin",
    "beth",
    "bob",
    "brandon",
    "brendan",
    "brian",
    "brittany",
    "bruce",
    "caleb",
    "cameron",
    "carl",
    "carlos",
    "carol",
    "caroline",
    "catherine",
    "charles",
    "charlie",
    "charlotte",
    "chloe",
    "chris",
    "christian",
    "christina",
    "christine",
    "christopher",
    "claire",
    "cody",
    "colin",
    "connor",
    "craig",
    "dad",
    "daisy",
    "dan",
    "daniel",
    "danny",
    "dave",
    "david",
    "debbie",
    "dennis",
    "diana",
    "diego",
    "dylan",
    "edward",
    "elena",
    "eli",
    "elijah",
    "elizabeth",
    "ella",
    "ellie",
    "emily",
    "emma",
    "eric",
    "erica",
    "erin",
    "ethan",
    "eva",
    "evan",
    "fiona",
    "frank",
    "gabriel",
    "gary",
    "george",
    "grace",
    "grandma",
    "grandpa",
    "greg",
    "hannah",
    "harry",
    "heather",
    "helen",
    "henry",
    "ian",
    "isaac",
    "isabel",
    "isabella",
    "jack",
    "jacob",
    "jake",
    "james",
    "jamie",
    "jane",
    "jason",
    "jeff",
    "jen",
    "jennifer",
    "jenny",
    "jeremy",
    "jess",
    "jessica",
    "jim",
    "joe",
    "joel",
    "john",
    "jon",
    "jonathan",
    "jose",
    "joseph",
    "josh",
    "joshua",
    "juan",
    "julia",
    "julie",
    "justin",
    "karen",
    "kate",
    "katie",
    "kayla",
    "keith",
    "kelly",
    "ken",
    "kevin",
    "kim",
    "kyle",
    "laura",
    "lauren",
    "leah",
    "leo",
    "liam",
    "lily",
    "linda",
    "lisa",
    "liz",
    "logan",
    "lucas",
    "lucy",
    "luis",
    "luke",
    "maddie",
    "madison",
    "maria",
    "mark",
    "martin",
    "mary",
    "mason",
    "matt",
    "matthew",
    "max",
    "megan",
    "mia",
    "michael",
    "michelle",
    "mike",
    "mom",
    "mum",
    "nana",
    "natalie",
    "nathan",
    "nick",
    "nicole",
    "noah",
    "nora",
    "oliver",
    "olivia",
    "oscar",
    "owen",
    "pat",
    "patrick",
    "paul",
    "peter",
    "phil",
    "rachel",
    "rebecca",
    "richard",
    "rob",
    "robert",
    "ryan",
    "sam",
    "samantha",
    "samuel",
    "sara",
    "sarah",
    "scott",
    "sean",
    "sophia",
    "sophie",
    "stephanie",
    "steve",
    "steven",
    "susan",
    "taylor",
    "thomas",
    "tim",
    "timothy",
    "tom",
    "tony",
    "tyler",
    "victoria",
    "william",
    "zach",
    "zoe",
];

pub(super) fn is_first_name(w: &str) -> bool {
    FIRST_NAMES.binary_search(&w).is_ok()
}

/// Well-known places, recognized without "in": countries, US states, big
/// cities, islands, parks and regions.
const PLACES: &[&str] = &[
    "acadia",
    "africa",
    "alabama",
    "alaska",
    "amsterdam",
    "antarctica",
    "arches",
    "argentina",
    "arizona",
    "arkansas",
    "asia",
    "athens",
    "austin",
    "australia",
    "austria",
    "bahamas",
    "bali",
    "banff",
    "barcelona",
    "beijing",
    "belgium",
    "berlin",
    "big island",
    "big sur",
    "boston",
    "brazil",
    "brooklyn",
    "budapest",
    "cabo",
    "cairo",
    "california",
    "cambodia",
    "canada",
    "cancun",
    "cape cod",
    "cape town",
    "caribbean",
    "chicago",
    "chile",
    "china",
    "colombia",
    "colorado",
    "connecticut",
    "copenhagen",
    "costa rica",
    "crete",
    "croatia",
    "cuba",
    "czech republic",
    "delaware",
    "denmark",
    "denver",
    "dubai",
    "dublin",
    "ecuador",
    "edinburgh",
    "egypt",
    "england",
    "europe",
    "fiji",
    "finland",
    "florence",
    "florida",
    "france",
    "galapagos",
    "germany",
    "glacier",
    "grand canyon",
    "greece",
    "greenland",
    "hawaii",
    "hong kong",
    "iceland",
    "idaho",
    "illinois",
    "india",
    "indiana",
    "indonesia",
    "iowa",
    "ireland",
    "israel",
    "istanbul",
    "italy",
    "jamaica",
    "japan",
    "joshua tree",
    "kansas",
    "kauai",
    "kentucky",
    "kenya",
    "kyoto",
    "lake tahoe",
    "lanai",
    "las vegas",
    "lisbon",
    "london",
    "los angeles",
    "louisiana",
    "madrid",
    "maine",
    "malibu",
    "maryland",
    "massachusetts",
    "maui",
    "mexico",
    "miami",
    "michigan",
    "milan",
    "minnesota",
    "mississippi",
    "missouri",
    "molokai",
    "montana",
    "montreal",
    "morocco",
    "moscow",
    "munich",
    "napa",
    "nebraska",
    "nepal",
    "netherlands",
    "nevada",
    "new hampshire",
    "new jersey",
    "new mexico",
    "new orleans",
    "new york",
    "new zealand",
    "nice",
    "north carolina",
    "north dakota",
    "norway",
    "oahu",
    "ohio",
    "oklahoma",
    "oregon",
    "orlando",
    "paris",
    "patagonia",
    "pennsylvania",
    "peru",
    "philadelphia",
    "philippines",
    "poland",
    "portland",
    "portugal",
    "prague",
    "provence",
    "puerto rico",
    "quebec",
    "reykjavik",
    "rhode island",
    "rio",
    "rome",
    "san diego",
    "san francisco",
    "santorini",
    "scotland",
    "seattle",
    "seoul",
    "sicily",
    "singapore",
    "south africa",
    "south carolina",
    "south dakota",
    "spain",
    "sri lanka",
    "sweden",
    "switzerland",
    "sydney",
    "tahiti",
    "tahoe",
    "taiwan",
    "tanzania",
    "tennessee",
    "texas",
    "thailand",
    "tokyo",
    "toronto",
    "turkey",
    "tuscany",
    "uk",
    "usa",
    "utah",
    "vancouver",
    "venice",
    "vermont",
    "vienna",
    "vietnam",
    "virginia",
    "waikiki",
    "wales",
    "washington",
    "west virginia",
    "wisconsin",
    "wyoming",
    "yellowstone",
    "yosemite",
    "zion",
];

/// Longest built-in place (in words) starting at `words[i]`, as `(words used,
/// name)`.
pub(super) fn place_at(words: &[&str], i: usize) -> Option<(usize, String)> {
    longest_at(words, i, PLACES.iter().copied())
}

/// Longest of `phrases` (folded, words separated by one space) at `words[i]`.
pub(super) fn longest_at<'a>(
    words: &[&str],
    i: usize,
    phrases: impl Iterator<Item = &'a str>,
) -> Option<(usize, String)> {
    let mut best: Option<(usize, String)> = None;
    for p in phrases {
        let n = p.split(' ').count();
        if n == 0 || i + n > words.len() || best.as_ref().is_some_and(|b| b.0 >= n) {
            continue;
        }
        if p.split(' ').zip(&words[i..i + n]).all(|(a, b)| a == *b) {
            best = Some((n, p.to_owned()));
        }
    }
    best
}

/// Nouns that follow "in"/"at" as a description, not a place: "in the
/// snow" is caught by its article, "in snow", "at night" by this list.
const DESCRIPTIVE: &[&str] = &[
    "action",
    "background",
    "bed",
    "black",
    "blue",
    "bokeh",
    "color",
    "colour",
    "costume",
    "dark",
    "daylight",
    "dress",
    "focus",
    "fog",
    "foreground",
    "front",
    "golden",
    "green",
    "home",
    "line",
    "love",
    "low",
    "mist",
    "motion",
    "night",
    "pink",
    "purple",
    "rain",
    "red",
    "school",
    "shade",
    "shadow",
    "silhouette",
    "snow",
    "suit",
    "sun",
    "sunlight",
    "sunrise",
    "sunset",
    "sunshine",
    "twilight",
    "uniform",
    "water",
    "white",
    "work",
    "yellow",
];

pub(super) fn is_descriptive(w: &str) -> bool {
    DESCRIPTIVE.binary_search(&w).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(term: &str, known: &[&str]) -> Vec<(String, PersonMatchKind)> {
        let known: Vec<String> = known.iter().map(|s| (*s).to_owned()).collect();
        match_people(term, &known)
            .into_iter()
            .map(|m| (m.name, m.kind))
            .collect()
    }

    #[test]
    fn lists_are_sorted_and_folded() {
        for list in [FIRST_NAMES, DESCRIPTIVE] {
            assert!(
                list.windows(2).all(|w| w[0] < w[1]),
                "{:?}",
                list.windows(2).find(|w| w[0] >= w[1])
            );
        }
        for p in PLACES.iter().chain(FIRST_NAMES).chain(DESCRIPTIVE) {
            assert_eq!(fold(p), *p);
        }
        for g in NICKNAMES {
            for n in *g {
                assert_eq!(fold(n), *n);
            }
        }
        // Ambiguous words are not names.
        for w in [
            "may", "june", "rose", "georgia", "summer", "will", "bill", "art",
        ] {
            assert!(!is_first_name(w), "{w}");
        }
    }

    #[test]
    fn folding() {
        assert_eq!(fold("José-María  O'Neil"), "jose maria o neil");
        assert_eq!(fold("  Ærø, Straße!"), "aero strasse");
        assert_eq!(fold("Łódź Žižkov Ślęża"), "lodz zizkov sleza");
        assert_eq!(fold("ÇÉÌÑÒÙÝ ÿ ā"), "ceinouy y a");
        assert_eq!(fold("..."), "");
        assert_eq!(fold("Alex"), "alex");
        // Decomposed accents (what macOS file names and some keyboards give).
        assert_eq!(fold("Mu\u{308}ller"), "muller");
        assert_eq!(fold("Jose\u{301} Mari\u{301}a"), "jose maria");
        assert!(is_combining('\u{301}') && !is_combining('e'));
    }

    #[test]
    fn decomposed_names_match() {
        assert_eq!(
            names("mu\u{308}ller", &["Müller"]),
            [("Müller".into(), PersonMatchKind::Exact)]
        );
        assert_eq!(
            names("müller", &["Mu\u{308}ller"]),
            [("Mu\u{308}ller".into(), PersonMatchKind::Exact)]
        );
    }

    #[test]
    fn alex_matches_alex_and_alexander() {
        let known = ["Alexander", "Alex", "Alexandra", "Bob", "Alec"];
        assert_eq!(
            names("alex", &known),
            [
                ("Alex".into(), PersonMatchKind::Exact),
                ("Alexander".into(), PersonMatchKind::Nickname),
                ("Alexandra".into(), PersonMatchKind::Nickname),
            ]
        );
        assert_eq!(
            names("ALEX", &["Alexander"]),
            [("Alexander".into(), PersonMatchKind::Nickname)]
        );
        assert_eq!(
            names("alexan", &["Alexander"]),
            [("Alexander".into(), PersonMatchKind::Prefix)]
        );
        assert_eq!(names("al", &["Alexandra"]).len(), 0);
    }

    #[test]
    fn full_names_last_names_and_nicknames() {
        let known = ["Alexander Lee", "Robert Smith", "Bob", "Maria José"];
        assert_eq!(
            names("lee", &known),
            [("Alexander Lee".into(), PersonMatchKind::Exact)]
        );
        assert_eq!(
            names("bob", &known),
            [
                ("Bob".into(), PersonMatchKind::Exact),
                ("Robert Smith".into(), PersonMatchKind::Nickname)
            ]
        );
        assert_eq!(
            names("alex lee", &known),
            [("Alexander Lee".into(), PersonMatchKind::Nickname)]
        );
        assert_eq!(
            names("alexander lee", &known),
            [("Alexander Lee".into(), PersonMatchKind::Exact)]
        );
        assert_eq!(names("alex lee jr", &known), []);
        assert_eq!(names("alex smith", &known), []);
        assert_eq!(
            names("maria jose", &known),
            [("Maria José".into(), PersonMatchKind::Exact)]
        );
        assert_eq!(
            names("jose", &known),
            [("Maria José".into(), PersonMatchKind::Exact)]
        );
        assert_eq!(
            names("mom", &["Mother", "Mom"]),
            [
                ("Mom".into(), PersonMatchKind::Exact),
                ("Mother".into(), PersonMatchKind::Nickname)
            ]
        );
    }

    #[test]
    fn typos() {
        assert_eq!(
            names("alexnder", &["Alexander"]),
            [("Alexander".into(), PersonMatchKind::Typo)]
        );
        assert_eq!(
            names("alexnader", &["Alexander"]),
            [("Alexander".into(), PersonMatchKind::Typo)]
        );
        assert_eq!(
            names("sarha", &["Sarah"]),
            [("Sarah".into(), PersonMatchKind::Typo)]
        );
        // Short names need to be exact: "tim" isn't "tom".
        assert_eq!(names("tom", &["Tim"]).len(), 0);
        // Two edits only in long names.
        assert_eq!(names("sabra", &["Sarah"]).len(), 0);
        assert_eq!(names("aleksandr", &["Alexander"]).len(), 0);
        assert_eq!(
            names("alexandr", &["Alexander"]),
            [("Alexander".into(), PersonMatchKind::Typo)]
        );
    }

    #[test]
    fn edge_cases() {
        assert_eq!(names("", &["Alex"]).len(), 0);
        assert_eq!(names("alex", &["", "--"]).len(), 0);
        assert_eq!(names("alex", &[]).len(), 0);
        // Duplicates in the store are reported once.
        assert_eq!(names("alex", &["Alex", "Alex"]).len(), 1);
        assert_eq!(edit_distance("abc", "abcdef", 1), 2);
        assert_eq!(edit_distance("abcdef", "uvwxyz", 1), 2);
        assert_eq!(edit_distance("abcdef", "abdcef", 1), 1);
        assert_eq!(edit_distance("", "", 0), 0);
    }

    #[test]
    fn places_and_phrases() {
        let w = ["photos", "in", "new", "york", "city"];
        assert_eq!(place_at(&w, 2), Some((2, "new york".into())));
        assert_eq!(place_at(&w, 3), None);
        assert_eq!(place_at(&["new"], 0), None);
        let w = ["big", "island", "beach"];
        assert_eq!(place_at(&w, 0), Some((2, "big island".into())));
        let known = ["lake tahoe", "lake"];
        assert_eq!(
            longest_at(&["lake", "tahoe"], 0, known.iter().copied()),
            Some((2, "lake tahoe".into()))
        );
        assert_eq!(
            longest_at(&["lake", "x"], 0, known.iter().copied()),
            Some((1, "lake".into()))
        );
        assert!(is_descriptive("snow") && !is_descriptive("iceland"));
    }
}
