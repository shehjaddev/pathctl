//! JSON export and import of PATH values and user variables.

use super::env::valid_var_name;
use super::*;

use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::path::Component;

/// Version of the export format this build writes and understands.
const EXPORT_VERSION: u32 = 1;

/// One value in a backup: the text and its registry type.
#[derive(Serialize, Deserialize)]
struct BackupValue {
    value: String,
    /// Optional on read: a hand-written file need not name a type.
    ty: Option<u32>,
}

/// One environment variable in a backup.
#[derive(Serialize, Deserialize)]
struct BackupVar {
    name: String,
    value: String,
    ty: Option<u32>,
}

/// The PATH of each scope; `None` means that scope is not in this backup.
#[derive(Serialize, Deserialize, Default)]
struct BackupPath {
    user: Option<BackupValue>,
    system: Option<BackupValue>,
}

#[derive(Serialize, Deserialize, Default)]
struct BackupVars {
    #[serde(default)]
    user: Vec<BackupVar>,
}

/// A backup file: what `export` writes and what `import` accepts. One set of
/// types serves both directions, so the two can never drift apart.
#[derive(Serialize, Deserialize)]
struct BackupFile {
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    path: BackupPath,
    #[serde(default)]
    variables: BackupVars,
}

impl BackupFile {
    /// Refuse a file this build cannot be sure it understands. `tool` and
    /// `version` stay optional so a hand-written file with only
    /// `path`/`variables` still imports, but a file that names another tool or
    /// a newer format is rejected instead of half-applied.
    fn check_origin(&self) -> Result<()> {
        if let Some(tool) = &self.tool
            && tool != "pathctl"
        {
            return Err(AppError::Usage(format!(
                "refusing to import: file was written by '{tool}', not pathctl"
            )));
        }
        if let Some(v) = self.version
            && v > EXPORT_VERSION
        {
            return Err(AppError::Usage(format!(
                "refusing to import: file uses export format {v}, this build understands {EXPORT_VERSION}"
            )));
        }
        Ok(())
    }
}

/// Write a file in one step: temp file next to the target, fsync, then rename.
/// A crash mid-write must not leave a half-written backup behind, which is the
/// same guarantee the snapshot store gives.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("output path has no file name"))?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut file = fs::File::create(&tmp)?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    drop(file);
    fs::rename(&tmp, path)
}

/// Refuse POSIX/MSYS-style output paths: on Windows a leading `/` means
/// "root of the current drive", so `/c/Users/...` from git-bash either fails
/// with a confusing error or writes somewhere unexpected. Failing loudly beats
/// a backup command that silently wrote nothing. `C:backup.json` is refused
/// for the same reason: drive-relative is not the file the user named.
fn check_output_path(path: &std::path::Path) -> Result<()> {
    let s = path.as_os_str().to_string_lossy();
    let drive_relative = path
        .components()
        .next()
        .is_some_and(|c| matches!(c, Component::Prefix(_)))
        && !path.has_root();
    if s.starts_with('/') || drive_relative {
        return Err(AppError::Usage(format!(
            "refusing non-Windows output path '{s}': use a Windows path such as C:\\backup.json"
        )));
    }
    Ok(())
}

pub fn export(reg: &Registry, scopes: &[Scope], output: Option<&std::path::Path>) -> Result<u8> {
    let read = |scope: Scope| -> Result<Option<BackupValue>> {
        if !scopes.contains(&scope) {
            return Ok(None);
        }
        match reg.read_path(scope)? {
            Some(v) => {
                guard_text_type(registry::PATH_VALUE, &v.ty)?;
                Ok(Some(BackupValue {
                    value: v.raw,
                    ty: Some(registry::reg_type_to_u32(v.ty)),
                }))
            }
            None => Ok(None),
        }
    };
    let mut vars = Vec::new();
    let mut skipped = Vec::new();
    if scopes.contains(&Scope::User) {
        for (name, v) in reg.enum_all(Scope::User)? {
            // Path already lives in `path.*`; repeating it in `variables.user`
            // exported the same value twice.
            // Registry value names are case-insensitive, so compare that way.
            if name.eq_ignore_ascii_case(registry::PATH_VALUE) {
                continue;
            }
            // Only strings fit the format; say so rather than writing a value
            // that import would refuse (or worse, that would come back mangled).
            if !registry::is_text_type(&v.ty) {
                skipped.push(format!("{name} ({})", ty_name(&v.ty)));
                continue;
            }
            vars.push(BackupVar {
                name,
                value: v.raw,
                ty: Some(registry::reg_type_to_u32(v.ty)),
            });
        }
    }
    if !skipped.is_empty() {
        eprintln!(
            "warning: export omits {} non-string value(s): {}",
            skipped.len(),
            skipped.join(", ")
        );
    }
    let file = BackupFile {
        tool: Some("pathctl".to_string()),
        version: Some(EXPORT_VERSION),
        path: BackupPath {
            user: read(Scope::User)?,
            system: read(Scope::System)?,
        },
        variables: BackupVars { user: vars },
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| AppError::Other(e.to_string()))?;
    match output {
        Some(path) => {
            check_output_path(path)?;
            write_atomic(path, json.as_bytes())
                .map_err(|e| AppError::Other(format!("could not write {}: {e}", path.display())))?;
        }
        None => println!("{json}"),
    }
    Ok(0)
}

