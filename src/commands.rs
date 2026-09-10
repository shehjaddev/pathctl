//! Command implementations: read/analyze/mutate PATH and env vars.
//!
//! Every mutating command follows the same safe flow (spec §4):
//! dry-run → length guard → confirm → snapshot (before write) → write →
//! broadcast (unless `--no-broadcast`). Snapshots are written and fsynced
//! *before* the registry write, so a crash mid-mutation is always undoable.

use crate::notify;
use crate::pathops::{self, Change};
use crate::registry::{self, PathValue, RegType, Registry, Scope};
use crate::snapshot::{self, Snapshot};
use crate::util;
use serde::Serialize;
use std::io;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    ElevationRequired(String),
    #[error("{0}")]
    NoOp(String),
    #[error("registry error: {0}")]
    Registry(#[from] io::Error),
    #[error("{0}")]
    Usage(String),
    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub fn exit_code(&self) -> u8 {
        match self {
            AppError::ElevationRequired(_) => 3,
            AppError::NoOp(_) => 4,
            AppError::Registry(_) => 5,
            AppError::Usage(_) => 2,
            AppError::Other(_) => 1,
        }
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

/// Global flags shared by all commands.
#[derive(Debug, Clone, Copy, Default)]
pub struct Global {
    pub json: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub no_broadcast: bool,
    pub elevate: bool,
}

/// Ordered scopes for `--scope all` (effective PATH order: system, then user).
pub const ALL_SCOPES: [Scope; 2] = [Scope::System, Scope::User];

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub fn scopes_from(scope: Option<&str>, allow_all: bool) -> Result<Vec<Scope>> {
    match scope.unwrap_or("user") {
        "user" => Ok(vec![Scope::User]),
        "system" => Ok(vec![Scope::System]),
        "all" if allow_all => Ok(ALL_SCOPES.to_vec()),
        "all" => Err(AppError::Usage(
            "scope 'all' is read-only; pick 'user' or 'system' for mutations".into(),
        )),
        other => Err(AppError::Usage(format!(
            "unknown scope '{other}' (expected user, system or all)"
        ))),
    }
}

fn current_path(reg: &Registry, scope: Scope) -> Result<(String, RegType)> {
    Ok(match reg.read_path(scope)? {
        Some(v) => (v.raw, v.ty),
        None => (String::new(), registry::default_path_type()),
    })
}

fn print_changes(json: bool, before: &[String], after: &[String]) {
    let changes = pathops::diff_entries(before, after);
    if json {
        let out = ChangesOut {
            before,
            after,
            changes: &changes,
        };
        println!("{}", serde_json::to_string(&out).expect("serialize"));
    } else {
        for c in &changes {
            match c {
                Change::Added(e) => println!("+ {e}"),
                Change::Removed(e) => println!("- {e}"),
                Change::Moved(e) => println!("~ {e}"),
            }
        }
    }
}

#[derive(Serialize)]
struct ChangesOut<'a> {
    before: &'a [String],
    after: &'a [String],
    changes: &'a [Change],
}

/// Refuse writes that would exceed the 32,767-char environment-variable limit
/// (spec §4). Counted in UTF-16 units, matching the registry's storage.
fn guard_length(name: &str, value: &str) -> Result<()> {
    let units = value.encode_utf16().count();
    if units > util::MAX_ENV_VALUE {
        return Err(AppError::Usage(format!(
            "refusing write: {name} is {units} characters, above the {} environment-variable limit",
            util::MAX_ENV_VALUE
        )));
    }
    Ok(())
}

fn warn_long_path(value: &str) {
    let len = value.chars().count();
    if len >= util::WARN_PATH_LEN {
        eprintln!(
            "warning: combined PATH is {len} characters (approaching the ~2,048 practical limit)"
        );
    }
}

fn confirm(g: &Global, action: &str) -> Result<()> {
    if g.yes {
        return Ok(());
    }
    eprint!("{action}? [y/N] ");
    let _ = io::Write::flush(&mut io::stderr());
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| AppError::Other(format!("could not read confirmation: {e}")))?;
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Err(AppError::NoOp("cancelled".into()));
    }
    Ok(())
}

