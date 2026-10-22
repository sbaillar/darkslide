//! Text a plug-in controls (its name, summary, labels, error messages) before the host
//! shows it: one line, no control or bidirectional-override characters, capped. The
//! app shows it only quoted inside its own sentences ("“Film Border” says: …"), never
//! as a prompt or a sheet title.

/// Caps, in characters.
pub const NAME_MAX: usize = 40;
pub const LABEL_MAX: usize = 40;
pub const SUMMARY_MAX: usize = 200;
pub const MESSAGE_MAX: usize = 300;

/// Bidi controls and invisible format characters that can reorder or hide text.
fn hidden(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{061C}')
}

/// `s` as one clean line of at most `max` characters (an ellipsis marks a cut).
#[must_use]
pub fn clean(s: &str, max: usize) -> String {
    let one: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !hidden(*c))
        .collect();
    let one = one.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        let mut cut: String = one.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

/// Names only Darkslide's own plug-ins may use (they would pass for the app).
#[must_use]
pub fn reserved_name(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        "darkslide",
        "built-in",
        "builtin",
        "apple",
        "macos",
        "system",
    ]
    .iter()
    .any(|r| n.contains(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_and_caps() {
        assert_eq!(clean("  Film\nBorder\t ", 40), "Film Border");
        assert_eq!(clean("a\u{202E}gnp.exe", 40), "agnp.exe");
        assert_eq!(clean("abcdef", 4), "abc…");
        assert_eq!(clean("\u{200B}", 4), "");
    }

    #[test]
    fn reserved_names() {
        assert!(reserved_name("Darkslide Helper"));
        assert!(reserved_name("BUILT-IN looks"));
        assert!(!reserved_name("Film Border"));
    }
}
