//! The CLIP byte-level BPE text tokenizer, as `MobileCLIP` and `open_clip` use it.
//!
//! [`ClipTokenizer::encode`] reproduces `open_clip.tokenize(text)` (which wraps
//! CLIP's `SimpleTokenizer`): clean the text, lowercase it, split it with
//! CLIP's word pattern, byte-encode and BPE-merge each word, then frame the
//! ids as `<|startoftext|> … <|endoftext|>` zero-padded to
//! [`CONTEXT_LENGTH`], truncating (and keeping `<|endoftext|>` last) when the
//! text is too long.
//!
//! The vocabulary is derived from the Hugging Face `merges.txt` exactly as
//! `SimpleTokenizer.__init__` derives it: the 256 byte characters in
//! `bytes_to_unicode` order, each again with `</w>`, one entry per merge, then
//! the two special tokens (49 408 entries in all).
//!
//! # Differences from the Python reference
//!
//! The reference cleans text with `ftfy.fix_text` and `html.unescape`. This
//! module implements:
//!
//! - ftfy's HTML unescaping (only when the text contains no `<`, like ftfy's
//!   `"auto"` mode) followed by Python's two `html.unescape` passes. Numeric
//!   references (`&#39;`, `&#x27;`) follow Python's rules, except that
//!   `&#x80;`–`&#x9f;` are dropped instead of mapped through Windows-1252.
//!   Named references cover a common subset (`&amp;`, `&lt;`, `&gt;`,
//!   `&quot;`, `&apos;`, `&nbsp;`, typographic quotes and dashes, and common
//!   accented Latin letters); others are left as typed.
//! - ftfy's `uncurl_quotes` (`‘ ’ ‚ ‛ ʼ` → `'`, `“ ” „ ‟` → `"`), which matters
//!   because macOS text fields insert curly quotes by default.
//!
//! Omitted ftfy steps: NFC normalization (decomposed input such as `e` +
//! U+0301 tokenizes differently from precomposed `é`), mojibake repair,
//! Windows-1252 C1-control repair, Latin ligature and full-width character
//! folding, line-break and control-character cleanup, terminal-escape removal.
//!
//! The word splitter is hand-written: `\p{L}` is approximated by
//! [`char::is_alphabetic`] minus numeric characters, which also counts
//! `Other_Alphabetic` combining marks (for example Devanagari vowel signs) as
//! letters where the regex would not, so such scripts can tokenize
//! differently from the reference.

use std::collections::HashMap;

/// Number of ids every [`ClipTokenizer::encode`] result has.
pub const CONTEXT_LENGTH: usize = 77;
/// Id of `<|startoftext|>` in the real CLIP vocabulary.
pub const START_OF_TEXT: i32 = 49_406;
/// Id of `<|endoftext|>` in the real CLIP vocabulary.
pub const END_OF_TEXT: i32 = 49_407;
/// Characters of the query [`ClipTokenizer::encode`] reads. 77 tokens cannot
/// come from much more than this once whitespace and entities collapse.
pub const MAX_TEXT_CHARS: usize = 2048;
/// Merges in CLIP's `merges.txt`: 49 152 − 256 byte tokens − 2 specials.
pub const MERGE_COUNT: usize = 49_152 - 256 - 2;

const MAX_UNESCAPE_PASSES: usize = 3;
const SOT_STR: &str = "<|startoftext|>";
const EOT_STR: &str = "<|endoftext|>";
const END_OF_WORD: &str = "</w>";

/// Errors from [`ClipTokenizer::from_merges`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenizerError {
    #[error("merges file does not start with a `#version` header line")]
    MissingHeader,
    #[error("merges line {line} is not two space-separated symbols: {content:?}")]
    MalformedLine { line: usize, content: String },
    #[error("merges line {line} uses symbol {symbol:?} that is not in the vocabulary yet")]
    UnknownSymbol { line: usize, symbol: String },
    #[error("expected {expected} merges, found {found}")]
    WrongMergeCount { expected: usize, found: usize },
}

