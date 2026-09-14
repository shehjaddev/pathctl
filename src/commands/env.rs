//! Environment variables outside PATH: get, set and delete.

use super::*;

pub fn env_get(reg: &Registry, g: &Global, scope: Scope, name: &str) -> Result<u8> {
    let value = reg.read_var(scope, name)?;
    match value {
        Some(v) => {
            guard_text_type(name, &v.ty)?;
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

pub(super) fn valid_var_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('=') || name.chars().any(|c| c == '\0') {
        return Err(AppError::Usage(format!("invalid variable name '{name}'")));
    }
    Ok(())
}

/// `Path` belongs to the path commands, which split entries, check existence
/// and guard length. Writing it as a plain variable replaces the whole PATH in
/// one opaque value (and `env delete Path` removes it), so send callers to the
/// commands meant for it. Reading it is fine.
fn reserve_path_name(name: &str) -> Result<()> {
    if name.eq_ignore_ascii_case("Path") {
        return Err(AppError::Usage(format!(
            "refusing to set or delete {name} as a variable: use `pathctl list`, `add`, \
             `remove`, `dedupe` or `prune`"
        )));
    }
    Ok(())
}

pub fn env_set(reg: &Registry, g: &Global, scope: Scope, name: &str, value: &str) -> Result<u8> {
    valid_var_name(name)?;
    reserve_path_name(name)?;
    let current = reg.read_var(scope, name)?;
    let before_raw = current.as_ref().map(|v| v.raw.as_str()).unwrap_or_default();
    // A dry run is a preview: report and exit 0 even when nothing would change.
    if g.dry_run {
        print_changes(
            g.json,
            &entries_of(name, before_raw),
            &entries_of(name, value),
        );
        return Ok(0);
    }
    if before_raw == value {
        return Err(AppError::NoOp(format!(
            "{name} is already set to that value"
        )));
    }
    confirm(g, &format!("set {name}"))?;
    let ty = current
        .as_ref()
        .map(|v| v.ty.clone())
        .unwrap_or_else(registry::default_var_type);
    let before = current.as_ref().map(|v| (v.raw.as_str(), &v.ty));
    if commit(
        reg,
        g,
        scope,
        name,
        before,
        Some((value, &ty)),
        &format!("set {name}"),
    )? == Committed::Delegated
    {
        return Ok(0);
    }
    report_mutation(
        g,
        scope,
        name,
        "set",
        before_raw,
        value,
        &format!("set: {name}"),
    );
    Ok(0)
}

pub fn env_delete(reg: &Registry, g: &Global, scope: Scope, name: &str) -> Result<u8> {
    valid_var_name(name)?;
    reserve_path_name(name)?;
    let current = reg.read_var(scope, name)?;
    let before_raw = current.as_ref().map(|v| v.raw.as_str()).unwrap_or_default();
    if g.dry_run {
        print_changes(g.json, &entries_of(name, before_raw), &[]);
        return Ok(0);
    }
    let Some(current) = current else {
        return Err(AppError::NoOp(format!("{name} is not set")));
    };
    confirm(g, &format!("delete {name}"))?;
    let before = Some((current.raw.as_str(), &current.ty));
    if commit(reg, g, scope, name, before, None, &format!("delete {name}"))? == Committed::Delegated
    {
        return Ok(0);
    }
    report_mutation(
        g,
        scope,
        name,
        "delete",
        &current.raw,
        "",
        &format!("deleted: {name}"),
    );
    Ok(0)
}
