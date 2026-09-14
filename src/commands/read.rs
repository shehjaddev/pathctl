//! `list` and `check`: read-only PATH analysis.

use super::*;

#[derive(Serialize)]
struct EntryOut {
    index: usize,
    entry: String,
    flags: Vec<&'static str>,
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
        for (i, entry) in entries.iter().enumerate() {
            // Analysis always runs on the expanded form: in `--raw` mode only
            // the `expanded` flag is suppressed, so a resolvable `%VAR%` entry
            // is not misreported as missing.
            let analysis = analyze_entry(entry);
            let mut flags = Vec::new();
            if !raw && analysis.expanded {
                flags.push("expanded");
            }
            if analysis.unresolvable {
                flags.push("unresolvable");
            }
            if analysis.missing {
                flags.push("missing");
            }
            if entries[..i].iter().any(|e| pathops::eq(e, entry)) {
                flags.push("dup");
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
                let flags: String = e
                    .flags
                    .iter()
                    .map(|f| match *f {
                        "missing" => "!",
                        "dup" => "d",
                        "unresolvable" => "%",
                        _ => "e",
                    })
                    .collect();
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

pub fn check(reg: &Registry, g: &Global, scopes: &[Scope]) -> Result<u8> {
    let mut findings: Vec<String> = Vec::new();
    // Read each scope once: the entries pass and the combined-length pass below
    // both need the raw value.
    let mut values: Vec<(Scope, Option<PathValue>)> = Vec::with_capacity(scopes.len());
    for scope in scopes {
        values.push((*scope, reg.read_path(*scope)?));
    }
    for (scope, value) in &values {
        let entries = match value {
            Some(v) => pathops::parse(&v.raw),
            None => Vec::new(),
        };
        let mut seen: Vec<String> = Vec::new();
        for entry in &entries {
            if seen.iter().any(|e| pathops::eq(e, entry)) {
                findings.push(format!(
                    "[{}] duplicate entry: {entry}",
                    scope.label()
                ));
            } else {
                seen.push(entry.clone());
            }
            let analysis = analyze_entry(entry);
            if analysis.unresolvable {
                findings.push(format!(
                    "[{}] unresolvable variable reference: {entry}",
                    scope.label()
                ));
            }
            if analysis.missing {
                findings.push(format!(
                    "[{}] missing directory: {entry}",
                    scope.label()
                ));
            }
            if entry.chars().count() > util::LONG_PATH_FLAG {
                findings.push(format!(
                    "[{}] entry longer than {} characters: {entry}",
                    scope.label(),
                    util::LONG_PATH_FLAG
                ));
            }
        }
    }
    // Near-limit warning for the combined value per scope.
    for (scope, value) in &values {
        if let Some(v) = value {
            let units = v.raw.encode_utf16().count();
            if units > util::MAX_ENV_VALUE {
                findings.push(format!(
                    "[{}] PATH exceeds the {} character limit!",
                    scope.label(),
                    util::MAX_ENV_VALUE
                ));
            } else if units >= util::WARN_PATH_LEN {
                findings.push(format!(
                    "[{}] PATH is {units} characters, near the practical limit",
                    scope.label()
                ));
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
            println!("{f}");
        }
    }
    if findings.is_empty() {
        Ok(0)
    } else {
        Ok(1)
    }
}
