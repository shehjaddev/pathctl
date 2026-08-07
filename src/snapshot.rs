//! Snapshot store: JSON before/after records that back `undo` and `diff`.
//!
//! Crash-safety (spec §4): a snapshot is written atomically (temp + fsync +
//! rename) *before* the registry write, so a crash mid-mutation always leaves
//! an undoable record.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_SNAPSHOTS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub scope: String,
    pub name: String,
    pub before: String,
    pub after: String,
    pub command: String,
    /// Unix nanos; the sort key and file name.
    pub ts: u128,
}

impl Snapshot {
    pub fn new(scope: &str, name: &str, before: &str, after: &str, command: &str) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Self {
            scope: scope.to_string(),
            name: name.to_string(),
            before: before.to_string(),
            after: after.to_string(),
            command: command.to_string(),
            ts,
        }
    }
}

/// Default store dir: `%LOCALAPPDATA%\pathctl\snapshots`; `PATHCTL_SNAPSHOT_DIR`
/// overrides it (CLI tests).
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PATHCTL_SNAPSHOT_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| ".".into());
    PathBuf::from(base).join("pathctl").join("snapshots")
}

pub fn save(s: &Snapshot) -> io::Result<PathBuf> {
    save_at(&dir(), s)
}

fn save_at(dir: &Path, s: &Snapshot) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", s.ts));
    let tmp = dir.join(format!("{}.json.tmp", s.ts));
    let data = serde_json::to_vec_pretty(s).map_err(|e| io::Error::other(e))?;
    fs::write(&tmp, data)?;
    fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn list() -> io::Result<Vec<Snapshot>> {
    list_at(&dir())
}

fn list_at(dir: &Path) -> io::Result<Vec<Snapshot>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".json") {
            if let Some(s) = fs::read_to_string(entry.path())
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
            {
                out.push(s);
            }
        }
    }
    out.sort_by_key(|s| s.ts);
    Ok(out)
}

/// Drop oldest snapshots beyond `MAX_SNAPSHOTS`. Returns the number removed.
pub fn prune() -> io::Result<usize> {
    prune_at(&dir())
}

fn prune_at(dir: &Path) -> io::Result<usize> {
    let snaps = list_at(dir)?;
    let mut removed = 0;
    if snaps.len() > MAX_SNAPSHOTS {
        for s in &snaps[..snaps.len() - MAX_SNAPSHOTS] {
            let _ = fs::remove_file(dir.join(format!("{}.json", s.ts)));
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_is_atomic_and_list_is_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let a = Snapshot::new("user", "Path", "", "A", "add");
        let b = Snapshot::new("user", "Path", "A", "B", "add");
        save_at(dir.path(), &a).unwrap();
        save_at(dir.path(), &b).unwrap();
        let snaps = list_at(dir.path()).unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].after, "A");
        assert_eq!(snaps[1].after, "B");
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.ends_with(".tmp")),
            "no temp files may remain"
        );
    }

    #[test]
    fn prune_keeps_newest_hundred() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..105u32 {
            let mut s = Snapshot::new("user", "Path", "", &format!("v{i}"), "add");
            s.ts = i as u128 + 1_000_000; // strictly increasing, deterministic
            save_at(dir.path(), &s).unwrap();
        }
        assert_eq!(prune_at(dir.path()).unwrap(), 5);
        let snaps = list_at(dir.path()).unwrap();
        assert_eq!(snaps.len(), MAX_SNAPSHOTS);
        assert_eq!(snaps[0].after, "v5");
    }
}
