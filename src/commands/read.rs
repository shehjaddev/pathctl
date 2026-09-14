//! `list` and `check`: read-only PATH analysis.

use super::*;

/// Why `list` flagged an entry. Serialized as a stable string for `--json`,
/// and mapped to the one-character marker the human output prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum EntryFlag {
    /// A `%VAR%` reference was expanded.
    Expanded,
    /// A `%VAR%` reference did not resolve.
    Unresolvable,
    /// The directory does not exist.
    Missing,
    /// An earlier entry points at the same directory.
    Dup,
}

impl EntryFlag {
    fn marker(self) -> char {
        match self {
            EntryFlag::Expanded => 'e',
            EntryFlag::Unresolvable => '%',
            EntryFlag::Missing => '!',
            EntryFlag::Dup => 'd',
        }
    }
}

#[derive(Serialize)]
struct EntryOut {
    index: usize,
    entry: String,
    flags: Vec<EntryFlag>,
}

#[derive(Serialize)]
struct ScopeOut {
    scope: &'static str,
    entries: Vec<EntryOut>,
}

pub fn list(reg: &Registry, g: &Global, scopes: &[Scope], raw: bool) -> Result<u8> {
    let mut out = Vec::new();
    for scope in scopes {
        let entries = match reg.read_path(*scope)? {
            Some(v) => pathops::parse(&v.raw),
            None => Vec::new(),
        };
        let mut rows = Vec::new();
        let dups = pathops::duplicates(&entries);
        for (i, entry) in entries.iter().enumerate() {
            // Analysis always runs on the expanded form: in `--raw` mode only
            // the `expanded` flag is suppressed, so a resolvable `%VAR%` entry
            // is not misreported as missing.
            let analysis = analyze_entry(entry);
            let mut flags = Vec::new();
            if !raw && analysis.expanded {
                flags.push(EntryFlag::Expanded);
            }
            if analysis.unresolvable {
                flags.push(EntryFlag::Unresolvable);
            }
            if analysis.missing {
                flags.push(EntryFlag::Missing);
            }
            if dups[i] {
                flags.push(EntryFlag::Dup);
            }
            rows.push(EntryOut {
                index: i + 1,
                entry: entry.clone(),
                flags,
            });
        }
        out.push(ScopeOut {
            scope: scope.label(),
            entries: rows,
        });
    }

    if g.json {
        println!("{}", serde_json::to_string(&out).expect("serialize"));
    } else {
        for so in &out {
            if scopes.len() > 1 {
                println!("{} PATH ({} entries):", so.scope, so.entries.len());
            }
            for e in &so.entries {
                let flags: String = e.flags.iter().map(|f| f.marker()).collect();
                let suffix = if flags.is_empty() {
                    String::new()
                } else {
                    format!("  [{flags}]")
                };
                println!("{:>3}. {}{}", e.index, e.entry, suffix);
            }
        }
    }
    Ok(0)
}

