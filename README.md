# pathctl

Safe PATH & environment-variable manager for Windows.

`pathctl` edits `PATH` (and any environment variable) in the Windows registry
directly, **never through `setx`**, which Microsoft documents to crop values
over 1,024 characters and flatten `%VAR%` references, losing data. Every
mutation is snapshotted before it is written, so it can always be undone.
Registry value types (`REG_EXPAND_SZ` vs `REG_SZ`) are preserved on every
write, so the .NET `SetEnvironmentVariable` gotcha that silently breaks `%VAR%`
expansion (dotnet/runtime#89695, unfixed by design) cannot happen here.

```powershell
pathctl add C:\tools --prepend     # put a directory first
pathctl add C:\tools               # or just append it
pathctl undo                       # changed your mind? restored
```

## Install

Download the release binary from [Releases](https://github.com/shehjaddev/pathctl/releases)
(single static exe, no dependencies; `sha256.txt` in the release lists its checksum),
or build from source: `cargo build --release` (Rust 1.88+, Windows).

## Commands

| Command | Purpose | Example |
|---|---|---|
| `pathctl list` | numbered entries; `!` missing dir, `d` duplicate, `%` unresolvable variable, `e` expandable (`%VAR%`) entry | `pathctl list --scope all` |
| `pathctl check` | analyze: dups, missing dirs, >260-char entries, near-limit (exit 1 if findings) | `pathctl check` |
| `pathctl add <dir>` | append (or `--prepend`) a directory; `--dedupe` = no-op if present | `pathctl add C:\tools --prepend` |
| `pathctl remove <dir\|#index>` | remove by path or 1-based list index | `pathctl remove 3` |
| `pathctl dedupe` | drop duplicates, keep first | `pathctl dedupe` |
| `pathctl prune` | remove entries whose directories no longer exist (dry-run to preview) | `pathctl prune --dry-run` |
| `pathctl move <from> <to>` | reorder (1-based) | `pathctl move 5 1` |
| `pathctl undo` | restore last snapshot (`--list` to browse, `--to <id>` specific, `--kind path\|env` to filter) | `pathctl undo --to 2` |
| `pathctl diff` | current PATH vs last recorded state (exit 1 if drifted; no baseline yet prints a note, exit 0) | `pathctl diff --json` |
| `pathctl export` / `pathctl import` | JSON backup/restore (merge; never truncates) | `pathctl export --output path.json` |
| `pathctl env get/set/delete` | any environment variable, same snapshot/undo safety; `set`/`delete` refuse `Path`, which the commands above own | `pathctl env set MY_FLAG 1` |
| `pathctl completions <shell>` | completion script for bash, elvish, fish, powershell or zsh | `pathctl completions powershell \| Out-String \| Invoke-Expression` |

Global flags: `--scope user|system|all` (default `user`; `all` is read-only.
`export` and `import` cover both scopes unless `--scope` narrows them),
`--json` (every command answers with one JSON document, `list`, `check`, `diff`
and `undo --list` included), `--dry-run` (prints the would-be diff, writes
nothing, and always exits 0), `-y` (skip confirmation), `--no-broadcast`
(batch scripts), `--elevate` (relaunch via UAC for system-scope writes).

### Exit codes

- `0` success
- `1` `check`/`diff` found findings
- `2` usage error
- `3` elevation required (system-scope write without admin)
- `4` no-op: nothing changed, or nothing to act on (`env get` reports it for
  "not set")
- `5` registry I/O error
- `6` other failure: snapshot store, file I/O, elevation launch, non-string
  registry value

## How it stays safe

- **Never `setx`** - registry writes go directly to `HKCU\Environment\Path`
  (user) or `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment\Path`
  (system), preserving the existing value type (`REG_EXPAND_SZ` stays
  `REG_EXPAND_SZ`; `%VAR%` entries stay literal).
- **Strings only** - a value that is not `REG_SZ` or `REG_EXPAND_SZ` is not a
  string: reading, writing or importing it as text would destroy it, so
  `pathctl` refuses and names the type instead. Nothing binary ever enters an
  export (which says which values it skipped).
- **Snapshot before write** - each mutation records `{before, after}` as JSON
  in `%LOCALAPPDATA%\pathctl\snapshots` (written atomically and fsynced
  *before* the registry write, so a crash mid-mutation is always undoable).
  `undo` restores `before` and is itself snapshotted, so undo is undoable.
  The 100 newest snapshots are kept.
- **Length guard** - writes that would exceed the 32,767-unit
  environment-variable limit are refused; a warning is printed as combined
  PATH approaches the ~2,048 practical limit. All length limits are counted in
  UTF-16 units, matching the registry.
- **`WM_SETTINGCHANGE` broadcast** - after each successful write, so Explorer
  and new processes pick up the change immediately.
- **Dry-run everywhere** - every mutating command prints the would-be diff
  with `--dry-run` and exits 0 without touching the registry, whether or not
  the real run would have changed anything.

## Development

```powershell
cargo test          # unit + CLI integration tests (isolated registry test keys)
cargo fmt --check   # must be clean
cargo clippy        # must be clean
```

Tests never touch the real PATH: they run against scratch keys under
`HKCU\Software\pathctl-test...` (controlled by `PATHCTL_TEST_REG`; unit tests
each take a key of their own and remove it when they finish, panics included)
and a temp snapshot dir (`PATHCTL_SNAPSHOT_DIR`). The two tests that do touch
the real environment are opt-in, and fail loudly if their gate is unset:

```powershell
$env:PATHCTL_TEST_REAL = "1"
cargo test --test cli real_path_gated -- --ignored

# writes HKLM, so it needs an elevated shell
$env:PATHCTL_TEST_REAL_SYSTEM = "1"
cargo test --test cli system_scope_gated -- --ignored
```

They set and undo a uniquely named variable (`PATHCTL_REAL_SMOKE`,
`PATHCTL_SYSTEM_SMOKE`).

## Notes

- `check` flags entries over 260 characters; such paths work when the OS
  setting `LongPathsEnabled` is on.
- Import never truncates: the 32,767-unit guard applies to every imported
  value too. A file that names another tool, or a format version newer than
  this build understands, is refused rather than half-applied.
- `export --output` takes Windows paths; POSIX/MSYS-style paths (`/c/...`) and
  drive-relative paths (`C:backup.json`) are refused with an error rather than
  silently writing somewhere else.
- Entries may be quoted (`"C:\Program Files\x"`); quotes are ignored when
  deciding whether an entry exists, so `prune` never removes a directory that
  is there.
