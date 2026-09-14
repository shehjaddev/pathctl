//! Snapshot history: `undo`, `undo --list` and `diff`.

use super::*;

pub fn undo_list(g: &Global, scope_filter: Option<Scope>, kind: Option<SnapshotKind>) -> Result<u8> {
    let snaps = snapshots_filtered(scope_filter, kind)?;
    if g.json {
        println!("{}", serde_json::to_string(&snaps).expect("serialize"));
    } else if snaps.is_empty() {
        println!("no snapshots");
    } else {
        for (i, s) in snaps.iter().enumerate() {
            println!(
                "{:>3}: [{}] {} {}: {} -> {}",
                i + 1,
                s.scope,
                s.name,
                s.command,
                if s.before.is_empty() { "(none)" } else { &s.before },
                if s.after.is_empty() { "(none)" } else { &s.after },
            );
        }
    }
    Ok(0)
}

pub fn undo(
    reg: &Registry,
    g: &Global,
    scope_filter: Option<Scope>,
    kind: Option<SnapshotKind>,
    to: Option<usize>,
) -> Result<u8> {
    let snaps = snapshots_filtered(scope_filter, kind)?;
    let target = match to {
        Some(i) => i
            .checked_sub(1)
            .and_then(|idx| snaps.get(idx))
            .ok_or_else(|| AppError::Usage(format!("no snapshot {i}")))?,
        None => snaps
            .last()
            .ok_or_else(|| AppError::NoOp("nothing to undo".into()))?,
    };
    let scope = match target.scope.as_str() {
        "system" => Scope::System,
        "user" => Scope::User,
        other => {
            return Err(AppError::Usage(format!(
                "snapshot has unknown scope '{other}' (expected user or system)"
            )));
        }
    };
    let name = target.name.clone();
    let restore_raw = target.before.clone();
    let ty = target
        .ty
        .map(registry::reg_type_from_u32)
        .unwrap_or_else(|| {
            if name == "Path" {
                registry::default_path_type()
            } else {
                registry::default_var_type()
            }
        });

    let current = reg.read_var(scope, &name)?;
    let before_raw = current.as_ref().map(|v| v.raw.clone()).unwrap_or_default();

    if g.dry_run {
        let before = pathops::parse(&before_raw);
        let after = pathops::parse(&restore_raw);
        print_changes(g.json, &before, &after);
        return Ok(0);
    }
    confirm(g, &format!("undo '{}' ({})", target.command, target.ts))?;
    guard_length(&name, &restore_raw)?;

    let delegated = if name == "Path" {
        let d = write_path_elev(reg, g, scope, &restore_raw, ty)?;
        if !d {
            warn_long_path(&restore_raw);
        }
        d
    } else if restore_raw.is_empty() {
        delete_var_elev(reg, g, scope, &name)?
    } else {
        write_var_elev(reg, g, scope, &name, &restore_raw, ty)?
    };
    if delegated {
        eprintln!("relaunched elevated; verify with list/env get (parent did not write)");
        return Ok(0);
    }

    // Snapshot the inverse AFTER the write succeeds so a failed restore
    // (e.g. system scope without admin) cannot poison the journal: the next
    // undo would otherwise target a snapshot whose restore is a no-op. The
    // pre-undo state stays recoverable from the undone snapshot's `before`,
    // so crash-safety is unchanged.
    journal(
        scope,
        &name,
        &before_raw,
        &restore_raw,
        &format!("undo of {}", target.ts),
        current.as_ref().map(|v| v.ty.clone()),
    )?;
    if !g.no_broadcast {
        notify::broadcast_environment();
    }
    if !g.json {
        println!("restored {} ({})", name, target.command);
    }
    Ok(0)
}

pub fn diff(reg: &Registry, g: &Global, scopes: &[Scope], to: Option<usize>) -> Result<u8> {
    let mut differs = false;
    let mut docs: Vec<serde_json::Value> = Vec::new();
    // Read once: the journal does not change mid-diff, and this avoids
    // duplicate corrupt-snapshot warnings under `--scope all`.
    let all = snapshot::list()
        .map_err(|e| AppError::Other(format!("could not read the snapshot journal: {e}")))?;
    for scope in scopes {
        let current_raw = reg.read_path(*scope)?.map(|v| v.raw).unwrap_or_default();
        let current = pathops::parse(&current_raw);
        let snaps: Vec<&Snapshot> = all
            .iter()
            .filter(|s| s.scope == scope.label() && s.name == "Path")
            .collect();
        let base_raw = match to {
            // Mirror undo: `--to 0` and out-of-range ids are usage errors,
            // not an underflow panic or a silent empty base.
            Some(i) => i
                .checked_sub(1)
                .and_then(|idx| snaps.get(idx))
                .map(|s| s.after.as_str())
                .ok_or_else(|| AppError::Usage(format!("no snapshot {i}")))?,
            None => match snaps.last() {
                Some(s) => s.after.as_str(),
                None => {
                    // No baseline: nothing was ever recorded, so there is no
                    // drift to report. Treating "" as the baseline would flag
                    // the entire PATH as added on a fresh install.
                    if g.json {
                        docs.push(serde_json::json!({
                            "scope": scope.label(),
                            "base": [],
                            "current": current,
                            "changes": [],
                            "note": "no snapshots recorded yet — nothing to diff against",
                        }));
                    } else {
                        println!(
                            "[{}] no snapshots recorded yet (run a pathctl mutation to establish a baseline)",
                            scope.label()
                        );
                    }
                    continue;
                }
            },
        };
        let base = pathops::parse(base_raw);
        let changes = pathops::diff_entries(&base, &current);
        if !changes.is_empty() {
            differs = true;
        }
        if g.json {
            docs.push(serde_json::json!({
                "scope": scope.label(),
                "base": base,
                "current": current,
                "changes": changes,
            }));
        } else if changes.is_empty() {
            println!("[{}] no changes", scope.label());
        } else {
            for c in &changes {
                match c {
                    Change::Added(e) => println!("[{}] + {e}", scope.label()),
                    Change::Removed(e) => println!("[{}] - {e}", scope.label()),
                    Change::Moved(e) => println!("[{}] ~ {e} (moved)", scope.label()),
                }
            }
        }
    }
    if g.json {
        // One document even with `--scope all` (was N concatenated objects).
        println!("{}", serde_json::to_string(&docs).expect("serialize"));
    }
    Ok(if differs { 1 } else { 0 })
}