/// Current state plus target type for an imported PATH value.
fn path_import_state(
    reg: &Registry,
    scope: Scope,
    value: &BackupValue,
) -> Result<(String, RegType, RegType)> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    guard_text_type(registry::PATH_VALUE, &before_ty)?;
    let write_ty = value
        .ty
        .map(registry::reg_type_from_u32)
        .unwrap_or_else(|| before_ty.clone());
    if !registry::is_text_type(&write_ty) {
        return Err(AppError::Usage(format!(
            "refusing to import the {} PATH: {} is not a string type",
            scope.label(),
            ty_name(&write_ty)
        )));
    }
    Ok((before_raw, before_ty, write_ty))
}

/// One imported PATH value as it appears in the JSON report, including the
/// registry type the import would write.
fn import_path_doc(
    scope: Scope,
    before: &str,
    after: &str,
    ty_before: &RegType,
    ty_after: &RegType,
) -> serde_json::Value {
    serde_json::json!({
        "scope": scope.label(),
        "name": registry::PATH_VALUE,
        "before": pathops::parse(before),
        "after": pathops::parse(after),
        "ty_before": registry::reg_type_to_u32(ty_before.clone()),
        "ty_after": registry::reg_type_to_u32(ty_after.clone()),
    })
}

/// One imported variable as it appears in the JSON report.
fn import_var_doc(name: &str, before: &str, after: &str) -> serde_json::Value {
    serde_json::json!({
        "scope": "user",
        "name": name,
        "before": entries_of(name, before),
        "after": entries_of(name, after),
    })
}