/// CLIP's BPE tokenizer. Build once with [`ClipTokenizer::from_merges`]; it
/// is immutable and `Send + Sync`.
#[derive(Debug, Clone)]
pub struct ClipTokenizer {
    /// Vocabulary id of each byte's character (the `</w>` form is `+ 256`).
    byte_ids: [i32; 256],
    /// `(left id, right id)` → `(rank, merged id)`.
    ranks: HashMap<(i32, i32), (u32, i32)>,
    sot: i32,
    eot: i32,
    /// Longest vocabulary token in bytes (without `</w>`): bounds how much of
    /// a word can matter for the first [`CONTEXT_LENGTH`] ids.
    max_token_bytes: usize,
}

impl ClipTokenizer {
    /// Builds the tokenizer from the text of CLIP's Hugging Face `merges.txt`
    /// (a `#version` header line, then exactly [`MERGE_COUNT`] merges).
    ///
    /// # Errors
    /// [`TokenizerError`] when the header is missing, a line is not two
    /// symbols, a merge refers to a symbol not yet in the vocabulary, or the
    /// merge count is not [`MERGE_COUNT`].
    pub fn from_merges(text: &str) -> Result<Self, TokenizerError> {
        Self::parse(text, MERGE_COUNT)
    }

    fn parse(text: &str, expected: usize) -> Result<Self, TokenizerError> {
        let mut lines = text.lines();
        if !lines.next().is_some_and(|l| l.starts_with("#version")) {
            return Err(TokenizerError::MissingHeader);
        }
        let mut merges = Vec::with_capacity(expected);
        for (i, line) in lines.enumerate() {
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split(' ');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(a), Some(b), None) if !a.is_empty() && !b.is_empty() => {
                    merges.push((i + 2, a, b));
                }
                _ => {
                    return Err(TokenizerError::MalformedLine {
                        line: i + 2,
                        content: line.to_owned(),
                    });
                }
            }
        }
        if merges.len() != expected {
            return Err(TokenizerError::WrongMergeCount {
                expected,
                found: merges.len(),
            });
        }

        // Vocabulary in SimpleTokenizer order; a repeated string keeps its
        // last id, like Python's `dict(zip(vocab, range(...)))`.
        let chars = byte_chars();
        let mut encoder: HashMap<String, i32> = HashMap::with_capacity(512 + merges.len() + 2);
        let mut next = 0;
        let mut add = |s: String| {
            encoder.insert(s, next);
            next += 1;
        };
        for (_, c) in &chars {
            add(c.to_string());
        }
        for (_, c) in &chars {
            add(format!("{c}{END_OF_WORD}"));
        }
        for (_, a, b) in &merges {
            add(format!("{a}{b}"));
        }
        add(SOT_STR.to_owned());
        add(EOT_STR.to_owned());

        let mut byte_ids = [0; 256];
        for (b, c) in &chars {
            byte_ids[usize::from(*b)] = encoder[&c.to_string()];
        }
        let lookup = |line: usize, symbol: &str| {
            encoder
                .get(symbol)
                .copied()
                .ok_or_else(|| TokenizerError::UnknownSymbol {
                    line,
                    symbol: symbol.to_owned(),
                })
        };
        let max_token_bytes = merges
            .iter()
            .map(|(_, a, b)| {
                let joined = format!("{a}{b}");
                joined
                    .strip_suffix(END_OF_WORD)
                    .unwrap_or(&joined)
                    .chars()
                    .count()
            })
            .fold(1, usize::max);
        let mut ranks = HashMap::with_capacity(merges.len());
        for (rank, (line, a, b)) in (0..).zip(&merges) {
            let merged = encoder[&format!("{a}{b}")];
            ranks.insert((lookup(*line, a)?, lookup(*line, b)?), (rank, merged));
        }
        Ok(Self {
            byte_ids,
            ranks,
            sot: encoder[SOT_STR],
            eot: encoder[EOT_STR],
            max_token_bytes,
        })
    }

    /// Tokenizes `text` into exactly [`CONTEXT_LENGTH`] ids:
    /// `<|startoftext|>`, the BPE tokens, `<|endoftext|>`, then zeros. Too
    /// long a text is truncated with `<|endoftext|>` kept as the last id.
    /// Only the first [`MAX_TEXT_CHARS`] characters are read: anything past
    /// that cannot fit the context, and cleaning and merging stay linear.
    #[must_use]
    pub fn encode(&self, text: &str) -> Vec<i32> {
        let end = text
            .char_indices()
            .nth(MAX_TEXT_CHARS)
            .map_or(text.len(), |(i, _)| i);
        let cleaned = clean(&text[..end]);
        // Room for the tokens before the final <|endoftext|>.
        let limit = CONTEXT_LENGTH - 1;
        let mut ids = Vec::with_capacity(CONTEXT_LENGTH + 16);
        ids.push(self.sot);
        for word in split_words(&cleaned) {
            if ids.len() >= limit {
                break;
            }
            match word {
                SOT_STR => ids.push(self.sot),
                EOT_STR => ids.push(self.eot),
                _ => self.bpe_into(word, limit - ids.len(), &mut ids),
            }
        }
        ids.truncate(limit);
        ids.push(self.eot);
        ids.resize(CONTEXT_LENGTH, 0);
        ids
    }

    /// Byte-encodes `word`, applies the lowest-rank merge until none applies
    /// (merging every non-overlapping occurrence left to right, as CLIP's
    /// `bpe` does), and appends at most `room` of the resulting ids. A word
    /// longer than `room` tokens can cover is cut first, so one huge word
    /// costs no more than a long sentence.
    fn bpe_into(&self, word: &str, room: usize, out: &mut Vec<i32>) {
        let reach = room.saturating_add(1).saturating_mul(self.max_token_bytes);
        let mut symbols: Vec<i32> = word
            .bytes()
            .take(reach)
            .map(|b| self.byte_ids[usize::from(b)])
            .collect();
        if symbols.len() == word.len()
            && let Some(last) = symbols.last_mut()
        {
            *last += 256;
        }
        let mut merged = Vec::with_capacity(symbols.len());
        while let Some((left, right, id)) = symbols
            .windows(2)
            .filter_map(|w| {
                self.ranks
                    .get(&(w[0], w[1]))
                    .map(|&(r, id)| (r, w[0], w[1], id))
            })
            .min_by_key(|t| t.0)
            .map(|(_, l, r, id)| (l, r, id))
        {
            merged.clear();
            let mut i = 0;
            while i < symbols.len() {
                if i + 1 < symbols.len() && symbols[i] == left && symbols[i + 1] == right {
                    merged.push(id);
                    i += 2;
                } else {
                    merged.push(symbols[i]);
                    i += 1;
                }
            }
            std::mem::swap(&mut symbols, &mut merged);
        }
        symbols.truncate(room);
        out.extend_from_slice(&symbols);
    }
}

