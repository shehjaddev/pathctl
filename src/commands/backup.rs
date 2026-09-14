//! JSON export and import of PATH values and user variables.

use super::env::valid_var_name;
use super::*;

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

pub fn export(reg: &Registry, scopes: &[Scope], output: Option<&std::path::Path>) -> Result<u8> {
    let read = |scope: Scope| -> Result<Option<ExportValue>> {
        if !scopes.contains(&scope) {
            return Ok(None);
        }
        match reg.read_path(scope)? {
            Some(v) => {
                guard_text_type("Path", &v.ty)?;
                Ok(Some(ExportValue {
                    value: v.raw,
                    ty: registry::reg_type_to_u32(v.ty),
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
            if name.eq_ignore_ascii_case("Path") {
                continue;
            }
            // Only strings fit the format; say so rather than writing a value
            // that import would refuse (or worse, that would come back mangled).
            if !registry::is_text_type(&v.ty) {
                skipped.push(format!("{name} ({})", ty_name(&v.ty)));
                continue;
            }
            vars.push(ExportVar {
                name,
                value: v.raw,
                ty: registry::reg_type_to_u32(v.ty),
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

/// Current state plus target type for an imported PATH value.
fn path_import_state(
    reg: &Registry,
    scope: Scope,
    value: &ImportValue,
) -> Result<(String, RegType, RegType)> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    guard_text_type("Path", &before_ty)?;
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
        "name": "Path",
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

/// What an imported variable would need: the value it currently holds and the
/// type to write. `None` means the entry must be skipped.
struct ImportVarState {
    current: Option<PathValue>,
    ty: RegType,
}

/// Validate and read one import entry. Skips what export never produces (the
/// reserved `Path` name, invalid names) and refuses what the format cannot
/// carry (non-string types), so the plan and the apply pass cannot disagree.
fn var_import_state(reg: &Registry, var: &ImportVar) -> Result<Option<ImportVarState>> {
    if var.name.eq_ignore_ascii_case("Path") || valid_var_name(&var.name).is_err() {
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
    Ok(Some(ImportVarState { current, ty }))
}

/// Count the changes an import would apply, without writing anything.
/// Mirrors the apply logic below (value and type per item).
fn import_plan_count(reg: &Registry, scopes: &[Scope], data: &ImportFile) -> Result<usize> {
    let mut n = 0;
    for (scope, value) in [
        (Scope::User, data.path.user.as_ref()),
        (Scope::System, data.path.system.as_ref()),
    ]
    .into_iter()
    .filter_map(|(s, v)| v.map(|v| (s, v)))
    .filter(|(s, _)| scopes.contains(s))
    {
        let (before_raw, before_ty, write_ty) = path_import_state(reg, scope, value)?;
        if before_raw != value.value || before_ty != write_ty {
            n += 1;
        }
    }
    if !scopes.contains(&Scope::User) {
        return Ok(n);
    }
    for var in &data.variables.user {
        let Some(state) = var_import_state(reg, var)? else {
            continue;
        };
        if var_import_changes(state.current.as_ref(), var) {
            n += 1;
        }
    }
    Ok(n)
}

/// Import merges: nothing existing is deleted, and no write may truncate
/// (the 32,767 guard applies per variable).
pub fn import(reg: &Registry, g: &Global, scopes: &[Scope], file: &std::path::Path) -> Result<u8> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| AppError::Usage(format!("cannot read {}: {e}", file.display())))?;
    let data: ImportFile =
        serde_json::from_str(&text).map_err(|e| AppError::Usage(format!("invalid export JSON: {e}")))?;

    // Read-only plan first: exit 4 when nothing would change (like every
    // other command's NoOp), and only prompt when there is real work.
    if import_plan_count(reg, scopes, &data)? == 0 {
        return Err(AppError::NoOp("import: nothing to change".into()));
    }

    // Bulk restore writes multiple values; confirm once up front like every
    // other mutating command (skipped by -y / --dry-run).
    if !g.dry_run {
        confirm(g, "import")?;
    }

    let mut planned = 0usize;
    // Per-item reports accumulate so stdout stays a single JSON document: the
    // dry run prints them as an array, the apply pass reports what it changed.
    let mut item_docs: Vec<serde_json::Value> = Vec::new();
    let mut apply_path = |scope: Scope, value: &ImportValue| -> Result<Committed> {
        let (before_raw, before_ty, write_ty) = path_import_state(reg, scope, value)?;
        if before_raw == value.value && before_ty == write_ty {
            return Ok(Committed::Written);
        }
        if g.dry_run {
            if g.json {
                item_docs.push(import_path_doc(
                    scope,
                    &before_raw,
                    &value.value,
                    &before_ty,
                    &write_ty,
                ));
            } else if before_raw == value.value {
                println!(
                    "~ Path [{}] (type {} -> {})",
                    scope.label(),
                    ty_name(&before_ty),
                    ty_name(&write_ty)
                );
            } else {
                print_changes(
                    false,
                    &pathops::parse(&before_raw),
                    &pathops::parse(&value.value),
                );
            }
            return Ok(Committed::Written);
        }
        planned += 1;
        let committed = commit(
            reg,
            g,
            scope,
            "Path",
            Some((&before_raw, &before_ty)),
            Some((&value.value, &write_ty)),
            "import",
        )?;
        if committed == Committed::Written && g.json {
            item_docs.push(import_path_doc(
                scope,
                &before_raw,
                &value.value,
                &before_ty,
                &write_ty,
            ));
        }
        Ok(committed)
    };

    if scopes.contains(&Scope::User)
        && let Some(v) = &data.path.user
        && apply_path(Scope::User, v)? == Committed::Delegated
    {
        return Ok(0);
    }
    if scopes.contains(&Scope::System)
        && let Some(v) = &data.path.system
        && apply_path(Scope::System, v)? == Committed::Delegated
    {
        return Ok(0);
    }
    // Variable entries are user-scope only; `--scope system` skips them.
    let vars: &[ImportVar] = if scopes.contains(&Scope::User) {
        &data.variables.user
    } else {
        &[]
    };
    for var in vars {
        let Some(state) = var_import_state(reg, var)? else {
            continue;
        };
        let current = state.current.as_ref();
        if g.dry_run {
            if var_import_changes(current, var) {
                let before = current.map(|v| v.raw.clone()).unwrap_or_default();
                if g.json {
                    item_docs.push(import_var_doc(&var.name, &before, &var.value));
                } else if before == var.value {
                    let old = current.map(|v| ty_name(&v.ty)).unwrap_or("(unset)".to_string());
                    println!("~ {} (type {old} -> {})", var.name, ty_name(&state.ty));
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
        if !var_import_changes(current, var) {
            continue;
        }
        planned += 1;
        // Delegation cannot happen for user scope, but propagate honestly.
        let before_raw = state.current.as_ref().map(|v| v.raw.as_str()).unwrap_or_default();
        let before = state.current.as_ref().map(|v| (v.raw.as_str(), &v.ty));
        let committed = commit(
            reg,
            g,
            Scope::User,
            &var.name,
            before,
            Some((&var.value, &state.ty)),
            "import",
        )?;
        if committed == Committed::Written && g.json {
            item_docs.push(import_var_doc(&var.name, before_raw, &var.value));
        }
        if committed == Committed::Delegated {
            return Ok(0);
        }
    }
    if g.dry_run {
        // Nothing was applied, so the report is just the list of changes.
        if g.json {
            println!("{}", serde_json::to_string(&item_docs).expect("serialize"));
        }
        return Ok(0);
    }
    if g.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "action": "import",
                "applied": planned,
                "changes": item_docs,
            }))
            .expect("serialize")
        );
    } else {
        println!("import complete ({planned} change(s) applied)");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
