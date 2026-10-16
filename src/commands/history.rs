//! Snapshot history: `undo`, `undo --list` and `diff`.

use super::*;

pub fn undo_list(
    g: &Global,
    scope_filter: Option<Scope>,
    kind: Option<SnapshotKind>,
) -> Result<u8> {
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
                if s.before_exists() {
                    &s.before
                } else {
                    "(none)"
                },
                if s.after.is_empty() {
                    "(none)"
                } else {
                    &s.after
                },
            );
        }
    }
    Ok(0)
}

/// The snapshot `undo` will restore: `--to` picks a 1-based id, otherwise the
/// newest one matching the filters. `None` means there is nothing to undo.
fn undo_target(snaps: &[Snapshot], to: Option<usize>) -> Result<Option<&Snapshot>> {
    match to {
        Some(i) => Ok(Some(
            i.checked_sub(1)
                .and_then(|idx| snaps.get(idx))
                .ok_or_else(|| AppError::Usage(format!("no snapshot {i}")))?,
        )),
        None => Ok(snaps.last()),
    }
}

/// What a snapshot says the value used to be.
struct Restore {
    scope: Scope,
    name: String,
    /// The value to write back; see `exists` for whether it existed.
    raw: String,
    ty: RegType,
    /// Whether the value existed before the snapshotted mutation: a present
    /// empty value restores that empty value, an absent one restores absence.
    exists: bool,
}

fn restore_of(target: &Snapshot) -> Result<Restore> {
    let scope = match target.scope.as_str() {
        "system" => Scope::System,
        "user" => Scope::User,
        other => {
            return Err(AppError::Usage(format!(
                "snapshot has unknown scope '{other}' (expected user or system)"
            )));
        }
    };
    let ty = target
        .ty
        .and_then(registry::reg_type_from_u32)
        .unwrap_or_else(|| {
            if target.name == registry::PATH_VALUE {
                registry::default_path_type()
            } else {
                registry::default_var_type()
            }
        });
    Ok(Restore {
        scope,
        name: target.name.clone(),
        raw: target.before.clone(),
        ty,
        exists: target.before_exists(),
    })
}

/// Put a snapshot's value back, or remove it when it did not exist then. The
/// current value is guarded first: restoring text over a non-string value would
/// destroy it, so refuse instead.
fn apply_restore(
    reg: &Registry,
    g: &Global,
    restore: &Restore,
    current: Option<&PathValue>,
) -> Result<Committed> {
    if let Some(v) = current {
        guard_text_type(&restore.name, &v.ty)?;
    }
    let Restore {
        scope,
        name,
        raw,
        ty,
        exists,
    } = restore;
    if !exists {
        // The snapshot recorded that the value did not exist, so restore
        // absence rather than writing an empty value.
        return delete_var_elev(reg, g, *scope, name);
    }
    if name == registry::PATH_VALUE {
        let committed = write_path_elev(reg, g, *scope, raw, ty.clone())?;
        if committed == Committed::Written {
            warn_long_path(reg, *scope, raw);
        }
        return Ok(committed);
    }
    write_var_elev(reg, g, *scope, name, raw, ty.clone())
}

pub fn undo(
    reg: &Registry,
    g: &Global,
    scope_filter: Option<Scope>,
    kind: Option<SnapshotKind>,
    to: Option<usize>,
) -> Result<u8> {
    let snaps = snapshots_filtered(scope_filter, kind)?;
    let target = undo_target(&snaps, to)?;
    if target.is_none() && g.dry_run {
        // A dry run previews, even when there is nothing to preview.
        return Ok(0);
    }
    let target = target.ok_or_else(|| AppError::NoOp("nothing to undo".into()))?;
    let restore = restore_of(target)?;
    let current = reg.read_var(restore.scope, &restore.name)?;
    let before = current.as_ref().map(|v| v.raw.as_str());
    let after = restore.exists.then_some(restore.raw.as_str());

    if g.dry_run {
        print_changes(
            g.json,
            &entries_of(&restore.name, before),
            &entries_of(&restore.name, after),
        );
        return Ok(0);
    }
    confirm(g, &format!("undo '{}' ({})", target.command, target.ts))?;
    guard_length(&restore.name, &restore.raw)?;

    // Journal the inverse *before* the write: a crash mid-restore leaves an
    // undoable record, and a failed write can drop the premature snapshot.
    let snap_path = journal(
        restore.scope,
        &restore.name,
        before,
        after,
        current.is_some(),
        &format!("undo of {}", target.ts),
        current.as_ref().map(|v| v.ty.clone()),
    )?;
    match apply_restore(reg, g, &restore, current.as_ref()) {
        Ok(Committed::Written) => {}
        Ok(Committed::Delegated(code)) => {
            // The elevated child journals itself; drop ours and forward its code.
            let _ = std::fs::remove_file(&snap_path);
            eprintln!("relaunched elevated; verify with list/env get (parent did not write)");
            return Ok(code);
        }
        Err(e) => {
            // Nothing was restored, so the snapshot claims a state that never
            // happened: drop it or the next undo replays a no-op.
            let _ = std::fs::remove_file(&snap_path);
            return Err(e);
        }
    }
    if !g.no_broadcast {
        notify::broadcast_environment();
    }
    report_mutation(
        g,
        restore.scope,
        &restore.name,
        "undo",
        before,
        after,
        &format!("restored {} ({})", restore.name, target.command),
    );
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
            .filter(|s| s.scope == scope.label() && s.name == registry::PATH_VALUE)
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
                            "note": "no snapshots recorded yet -- nothing to diff against",
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
        // One document even with `--scope all`.
        println!("{}", serde_json::to_string(&docs).expect("serialize"));
    }
    Ok(if differs { 1 } else { 0 })
}
