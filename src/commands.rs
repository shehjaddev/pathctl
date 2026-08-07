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
    let _ = io::Write::flush(&mut io::stdout());
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| AppError::Other(format!("could not read confirmation: {e}")))?;
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Err(AppError::NoOp("cancelled".into()));
    }
    Ok(())
}

/// Write with elevation fallback for system scope (exit 3 / --elevate).
fn write_path_elev(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    value: &str,
    ty: RegType,
) -> Result<()> {
    match reg.write_path(scope, value, ty) {
        Ok(()) => Ok(()),
        Err(e) if scope == Scope::System && e.kind() == io::ErrorKind::PermissionDenied => {
            if g.elevate {
                crate::elevate::relaunch_elevated()
                    .map_err(|e| AppError::Other(format!("elevation failed: {e}")))?;
                Ok(())
            } else {
                Err(AppError::ElevationRequired(
                    "writing the system PATH requires elevation; rerun with --elevate (UAC) or from an elevated shell"
                        .into(),
                ))
            }
        }
        Err(e) => Err(AppError::Registry(e)),
    }
}

fn write_var_elev(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    name: &str,
    value: &str,
    ty: RegType,
) -> Result<()> {
    match reg.write_var(scope, name, value, ty) {
        Ok(()) => Ok(()),
        Err(e) if scope == Scope::System && e.kind() == io::ErrorKind::PermissionDenied => {
            if g.elevate {
                crate::elevate::relaunch_elevated()
                    .map_err(|e| AppError::Other(format!("elevation failed: {e}")))?;
                Ok(())
            } else {
                Err(AppError::ElevationRequired(
                    "writing system environment variables requires elevation; rerun with --elevate (UAC) or from an elevated shell"
                        .into(),
                ))
            }
        }
        Err(e) => Err(AppError::Registry(e)),
    }
}

/// The shared mutation tail: snapshot → write → broadcast.
fn commit_path(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    before_raw: &str,
    before_ty: RegType,
    after_raw: &str,
    command: &str,
) -> Result<()> {
    if before_raw == after_raw {
        return Err(AppError::NoOp("PATH unchanged".into()));
    }
    guard_length("Path", after_raw)?;
    snapshot::save(&Snapshot::with_ty(
        scope.label(),
        "Path",
        before_raw,
        after_raw,
        command,
        Some(registry::reg_type_to_u32(before_ty.clone())),
    ))?;
    snapshot::prune()?;
    write_path_elev(reg, g, scope, after_raw, before_ty)?;
    warn_long_path(after_raw);
    if !g.no_broadcast {
        notify::broadcast_environment();
    }
    Ok(())
}

/// Mutation tail for general env vars (set/delete).
fn commit_var(
    reg: &Registry,
    g: &Global,
    scope: Scope,
    name: &str,
    before: Option<&PathValue>,
    after: Option<(&str, RegType)>,
    command: &str,
) -> Result<()> {
    let before_raw = before.map(|v| v.raw.clone()).unwrap_or_default();
    let after_str = after.as_ref().map(|(v, _)| v.to_string());
    let after_raw = after_str.clone().unwrap_or_default();
    if before_raw == after_raw {
        return Err(AppError::NoOp(format!("{name} already in that state")));
    }
    if let Some((value, _)) = &after {
        guard_length(name, value)?;
    }
    let ty = before
        .map(|v| v.ty.clone())
        .or_else(|| after.as_ref().map(|(_, t)| t.clone()));
    snapshot::save(&Snapshot::with_ty(
        scope.label(),
        name,
        &before_raw,
        &after_raw,
        command,
        ty.map(registry::reg_type_to_u32),
    ))?;
    snapshot::prune()?;
    match &after {
        Some((value, t)) => write_var_elev(reg, g, scope, name, value, t.clone())?,
        None => reg.delete_var(scope, name)?,
    }
    if !g.no_broadcast {
        notify::broadcast_environment();
    }
    Ok(())
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
            let expanded = if raw {
                entry.clone()
            } else {
                util::expand(entry)
            };
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
    commit_path(reg, g, scope, &before_raw, before_ty, &after_raw, &format!("add {entry}"))?;
    if !g.json {
        println!("added: {entry}");
    }
    Ok(0)
}

