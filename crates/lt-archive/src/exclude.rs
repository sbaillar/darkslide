//! rsync's exclude-pattern rules, for the destinations that don't run rsync (object
//! storage): the same patterns ([`crate::target::DEFAULT_EXCLUDES`], the rejected
//! photos, `.lt/cache/`, …) keep the same files out.
//!
//! The subset rsync documents for `--exclude`: a leading `/` anchors the pattern at
//! the top of the transfer; a trailing `/` matches directories only; a pattern with
//! another `/` is matched against the whole path (unanchored: at any directory
//! boundary), one without against the last component; `*` matches within a
//! component, `**` across them, `?` one character, `[…]` a class, `\` escapes. An
//! excluded directory excludes everything below it.

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    glob: Vec<char>,
    anchored: bool,
    dir_only: bool,
    /// Matched against the whole path (it has a `/` inside).
    full_path: bool,
}

/// A list of rsync exclude patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Excludes {
    patterns: Vec<Pattern>,
}

impl Excludes {
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Self {
        let patterns = patterns
            .iter()
            .filter_map(|p| {
                let p = p.as_ref();
                let anchored = p.starts_with('/');
                let p = p.trim_start_matches('/');
                let dir_only = p.ends_with('/');
                let p = p.trim_end_matches('/');
                if p.is_empty() {
                    return None;
                }
                Some(Pattern {
                    glob: p.chars().collect(),
                    anchored,
                    dir_only,
                    full_path: p.contains('/'),
                })
            })
            .collect();
        Self { patterns }
    }

    /// Is `rel` (a path relative to the top, `/`-separated) excluded? Only the entry
    /// itself is tested; a walker doesn't descend into an excluded directory.
    pub fn excludes(&self, rel: &str, is_dir: bool) -> bool {
        self.patterns.iter().any(|p| {
            if p.dir_only && !is_dir {
                return false;
            }
            if p.anchored {
                return glob(&p.glob, &rel.chars().collect::<Vec<_>>());
            }
            if !p.full_path {
                let name = rel.rsplit('/').next().unwrap_or(rel);
                return glob(&p.glob, &name.chars().collect::<Vec<_>>());
            }
            // Unanchored with a `/`: the path, or any tail of it after a `/`.
            let chars: Vec<char> = rel.chars().collect();
            std::iter::once(0)
                .chain(
                    chars
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| **c == '/')
                        .map(|(i, _)| i + 1),
                )
                .any(|start| glob(&p.glob, &chars[start..]))
        })
    }
}

/// rsync wildcard match of the whole of `s`.
fn glob(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') if p.get(1) == Some(&'*') => {
            let rest = &p[2..];
            (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        Some('*') => {
            let rest = &p[1..];
            for i in 0..=s.len() {
                if glob(rest, &s[i..]) {
                    return true;
                }
                if s.get(i) == Some(&'/') {
                    return false;
                }
            }
            false
        }
        Some('?') => s.first().is_some_and(|c| *c != '/') && glob(&p[1..], &s[1..]),
        Some('[') => {
            let Some((matched, len)) = s.first().and_then(|c| class(&p[1..], *c)) else {
                return false;
            };
            matched && glob(&p[1 + len..], &s[1..])
        }
        Some('\\') if p.len() > 1 => s.first() == Some(&p[1]) && glob(&p[2..], &s[1..]),
        Some(c) => s.first() == Some(c) && glob(&p[1..], &s[1..]),
    }
}

/// Match `c` against a class body (after `[`): whether it matched, and how many
/// pattern characters the class used (through `]`). `None`: no closing `]`.
fn class(p: &[char], c: char) -> Option<(bool, usize)> {
    let (negate, mut i) = match p.first() {
        Some('!' | '^') => (true, 1),
        _ => (false, 0),
    };
    let mut hit = false;
    let mut first = true;
    while let Some(&x) = p.get(i) {
        if x == ']' && !first {
            return Some((hit != negate && c != '/', i + 1));
        }
        first = false;
        if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|e| *e != ']') {
            let hi = p[i + 2];
            hit |= (x..=hi).contains(&c);
            i += 3;
        } else {
            hit |= x == c;
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rsync_rules() {
        let e = Excludes::new(&[
            "*.ltsession",
            ".lt/*.lock",
            ".lt/cache/",
            "/.lt/index-snapshot.db",
            "*.partial",
            "/Trash/",
            "/Capture/a\\*b.ARW",
            ".lt-*.tmp",
            "",
            "/",
        ]);
        // Basename patterns at any depth.
        assert!(e.excludes("x.ltsession", false));
        assert!(e.excludes("Capture/deep/y.partial", false));
        assert!(!e.excludes("x.ltsession.bak", false));
        // A `/` inside: the whole path or a tail at a boundary; `*` stays in a component.
        assert!(e.excludes(".lt/index.lock", false));
        assert!(e.excludes("sub/.lt/index.lock", false));
        assert!(!e.excludes(".lt/sub/index.lock", false));
        assert!(!e.excludes("x.lt/index.lock", false));
        // Directory-only.
        assert!(e.excludes(".lt/cache", true));
        assert!(!e.excludes(".lt/cache", false));
        // Anchored.
        assert!(e.excludes(".lt/index-snapshot.db", false));
        assert!(!e.excludes("x/.lt/index-snapshot.db", false));
        assert!(e.excludes("Trash", true));
        assert!(!e.excludes("Capture/Trash", true));
        // Escapes: a literal `*`.
        assert!(e.excludes("Capture/a*b.ARW", false));
        assert!(!e.excludes("Capture/aXb.ARW", false));
        assert!(e.excludes(".lt-123.tmp", false));
        assert!(!Excludes::default().excludes("anything", false));
    }

    #[test]
    fn wildcards() {
        let m = |p: &str, s: &str| {
            glob(
                &p.chars().collect::<Vec<_>>(),
                &s.chars().collect::<Vec<_>>(),
            )
        };
        assert!(m("**/x", "a/b/x"));
        assert!(m("a/**", "a/b/c"));
        assert!(!m("a/*", "a/b/c"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "a/c"));
        assert!(m("[abc]x", "bx"));
        assert!(!m("[!abc]x", "bx"));
        assert!(m("[^abc]x", "dx"));
        assert!(m("[a-c]", "b"));
        assert!(!m("[a-c]", "d"));
        assert!(m("[]]", "]"));
        assert!(m("[a-]", "-"));
        assert!(!m("[abc", "a"));
        assert!(!m("[a]", "/"));
        assert!(!m("[a]", ""));
        assert!(m("\\", "\\"));
        assert!(!m("x", ""));
    }
}
