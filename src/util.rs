//! Small Win32 helpers: variable expansion and length limits.

/// Max size of a user-defined environment variable (Microsoft docs:
/// GetEnvironmentVariable / CreateEnvironmentBlock limit -- not a registry
/// limit; the registry value-data limit is ~1 MB).
pub const MAX_ENV_VALUE: usize = 32_767;

/// Practical warning threshold for combined PATH length (command-line pain
/// starts around 2,048 chars).
pub const WARN_PATH_LEN: usize = 2_048;

/// PATH entry length worth surfacing by `check` (long-path support varies).
pub const LONG_PATH_FLAG: usize = 260;

/// Length in UTF-16 code units: what the registry stores, and what Windows
/// counts against its environment-variable limit. Every length limit in
/// pathctl is measured this way, so the numbers agree with each other.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// How deep a replacement may itself be expanded. Windows resolves a variable
/// whose value holds another reference; the bound stops a self-referential
/// value (`A=%A%`) from looping forever.
const MAX_EXPAND_DEPTH: usize = 16;

/// Expand every `%NAME%` reference in `s` by asking `lookup`. The name is
/// passed without its `%`; `lookup` is case-insensitive and returns `None` for
/// a name it does not know, which makes the whole expansion `None`
/// (unresolvable) so callers keep the entry instead of pruning it.
///
/// Not-a-reference forms (`%%`, `%A\B%`, a lone `%`) are left literal. A
/// replacement that itself holds a reference is expanded too, like the
/// environment a new process inherits.
pub fn expand_from(s: &str, lookup: &impl Fn(&str) -> Option<String>) -> Option<String> {
    expand_rec(s, lookup, 0)
}

fn expand_rec(s: &str, lookup: &impl Fn(&str) -> Option<String>, depth: usize) -> Option<String> {
    if depth > MAX_EXPAND_DEPTH || !s.contains('%') {
        return Some(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('%') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('%') else {
            // No `%` left to close anything: the remainder is literal.
            out.push('%');
            out.push_str(after);
            return Some(out);
        };
        let name = &after[..close];
        if !is_reference_name(name) {
            // Not a reference; keep the `%` and rescan from just after it so
            // its closing `%` can open the next pair (as in `%%A%`).
            out.push('%');
            rest = after;
            continue;
        }
        let value = lookup(name)?;
        out.push_str(&expand_rec(&value, lookup, depth + 1)?);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// True if `name` (the text between two `%`) is a variable reference. A lone
/// `%` (e.g. `C:\100%_coverage`) is not a reference, and neither is `%A\B%`:
/// variable names never contain path separators or `;`.
fn is_reference_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['\\', '/', ';'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn expand_passthrough_without_vars() {
        let l = lookup(vec![]);
        assert_eq!(expand_from(r"C:\tools", &l), Some(r"C:\tools".to_string()));
        assert_eq!(expand_from("", &l), Some(String::new()));
    }

    #[test]
    fn expand_resolves_known_var() {
        let l = lookup(vec![("PATHCTL_EXPAND_TEST", r"C:\resolved")]);
        assert_eq!(
            expand_from("%PATHCTL_EXPAND_TEST%", &l),
            Some(r"C:\resolved".to_string())
        );
        assert_eq!(
            expand_from(r"%PATHCTL_EXPAND_TEST%\tail", &l),
            Some(r"C:\resolved\tail".to_string())
        );
    }

    #[test]
    fn expand_is_case_insensitive() {
        let l = lookup(vec![("path", r"C:\bin")]);
        assert_eq!(expand_from("%PATH%", &l), Some(r"C:\bin".to_string()));
    }

    #[test]
    fn expand_unknown_var_is_none() {
        let l = lookup(vec![("Known", "v")]);
        assert_eq!(expand_from("%Missing%", &l), None);
        assert_eq!(expand_from(r"%Known%\%Missing%", &l), None);
    }

    #[test]
    fn expand_does_not_recurse_forever() {
        let l = lookup(vec![("A", "%A%")]);
        assert!(expand_from("%A%", &l).is_some());
    }

    #[test]
    fn expand_leaves_non_references_literal() {
        let l = lookup(vec![]);
        assert_eq!(
            expand_from(r"C:\100%_coverage", &l),
            Some(r"C:\100%_coverage".to_string())
        );
        assert_eq!(expand_from("%A\\B%", &l), Some("%A\\B%".to_string()));
        assert_eq!(expand_from("%%", &l), Some("%%".to_string()));
        assert_eq!(expand_from("%%A%", &l), None);
    }

    #[test]
    fn reference_names_exclude_separators() {
        assert!(is_reference_name("SystemRoot"));
        assert!(!is_reference_name(""));
        assert!(!is_reference_name("A\\B"));
        assert!(!is_reference_name("a;b"));
    }
}
