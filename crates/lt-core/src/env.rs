//! Environment variables: `LT_*`. The `DS_*` spelling they had before the rename
//! is still accepted for one release, so a CI configuration or a habit doesn't
//! break silently; `LT_*` wins when both are set.

use std::ffi::OsString;

/// Migration source: the prefix the variables had before the rename.
pub const LEGACY_PREFIX: &str = "DS_";

/// `name` (`LT_…`) from the process environment, else its old `DS_…` spelling.
#[must_use]
pub fn var_os(name: &str) -> Option<OsString> {
    lookup(name, |n| std::env::var_os(n))
}

/// [`var_os`] as UTF-8 (`None` when unset or not UTF-8).
#[must_use]
pub fn var(name: &str) -> Option<String> {
    var_os(name).and_then(|v| v.into_string().ok())
}

/// `name` through `get`, falling back to the old spelling of an `LT_` name.
fn lookup(name: &str, get: impl Fn(&str) -> Option<OsString>) -> Option<OsString> {
    get(name).or_else(|| {
        let rest = name.strip_prefix("LT_")?;
        get(&format!("{LEGACY_PREFIX}{rest}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_name_wins_and_the_old_one_is_a_fallback() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |n: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, v)| OsString::from(*v))
            }
        };
        let both = env(&[("LT_X", "new"), ("DS_X", "old")]);
        assert_eq!(lookup("LT_X", both), Some("new".into()));
        assert_eq!(lookup("LT_X", env(&[("DS_X", "old")])), Some("old".into()));
        assert_eq!(lookup("OTHER", env(&[("DS_OTHER", "old")])), None);
        assert_eq!(lookup("LT_Y", env(&[])), None);
        // The process environment: something always set, something never set.
        assert!(var_os("PATH").is_some() && var("PATH").is_some());
        assert_eq!(var("LT_NEVER_SET_IN_ANY_TEST_RUN"), None);
    }
}