/// Map a registry write error to the elevation contract: a system-scope
/// `PermissionDenied` without `--elevate` is `ElevationRequired` (exit 3);
/// with `--elevate` the process relaunches via UAC. Anything else is a plain
/// registry error (exit 5). Extracted from the write helpers so the exit-3
/// contract is unit-testable (handoff #4).
///
/// Returns `Ok(true)` when the change was delegated to an elevated child
/// (parent must not claim success); `Ok(false)` when written directly.
fn write_error_to_app(
    g: &Global,
    scope: Scope,
    e: io::Error,
    elevate_msg: &str,
) -> Result<bool> {
    if scope == Scope::System && e.kind() == io::ErrorKind::PermissionDenied {
        if g.elevate {
            crate::elevate::relaunch_elevated()
                .map_err(|e| AppError::Other(format!("elevation failed: {e}")))?;
            Ok(true)
        } else {
            Err(AppError::ElevationRequired(elevate_msg.into()))
        }
    } else {
        Err(AppError::Registry(e))
    }
}

/// Write with elevation fallback for system scope (exit 3 / --elevate).
/// Returns `true` if delegated to an elevated child.
fn write_path_elev(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    value: &str,
    ty: RegType,
) -> Result<bool> {
    match reg.write_path(scope, value, ty) {
        Ok(()) => Ok(false),
        Err(e) => write_error_to_app(
            g,
            scope,
            e,
            "writing the system PATH requires elevation; rerun with --elevate (UAC) or from an elevated shell",
        ),
    }
}

fn write_var_elev(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    name: &str,
    value: &str,
    ty: RegType,
) -> Result<bool> {
    match reg.write_var(scope, name, value, ty) {
        Ok(()) => Ok(false),
        Err(e) => write_error_to_app(
            g,
            scope,
            e,
            "writing system environment variables requires elevation; rerun with --elevate (UAC) or from an elevated shell",
        ),
    }
}

/// Delete with the same elevation contract as writes: a system-scope
/// `PermissionDenied` without `--elevate` is exit 3, not a raw registry error.
/// Returns `true` if delegated to an elevated child.
fn delete_var_elev(reg: &Registry, g: &Global, scope: Scope, name: &str) -> Result<bool> {
    match reg.delete_var(scope, name) {
        Ok(()) => Ok(false),
        Err(e) => write_error_to_app(
            g,
            scope,
            e,
            "deleting system environment variables requires elevation; rerun with --elevate (UAC) or from an elevated shell",
        ),
    }
}

/// The shared mutation tail: snapshot → write → broadcast.
/// `before_ty` is the snapshotted (old) type; `write_ty` is the type to write
/// (usually the same — type-preserving — except import, which restores the
/// exported type).
#[allow(clippy::too_many_arguments)]
fn commit_path(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    before_raw: &str,
    before_ty: RegType,
    after_raw: &str,
    write_ty: RegType,
    command: &str,
) -> Result<bool> {
    if before_raw == after_raw && before_ty == write_ty {
        return Err(AppError::NoOp("PATH unchanged".into()));
    }
    guard_length("Path", after_raw)?;
    let snap_path = snapshot::save(&Snapshot::with_ty(
        scope.label(),
        "Path",
        before_raw,
        after_raw,
        command,
        Some(registry::reg_type_to_u32(before_ty.clone())),
    ))
    .map_err(|e| AppError::Other(format!("snapshot failed: {e}")))?;
    if let Err(e) = snapshot::prune() {
        eprintln!("warning: snapshot prune failed: {e}");
    }
    match write_path_elev(reg, g, scope, after_raw, write_ty) {
        Ok(false) => {},
        Ok(true) => {
            // Delegated to an elevated child: it will journal itself, so drop
            // our premature snapshot to avoid a phantom entry if the child
            // is cancelled or fails.
            let _ = std::fs::remove_file(&snap_path);
            eprintln!("relaunched elevated; verify with list/check (parent did not write)");
            return Ok(true);
        }
        Err(e) => {
            // The write failed (e.g. exit 3 without admin, or a registry error),
            // so the just-saved snapshot claims a state that never happened.
            // Best-effort remove it; otherwise `diff` would report false drift
            // and `undo` would replay a no-op.
            let _ = std::fs::remove_file(&snap_path);
            return Err(e);
        }
    }
    warn_long_path(after_raw);
    if !g.no_broadcast {
        notify::broadcast_environment();
    }
    Ok(false)
}

