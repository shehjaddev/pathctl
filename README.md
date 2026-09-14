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

## JSON output

`--json` prints exactly one JSON document per run (never two concatenated
objects). Where a field can be absent, the shape below says which way: a
`check` finding carries only the fields its `kind` has, a snapshot omits `ty`
when the type is unknown, and an export writes `null` for a scope's PATH when
that value does not exist.

| Command | Shape |
|---|---|
| `list` | `[{"scope","entries":[{"index","entry","flags"}]}]`, one object per scope. `flags` holds any of `expanded`, `unresolvable`, `missing`, `dup` |
| `check` | `{"ok":bool,"findings":[{"kind","scope",...}]}`. `kind` is one of `duplicate`, `unresolvable`, `missing` (each with `entry`), `entry_too_long` (with `entry` and `limit`), `value_over_limit`, `value_near_limit` (each with `units` and `limit`) |
| `diff` | `[{"scope","base","current","changes"}]`. With no recorded baseline the object also carries `"note"` and empty `changes` |
| `undo --list` | the snapshots themselves: `[{"scope","name","before","after","command","ts","ty"}]` (`ts` is Unix nanoseconds, `ty` the registry type, omitted when unknown) |
| `env get` | `{"scope","name","value","ty"}`; an unset variable exits `4` and prints nothing |
| `add`, `remove`, `dedupe`, `prune`, `move`, `env set`, `env delete`, `undo` | `{"scope","name","action","before","after","changes"}`, where `action` is the command, `before`/`after` are the entry lists (a variable is a one-element list, empty when unset) and `changes` holds `{"added"\|"removed"\|"moved": entry}` objects |
| any mutation with `--dry-run` | `{"before","after","changes"}` -- the preview has no `scope`, `name` or `action` because nothing happened yet |
| `import` | `{"action":"import","applied":N,"changes":[...]}`; with `--dry-run` it prints the bare `changes` array. Each item is `{"scope","name","before","after"}`, plus `ty_before`/`ty_after` for PATH items |
| `export` | the backup file itself: `{"tool","version","path":{"user":{"value","ty"},"system":{...}\|null},"variables":{"user":[{"name","value","ty"}]}}`. This is also the input format for `import` |
| `completions` | a shell script, not JSON |

Example -- the same change, previewed and then applied:

```console
$ pathctl add C:\tools --dry-run --json
{"before":["C:\\bin"],"after":["C:\\bin","C:\\tools"],"changes":[{"added":"C:\\tools"}]}

$ pathctl add C:\tools --json
{"scope":"user","name":"Path","action":"add","before":["C:\\bin"],"after":["C:\\bin","C:\\tools"],"changes":[{"added":"C:\\tools"}]}
```

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
