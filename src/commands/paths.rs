//! PATH mutations: add, remove, dedupe, prune and move.

use super::*;

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
    if commit(
        reg,
        g,
        scope,
        "Path",
        Some((&before_raw, &before_ty)),
        Some((&after_raw, &before_ty)),
        &format!("add {entry}"),
    )? == Committed::Delegated
    {
        return Ok(0);
    }
    report_mutation(g, scope, "Path", "add", &before_raw, &after_raw, &format!("added: {entry}"));
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
    if commit(
        reg,
        g,
        scope,
        "Path",
        Some((&before_raw, &before_ty)),
        Some((&after_raw, &before_ty)),
        &format!("remove {removed}"),
    )? == Committed::Delegated
    {
        return Ok(0);
    }
    report_mutation(
        g,
        scope,
        "Path",
        "remove",
        &before_raw,
        &after_raw,
        &format!("removed: {removed}"),
    );
    Ok(0)
}

pub fn dedupe(reg: &Registry, g: &Global, scope: Scope) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let before = pathops::parse(&before_raw);
    let after = pathops::dedupe(&before);
    let after_raw = after.join(";");
    if after_raw == before_raw {
        return Err(AppError::NoOp("PATH already has no duplicates".into()));
    }
    if g.dry_run {
        print_changes(g.json, &before, &after);
        return Ok(0);
    }
    confirm(g, "dedupe PATH")?;
    if commit(
        reg,
        g,
        scope,
        "Path",
        Some((&before_raw, &before_ty)),
        Some((&after_raw, &before_ty)),
        "dedupe",
    )? == Committed::Delegated
    {
        return Ok(0);
    }
    report_mutation(
        g,
        scope,
        "Path",
        "dedupe",
        &before_raw,
        &after_raw,
        &format!("deduped: removed {} duplicate(s)", before.len() - after.len()),
    );
    Ok(0)
}

/// Remove PATH entries whose directories no longer exist. `%VAR%` references
/// that cannot be resolved are kept: existence cannot be determined for them.
pub fn prune(reg: &Registry, g: &Global, scope: Scope) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let before = pathops::parse(&before_raw);
    let mut kept: Vec<String> = Vec::with_capacity(before.len());
    let mut removed: Vec<String> = Vec::new();
    for e in &before {
        let analysis = analyze_entry(e);
        if !analysis.unresolvable && analysis.missing {
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
    let plural = if removed.len() == 1 { "y" } else { "ies" };
    confirm(g, &format!("prune {} missing entr{plural}", removed.len()))?;
    if commit(
        reg,
        g,
        scope,
        "Path",
        Some((&before_raw, &before_ty)),
        Some((&after_raw, &before_ty)),
        &format!("prune {}", removed.len()),
    )? == Committed::Delegated
    {
        return Ok(0);
    }
    let mut summary = String::new();
    for e in &removed {
        summary.push_str(&format!("- {e}\n"));
    }
    summary.push_str(&format!("pruned: {} missing entr{plural}", removed.len()));
    report_mutation(g, scope, "Path", "prune", &before_raw, &after_raw, &summary);
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
    if commit(
        reg,
        g,
        scope,
        "Path",
        Some((&before_raw, &before_ty)),
        Some((&after_raw, &before_ty)),
        &format!("move {from} {to}"),
    )? == Committed::Delegated
    {
        return Ok(0);
    }
    report_mutation(
        g,
        scope,
        "Path",
        "move",
        &before_raw,
        &after_raw,
        &format!("moved: {from} -> {to}"),
    );
    Ok(0)
}

/// Accepts `3` or `#3`.
fn parse_index(target: &str) -> Option<usize> {
    let s = target.strip_prefix('#').unwrap_or(target);
    s.parse::<usize>().ok()
}
