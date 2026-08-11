//! Pure PATH string operations: parsing, normalization, dedupe, reorder, diff.
//!
//! No I/O here — every function is unit-testable without a registry.

use serde::Serialize;

/// Split a PATH value into entries.
///
/// Entries are trimmed; empty/whitespace entries are dropped. A value of `""`
/// or `";"` yields no entries.
pub fn parse(value: &str) -> Vec<String> {
    value
        .split(';')
        .filter_map(|e| {
            let t = e.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        })
        .collect()
}

/// Trim surrounding double quotes (input hygiene; PATH entries are rarely quoted).
pub fn trim_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Canonical comparison form: quotes trimmed, trailing backslashes stripped,
/// except at drive roots where the root backslash is mandatory (`C:\`).
pub fn canon(s: &str) -> String {
    let t = trim_quotes(s).trim_end_matches('\\');
    if is_drive_root(t) {
        format!("{t}\\")
    } else {
        t.to_string()
    }
}

/// True for `C:` (the stripped form of drive root `C:\`).
fn is_drive_root(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Case-insensitive equality per the spec's OrdinalIgnoreCase rule,
/// with trailing-backslash equivalence. Windows path comparison is
/// case-insensitive for the full Unicode alphabet, not just ASCII
/// (e.g. Cyrillic paths), so use Unicode-aware folding.
pub fn eq(a: &str, b: &str) -> bool {
    canon(a).to_lowercase() == canon(b).to_lowercase()
}

/// Case-insensitive membership test.
pub fn contains(entries: &[String], entry: &str) -> bool {
    entries.iter().any(|e| eq(e, entry))
}

/// Remove duplicates keeping the first occurrence (case-insensitive).
pub fn dedupe(entries: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in entries {
        if !contains(&out, &e) {
            out.push(e);
        }
    }
    out
}

/// Remove the entry at 1-based `index`. Returns the removed entry.
pub fn remove_index(entries: &mut Vec<String>, index: usize) -> Result<String, PathOpsError> {
    if index == 0 || index > entries.len() {
        return Err(PathOpsError::OutOfRange {
            index,
            len: entries.len(),
        });
    }
    Ok(entries.remove(index - 1))
}

/// Move the entry at 1-based `from` to 1-based `to`.
pub fn reorder(entries: &mut Vec<String>, from: usize, to: usize) -> Result<(), PathOpsError> {
    if from == 0 || from > entries.len() || to == 0 || to > entries.len() {
        return Err(PathOpsError::OutOfRange {
            index: from.max(to),
            len: entries.len(),
        });
    }
    if from != to {
        let e = entries.remove(from - 1);
        entries.insert(to - 1, e);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathOpsError {
    #[error("index {index} out of range (1..={len})")]
    OutOfRange { index: usize, len: usize },
}

/// One entry-level change between two PATH states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added(String),
    Removed(String),
}

/// Set-style diff: entries only in `after` are Added, only in `before` Removed.
pub fn diff_entries(before: &[String], after: &[String]) -> Vec<Change> {
    let mut out = Vec::new();
    for e in after {
        if !contains(before, e) {
            out.push(Change::Added(e.clone()));
        }
    }
    for e in before {
        if !contains(after, e) {
            out.push(Change::Removed(e.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_and_trims() {
        assert_eq!(parse(r"C:\a;C:\b"), vec!["C:\\a", "C:\\b"]);
        assert_eq!(parse(r" C:\a ; ;C:\b;"), vec!["C:\\a", "C:\\b"]);
        assert_eq!(parse(""), Vec::<String>::new());
        assert_eq!(parse(";"), Vec::<String>::new());
    }

    #[test]
    fn canon_strips_trailing_backslash_but_keeps_drive_root() {
        assert_eq!(canon(r"C:\tools\"), r"C:\tools");
        assert_eq!(canon(r"C:\"), r"C:\");
        assert_eq!(canon(r"C:"), r"C:\");
        assert_eq!(canon(r"\\server\share\"), r"\\server\share");
    }

    #[test]
    fn canon_trims_quotes() {
        assert_eq!(canon(r#""C:\tools""#), r"C:\tools");
    }

    #[test]
    fn eq_is_case_insensitive_and_slash_insensitive() {
        assert!(eq(r"C:\Tools", r"c:\tools"));
        assert!(eq(r"C:\tools\", r"C:\tools"));
        assert!(eq(r"C:\", r"c:\"));
        assert!(!eq(r"C:\tools", r"C:\tool"));
    }

    #[test]
    fn dedupe_keeps_first() {
        let v = dedupe(vec![
            "C:\\a".into(),
            "c:\\A\\".into(),
            "C:\\b".into(),
            "C:\\a".into(),
        ]);
        assert_eq!(v, vec!["C:\\a", "C:\\b"]);
    }

    #[test]
    fn remove_index_is_one_based() {
        let mut v = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(remove_index(&mut v, 2).unwrap(), "b");
        assert_eq!(v, vec!["a", "c"]);
        assert!(remove_index(&mut v, 0).is_err());
        assert!(remove_index(&mut v, 3).is_err());
    }

    #[test]
    fn reorder_moves_entry() {
        let mut v = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        reorder(&mut v, 3, 1).unwrap();
        assert_eq!(v, vec!["c", "a", "b"]);
        let mut v = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        reorder(&mut v, 1, 3).unwrap();
        assert_eq!(v, vec!["b", "c", "a"]);
        assert!(reorder(&mut v, 0, 1).is_err());
        assert!(reorder(&mut v, 4, 1).is_err());
    }

    #[test]
    fn reorder_to_same_position_is_noop() {
        let mut v = vec!["a".to_string(), "b".to_string()];
        reorder(&mut v, 1, 1).unwrap();
        assert_eq!(v, vec!["a", "b"]);
    }

    #[test]
    fn diff_entries_reports_added_and_removed() {
        let before = vec!["C:\\a".to_string(), "C:\\b".to_string()];
        let after = vec!["C:\\b".to_string(), "C:\\c".to_string()];
        let d = diff_entries(&before, &after);
        assert!(d.contains(&Change::Added("C:\\c".to_string())));
        assert!(d.contains(&Change::Removed("C:\\a".to_string())));
        assert_eq!(d.len(), 2);
        assert!(diff_entries(&before, &before).is_empty());
    }
}
