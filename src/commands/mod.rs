//! Command implementations, grouped by domain.
//!
//! Every mutating command follows the same safe flow: dry-run, length guard,
//! confirm, snapshot (written before the write), write, broadcast (unless
//! `--no-broadcast`). A crash mid-mutation therefore always leaves an
//! undoable snapshot behind.
//!
//! Shared plumbing lives here; each command family has its own submodule.

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

/// The scope a mutating command operates on. `all` is read-only, so such a
/// command always has exactly one scope.
pub fn mutation_scope(scope: Option<&str>) -> Result<Scope> {
    match scopes_from(scope, false)?.as_slice() {
        [only] => Ok(*only),
        // Unreachable: scopes_from rejects `all` and never returns an empty
        // list, but stay honest instead of indexing.
        _ => Err(AppError::Usage("expected a single scope".into())),
    }
}

/// Scopes `export` and `import` cover: both by default, or exactly the one
/// named by `--scope`.
pub fn backup_scopes(scope: Option<&str>) -> Result<Vec<Scope>> {
    match scope {
        None => Ok(ALL_SCOPES.to_vec()),
        Some(s) => scopes_from(Some(s), true),
    }
}

fn current_path(reg: &Registry, scope: Scope) -> Result<(String, RegType)> {
    Ok(match reg.read_path(scope)? {
        Some(v) => (v.raw, v.ty),
        None => (String::new(), registry::default_path_type()),
    })
}

/// What `list`, `check` and `prune` need to know about one raw PATH entry.
///
/// Existence is decided on the unquoted, expanded form. Installers do write
/// quoted entries (`"C:\Program Files\x"`), and treating those as missing
/// would make `prune` delete a directory that exists.
struct EntryAnalysis {
    /// A `%VAR%` reference that did not resolve, so existence is unknown.
    unresolvable: bool,
    /// The target is not an existing directory.
    missing: bool,
    /// Expansion replaced at least one `%VAR%` reference.
    expanded: bool,
}

fn analyze_entry(entry: &str) -> EntryAnalysis {
    let unquoted = pathops::trim_quotes(entry);
    let target = util::expand(unquoted);
    EntryAnalysis {
        unresolvable: util::has_var_ref(unquoted) && target == unquoted,
        missing: !util::dir_exists(&target),
        expanded: target != unquoted,
    }
}

/// Display name for a registry value type (`REG_SZ`, `REG_BINARY`, ...).
fn ty_name(ty: &RegType) -> String {
    format!("{ty:?}")
}

/// The JSON backup format and the `env` commands model plain strings only.
/// Reading or writing any other value type as text destroys its contents, so
/// refuse instead of guessing.
fn guard_text_type(name: &str, ty: &RegType) -> Result<()> {
    if registry::is_text_type(ty) {
        return Ok(());
    }
    Err(AppError::Other(format!(
        "refusing to handle {name}: it has registry type {}; pathctl only handles string values (REG_SZ, REG_EXPAND_SZ)",
        ty_name(ty)
    )))
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
    // Last line of defence for the string-only rule: writing text over a
    // binary value (or the reverse) would destroy it.
    if after.is_some()
        && let Some(v) = before
    {
        guard_text_type(name, &v.ty)?;
    }
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

mod backup;
mod env;
mod history;
mod paths;
mod read;

pub use backup::{export, import};
pub use env::{env_delete, env_get, env_set};
pub use history::{diff, undo, undo_list};
pub use paths::{add, dedupe, move_entry, prune, remove};
pub use read::{check, list};

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

}
