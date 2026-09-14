//! Pure PATH string operations: parsing, normalization, dedupe, reorder, diff.
//!
//! No I/O here -- every function is unit-testable without a registry.

use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashSet;

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

/// Canonical comparison form: quotes trimmed, `/` normalized to `\`,
/// trailing backslashes stripped, except at drive roots where the root
/// backslash is mandatory (`C:\`).
pub fn canon(s: &str) -> String {
    let t = trim_quotes(s);
    let t = if t.contains('/') {
        Cow::Owned(t.replace('/', "\\"))
    } else {
        Cow::Borrowed(t)
    };
    let t = t.trim_end_matches('\\');
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

/// Comparison key: the canonical form, case-folded. Comparing precomputed keys
/// instead of canonicalizing both sides on every comparison is what keeps the
/// duplicate scans linear rather than quadratic.
fn key(s: &str) -> String {
    canon(s).to_lowercase()
}

/// One `true` per entry that repeats an earlier entry (case-, separator- and
/// quote-insensitive); the first occurrence of each value is `false`.
pub fn duplicates(entries: &[String]) -> Vec<bool> {
    let mut seen: HashSet<String> = HashSet::with_capacity(entries.len());
    entries.iter().map(|e| !seen.insert(key(e))).collect()
}

/// Path equality: case-insensitive, with trailing-backslash, slash and quote
/// equivalence. Windows compares paths case-insensitively for the full Unicode
/// alphabet, not just ASCII, so use Unicode-aware folding.
pub fn eq(a: &str, b: &str) -> bool {
    key(a) == key(b)
}

/// Case-insensitive membership test.
pub fn contains(entries: &[String], entry: &str) -> bool {
    let needle = key(entry);
    entries.iter().any(|e| key(e) == needle)
}

/// Remove duplicates keeping the first occurrence (case-insensitive).
pub fn dedupe(entries: &[String]) -> Vec<String> {
    let dups = duplicates(entries);
    entries
        .iter()
        .zip(dups)
        .filter(|(_, dup)| !*dup)
        .map(|(e, _)| e.clone())
        .collect()
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
    // Report the offending index, `from` first: `max(from, to)` could blame
    // an in-range index (e.g. `move 0 1` reported index 1).
    if from == 0 || from > entries.len() {
        return Err(PathOpsError::OutOfRange {
            index: from,
            len: entries.len(),
        });
    }
    if to == 0 || to > entries.len() {
        return Err(PathOpsError::OutOfRange {
            index: to,
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
    Moved(String),
}

/// Order- and multiplicity-aware diff.
///
/// Entries are matched one-to-one (case-insensitive) so duplicates count:
/// `[a,a] -> [a]` reports one `Removed`. When nothing was added or removed
/// but the order changed (e.g. `move`), entries whose position changed are
/// reported as `Moved` so reorders and dry-runs are never silently empty.
pub fn diff_entries(before: &[String], after: &[String]) -> Vec<Change> {
    let mut used = vec![false; before.len()];
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut out = Vec::new();
    for (after_idx, e) in after.iter().enumerate() {
        if let Some(before_idx) = before
            .iter()
            .enumerate()
            .find(|(bi, b)| !used[*bi] && eq(b, e))
            .map(|(bi, _)| bi)
        {
            used[before_idx] = true;
            pairs.push((before_idx, after_idx));
        } else {
            out.push(Change::Added(e.clone()));
        }
    }
    for (bi, e) in before.iter().enumerate() {
        if !used[bi] {
            out.push(Change::Removed(e.clone()));
        }
    }
    // Pure reorder: no adds/removes, but positions differ.
    if out.is_empty() {
        for (bi, ai) in pairs {
            if bi != ai {
                out.push(Change::Moved(after[ai].clone()));
            }
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
        let v = dedupe(&[
            "C:\\a".into(),
            "c:\\A\\".into(),
            "C:\\b".into(),
            "C:\\a".into(),
        ]);
        assert_eq!(v, vec!["C:\\a", "C:\\b"]);
    }

    #[test]
    fn duplicates_flags_only_repeats() {
        let v: Vec<String> = vec![
            r"C:\a".into(),
            "c:/A/".into(),
            r"C:\b".into(),
            r"C:\a".into(),
        ];
        assert_eq!(duplicates(&v), vec![false, true, false, true]);
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
    fn reorder_reports_the_offending_index() {
        let mut v = vec!["a".to_string()];
        assert_eq!(
            reorder(&mut v, 0, 1).unwrap_err(),
            PathOpsError::OutOfRange { index: 0, len: 1 }
        );
        assert_eq!(
            reorder(&mut v, 1, 2).unwrap_err(),
            PathOpsError::OutOfRange { index: 2, len: 1 }
        );
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

    #[test]
    fn diff_entries_reports_pure_reorder_as_moved() {
        let before = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let after = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        let d = diff_entries(&before, &after);
        assert!(!d.is_empty(), "reorder must not diff empty");
        assert!(d.iter().all(|c| matches!(c, Change::Moved(_))));
    }

    #[test]
    fn diff_entries_counts_duplicates() {
        let before = vec!["a".to_string(), "a".to_string()];
        let after = vec!["a".to_string()];
        let d = diff_entries(&before, &after);
        assert_eq!(d, vec![Change::Removed("a".to_string())]);
    }

    #[test]
    fn eq_treats_slashes_as_equal() {
        assert!(eq(r"C:/tools", r"C:\tools"));
        assert!(eq(r"C:/", r"C:\"));
    }
}