/// Mutation tail for general env vars (set/delete).
/// Returns `true` if delegated to an elevated child.
fn commit_var(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    name: &str,
    before: Option<&PathValue>,
    after: Option<(&str, RegType)>,
    command: &str,
) -> Result<bool> {
    let before_raw = before.map(|v| v.raw.clone()).unwrap_or_default();
    let after_str = after.as_ref().map(|(v, _)| v.to_string());
    let after_raw = after_str.clone().unwrap_or_default();
    let before_ty = before.map(|v| v.ty.clone());
    let after_ty = after.as_ref().map(|(_, t)| t.clone());
    if before_raw == after_raw && before_ty == after_ty {
        return Err(AppError::NoOp(format!("{name} already in that state")));
    }
    if let Some((value, _)) = &after {
        guard_length(name, value)?;
    }
    let ty = before
        .map(|v| v.ty.clone())
        .or_else(|| after.as_ref().map(|(_, t)| t.clone()));
    let snap_path = snapshot::save(&Snapshot::with_ty(
        scope.label(),
        name,
        &before_raw,
        &after_raw,
        command,
        ty.map(registry::reg_type_to_u32),
    ))
    .map_err(|e| AppError::Other(format!("snapshot failed: {e}")))?;
    if let Err(e) = snapshot::prune() {
        eprintln!("warning: snapshot prune failed: {e}");
    }
    let result = match &after {
        Some((value, t)) => write_var_elev(reg, g, scope, name, value, t.clone()),
        None => delete_var_elev(reg, g, scope, name),
    };
    match result {
        Ok(false) => {},
        Ok(true) => {
            let _ = std::fs::remove_file(&snap_path);
            eprintln!("relaunched elevated; verify with env get (parent did not write)");
            return Ok(true);
        }
        Err(e) => {
            // Same phantom-snapshot guard as commit_path: a failed write must not
            // leave a journal entry claiming the new state exists.
            let _ = std::fs::remove_file(&snap_path);
            return Err(e);
        }
    }
    if !g.no_broadcast {
        notify::broadcast_environment();
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Read commands
// ---------------------------------------------------------------------------

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
            // Validation always runs on the expanded form: in `--raw` mode
            // only the `expanded` flag is suppressed, so a resolvable
            // `%VAR%` entry is not misreported as missing.
            let expanded = util::expand(entry);
            let mut flags = Vec::new();
            if !raw && expanded != *entry {
                flags.push("expanded");
            }
            if util::has_var_ref(&expanded) && util::expand(&expanded) == expanded {
                flags.push("unresolvable");
            }
            if !util::dir_exists(&expanded) {
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
    for scope in scopes {
        let entries = match reg.read_path(*scope)? {
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
            let expanded = util::expand(entry);
            if util::has_var_ref(entry) && expanded == *entry {
                findings.push(format!(
                    "[{}] unresolvable variable reference: {entry}",
                    scope.label()
                ));
            }
            if !util::dir_exists(&expanded) {
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
    for scope in scopes {
        if let Some(v) = reg.read_path(*scope)? {
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

// ---------------------------------------------------------------------------
// Mutating commands
// ---------------------------------------------------------------------------

pub fn add(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    dir: &str,
    prepend: bool,
    dedupe: bool,
) -> Result<u8> {
    let entry = pathops::trim_quotes(dir).to_string();
    if entry.is_empty() {
        return Err(AppError::Usage("empty PATH entry".into()));
    }
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let mut entries = pathops::parse(&before_raw);
    if dedupe && pathops::contains(&entries, &entry) {
        return Err(AppError::NoOp(format!("{entry} is already in PATH")));
    }
    if prepend {
        entries.insert(0, entry.clone());
    } else {
        entries.push(entry.clone());
    }
    let after_raw = entries.join(";");

    if g.dry_run {
        print_changes(g.json, &pathops::parse(&before_raw), &entries);
        return Ok(0);
    }
    confirm(g, &format!("add '{entry}' to PATH"))?;
    if commit_path(reg, g, scope, &before_raw, before_ty.clone(), &after_raw, before_ty, &format!("add {entry}"))? {
        return Ok(0);
    }
    if !g.json {
        println!("added: {entry}");
    }
    Ok(0)
}

pub fn remove(reg: &Registry, g: &Global, scope: Scope, target: &str) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let mut entries = pathops::parse(&before_raw);
    // `#3` is always an index. A bare number prefers an exact path match
    // first (so a directory literally named `123` stays removable by path),
    // falling back to index for backwards compatibility with `remove 3`.
    let removed = if let Some(hash_idx) = target
        .strip_prefix('#')
        .and_then(|s| s.parse::<usize>().ok())
    {
        pathops::remove_index(&mut entries, hash_idx)
            .map_err(|e| AppError::Usage(e.to_string()))?
    } else if let Some(pos) = entries.iter().position(|e| pathops::eq(e, target)) {
        entries.remove(pos)
    } else if let Some(idx) = parse_index(target) {
        pathops::remove_index(&mut entries, idx)
            .map_err(|e| AppError::Usage(e.to_string()))?
    } else {
        return Err(AppError::NoOp(format!("{target} is not in PATH")));
    };
    let after_raw = entries.join(";");

    if g.dry_run {
        print_changes(g.json, &pathops::parse(&before_raw), &entries);
        return Ok(0);
    }
    confirm(g, &format!("remove '{removed}' from PATH"))?;
    if commit_path(reg, g, scope, &before_raw, before_ty.clone(), &after_raw, before_ty, &format!("remove {removed}"))? {
        return Ok(0);
    }
    if !g.json {
        println!("removed: {removed}");
    }
    Ok(0)
}

pub fn dedupe(reg: &Registry, g: &Global, scope: Scope) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let before = pathops::parse(&before_raw);
    let after = pathops::dedupe(before.clone());
    let after_raw = after.join(";");
    if after_raw == before_raw {
        return Err(AppError::NoOp("PATH already has no duplicates".into()));
    }
    if g.dry_run {
        print_changes(g.json, &before, &after);
        return Ok(0);
    }
    confirm(g, "dedupe PATH")?;
    if commit_path(reg, g, scope, &before_raw, before_ty.clone(), &after_raw, before_ty, "dedupe")? {
        return Ok(0);
    }
    if !g.json {
        println!(
            "deduped: removed {} duplicate(s)",
            before.len() - after.len()
        );
    }
    Ok(0)
}

/// Remove PATH entries whose directories no longer exist (roadmap v2, spec
/// §10). `%VAR%` references that cannot be resolved are kept — existence
/// cannot be determined for them.
pub fn prune(reg: &Registry, g: &Global, scope: Scope) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let before = pathops::parse(&before_raw);
    let mut kept: Vec<String> = Vec::with_capacity(before.len());
    let mut removed: Vec<String> = Vec::new();
    for e in &before {
        let expanded = util::expand(e);
        let resolvable = !util::has_var_ref(e) || expanded != *e;
        if resolvable && !util::dir_exists(&expanded) {
            removed.push(e.clone());
        } else {
            kept.push(e.clone());
        }
    }
    if removed.is_empty() {
        return Err(AppError::NoOp("PATH has no missing entries".into()));
    }
    let after_raw = kept.join(";");

    if g.dry_run {
        print_changes(g.json, &before, &kept);
        return Ok(0);
    }
    confirm(
        g,
        &format!(
            "prune {} missing entr{}",
            removed.len(),
            if removed.len() == 1 { "y" } else { "ies" }
        ),
    )?;
    if commit_path(
        reg,
        g,
        scope,
        &before_raw,
        before_ty.clone(),
        &after_raw,
        before_ty,
        &format!("prune {}", removed.len()),
    )? {
        return Ok(0);
    }
    if !g.json {
        for e in &removed {
            println!("- {e}");
        }
        println!(
            "pruned: {} missing entr{}",
            removed.len(),
            if removed.len() == 1 { "y" } else { "ies" }
        );
    }
    Ok(0)
}

pub fn move_entry(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    from: usize,
    to: usize,
) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let mut entries = pathops::parse(&before_raw);
    pathops::reorder(&mut entries, from, to)
        .map_err(|e| AppError::Usage(e.to_string()))?;
    let after_raw = entries.join(";");
    if after_raw == before_raw {
        return Err(AppError::NoOp("entry already at that position".into()));
    }
    if g.dry_run {
        print_changes(g.json, &pathops::parse(&before_raw), &entries);
        return Ok(0);
    }
    confirm(g, &format!("move entry {from} to position {to}"))?;
    if commit_path(reg, g, scope, &before_raw, before_ty.clone(), &after_raw, before_ty, &format!("move {from} {to}"))? {
        return Ok(0);
    }
    if !g.json {
        println!("moved: {from} -> {to}");
    }
    Ok(0)
}

/// Accepts `3` or `#3`.
fn parse_index(target: &str) -> Option<usize> {
    let s = target.strip_prefix('#').unwrap_or(target);
    s.parse::<usize>().ok()
}

// ---------------------------------------------------------------------------
// Undo / diff
// ---------------------------------------------------------------------------

/// Which snapshot domain an `undo --kind` filter selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    /// PATH snapshots (name `Path`).
    Path,
    /// Environment-variable snapshots (any other name).
    Var,
}

fn snapshots_filtered(
    scope_filter: Option<Scope>,
    kind: Option<SnapshotKind>,
) -> Result<Vec<Snapshot>> {
    let snaps = snapshot::list()?;
    Ok(snaps
        .into_iter()
        .filter(|s| scope_filter.is_none_or(|sc| s.scope == sc.label()))
        .filter(|s| {
            kind.is_none_or(|k| match k {
                SnapshotKind::Path => s.name == "Path",
                SnapshotKind::Var => s.name != "Path",
            })
        })
        .collect())
}

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
    snapshot::save(&Snapshot::with_ty(
        &target.scope,
        &name,
        &before_raw,
        &restore_raw,
        &format!("undo of {}", target.ts),
        current.as_ref().map(|v| registry::reg_type_to_u32(v.ty.clone())),
    ))
    .map_err(|e| AppError::Other(format!("snapshot failed: {e}")))?;
    if let Err(e) = snapshot::prune() {
        eprintln!("warning: snapshot prune failed: {e}");
    }
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
    let all = snapshot::list()?;
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

// ---------------------------------------------------------------------------
// Export / import
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ExportFile {
    tool: &'static str,
    version: u32,
    path: ExportPath,
    variables: ExportVars,
}

#[derive(Serialize)]
struct ExportPath {
    user: Option<ExportValue>,
    system: Option<ExportValue>,
}

#[derive(Serialize)]
struct ExportVars {
    user: Vec<ExportVar>,
}

#[derive(Serialize)]
struct ExportValue {
    value: String,
    ty: u32,
}

#[derive(Serialize)]
struct ExportVar {
    name: String,
    value: String,
    ty: u32,
}

/// Refuse POSIX/MSYS-style output paths: on Windows a leading `/` means
/// "root of the current drive", so `/c/Users/...` from git-bash either fails
/// with a confusing error or writes somewhere unexpected. Failing loudly beats
/// a backup command that silently wrote nothing (handoff #1).
fn check_output_path(path: &std::path::Path) -> Result<()> {
    let s = path.as_os_str().to_string_lossy();
    if s.starts_with('/') {
        return Err(AppError::Usage(format!(
            "refusing non-Windows output path '{s}': use a Windows path such as C:\\backup.json"
        )));
    }
    Ok(())
}

pub fn export(reg: &Registry, output: Option<&std::path::Path>) -> Result<u8> {
    let read = |scope: Scope| -> Result<Option<ExportValue>> {
        Ok(reg.read_path(scope)?.map(|v| ExportValue {
            value: v.raw,
            ty: registry::reg_type_to_u32(v.ty),
        }))
    };
    let vars = reg
        .enum_all(Scope::User)?
        .into_iter()
        // Path already lives in `path.*`; repeating it in `variables.user`
        // exported the same value twice (handoff #3).
        .filter(|(name, _)| !name.eq_ignore_ascii_case("Path"))
        .map(|(name, v)| ExportVar {
            name,
            value: v.raw,
            ty: registry::reg_type_to_u32(v.ty),
        })
        .collect();
    let file = ExportFile {
        tool: "pathctl",
        version: 1,
        path: ExportPath {
            user: read(Scope::User)?,
            system: read(Scope::System)?,
        },
        variables: ExportVars { user: vars },
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| AppError::Other(e.to_string()))?;
    match output {
        Some(path) => {
            check_output_path(path)?;
            std::fs::write(path, json)
                .map_err(|e| AppError::Other(format!("could not write {}: {e}", path.display())))?;
        }
        None => println!("{json}"),
    }
    Ok(0)
}

#[derive(serde::Deserialize)]
struct ImportFile {
    #[serde(default)]
    path: ImportPath,
    #[serde(default)]
    variables: ImportVars,
}

#[derive(serde::Deserialize, Default)]
struct ImportPath {
    user: Option<ImportValue>,
    system: Option<ImportValue>,
}

#[derive(serde::Deserialize, Default)]
struct ImportVars {
    #[serde(default)]
    user: Vec<ImportVar>,
}

#[derive(serde::Deserialize)]
struct ImportValue {
    value: String,
    ty: Option<u32>,
}

#[derive(serde::Deserialize)]
struct ImportVar {
    name: String,
    value: String,
    ty: Option<u32>,
}

/// Short display name for common registry types (dry-run notes).
fn ty_name(ty: RegType) -> String {
    match registry::reg_type_to_u32(ty) {
        1 => "REG_SZ".to_string(),
        2 => "REG_EXPAND_SZ".to_string(),
        n => format!("type {n}"),
    }
}

/// Current state plus target type for an imported PATH value.
fn path_import_state(
    reg: &Registry,
    scope: Scope,
    value: &ImportValue,
) -> Result<(String, RegType, RegType)> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let write_ty = value
        .ty
        .map(registry::reg_type_from_u32)
        .unwrap_or_else(|| before_ty.clone());
    Ok((before_raw, before_ty, write_ty))
}

/// Would importing `var` change anything? Mirrors `env set` conventions:
/// an unset variable with an empty value is a no-op.
fn var_import_changes(current: Option<&PathValue>, var: &ImportVar) -> bool {
    let before_raw = current.map(|v| v.raw.as_str()).unwrap_or("");
    if before_raw != var.value.as_str() {
        return true;
    }
    match current {
        None => false,
        Some(v) => var
            .ty
            .is_some_and(|t| registry::reg_type_from_u32(t) != v.ty),
    }
}

/// Target type for an imported variable (file type, else keep, else default).
fn var_import_ty(current: Option<&PathValue>, var: &ImportVar) -> RegType {
    var.ty
        .map(registry::reg_type_from_u32)
        .or_else(|| current.map(|v| v.ty.clone()))
        .unwrap_or_else(registry::default_var_type)
}

/// Count the changes an import would apply, without writing anything.
/// Mirrors the apply logic below (value and type per item).
fn import_plan_count(reg: &Registry, data: &ImportFile) -> Result<usize> {
    let mut n = 0;
    for (scope, value) in [
        (Scope::User, data.path.user.as_ref()),
        (Scope::System, data.path.system.as_ref()),
    ]
    .into_iter()
    .filter_map(|(s, v)| v.map(|v| (s, v)))
    {
        let (before_raw, before_ty, write_ty) = path_import_state(reg, scope, value)?;
        if before_raw != value.value || before_ty != write_ty {
            n += 1;
        }
    }
    for var in &data.variables.user {
        if var.name.eq_ignore_ascii_case("Path") || valid_var_name(&var.name).is_err() {
            continue;
        }
        let current = reg.read_var(Scope::User, &var.name)?;
        if var_import_changes(current.as_ref(), var) {
            n += 1;
        }
    }
    Ok(n)
}

/// Import merges: nothing existing is deleted, and no write may truncate
/// (the 32,767 guard applies per variable, spec §4).
pub fn import(reg: &Registry, g: &Global, file: &std::path::Path) -> Result<u8> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| AppError::Usage(format!("cannot read {}: {e}", file.display())))?;
    let data: ImportFile =
        serde_json::from_str(&text).map_err(|e| AppError::Usage(format!("invalid export JSON: {e}")))?;

    // Read-only plan first: exit 4 when nothing would change (like every
    // other command's NoOp), and only prompt when there is real work.
    if import_plan_count(reg, &data)? == 0 {
        return Err(AppError::NoOp("import: nothing to change".into()));
    }

    // Bulk restore writes multiple values; confirm once up front like every
    // other mutating command (skipped by -y / --dry-run).
    if !g.dry_run {
        confirm(g, "import")?;
    }

    let mut planned = 0usize;
    // Dry-run JSON accumulates into one document; the per-item printer emits
    // one document per change, which would concatenate on stdout.
    let mut dry_changes: Vec<serde_json::Value> = Vec::new();
    let mut apply_path = |scope: Scope, value: &ImportValue| -> Result<bool> {
        let (before_raw, before_ty, write_ty) = path_import_state(reg, scope, value)?;
        if before_raw == value.value && before_ty == write_ty {
            return Ok(false);
        }
        if g.dry_run {
            if g.json {
                dry_changes.push(serde_json::json!({
                    "scope": scope.label(),
                    "name": "Path",
                    "before": pathops::parse(&before_raw),
                    "after": pathops::parse(&value.value),
                    "ty_before": registry::reg_type_to_u32(before_ty.clone()),
                    "ty_after": registry::reg_type_to_u32(write_ty.clone()),
                }));
            } else if before_raw == value.value {
                println!(
                    "~ Path [{}] (type {} -> {})",
                    scope.label(),
                    ty_name(before_ty),
                    ty_name(write_ty)
                );
            } else {
                print_changes(
                    false,
                    &pathops::parse(&before_raw),
                    &pathops::parse(&value.value),
                );
            }
            return Ok(false);
        }
        planned += 1;
        commit_path(reg, g, scope, &before_raw, before_ty, &value.value, write_ty, "import")
    };

    if let Some(v) = &data.path.user
        && apply_path(Scope::User, v)?
    {
        return Ok(0);
    }
    if let Some(v) = &data.path.system
        && apply_path(Scope::System, v)?
    {
        return Ok(0);
    }
    for var in &data.variables.user {
        // Skip entries export would never produce: the reserved `Path` name
        // (it lives in `path.*`; importing a rogue copy would overwrite the
        // just-imported user PATH) and names that fail validation.
        if var.name.eq_ignore_ascii_case("Path") || valid_var_name(&var.name).is_err() {
            continue;
        }
        let current = reg.read_var(Scope::User, &var.name)?;
        let ty = var_import_ty(current.as_ref(), var);
        if g.dry_run {
            if var_import_changes(current.as_ref(), var) {
                let before = current.as_ref().map(|v| v.raw.clone()).unwrap_or_default();
                if g.json {
                    dry_changes.push(serde_json::json!({
                        "scope": "user",
                        "name": var.name,
                        "before": [before],
                        "after": [var.value],
                    }));
                } else if before == var.value {
                    let old = current.as_ref().map(|v| ty_name(v.ty.clone())).unwrap_or("(unset)".to_string());
                    println!("~ {} (type {old} -> {})", var.name, ty_name(ty.clone()));
                } else {
                    print_changes(
                        false,
                        std::slice::from_ref(&before),
                        std::slice::from_ref(&var.value),
                    );
                }
            }
            continue;
        }
        if !var_import_changes(current.as_ref(), var) {
            continue;
        }
        planned += 1;
        // Delegation cannot happen for user scope, but propagate honestly.
        if commit_var(reg, g, Scope::User, &var.name, current.as_ref(), Some((&var.value, ty)), "import")? {
            return Ok(0);
        }
    }
    if g.json && g.dry_run {
        println!("{}", serde_json::to_string(&dry_changes).expect("serialize"));
    } else if !g.json && !g.dry_run {
        println!("import complete ({planned} change(s) applied)");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Env vars
// ---------------------------------------------------------------------------

pub fn env_get(reg: &Registry, g: &Global, scope: Scope, name: &str) -> Result<u8> {
    let value = reg.read_var(scope, name)?;
    match value {
        Some(v) => {
            if g.json {
                println!(
                    "{}",
                    serde_json::json!({ "scope": scope.label(), "name": name, "value": v.raw, "ty": registry::reg_type_to_u32(v.ty) })
                );
            } else {
                println!("{}", v.raw);
            }
            Ok(0)
        }
        None => Err(AppError::NoOp(format!("{name} is not set"))),
    }
}

fn valid_var_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('=') || name.chars().any(|c| c == '\0') {
        return Err(AppError::Usage(format!("invalid variable name '{name}'")));
    }
    Ok(())
}

pub fn env_set(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    name: &str,
    value: &str,
) -> Result<u8> {
    valid_var_name(name)?;
    let current = reg.read_var(scope, name)?;
    let before_raw = current.as_ref().map(|v| v.raw.clone()).unwrap_or_default();
    if before_raw == value {
        return Err(AppError::NoOp(format!("{name} is already set to that value")));
    }
    if g.dry_run {
        let before: Vec<String> = if before_raw.is_empty() { vec![] } else { vec![before_raw.clone()] };
        print_changes(g.json, &before, &[value.to_string()]);
        return Ok(0);
    }
    confirm(g, &format!("set {name}"))?;
    let ty = current
        .as_ref()
        .map(|v| v.ty.clone())
        .unwrap_or_else(registry::default_var_type);
    if commit_var(reg, g, scope, name, current.as_ref(), Some((value, ty)), &format!("set {name}"))? {
        return Ok(0);
    }
    if !g.json {
        println!("set: {name}");
    }
    Ok(0)
}

pub fn env_delete(reg: &Registry, g: &Global, scope: Scope, name: &str) -> Result<u8> {
    valid_var_name(name)?;
    let current = reg.read_var(scope, name)?;
    if current.is_none() {
        return Err(AppError::NoOp(format!("{name} is not set")));
    }
    if g.dry_run {
        let before = vec![current.as_ref().unwrap().raw.clone()];
        print_changes(g.json, &before, &[]);
        return Ok(0);
    }
    confirm(g, &format!("delete {name}"))?;
    if commit_var(reg, g, scope, name, current.as_ref(), None, &format!("delete {name}"))? {
        return Ok(0);
    }
    if !g.json {
        println!("deleted: {name}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_accepts_limit_and_rejects_over() {
        let ok = "x".repeat(util::MAX_ENV_VALUE);
        assert!(guard_length("X", &ok).is_ok());
        let over = "x".repeat(util::MAX_ENV_VALUE + 1);
        let err = guard_length("X", &over).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("limit"));
    }

    #[test]
    fn system_write_denied_maps_to_exit_3_without_elevate() {
        let g = Global::default();
        let e = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let err = write_error_to_app(&g, Scope::System, e, "elevate me").unwrap_err();
        assert_eq!(err.exit_code(), 3, "README contract: exit 3 = elevation required");
        assert!(matches!(err, AppError::ElevationRequired(_)));
    }

    #[test]
    fn user_scope_denied_is_registry_error_not_exit_3() {
        // Elevation never applies to user scope, even with --elevate (which
        // would otherwise relaunch — this proves the branch is scope-gated).
        let g = Global { elevate: true, ..Global::default() };
        let e = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let err = write_error_to_app(&g, Scope::User, e, "elevate me").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(matches!(err, AppError::Registry(_)));
    }

    #[test]
    fn system_non_permission_error_is_registry_error() {
        let g = Global::default();
        let e = io::Error::new(io::ErrorKind::NotFound, "missing");
        let err = write_error_to_app(&g, Scope::System, e, "elevate me").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(matches!(err, AppError::Registry(_)));
    }

    #[test]
    fn delete_var_elev_wraps_registry_delete() {
        let reg = Registry::test();
        reg.write_var(Scope::User, "PATHCTL_DEL_TEST", "x", registry::default_var_type())
            .unwrap();
        let g = Global::default();
        assert!(!delete_var_elev(&reg, &g, Scope::User, "PATHCTL_DEL_TEST").unwrap());
        assert!(reg.read_var(Scope::User, "PATHCTL_DEL_TEST").unwrap().is_none());
    }

    #[test]
    fn export_refuses_non_windows_output_path() {
        let err =
            check_output_path(std::path::Path::new("/c/Users/User/backup.json")).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("non-Windows"));
        assert!(check_output_path(std::path::Path::new(r"C:\backup.json")).is_ok());
        assert!(check_output_path(std::path::Path::new(r"\\server\share\backup.json")).is_ok());
    }
}