/// Would importing `var` change anything? Mirrors `env set` conventions:
/// an unset variable with an empty value is a no-op.
fn var_import_changes(current: Option<&PathValue>, var: &BackupVar) -> bool {
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
fn var_import_ty(current: Option<&PathValue>, var: &BackupVar) -> RegType {
    var.ty
        .map(registry::reg_type_from_u32)
        .or_else(|| current.map(|v| v.ty.clone()))
        .unwrap_or_else(registry::default_var_type)
}

/// What an imported variable would need: the value it currently holds and the
/// type to write. `None` means the entry must be skipped.
struct BackupVarState {
    current: Option<PathValue>,
    ty: RegType,
}

/// Validate and read one import entry. Skips what export never produces (the
/// reserved `Path` name, invalid names) and refuses what the format cannot
/// carry (non-string types), so the plan and the apply pass cannot disagree.
fn var_import_state(reg: &Registry, var: &BackupVar) -> Result<Option<BackupVarState>> {
    if var.name.eq_ignore_ascii_case(registry::PATH_VALUE) || valid_var_name(&var.name).is_err() {
        return Ok(None);
    }
    if let Some(t) = var.ty {
        let declared = registry::reg_type_from_u32(t);
        if !registry::is_text_type(&declared) {
            return Err(AppError::Usage(format!(
                "refusing to import {}: {} is not a string type",
                var.name,
                ty_name(&declared)
            )));
        }
    }
    let current = reg.read_var(Scope::User, &var.name)?;
    if let Some(v) = &current {
        guard_text_type(&var.name, &v.ty)?;
    }
    let ty = var_import_ty(current.as_ref(), var);
    Ok(Some(BackupVarState { current, ty }))
}

/// One change an import would make, resolved against the registry. Everything
/// needed to preview it, apply it and report it lives here, so the dry run and
/// the apply pass cannot disagree about what a file means.
enum Planned {
    Path {
        scope: Scope,
        before: String,
        before_ty: RegType,
        after: String,
        after_ty: RegType,
    },
    Var {
        name: String,
        before: Option<PathValue>,
        after: String,
        ty: RegType,
    },
}

/// The value a planned variable holds now; empty means it is not set.
fn before_raw(before: &Option<PathValue>) -> &str {
    before.as_ref().map(|v| v.raw.as_str()).unwrap_or_default()
}

impl Planned {
    /// The JSON document describing this change.
    fn doc(&self) -> serde_json::Value {
        match self {
            Planned::Path {
                scope,
                before,
                before_ty,
                after,
                after_ty,
            } => import_path_doc(*scope, before, after, before_ty, after_ty),
            Planned::Var {
                name,
                before,
                after,
                ..
            } => import_var_doc(name, before_raw(before), after),
        }
    }

    /// The human preview of this change.
    fn preview(&self) {
        match self {
            Planned::Path {
                scope,
                before,
                before_ty,
                after,
                after_ty,
            } => {
                if before == after {
                    println!(
                        "~ Path [{}] (type {} -> {})",
                        scope.label(),
                        ty_name(before_ty),
                        ty_name(after_ty)
                    );
                } else {
                    print_changes(false, &pathops::parse(before), &pathops::parse(after));
                }
            }
            Planned::Var {
                name,
                before,
                after,
                ty,
            } => {
                let raw = before_raw(before);
                if raw == after {
                    let old = before
                        .as_ref()
                        .map(|v| ty_name(&v.ty))
                        .unwrap_or_else(|| "(unset)".to_string());
                    println!("~ {name} (type {old} -> {})", ty_name(ty));
                } else {
                    print_changes(false, &[raw.to_string()], std::slice::from_ref(after));
                }
            }
        }
    }

    /// Apply the change; `Delegated` means an elevated child took over.
    fn apply(&self, reg: &Registry, g: &Global) -> Result<Committed> {
        match self {
            Planned::Path {
                scope,
                before,
                before_ty,
                after,
                after_ty,
            } => commit(
                reg,
                g,
                *scope,
                registry::PATH_VALUE,
                Some((before, before_ty)),
                Some((after, after_ty)),
                "import",
            ),
            Planned::Var {
                name,
                before,
                after,
                ty,
            } => commit(
                reg,
                g,
                Scope::User,
                name,
                before.as_ref().map(|v| (v.raw.as_str(), &v.ty)),
                Some((after, ty)),
                "import",
            ),
        }
    }
}

/// Resolve a backup file into the changes it would make, validating every entry
/// as it goes (value types included). Empty means there is nothing to do.
fn plan_changes(reg: &Registry, scopes: &[Scope], data: &BackupFile) -> Result<Vec<Planned>> {
    let mut planned = Vec::new();
    for (scope, value) in [
        (Scope::User, data.path.user.as_ref()),
        (Scope::System, data.path.system.as_ref()),
    ] {
        let Some(value) = value else {
            continue;
        };
        if !scopes.contains(&scope) {
            continue;
        }
        let (before, before_ty, after_ty) = path_import_state(reg, scope, value)?;
        if before != value.value || before_ty != after_ty {
            planned.push(Planned::Path {
                scope,
                before,
                before_ty,
                after: value.value.clone(),
                after_ty,
            });
        }
    }
    // Variable entries are user-scope only; `--scope system` skips them.
    if scopes.contains(&Scope::User) {
        for var in &data.variables.user {
            let Some(state) = var_import_state(reg, var)? else {
                continue;
            };
            if var_import_changes(state.current.as_ref(), var) {
                planned.push(Planned::Var {
                    name: var.name.clone(),
                    before: state.current,
                    after: var.value.clone(),
                    ty: state.ty,
                });
            }
        }
    }
    Ok(planned)
}

/// Import merges: nothing existing is deleted, and no write may truncate
/// (the 32,767 guard applies per variable).
pub fn import(reg: &Registry, g: &Global, scopes: &[Scope], file: &std::path::Path) -> Result<u8> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| AppError::Usage(format!("cannot read {}: {e}", file.display())))?;
    let data: BackupFile = serde_json::from_str(&text)
        .map_err(|e| AppError::Usage(format!("invalid export JSON: {e}")))?;
    data.check_origin()?;

    // Resolve the file first: this validates every entry and says whether a
    // real run would have anything to do.
    let planned = plan_changes(reg, scopes, &data)?;
    // A real run with nothing to do is a no-op; a dry run previews either way
    // and exits 0.
    if !g.dry_run && planned.is_empty() {
        return Err(AppError::NoOp("import: nothing to change".into()));
    }

    // Bulk restore writes multiple values; confirm once up front like every
    // other mutating command (skipped by -y / --dry-run).
    if !g.dry_run {
        confirm(g, "import")?;
    }

    // Per-item reports accumulate so stdout stays a single JSON document.
    let mut docs: Vec<serde_json::Value> = Vec::new();
    if g.dry_run {
        for change in &planned {
            if g.json {
                docs.push(change.doc());
            } else {
                change.preview();
            }
        }
        // Nothing was applied, so the report is just the list of changes.
        if g.json {
            println!("{}", serde_json::to_string(&docs).expect("serialize"));
        }
        return Ok(0);
    }

    let mut applied = 0usize;
    for change in &planned {
        if change.apply(reg, g)? == Committed::Delegated {
            return Ok(0);
        }
        applied += 1;
        if g.json {
            docs.push(change.doc());
        }
    }
    if g.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "action": "import",
                "applied": applied,
                "changes": docs,
            }))
            .expect("serialize")
        );
    } else {
        println!("import complete ({applied} change(s) applied)");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_refuses_non_windows_output_path() {
        let err = check_output_path(std::path::Path::new("/c/Users/User/backup.json")).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("non-Windows"));
        assert!(check_output_path(std::path::Path::new(r"C:\backup.json")).is_ok());
        assert!(check_output_path(std::path::Path::new(r"\\server\share\backup.json")).is_ok());
        // Drive-relative paths are not what the user named; plain relative
        // paths are still fine.
        assert!(check_output_path(std::path::Path::new(r"C:backup.json")).is_err());
        assert!(check_output_path(std::path::Path::new(r"backup.json")).is_ok());
    }
}