/// CLIP's `bytes_to_unicode`: `(byte, char)` in vocabulary order — printable
/// Latin-1 bytes map to themselves, the rest to U+0100 upward.
fn byte_chars() -> Vec<(u8, char)> {
    let printable = |b: u8| matches!(b, b'!'..=b'~' | 0xA1..=0xAC | 0xAE..=0xFF);
    let mut out: Vec<(u8, char)> = (0..=255u8)
        .filter(|&b| printable(b))
        .map(|b| (b, char::from(b)))
        .collect();
    for (code, b) in (256u32..).zip((0..=255u8).filter(|&b| !printable(b))) {
        out.push((
            b,
            char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER),
        ));
    }
    out
}

/// `whitespace_clean(basic_clean(text)).lower()`, with the ftfy subset
/// described in the module docs.
fn clean(text: &str) -> String {
    let text = if text.contains('<') {
        uncurl_quotes(text)
    } else {
        // ftfy unescapes to a fixed point; three passes cover every real
        // input and bound the cost of nested `&amp;amp;...` blobs.
        let mut t = text.to_owned();
        for _ in 0..MAX_UNESCAPE_PASSES {
            let next = html_unescape(&t);
            if next == t {
                break;
            }
            t = next;
        }
        uncurl_quotes(&t)
    };
    let text = html_unescape(&html_unescape(&text));
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn uncurl_quotes(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{2bc}' | '\u{2018}'..='\u{201b}' => '\'',
            '\u{201c}'..='\u{201f}' => '"',
            c => c,
        })
        .collect()
}