pub fn remove(reg: &Registry, g: &Global, scope: Scope, target: &str) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let mut entries = pathops::parse(&before_raw);
    let removed = if let Some(idx) = parse_index(target) {
        pathops::remove_index(&mut entries, idx)
            .map_err(|e| AppError::Usage(e.to_string()))?
    } else {
        let pos = entries
            .iter()
            .position(|e| pathops::eq(e, target))
            .ok_or_else(|| AppError::NoOp(format!("{target} is not in PATH")))?;
        entries.remove(pos)
    };
    let after_raw = entries.join(";");

    if g.dry_run {
        print_changes(g.json, &pathops::parse(&before_raw), &entries);
        return Ok(0);
    }
    confirm(g, &format!("remove '{removed}' from PATH"))?;
    commit_path(reg, g, scope, &before_raw, before_ty, &after_raw, &format!("remove {removed}"))?;
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
    commit_path(reg, g, scope, &before_raw, before_ty, &after_raw, "dedupe")?;
    if !g.json {
        println!(
            "deduped: removed {} duplicate(s)",
            before.len() - after.len()
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
    commit_path(reg, g, scope, &before_raw, before_ty, &after_raw, &format!("move {from} {to}"))?;
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

fn snapshots_filtered(scope_filter: Option<Scope>) -> Result<Vec<Snapshot>> {
    let snaps = snapshot::list()?;
    Ok(snaps
        .into_iter()
        .filter(|s| scope_filter.is_none_or(|sc| s.scope == sc.label()))
        .collect())
}

pub fn undo_list(g: &Global, scope_filter: Option<Scope>) -> Result<u8> {
    let snaps = snapshots_filtered(scope_filter)?;
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
    to: Option<usize>,
) -> Result<u8> {
    let snaps = snapshots_filtered(scope_filter)?;
    let target = match to {
        Some(i) => snaps
            .get(i - 1)
            .ok_or_else(|| AppError::Usage(format!("no snapshot {i}")))?,
        None => snaps
            .last()
            .ok_or_else(|| AppError::NoOp("nothing to undo".into()))?,
    };
    let scope = if target.scope == "system" {
        Scope::System
    } else {
        Scope::User
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

    // Snapshot the inverse so undo is itself undoable (spec §4).
    snapshot::save(&Snapshot::with_ty(
        &target.scope,
        &name,
        &before_raw,
        &restore_raw,
        &format!("undo of {}", target.ts),
        current.as_ref().map(|v| registry::reg_type_to_u32(v.ty.clone())),
    ))?;
    snapshot::prune()?;

    if name == "Path" {
        write_path_elev(reg, g, scope, &restore_raw, ty)?;
        warn_long_path(&restore_raw);
    } else {
        if restore_raw.is_empty() {
            reg.delete_var(scope, &name)?;
        } else {
            write_var_elev(reg, g, scope, &name, &restore_raw, ty)?;
        }
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
    for scope in scopes {
        let current_raw = reg.read_path(*scope)?.map(|v| v.raw).unwrap_or_default();
        let current = pathops::parse(&current_raw);
        let all = snapshot::list()?;
        let snaps: Vec<&Snapshot> = all
            .iter()
            .filter(|s| s.scope == scope.label() && s.name == "Path")
            .collect();
        let base_raw = match to {
            Some(i) => snaps
                .get(i - 1)
                .map(|s| s.after.as_str())
                .unwrap_or(""),
            None => snaps.last().map(|s| s.after.as_str()).unwrap_or(""),
        };
        let base = pathops::parse(base_raw);
        let changes = pathops::diff_entries(&base, &current);
        if !changes.is_empty() {
            differs = true;
        }
        if g.json {
            println!(
                "{}",
                serde_json::json!({
                    "scope": scope.label(),
                    "base": base,
                    "current": current,
                    "changes": changes,
                })
            );
        } else if changes.is_empty() {
            println!("[{}] no changes", scope.label());
        } else {
            for c in &changes {
                match c {
                    Change::Added(e) => println!("[{}] + {e}", scope.label()),
                    Change::Removed(e) => println!("[{}] - {e}", scope.label()),
                }
            }
        }
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
        Some(path) => std::fs::write(path, json)
            .map_err(|e| AppError::Other(format!("could not write {}: {e}", path.display())))?,
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
}

#[derive(serde::Deserialize)]
struct ImportVar {
    name: String,
    value: String,
    ty: Option<u32>,
}

/// Import merges: nothing existing is deleted, and no write may truncate
/// (the 32,767 guard applies per variable, spec §4).
pub fn import(reg: &Registry, g: &Global, file: &std::path::Path) -> Result<u8> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| AppError::Usage(format!("cannot read {}: {e}", file.display())))?;
    let data: ImportFile =
        serde_json::from_str(&text).map_err(|e| AppError::Usage(format!("invalid export JSON: {e}")))?;

    let mut planned = 0usize;
    let apply_path = |scope: Scope, value: &ImportValue| -> Result<()> {
        let (before_raw, before_ty) = current_path(reg, scope)?;
        if before_raw == value.value {
            return Ok(());
        }
        if g.dry_run {
            print_changes(
                g.json,
                &pathops::parse(&before_raw),
                &pathops::parse(&value.value),
            );
            return Ok(());
        }
        commit_path(reg, g, scope, &before_raw, before_ty, &value.value, "import")
    };

    if let Some(v) = &data.path.user {
        apply_path(Scope::User, v)?;
    }
    if let Some(v) = &data.path.system {
        apply_path(Scope::System, v)?;
    }
    for var in &data.variables.user {
        let current = reg.read_var(Scope::User, &var.name)?;
        let ty = var
            .ty
            .map(registry::reg_type_from_u32)
            .or_else(|| current.as_ref().map(|v| v.ty.clone()))
            .unwrap_or_else(registry::default_var_type);
        planned += 1;
        if g.dry_run {
            let before = current.as_ref().map(|v| v.raw.clone()).unwrap_or_default();
            if before != var.value {
                print_changes(
                    g.json,
                    std::slice::from_ref(&before),
                    std::slice::from_ref(&var.value),
                );
            }
            continue;
        }
        if current.as_ref().map(|v| v.raw.as_str()) == Some(var.value.as_str()) {
            continue;
        }
        commit_var(reg, g, Scope::User, &var.name, current.as_ref(), Some((&var.value, ty)), "import")?;
    }
    if !g.json && !g.dry_run {
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
    commit_var(reg, g, scope, name, current.as_ref(), Some((value, ty)), &format!("set {name}"))?;
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
    commit_var(reg, g, scope, name, current.as_ref(), None, &format!("delete {name}"))?;
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
}