/// What `check` found. Serialized with a `kind` tag so a script can act on a
/// finding without parsing the sentence the human output prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Finding {
    /// The same entry appears more than once.
    Duplicate { scope: &'static str, entry: String },
    /// A `%VAR%` reference did not resolve.
    Unresolvable { scope: &'static str, entry: String },
    /// The directory does not exist.
    Missing { scope: &'static str, entry: String },
    /// The entry is longer than `util::LONG_PATH_FLAG`.
    EntryTooLong {
        scope: &'static str,
        entry: String,
        limit: usize,
    },
    /// The whole value is over what an environment variable may hold.
    ValueOverLimit {
        scope: &'static str,
        units: usize,
        limit: usize,
    },
    /// The whole value is approaching that limit.
    ValueNearLimit {
        scope: &'static str,
        units: usize,
        limit: usize,
    },
}

impl Finding {
    /// The line `check` prints for people.
    fn describe(&self) -> String {
        match self {
            Finding::Duplicate { scope, entry } => {
                format!("[{scope}] duplicate entry: {entry}")
            }
            Finding::Unresolvable { scope, entry } => {
                format!("[{scope}] unresolvable variable reference: {entry}")
            }
            Finding::Missing { scope, entry } => format!("[{scope}] missing directory: {entry}"),
            Finding::EntryTooLong {
                scope,
                entry,
                limit,
            } => format!("[{scope}] entry longer than {limit} characters: {entry}"),
            Finding::ValueOverLimit {
                scope,
                units: _,
                limit,
            } => format!("[{scope}] PATH exceeds the {limit} character limit!"),
            Finding::ValueNearLimit {
                scope,
                units,
                limit: _,
            } => format!("[{scope}] PATH is {units} characters, near the practical limit"),
        }
    }
}

pub fn check(reg: &Registry, g: &Global, scopes: &[Scope]) -> Result<u8> {
    let mut findings: Vec<Finding> = Vec::new();
    // Read each scope once: the entries pass and the combined-length pass below
    // both need the raw value.
    let mut values: Vec<(Scope, Option<PathValue>)> = Vec::with_capacity(scopes.len());
    for scope in scopes {
        values.push((*scope, reg.read_path(*scope)?));
    }
    for (scope, value) in &values {
        let label = scope.label();
        let entries = match value {
            Some(v) => pathops::parse(&v.raw),
            None => Vec::new(),
        };
        let dups = pathops::duplicates(&entries);
        for (i, entry) in entries.iter().enumerate() {
            if dups[i] {
                findings.push(Finding::Duplicate {
                    scope: label,
                    entry: entry.clone(),
                });
            }
            let analysis = analyze_entry(entry);
            if analysis.unresolvable {
                findings.push(Finding::Unresolvable {
                    scope: label,
                    entry: entry.clone(),
                });
            }
            if analysis.missing {
                findings.push(Finding::Missing {
                    scope: label,
                    entry: entry.clone(),
                });
            }
            if util::utf16_len(entry) > util::LONG_PATH_FLAG {
                findings.push(Finding::EntryTooLong {
                    scope: label,
                    entry: entry.clone(),
                    limit: util::LONG_PATH_FLAG,
                });
            }
        }
    }
    // Near-limit warning for the combined value per scope.
    for (scope, value) in &values {
        let label = scope.label();
        if let Some(v) = value {
            let units = util::utf16_len(&v.raw);
            if units > util::MAX_ENV_VALUE {
                findings.push(Finding::ValueOverLimit {
                    scope: label,
                    units,
                    limit: util::MAX_ENV_VALUE,
                });
            } else if units >= util::WARN_PATH_LEN {
                findings.push(Finding::ValueNearLimit {
                    scope: label,
                    units,
                    limit: util::WARN_PATH_LEN,
                });
            }
        }
    }

    if g.json {
        println!(
            "{}",
            serde_json::json!({ "ok": findings.is_empty(), "findings": findings })
        );
    } else if findings.is_empty() {
        println!("OK: no issues found");
    } else {
        for f in &findings {
            println!("{}", f.describe());
        }
    }
    if findings.is_empty() { Ok(0) } else { Ok(1) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_lines_are_the_documented_output() {
        // These strings are what `check` prints; the summary above them is
        // derived from the same values, so only the wording is pinned here.
        let entry = || "C:\\x".to_string();
        assert_eq!(
            Finding::Duplicate {
                scope: "user",
                entry: entry()
            }
            .describe(),
            "[user] duplicate entry: C:\\x"
        );
        assert_eq!(
            Finding::Unresolvable {
                scope: "user",
                entry: entry()
            }
            .describe(),
            "[user] unresolvable variable reference: C:\\x"
        );
        assert_eq!(
            Finding::Missing {
                scope: "system",
                entry: entry()
            }
            .describe(),
            "[system] missing directory: C:\\x"
        );
        assert_eq!(
            Finding::EntryTooLong {
                scope: "user",
                entry: entry(),
                limit: 260,
            }
            .describe(),
            "[user] entry longer than 260 characters: C:\\x"
        );
        assert_eq!(
            Finding::ValueOverLimit {
                scope: "user",
                units: 40_000,
                limit: util::MAX_ENV_VALUE,
            }
            .describe(),
            "[user] PATH exceeds the 32767 character limit!"
        );
        assert_eq!(
            Finding::ValueNearLimit {
                scope: "user",
                units: 3_000,
                limit: util::WARN_PATH_LEN,
            }
            .describe(),
            "[user] PATH is 3000 characters, near the practical limit"
        );
    }
}