/// Named references: `(name, replacement, also valid without ';')`.
const ENTITIES: &[(&str, &str, bool)] = &[
    ("amp", "&", true),
    ("lt", "<", true),
    ("gt", ">", true),
    ("quot", "\"", true),
    ("apos", "'", false),
    ("nbsp", "\u{a0}", true),
    ("copy", "©", true),
    ("reg", "®", true),
    ("deg", "°", true),
    ("times", "×", true),
    ("trade", "™", false),
    ("hellip", "…", false),
    ("ndash", "–", false),
    ("mdash", "—", false),
    ("lsquo", "‘", false),
    ("rsquo", "’", false),
    ("ldquo", "“", false),
    ("rdquo", "”", false),
    ("agrave", "à", true),
    ("aacute", "á", true),
    ("acirc", "â", true),
    ("auml", "ä", true),
    ("ccedil", "ç", true),
    ("egrave", "è", true),
    ("eacute", "é", true),
    ("ecirc", "ê", true),
    ("euml", "ë", true),
    ("iuml", "ï", true),
    ("ntilde", "ñ", true),
    ("ouml", "ö", true),
    ("uuml", "ü", true),
    ("szlig", "ß", true),
];

/// One pass of Python's `html.unescape` (see the module docs for the subset).
fn html_unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let (replacement, consumed) = if let Some(num) = after.strip_prefix('#') {
            numeric_reference(num).map_or((None, 0), |(s, n)| (Some(s), n + 1))
        } else {
            named_reference(after)
        };
        if let Some(s) = replacement {
            out.push_str(&s);
            rest = &after[consumed..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Parses the part after `&#`; returns the replacement and bytes consumed.
fn numeric_reference(s: &str) -> Option<(String, usize)> {
    let (digits_start, radix) = match s.as_bytes().first() {
        Some(b'x' | b'X') => (1, 16),
        _ => (0, 10),
    };
    let digits = s[digits_start..]
        .bytes()
        .take_while(|b| char::from(*b).is_digit(radix))
        .count();
    if digits == 0 {
        return None;
    }
    let end = digits_start + digits;
    let value = s[digits_start..end]
        .chars()
        .filter_map(|c| c.to_digit(radix))
        .fold(0u32, |acc, d| acc.saturating_mul(radix).saturating_add(d));
    let consumed = end + usize::from(s[end..].starts_with(';'));
    let replacement = match value {
        0 | 0xD800..=0xDFFF | 0x11_0000.. => "\u{fffd}".to_owned(),
        0x0D => "\r".to_owned(),
        0x01..=0x08 | 0x0B | 0x0E..=0x1F | 0x7F..=0x9F | 0xFDD0..=0xFDEF => String::new(),
        v if v & 0xFFFE == 0xFFFE => String::new(),
        v => char::from_u32(v).map(String::from).unwrap_or_default(),
    };
    Some((replacement, consumed))
}

/// Parses the part after `&` as a named reference; Python's rules: up to 32
/// name characters plus an optional `;`, exact match first, then the longest
/// semicolon-less legacy prefix (at least two characters).
fn named_reference(s: &str) -> (Option<String>, usize) {
    let name_len: usize = s
        .chars()
        .take_while(|c| !matches!(c, '\t' | '\n' | '\x0c' | ' ' | '<' | '&' | '#' | ';'))
        .take(32)
        .map(char::len_utf8)
        .sum();
    if name_len == 0 {
        return (None, 0);
    }
    let name = &s[..name_len];
    if s[name_len..].starts_with(';') {
        if let Some((_, v, _)) = ENTITIES.iter().find(|(n, _, _)| *n == name) {
            return (Some((*v).to_owned()), name_len + 1);
        }
    } else if let Some((_, v, _)) = ENTITIES.iter().find(|(n, _, legacy)| *legacy && *n == name) {
        return (Some((*v).to_owned()), name_len);
    }
    // Longest legacy prefix shorter than the whole match; the rest stays.
    let full = name_len + usize::from(s[name_len..].starts_with(';'));
    ENTITIES
        .iter()
        .filter(|(n, _, legacy)| *legacy && n.len() >= 2 && n.len() < full && name.starts_with(n))
        .max_by_key(|(n, _, _)| n.len())
        .map_or((None, 0), |(n, v, _)| (Some((*v).to_owned()), n.len()))
}

fn is_letter(c: char) -> bool {
    c.is_alphabetic() && !c.is_numeric()
}

/// CLIP's word pattern, applied to already-lowercased text:
/// `<\|startoftext\|>|<\|endoftext\|>|'s|'t|'re|'ve|'m|'ll|'d|[\p{L}]+|[\p{N}]|[^\s\p{L}\p{N}]+`.
fn split_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let run = |pred: fn(char) -> bool| {
            rest.char_indices()
                .find(|&(_, c)| !pred(c))
                .map_or(rest.len(), |(i, _)| i)
        };
        let len = if rest.starts_with(SOT_STR) {
            SOT_STR.len()
        } else if rest.starts_with(EOT_STR) {
            EOT_STR.len()
        } else if let Some(n) = contraction_len(rest) {
            n
        } else if is_letter(c) {
            run(is_letter)
        } else if c.is_numeric() {
            c.len_utf8()
        } else if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
            continue;
        } else {
            run(|c| !c.is_whitespace() && !is_letter(c) && !c.is_numeric())
        };
        words.push(&rest[..len]);
        rest = &rest[len..];
    }
    words
}

