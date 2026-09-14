//! Snapshot store: JSON before/after records that back `undo` and `diff`.
//!
//! Crash-safety: a snapshot is written atomically (temp + fsync +
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
    /// Registry value type of `before`, when known, so undo of a delete can
    /// restore `REG_EXPAND_SZ` faithfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<u32>,
}

impl Snapshot {
    #[cfg(test)]
    pub fn new(scope: &str, name: &str, before: &str, after: &str, command: &str) -> Self {
        Self::with_ty(scope, name, before, after, command, None)
    }

    pub fn with_ty(
        scope: &str,
        name: &str,
        before: &str,
        after: &str,
        command: &str,
        ty: Option<u32>,
    ) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static LAST_TS: AtomicU64 = AtomicU64::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
            .unwrap_or_default();
        // Monotonic in-process clock: two rapid saves must never share a
        // timestamp, otherwise the second file overwrites the first.
        let mut ts = now.max(LAST_TS.load(Ordering::Relaxed).saturating_add(1));
        loop {
            let last = LAST_TS.load(Ordering::Relaxed);
            if ts <= last {
                ts = last.saturating_add(1);
            }
            if LAST_TS
                .compare_exchange_weak(last, ts, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        Self {
            scope: scope.to_string(),
            name: name.to_string(),
            before: before.to_string(),
            after: after.to_string(),
            command: command.to_string(),
            ts: u128::from(ts),
            ty,
        }
    }
}

/// Default store dir: `%LOCALAPPDATA%\pathctl\snapshots`; `PATHCTL_SNAPSHOT_DIR`
/// overrides it (CLI tests).
pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PATHCTL_SNAPSHOT_DIR") {
        return PathBuf::from(d);
    }
    // Never fall back to "." -- that would pollute the caller's CWD with
    // snapshots. Use the OS temp dir when LOCALAPPDATA is unavailable.
    match std::env::var_os("LOCALAPPDATA") {
        Some(base) => PathBuf::from(base).join("pathctl").join("snapshots"),
        None => std::env::temp_dir().join("pathctl").join("snapshots"),
    }
}

pub fn save(s: &Snapshot) -> io::Result<PathBuf> {
    save_at(&dir(), s)
}

fn save_at(dir: &Path, s: &Snapshot) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    // Cross-process collision guard: if another process already claimed this
    // timestamp, bump forward until the filename is free (bounded retries).
    let mut owned = s.clone();
    for _ in 0..1000 {
        let path = dir.join(format!("{}.json", owned.ts));
        if !path.exists() {
            let tmp = dir.join(format!("{}.json.tmp", owned.ts));
            let data = serde_json::to_vec_pretty(&owned).map_err(io::Error::other)?;
            fs::write(&tmp, &data)?;
            fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
            fs::rename(&tmp, &path)?;
            return Ok(path);
        }
        // Same timestamp taken: keep the journal lossless by moving forward.
        // If the existing file is byte-identical, overwriting would be safe,
        // but bumping is simpler and still sorts correctly.
        owned.ts = owned.ts.saturating_add(1);
    }
    Err(io::Error::other("snapshot timestamp collision: retries exhausted"))
}

pub fn list() -> io::Result<Vec<Snapshot>> {
    list_at(&dir())
}

fn list_at(dir: &Path) -> io::Result<Vec<Snapshot>> {
    list_with_paths(dir)
        .map(|v| v.into_iter().map(|(s, _)| s).collect())
}

fn list_with_paths(dir: &Path) -> io::Result<Vec<(Snapshot, PathBuf)>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Snapshot>(&text) {
                Ok(s) => out.push((s, path)),
                Err(e) => eprintln!(
                    "warning: ignoring corrupt snapshot {}: {e}",
                    path.display()
                ),
            },
            Err(e) => eprintln!(
                "warning: ignoring unreadable snapshot {}: {e}",
                path.display()
            ),
        }
    }
    out.sort_by_key(|(s, _)| s.ts);
    Ok(out)
}

/// Drop oldest snapshots beyond `MAX_SNAPSHOTS`. Returns the number removed.
pub fn prune() -> io::Result<usize> {
    prune_at(&dir())
}

fn prune_at(dir: &Path) -> io::Result<usize> {
    // Best-effort cleanup of stale crash leftovers.
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let snaps = list_with_paths(dir)?;
    let mut removed = 0;
    if snaps.len() > MAX_SNAPSHOTS {
        for (_, path) in &snaps[..snaps.len() - MAX_SNAPSHOTS] {
            if fs::remove_file(path).is_ok() {
                removed += 1;
            }
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
