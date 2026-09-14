//! PATH mutations: add, remove, dedupe, prune and move.

use super::*;
use std::fmt::Write as _;

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
    let already_present = dedupe && pathops::contains(&entries, &entry);
    if prepend {
        entries.insert(0, entry.clone());
    } else {
        entries.push(entry.clone());
    }
    let after_raw = entries.join(";");

    // A dry run is a preview: it reports and exits 0 even when the real run
    // would be a no-op.
    if g.dry_run {
        print_changes(g.json, &pathops::parse(&before_raw), &entries);
        return Ok(0);
    }
    if already_present {
        return Err(AppError::NoOp(format!("{entry} is already in PATH")));
    }
    confirm(g, &format!("add '{entry}' to PATH"))?;
    if commit(
        reg,
        g,
        scope,
        registry::PATH_VALUE,
        Some((&before_raw, &before_ty)),
        Some((&after_raw, &before_ty)),
        &format!("add {entry}"),
    )? == Committed::Delegated
    {
        return Ok(0);
    }
    report_mutation(
        g,
        scope,
        registry::PATH_VALUE,
        "add",
        &before_raw,
        &after_raw,
        &format!("added: {entry}"),
    );
    Ok(0)
}

/// A `remove` target. `#3` is always an index; anything else is an entry name
/// first, and a bare number is a 1-based index only when no entry has it.
enum Target<'a> {
    Index(usize),
    Name(&'a str),
}

fn parse_target(target: &str) -> Target<'_> {
    if let Some(rest) = target.strip_prefix('#')
        && let Ok(index) = rest.parse::<usize>()
    {
        return Target::Index(index);
    }
    Target::Name(target)
}

pub fn remove(reg: &Registry, g: &Global, scope: Scope, target: &str) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let mut entries = pathops::parse(&before_raw);
    let removed = match parse_target(target) {
        Target::Index(index) => Some(remove_at(&mut entries, index)?),
        Target::Name(name) => match entries.iter().position(|e| pathops::eq(e, name)) {
            Some(pos) => Some(entries.remove(pos)),
            None => match name.parse::<usize>() {
                Ok(index) => Some(remove_at(&mut entries, index)?),
                Err(_) => None,
            },
        },
    };
    let after_raw = entries.join(";");

    if g.dry_run {
        print_changes(g.json, &pathops::parse(&before_raw), &entries);
        return Ok(0);
    }
    let removed = removed.ok_or_else(|| AppError::NoOp(format!("{target} is not in PATH")))?;
    confirm(g, &format!("remove '{removed}' from PATH"))?;
    if commit(
        reg,
        g,
        scope,
        registry::PATH_VALUE,
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
        registry::PATH_VALUE,
        "remove",
        &before_raw,
        &after_raw,
        &format!("removed: {removed}"),
    );
    Ok(0)
}

/// Remove the entry at 1-based `index`, reporting a bad index as a usage error.
fn remove_at(entries: &mut Vec<String>, index: usize) -> Result<String> {
    pathops::remove_index(entries, index).map_err(|e| AppError::Usage(e.to_string()))
}

pub fn dedupe(reg: &Registry, g: &Global, scope: Scope) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let before = pathops::parse(&before_raw);
    let after = pathops::dedupe(&before);
    let after_raw = after.join(";");
    if g.dry_run {
        print_changes(g.json, &before, &after);
        return Ok(0);
    }
    if after_raw == before_raw {
        return Err(AppError::NoOp("PATH already has no duplicates".into()));
    }
    confirm(g, "dedupe PATH")?;
    if commit(
        reg,
        g,
        scope,
        registry::PATH_VALUE,
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
        registry::PATH_VALUE,
        "dedupe",
        &before_raw,
        &after_raw,
        &format!(
            "deduped: removed {} duplicate(s)",
            before.len() - after.len()
        ),
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
    let after_raw = kept.join(";");
    if g.dry_run {
        print_changes(g.json, &before, &kept);
        return Ok(0);
    }
    if removed.is_empty() {
        return Err(AppError::NoOp("PATH has no missing entries".into()));
    }
    let plural = if removed.len() == 1 { "y" } else { "ies" };
    confirm(g, &format!("prune {} missing entr{plural}", removed.len()))?;
    if commit(
        reg,
        g,
        scope,
        registry::PATH_VALUE,
        Some((&before_raw, &before_ty)),
        Some((&after_raw, &before_ty)),
        &format!("prune {}", removed.len()),
    )? == Committed::Delegated
    {
        return Ok(0);
    }
    let mut summary = String::new();
    for e in &removed {
        let _ = writeln!(summary, "- {e}");
    }
    let _ = writeln!(summary, "pruned: {} missing entr{plural}", removed.len());
    report_mutation(
        g,
        scope,
        registry::PATH_VALUE,
        "prune",
        &before_raw,
        &after_raw,
        &summary,
    );
    Ok(0)
}

pub fn move_entry(reg: &Registry, g: &Global, scope: Scope, from: usize, to: usize) -> Result<u8> {
    let (before_raw, before_ty) = current_path(reg, scope)?;
    let mut entries = pathops::parse(&before_raw);
    pathops::reorder(&mut entries, from, to).map_err(|e| AppError::Usage(e.to_string()))?;
    let after_raw = entries.join(";");
    if g.dry_run {
        print_changes(g.json, &pathops::parse(&before_raw), &entries);
        return Ok(0);
    }
    if after_raw == before_raw {
        return Err(AppError::NoOp("entry already at that position".into()));
    }
    confirm(g, &format!("move entry {from} to position {to}"))?;
    if commit(
        reg,
        g,
        scope,
        registry::PATH_VALUE,
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
        registry::PATH_VALUE,
        "move",
        &before_raw,
        &after_raw,
        &format!("moved: {from} -> {to}"),
    );
    Ok(0)
}
