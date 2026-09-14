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
