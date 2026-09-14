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