fn contraction_len(s: &str) -> Option<usize> {
    let tail = s.strip_prefix('\'')?;
    ["s", "t", "re", "ve", "m", "ll", "d"]
        .iter()
        .find(|suffix| tail.starts_with(*suffix))
        .map(|suffix| 1 + suffix.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny vocabulary: 512 byte tokens, then merges 512.., then SOT/EOT.
    const SMALL: &str = "#version: 0.2\nh e\nl l\nhe ll\no</w> '\ni n\nin g</w>\n";
    const SMALL_N: usize = 6;
    const SMALL_SOT: i32 = 512 + 6;
    const SMALL_EOT: i32 = 513 + 6;

    fn small() -> ClipTokenizer {
        ClipTokenizer::parse(SMALL, SMALL_N).expect("synthetic merges parse")
    }

    fn id(c: char) -> i32 {
        let b = u8::try_from(u32::from(c)).expect("ascii");
        small().byte_ids[usize::from(b)]
    }

    fn body(ids: &[i32]) -> Vec<i32> {
        assert_eq!(ids.len(), CONTEXT_LENGTH);
        assert_eq!(ids[0], SMALL_SOT);
        let end = ids
            .iter()
            .rposition(|&i| i == SMALL_EOT)
            .expect("eot present");
        assert!(ids[end + 1..].iter().all(|&i| i == 0), "zero padded");
        ids[1..end].to_vec()
    }

    #[test]
    fn byte_chars_match_clip_order() {
        let chars = byte_chars();
        assert_eq!(chars.len(), 256);
        assert_eq!(chars[0], (b'!', '!'));
        assert_eq!(chars[93], (b'~', '~'));
        assert_eq!(chars[94], (0xA1, '¡'));
        assert_eq!(chars[188], (0, '\u{100}'));
        assert_eq!(chars[188 + 32], (b' ', '\u{120}'));
        assert_eq!(chars[255], (0xAD, '\u{143}'));
        let mut bytes: Vec<u8> = chars.iter().map(|(b, _)| *b).collect();
        bytes.sort_unstable();
        assert_eq!(bytes, (0..=255).collect::<Vec<u8>>());
    }

    #[test]
    fn bpe_applies_merges_by_rank() {
        let t = small();
        // "hello" -> h e l l o</w> -> he ll o</w> -> hell o</w>.
        assert_eq!(body(&t.encode("hello")), vec![514, id('o') + 256]);
        // "hi" has no merge: h, i</w>.
        assert_eq!(body(&t.encode("Hi")), vec![id('h'), id('i') + 256]);
        // "sing" -> s i n g</w> -> s in g</w> -> s ing</w>.
        assert_eq!(body(&t.encode("sing")), vec![id('s'), 517]);
        // Single character words get </w>.
        assert_eq!(body(&t.encode("x")), vec![id('x') + 256]);
    }

    #[test]
    fn bpe_merges_every_occurrence_left_to_right() {
        let t = small();
        // "lll" -> ll, l</w> (overlap resolved leftmost).
        assert_eq!(body(&t.encode("lll")), vec![513, id('l') + 256]);
        // "llll" -> l l l l</w>: the third l cannot pair with l</w>.
        assert_eq!(body(&t.encode("llll")), vec![513, id('l'), id('l') + 256]);
    }

    #[test]
    fn encode_frames_and_pads() {
        let t = small();
        let ids = t.encode("");
        assert_eq!(&ids[..3], &[SMALL_SOT, SMALL_EOT, 0]);
        assert_eq!(ids.len(), CONTEXT_LENGTH);
        // Literal special tokens map to their ids.
        assert_eq!(
            body(&t.encode("x <|startoftext|><|endoftext|> x")),
            vec![id('x') + 256, SMALL_SOT, SMALL_EOT, id('x') + 256]
        );
    }

    #[test]
    fn encode_truncates_keeping_eot_last() {
        let t = small();
        let ids = t.encode(&"x ".repeat(200));
        assert_eq!(ids.len(), CONTEXT_LENGTH);
        assert_eq!(ids[0], SMALL_SOT);
        assert_eq!(ids[CONTEXT_LENGTH - 1], SMALL_EOT);
        assert!(
            ids[1..CONTEXT_LENGTH - 1]
                .iter()
                .all(|&i| i == id('x') + 256)
        );
        // A long single word that overflows inside one BPE call.
        let ids = t.encode(&"xy".repeat(100));
        assert_eq!(ids[CONTEXT_LENGTH - 1], SMALL_EOT);
        assert_eq!(ids[1], id('x'));
        assert_eq!(ids[CONTEXT_LENGTH - 2], id('x'));
    }

    #[test]
    fn bpe_stops_at_the_room_even_inside_one_word() {
        let t = small();
        let word = "x".repeat(100_000);
        let mut out = Vec::new();
        t.bpe_into(&word, 5, &mut out);
        assert_eq!(out, vec![id('x'); 5]);
        // A word that fits keeps its end-of-word marker.
        let mut out = Vec::new();
        t.bpe_into("xy", 5, &mut out);
        assert_eq!(out, vec![id('x'), id('y') + 256]);
    }

    #[test]
    fn encode_reads_a_bounded_prefix() {
        let t = small();
        let start = std::time::Instant::now();
        let ids = t.encode(&"x".repeat(5_000_000));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(ids[1], id('x'));
        assert_eq!(ids[CONTEXT_LENGTH - 1], SMALL_EOT);
        // Text past the cap is ignored, wherever the cut falls (here inside
        // a multi-byte character run).
        let head = "é".repeat(MAX_TEXT_CHARS);
        assert_eq!(
            t.encode(&format!("{head}tail")),
            t.encode(&format!("{head}other"))
        );
    }

    #[test]
    fn splitter_follows_clip_pattern() {
        assert_eq!(
            split_words("a photo of a dog!!"),
            ["a", "photo", "of", "a", "dog", "!!"]
        );
        assert_eq!(split_words("don't it's"), ["don", "'t", "it", "'s"]);
        assert_eq!(
            split_words("we're i've i'm we'll he'd 'x"),
            [
                "we", "'re", "i", "'ve", "i", "'m", "we", "'ll", "he", "'d", "'", "x"
            ]
        );
        assert_eq!(split_words("3pm 2024"), ["3", "pm", "2", "0", "2", "4"]);
        assert_eq!(split_words("!!'s"), ["!!'", "s"]);
        assert_eq!(split_words("!<|endoftext|>"), ["!<|", "endoftext", "|>"]);
        assert_eq!(split_words("café 東京"), ["café", "東京"]);
        assert_eq!(split_words("ⅻ"), ["ⅻ"]);
        assert_eq!(split_words("🌅🌊 x"), ["🌅🌊", "x"]);
        assert_eq!(split_words("   ").len(), 0);
    }

    #[test]
    fn clean_matches_basic_and_whitespace_clean() {
        assert_eq!(clean("  Multiple \t\n Spaces  "), "multiple spaces");
        assert_eq!(
            clean("Nemo\u{2019}s \u{201c}Fish\u{201d}"),
            "nemo's \"fish\""
        );
        // ftfy unescapes repeatedly when there is no '<'.
        assert_eq!(clean("&amp;amp;amp; x"), "& x");
        // Unescaping stops after three passes, then Python's two: five levels
        // of nesting go, the other five stay.
        assert_eq!(
            clean(&format!("&{}", "amp;".repeat(10))),
            format!("&{}", "amp;".repeat(5))
        );
        // With '<' only Python's two passes run.
        assert_eq!(clean("<b> &amp;amp;amp;"), "<b> &amp;");
        assert_eq!(clean("&rsquo;s"), "'s");
    }

    #[test]
    fn html_unescape_numeric() {
        assert_eq!(html_unescape("&#39;x&#x27;&#X41"), "'x'A");
        assert_eq!(
            html_unescape("&#0;&#xD800;&#99999999999;"),
            "\u{fffd}\u{fffd}\u{fffd}"
        );
        assert_eq!(html_unescape("&#1;&#x80;&#xFFFF;&#xfdd0;"), "");
        assert_eq!(html_unescape("&#13;"), "\r");
        assert_eq!(html_unescape("&#; &#x; &#"), "&#; &#x; &#");
    }

    #[test]
    fn html_unescape_named() {
        assert_eq!(html_unescape("&amp; &lt;b&gt; &quot;"), "& <b> \"");
        assert_eq!(html_unescape("&apos; &eacute;t&eacute;"), "' été");
        // Legacy names work without ';'; others do not.
        assert_eq!(html_unescape("&amp &copy2 &apos"), "& ©2 &apos");
        // Longest legacy prefix, rest kept.
        assert_eq!(
            html_unescape("&ampx; &notanentity; &"),
            "&x; &notanentity; &"
        );
        assert_eq!(html_unescape("& ; &;"), "& ; &;");
        assert_eq!(html_unescape("plain"), "plain");
    }

    #[test]
    fn parse_errors() {
        assert_eq!(
            ClipTokenizer::parse("h e\n", 1).unwrap_err(),
            TokenizerError::MissingHeader
        );
        assert_eq!(
            ClipTokenizer::parse("", 0).unwrap_err(),
            TokenizerError::MissingHeader
        );
        assert_eq!(
            ClipTokenizer::parse("#version\nh e\nabc\n", 2).unwrap_err(),
            TokenizerError::MalformedLine {
                line: 3,
                content: "abc".into()
            }
        );
        assert_eq!(
            ClipTokenizer::parse("#version\na b c\n", 1).unwrap_err(),
            TokenizerError::MalformedLine {
                line: 2,
                content: "a b c".into()
            }
        );
        assert_eq!(
            ClipTokenizer::parse("#version\nh  e\n", 1).unwrap_err(),
            TokenizerError::MalformedLine {
                line: 2,
                content: "h  e".into()
            }
        );
        assert_eq!(
            ClipTokenizer::parse("#version\nzz q\n", 1).unwrap_err(),
            TokenizerError::UnknownSymbol {
                line: 2,
                symbol: "zz".into()
            }
        );
        assert_eq!(
            ClipTokenizer::parse(SMALL, 7).unwrap_err(),
            TokenizerError::WrongMergeCount {
                expected: 7,
                found: 6
            }
        );
        assert_eq!(
            ClipTokenizer::from_merges(SMALL).unwrap_err(),
            TokenizerError::WrongMergeCount {
                expected: MERGE_COUNT,
                found: 6
            }
        );
        assert!(
            TokenizerError::MissingHeader
                .to_string()
                .contains("#version")
        );
    }

    #[test]
    fn duplicate_merge_keeps_last_rank_and_id() {
        // "l l" appears twice (blank lines are skipped): Python keeps the later
        // rank and the later id.
        let t = ClipTokenizer::parse("#version\nl l\n\nh e\nl l\n", 3).expect("parses");
        assert_eq!(t.ranks[&(id('l'), id('l'))], (2, 514));
        assert_eq!((t.sot, t.eot), (515, 516));
    }

    /// Golden ids from the reference `simple_tokenizer.py` + `open_clip`-style
    /// truncation, without the trailing zero padding. Needs the real
    /// `merges.txt` in `LT_CLIP_MERGES`; skipped otherwise.
    #[test]
    fn golden_against_reference() {
        let Ok(path) = std::env::var("LT_CLIP_MERGES") else {
            eprintln!("LT_CLIP_MERGES unset; skipping CLIP golden test");
            return;
        };
        let text = std::fs::read_to_string(&path).expect("read merges");
        let start = std::time::Instant::now();
        let t = ClipTokenizer::from_merges(&text).expect("real merges parse");
        eprintln!("from_merges: {:?}", start.elapsed());
        assert_eq!((t.sot, t.eot), (START_OF_TEXT, END_OF_TEXT));

        for (text, want) in &golden_cases() {
            let got = t.encode(text);
            assert_eq!(got.len(), CONTEXT_LENGTH, "{text:?}");
            assert_eq!(&got[..want.len()], want.as_slice(), "{text:?}");
            assert!(
                got[want.len()..].iter().all(|&i| i == 0),
                "{text:?} padding"
            );
        }

        let start = std::time::Instant::now();
        let n = 1000;
        for _ in 0..n {
            std::hint::black_box(t.encode(std::hint::black_box("kids on the beach at sunset")));
        }
        eprintln!("encode short query: {:?} each", start.elapsed() / n);
    }

    /// `(text, unpadded ids)` produced by the Python reference.
    fn golden_cases() -> Vec<(String, Vec<i32>)> {
        let fox = "the quick brown fox jumps over the lazy dog ".repeat(12);
        let long: String = fox.split(' ').take(100).collect::<Vec<_>>().join(" ");
        let long_ids: Vec<i32> = std::iter::once(START_OF_TEXT)
            .chain(
                [518, 3712, 2866, 3240, 18911, 962, 518, 10753, 1929]
                    .into_iter()
                    .cycle()
                    .take(75),
            )
            .chain(std::iter::once(END_OF_TEXT))
            .collect();
        let cases: Vec<(&str, Vec<i32>)> = vec![
            (
                "kids on the beach at sunset",
                vec![49406, 1911, 525, 518, 2117, 536, 3424, 49407],
            ),
            (
                "waterfall with people",
                vec![49406, 16403, 593, 1047, 49407],
            ),
            ("red car", vec![49406, 736, 1615, 49407]),
            (
                "A Photo of a DOG!!",
                vec![49406, 320, 1125, 539, 320, 1929, 748, 49407],
            ),
            ("don't", vec![49406, 847, 713, 49407]),
            ("it's 3pm", vec![49406, 585, 568, 274, 990, 49407]),
            ("café crème", vec![49406, 15304, 1075, 12138, 614, 49407]),
            ("  multiple   spaces ", vec![49406, 6470, 9006, 49407]),
            ("&amp; html", vec![49406, 261, 18231, 49407]),
            (
                "&amp;amp; &lt;b&gt; &#39;x&#x27; &eacute;",
                vec![49406, 261, 283, 321, 285, 262, 343, 262, 4166, 49407],
            ),
            (
                "sunset \u{1f305} over the sea \u{1f30a}",
                vec![49406, 3424, 27547, 962, 518, 2102, 14302, 49407],
            ),
            (
                "東京の夜景",
                vec![
                    49406, 48338, 21078, 105, 21575, 23170, 250, 48132, 363, 49407,
                ],
            ),
            (
                "日本語 한국어 中文",
                vec![
                    49406, 39121, 44353, 34002, 508, 15197, 250, 31871, 255, 31625, 368, 19759,
                    255, 29032, 485, 49407,
                ],
            ),
            ("", vec![49406, 49407]),
            (
                "!!'s <|endoftext|> mid",
                vec![49406, 1443, 262, 338, 49407, 4734, 49407],
            ),
            (
                "2024-06-01 photos",
                vec![
                    49406, 273, 271, 273, 275, 268, 271, 277, 268, 271, 272, 2479, 49407,
                ],
            ),
            (
                "naïve façade Ünïcödé",
                vec![
                    49406, 1097, 35689, 563, 778, 10067, 1928, 6522, 77, 35689, 66, 7255, 67, 4166,
                    49407,
                ],
            ),
            ("THE MATRIX", vec![49406, 518, 18078, 49407]),
            (
                "hello\tworld\nnew line",
                vec![49406, 3306, 1002, 686, 1148, 49407],
            ),
            (
                "nemo\u{2019}s fish \u{201c}quoted\u{201d}",
                vec![49406, 30441, 568, 2759, 257, 27706, 257, 49407],
            ),
            ("\u{216b} roman", vec![49406, 158, 227, 375, 7370, 49407]),
            (
                "x2 1234567",
                vec![49406, 343, 273, 272, 273, 274, 275, 276, 277, 278, 49407],
            ),
            ("it\u{2bc}s", vec![49406, 585, 568, 49407]),
        ];
        cases
            .into_iter()
            .map(|(t, ids)| (t.to_owned(), ids))
            .chain(std::iter::once((long, long_ids)))
            .collect()
    }
}
