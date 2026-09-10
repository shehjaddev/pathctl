//! CLI integration tests: exercise the real binary end to end against an
//! isolated registry test key (`Software\pathctl-test-<name>`) and a temp
//! snapshot dir. The real HKCU\Environment key is never touched (spec §8).
//! A real-path smoke test is gated behind PATHCTL_TEST_REAL=1 (see
//! `real_path_gated`).

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use std::process::Output;

/// Run the binary against an isolated test key + snapshot dir, with
/// confirmation skipped and broadcasts disabled.
fn pathctl(name: &str, snap_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("pathctl").unwrap();
    cmd.env("PATHCTL_TEST_REG", name)
        .env("PATHCTL_SNAPSHOT_DIR", snap_dir)
        .arg("--no-broadcast")
        .arg("-y");
    cmd
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn cleanup(name: &str) {
    let _ = winreg::HKCU.delete_subkey_all(format!(r"Software\pathctl-test-{name}"));
}

/// Isolated test context: a temp snapshot dir plus a registry test key
/// that is removed before *and* after the test (even on failure), so
/// passing or failing runs never litter `HKCU\Software\pathctl-test-*`.
struct TestCtx {
    tmp: tempfile::TempDir,
    name: String,
}

impl TestCtx {
    fn path(&self) -> &Path {
        self.tmp.path()
    }
}

impl Drop for TestCtx {
    fn drop(&mut self) {
        cleanup(&self.name);
    }
}

fn setup(name: &str) -> TestCtx {
    cleanup(name);
    TestCtx {
        tmp: tempfile::tempdir().unwrap(),
        name: name.to_string(),
    }
}

// ---------------------------------------------------------------------------
// list / check
// ---------------------------------------------------------------------------

#[test]
fn empty_key_lists_nothing() {
    let dir = setup("empty_list");
    pathctl("empty_list", dir.path())
        .arg("list")
        .assert()
        .success()
        .stdout("");
}

#[test]
fn check_is_clean_on_empty_key() {
    let dir = setup("check_clean");
    pathctl("check_clean", dir.path())
        .arg("check")
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("OK"));
}

#[test]
fn check_flags_missing_directory_exit_1() {
    let dir = setup("check_missing");
    let missing = r"C:\pathctl-definitely-missing-xyz";
    pathctl("check_missing", dir.path())
        .arg("add")
        .arg(missing)
        .assert()
        .success();
    pathctl("check_missing", dir.path())
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("missing directory"));
}

// ---------------------------------------------------------------------------
// add / remove / dedupe / move
// ---------------------------------------------------------------------------

#[test]
fn add_then_list_shows_entry() {
    let dir = setup("add_list");
    let entry = r"C:\pathctl-test-bin";
    pathctl("add_list", dir.path())
        .arg("add")
        .arg(entry)
        .assert()
        .success()
        .stdout(predicate::str::contains("added"));
    pathctl("add_list", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(entry));
}

#[test]
fn add_prepend_puts_entry_first() {
    let dir = setup("add_prepend");
    let a = r"C:\pathctl-p-a";
    let b = r"C:\pathctl-p-b";
    pathctl("add_prepend", dir.path()).arg("add").arg(a).assert().success();
    pathctl("add_prepend", dir.path())
        .arg("add")
        .arg(b)
        .arg("--prepend")
        .assert()
        .success();
    let out = pathctl("add_prepend", dir.path())
        .arg("list")
        .arg("--raw")
        .output()
        .unwrap();
    let text = stdout(&out);
    let pos_b = text.find(b).expect("b present");
    let pos_a = text.find(a).expect("a present");
    assert!(pos_b < pos_a, "prepend entry must come first");
}

#[test]
fn add_dedupe_noop_exits_4() {
    let dir = setup("add_dedupe_noop");
    let entry = r"C:\pathctl-dd";
    pathctl("add_dedupe_noop", dir.path())
        .arg("add")
        .arg(entry)
        .assert()
        .success();
    pathctl("add_dedupe_noop", dir.path())
        .arg("add")
        .arg(entry)
        .arg("--dedupe")
        .assert()
        .code(4);
}

#[test]
fn add_duplicate_without_dedupe_is_allowed() {
    let dir = setup("add_dup_allowed");
    let entry = r"C:\pathctl-dup";
    pathctl("add_dup_allowed", dir.path()).arg("add").arg(entry).assert().success();
    pathctl("add_dup_allowed", dir.path()).arg("add").arg(entry).assert().success();
    pathctl("add_dup_allowed", dir.path())
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("duplicate"));
}

#[test]
fn dry_run_writes_nothing() {
    let dir = setup("dry_run");
    let entry = r"C:\pathctl-dry";
    pathctl("dry_run", dir.path())
        .arg("add")
        .arg(entry)
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains('+'));
    pathctl("dry_run", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(entry).not());
}

#[test]
fn remove_by_path_and_by_index() {
    let dir = setup("remove");
    let a = r"C:\pathctl-rm-a";
    let b = r"C:\pathctl-rm-b";
    pathctl("remove", dir.path()).arg("add").arg(a).assert().success();
    pathctl("remove", dir.path()).arg("add").arg(b).assert().success();
    // by path
    pathctl("remove", dir.path())
        .arg("remove")
        .arg(a)
        .assert()
        .success()
        .stdout(predicate::str::contains("removed"));
    // by #index
    pathctl("remove", dir.path())
        .arg("remove")
        .arg("#1")
        .assert()
        .success();
    pathctl("remove", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout("");
}

#[test]
fn remove_missing_exits_4() {
    let dir = setup("remove_missing");
    pathctl("remove_missing", dir.path())
        .arg("remove")
        .arg(r"C:\pathctl-not-there")
        .assert()
        .code(4);
}

#[test]
fn remove_numeric_dir_name_prefers_path_over_index() {
    let dir = setup("remove_numeric");
    pathctl("remove_numeric", dir.path())
        .arg("add")
        .arg(r"C:\pathctl-num-a")
        .assert()
        .success();
    // A directory literally named `123` must be removable by path even
    // though it also parses as an index.
    pathctl("remove_numeric", dir.path()).arg("add").arg("123").assert().success();
    pathctl("remove_numeric", dir.path())
        .arg("remove")
        .arg("123")
        .assert()
        .success()
        .stdout(predicate::str::contains("removed: 123"));
    pathctl("remove_numeric", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(r"C:\pathctl-num-a"))
        .stdout(predicate::str::contains("123").not());
}

#[test]
fn move_dry_run_reports_moved_entries() {
    let dir = setup("move_dry");
    pathctl("move_dry", dir.path()).arg("add").arg("a").assert().success();
    pathctl("move_dry", dir.path()).arg("add").arg("b").assert().success();
    pathctl("move_dry", dir.path())
        .arg("move")
        .arg("2")
        .arg("1")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains('~'));
}

#[test]
fn import_restores_path_registry_type() {
    let dir = setup("import_ty");
    let entry = r"C:\pathctl-ty";
    pathctl("import_ty", dir.path()).arg("add").arg(entry).assert().success();
    let export_file = dir.path().join("ty.json");
    pathctl("import_ty", dir.path())
        .arg("export")
        .arg("--output")
        .arg(&export_file)
        .assert()
        .success();
    let text = std::fs::read_to_string(&export_file).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    // Flip the exported PATH type (2 <-> 1) and reimport; the stored
    // type must follow the file, not the pre-existing value.
    let cur_ty = v["path"]["user"]["ty"].as_u64().unwrap();
    let new_ty = if cur_ty == 2 { 1 } else { 2 };
    v["path"]["user"]["ty"] = serde_json::json!(new_ty);
    std::fs::write(&export_file, serde_json::to_string(&v).unwrap()).unwrap();
    pathctl("import_ty", dir.path())
        .arg("import")
        .arg(&export_file)
        .assert()
        .success();
    let out = pathctl("import_ty", dir.path())
        .arg("export")
        .output()
        .unwrap();
    let v2: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v2["path"]["user"]["ty"].as_u64().unwrap(), new_ty);
    assert!(v2["path"]["user"]["value"].as_str().unwrap().contains(entry));
}

#[test]
fn dedupe_keeps_first_and_second_run_noops() {
    let dir = setup("dedupe");
    let entry = r"C:\pathctl-dedupe";
    pathctl("dedupe", dir.path()).arg("add").arg(entry).assert().success();
    pathctl("dedupe", dir.path()).arg("add").arg(entry).assert().success();
    pathctl("dedupe", dir.path())
        .arg("dedupe")
        .assert()
        .success()
        .stdout(predicate::str::contains("1 duplicate"));
    pathctl("dedupe", dir.path()).arg("dedupe").assert().code(4);
    let out = pathctl("dedupe", dir.path())
        .arg("list")
        .arg("--raw")
        .output()
        .unwrap();
    assert_eq!(stdout(&out).matches(entry).count(), 1, "entry appears once");
}

#[test]
fn move_reorders_entries() {
    let dir = setup("move");
    let a = r"C:\pathctl-mv-a";
    let b = r"C:\pathctl-mv-b";
    pathctl("move", dir.path()).arg("add").arg(a).assert().success();
    pathctl("move", dir.path()).arg("add").arg(b).assert().success();
    pathctl("move", dir.path())
        .arg("move")
        .arg("2")
        .arg("1")
        .assert()
        .success();
    let out = pathctl("move", dir.path())
        .arg("list")
        .arg("--raw")
        .output()
        .unwrap();
    let text = stdout(&out);
    assert!(text.find(b).unwrap() < text.find(a).unwrap());
    pathctl("move", dir.path())
        .arg("move")
        .arg("0")
        .arg("1")
        .assert()
        .code(2);
}

#[test]
fn prune_removes_missing_keeps_existing() {
    let dir = setup("prune");
    let missing = r"C:\pathctl-prune-missing";
    let existing = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    pathctl("prune", dir.path()).arg("add").arg(missing).assert().success();
    pathctl("prune", dir.path()).arg("add").arg(&existing).assert().success();
    pathctl("prune", dir.path())
        .arg("prune")
        .assert()
        .success()
        .stdout(predicate::str::contains("pruned: 1 missing"));
    let out = pathctl("prune", dir.path())
        .arg("list")
        .arg("--raw")
        .output()
        .unwrap();
    let text = stdout(&out);
    assert!(text.contains(&existing), "existing dir must be kept");
    assert!(!text.contains(missing), "missing dir must be pruned");
    pathctl("prune", dir.path()).arg("prune").assert().code(4);
}

#[test]
fn prune_dry_run_writes_nothing() {
    let dir = setup("prune_dry");
    let missing = r"C:\pathctl-prune-dry-missing";
    pathctl("prune_dry", dir.path()).arg("add").arg(missing).assert().success();
    pathctl("prune_dry", dir.path())
        .arg("prune")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains('-'));
    pathctl("prune_dry", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(missing));
}

// ---------------------------------------------------------------------------
// undo / diff
// ---------------------------------------------------------------------------

#[test]
fn undo_restores_and_is_itself_undoable() {
    let dir = setup("undo");
    let entry = r"C:\pathctl-undo";
    pathctl("undo", dir.path()).arg("add").arg(entry).assert().success();
    pathctl("undo", dir.path())
        .arg("undo")
        .assert()
        .success()
        .stdout(predicate::str::contains("restored"));
    pathctl("undo", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(entry).not());
    // undo of the undo brings it back
    pathctl("undo", dir.path()).arg("undo").assert().success();
    pathctl("undo", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(entry));
}

#[test]
fn undo_to_specific_snapshot() {
    let dir = setup("undo_to");
    let a = r"C:\pathctl-uto-a";
    let b = r"C:\pathctl-uto-b";
    pathctl("undo_to", dir.path()).arg("add").arg(a).assert().success();
    pathctl("undo_to", dir.path()).arg("add").arg(b).assert().success();
    // snapshot 1 = "" -> a ; snapshot 2 = "a" -> "a;b"
    pathctl("undo_to", dir.path())
        .arg("undo")
        .arg("--to")
        .arg("1")
        .assert()
        .success();
    pathctl("undo_to", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout("");
}

#[test]
fn undo_with_nothing_exits_4() {
    let dir = setup("undo_empty");
    pathctl("undo_empty", dir.path()).arg("undo").assert().code(4);
}

#[test]
fn undo_kind_filter_selects_domain() {
    let dir = setup("undo_kind");
    let entry = r"C:\pathctl-uk";
    pathctl("undo_kind", dir.path()).arg("add").arg(entry).assert().success();
    pathctl("undo_kind", dir.path())
        .arg("env")
        .arg("set")
        .arg("PATHCTL_UK_FLAG")
        .arg("1")
        .assert()
        .success();
    pathctl("undo_kind", dir.path())
        .arg("env")
        .arg("set")
        .arg("PATHCTL_UK_FLAG")
        .arg("2")
        .assert()
        .success();
    // Newest snapshot is the env var; --kind path must skip it and undo the add.
    pathctl("undo_kind", dir.path())
        .arg("undo")
        .arg("--kind")
        .arg("path")
        .assert()
        .success()
        .stdout(predicate::str::contains("restored Path"));
    pathctl("undo_kind", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(entry).not());
    // The env var was untouched by the path undo.
    pathctl("undo_kind", dir.path())
        .arg("env")
        .arg("get")
        .arg("PATHCTL_UK_FLAG")
        .assert()
        .success()
        .stdout("2\n");
    // --kind env targets the env snapshots only.
    pathctl("undo_kind", dir.path())
        .arg("undo")
        .arg("--kind")
        .arg("env")
        .assert()
        .success()
        .stdout(predicate::str::contains("restored PATHCTL_UK_FLAG"));
    pathctl("undo_kind", dir.path())
        .arg("env")
        .arg("get")
        .arg("PATHCTL_UK_FLAG")
        .assert()
        .success()
        .stdout("1\n");
}

#[test]
fn undo_kind_bogus_value_exits_2() {
    let dir = setup("undo_kind_bad");
    pathctl("undo_kind_bad", dir.path())
        .arg("undo")
        .arg("--kind")
        .arg("bogus")
        .assert()
        .code(2);
}

#[test]
fn undo_to_zero_exits_2() {
    let dir = setup("undo_zero");
    pathctl("undo_zero", dir.path())
        .arg("add")
        .arg(r"C:\pathctl-uz")
        .assert()
        .success();
    // `--to 0` used to underflow (panic in debug builds); must be a usage error.
    pathctl("undo_zero", dir.path())
        .arg("undo")
        .arg("--to")
        .arg("0")
        .assert()
        .code(2);
}

#[test]
fn diff_without_baseline_reports_no_drift() {
    let dir = setup("diff_nobase");
    // No mutations → no snapshots. Must not flag the whole PATH as drift.
    pathctl("diff_nobase", dir.path())
        .arg("diff")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("no snapshots recorded yet"));
}

#[test]
fn read_commands_do_not_create_the_key() {
    let name = "list_nocreate";
    cleanup(name);
    let dir = tempfile::tempdir().unwrap();
    pathctl(name, dir.path()).arg("list").assert().success();
    let exists = winreg::HKCU
        .open_subkey(format!(r"Software\pathctl-test-{name}"))
        .is_ok();
    assert!(!exists, "read-only list must not create the registry key");
}

#[test]
fn diff_to_out_of_range_exits_2() {
    let dir = setup("diff_range");
    pathctl("diff_range", dir.path())
        .arg("add")
        .arg(r"C:\pathctl-dr")
        .assert()
        .success();
    // Used to silently diff against an empty base (false drift, exit 1).
    pathctl("diff_range", dir.path())
        .arg("diff")
        .arg("--to")
        .arg("99")
        .assert()
        .code(2);
}

#[test]
fn diff_scope_all_json_parses_as_one_document() {
    let dir = setup("diff_json");
    pathctl("diff_json", dir.path())
        .arg("add")
        .arg(r"C:\pathctl-dj")
        .assert()
        .success();
    let out = pathctl("diff_json", dir.path())
        .arg("diff")
        .arg("--scope")
        .arg("all")
        .arg("--json")
        .output()
        .unwrap();
    // Was two concatenated JSON objects; must be a single parseable document.
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("single JSON document");
    assert!(v.is_array());
    assert_eq!(v.as_array().unwrap().len(), 2, "user + system scopes");
}

#[test]
fn import_ignores_reserved_path_variable() {
    let dir = setup("import_pathvar");
    let entry = r"C:\pathctl-ipv";
    pathctl("import_pathvar", dir.path())
        .arg("add")
        .arg(entry)
        .assert()
        .success();
    // Hand-crafted file with a rogue `Path` entry in variables.user that would
    // overwrite the legit path.user value if imported naively.
    let f = dir.path().join("rogue.json");
    let rogue = serde_json::json!({
        "tool": "pathctl",
        "version": 1,
        "path": { "user": { "value": entry, "ty": 2 }, "system": null },
        "variables": { "user": [{ "name": "Path", "value": r"C:\evil", "ty": 2 }] },
    });
    std::fs::write(&f, serde_json::to_string(&rogue).unwrap()).unwrap();
    // Nothing applicable changes (path.user already matches; the rogue
    // `Path` variable is skipped), so import honestly reports no-op.
    pathctl("import_pathvar", dir.path())
        .arg("import")
        .arg(&f)
        .assert()
        .code(4);
    pathctl("import_pathvar", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(entry))
        .stdout(predicate::str::contains(r"C:\evil").not());
}

#[test]
fn diff_tracks_external_drift_and_clears_after_undo() {
    let dir = setup("diff");
    let entry = r"C:\pathctl-diff";
    pathctl("diff", dir.path()).arg("add").arg(entry).assert().success();
    // recorded state matches current → clean
    pathctl("diff", dir.path()).arg("diff").assert().code(0);
    // simulate external drift by writing the test key behind the tool's back
    let drift = r"C:\pathctl-external-drift";
    write_path_direct("diff", &format!("{entry};{drift}"));
    pathctl("diff", dir.path())
        .arg("diff")
        .assert()
        .code(1)
        .stdout(predicate::str::contains('+'));
    // undo restores the recorded before; diff clears
    pathctl("diff", dir.path()).arg("undo").assert().success();
    pathctl("diff", dir.path()).arg("diff").assert().code(0);
}

/// Write the Path value of a test key directly, bypassing the binary.
fn write_path_direct(name: &str, value: &str) {
    use winreg::enums::{KEY_READ, KEY_WRITE, REG_EXPAND_SZ};
    use winreg::RegValue;
    let key = winreg::HKCU
        .open_subkey_with_flags(
            format!(r"Software\pathctl-test-{name}\user"),
            KEY_READ | KEY_WRITE,
        )
        .unwrap();
    let mut wide: Vec<u16> = value.encode_utf16().collect();
    wide.push(0);
    let mut bytes = Vec::new();
    for u in wide {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    key.set_raw_value("Path", &RegValue { bytes: bytes.into(), vtype: REG_EXPAND_SZ })
        .unwrap();
}

// ---------------------------------------------------------------------------
// env vars
// ---------------------------------------------------------------------------

#[test]
fn env_set_get_delete_roundtrip() {
    let dir = setup("env");
    let name = "PATHCTL_TEST_VAR";
    pathctl("env", dir.path())
        .arg("env")
        .arg("set")
        .arg(name)
        .arg("hello world")
        .assert()
        .success();
    pathctl("env", dir.path())
        .arg("env")
        .arg("get")
        .arg(name)
        .assert()
        .success()
        .stdout("hello world\n");
    // idempotent set → no-op
    pathctl("env", dir.path())
        .arg("env")
        .arg("set")
        .arg(name)
        .arg("hello world")
        .assert()
        .code(4);
    pathctl("env", dir.path())
        .arg("env")
        .arg("delete")
        .arg(name)
        .assert()
        .success();
    pathctl("env", dir.path())
        .arg("env")
        .arg("get")
        .arg(name)
        .assert()
        .code(4);
}

#[test]
fn env_set_is_undoable() {
    let dir = setup("env_undo");
    let name = "PATHCTL_TEST_VAR2";
    pathctl("env_undo", dir.path())
        .arg("env")
        .arg("set")
        .arg(name)
        .arg("v1")
        .assert()
        .success();
    pathctl("env_undo", dir.path())
        .arg("env")
        .arg("set")
        .arg(name)
        .arg("v2")
        .assert()
        .success();
    pathctl("env_undo", dir.path())
        .arg("undo")
        .assert()
        .success();
    pathctl("env_undo", dir.path())
        .arg("env")
        .arg("get")
        .arg(name)
        .assert()
        .success()
        .stdout("v1\n");
}

#[test]
fn guard_refuses_oversized_value_on_import() {
    let dir = setup("guard");
    let big = "x".repeat(33_000);
    let json = format!(
        r#"{{"path":{{}},"variables":{{"user":[{{"name":"PATHCTL_BIG_VAR","value":"{big}","ty":1}}]}}}}"#
    );
    let file = dir.path().join("big.json");
    std::fs::write(&file, &json).unwrap();
    // The 33,000-char value cannot be passed on argv (Windows 32,767-char
    // command-line limit), so the guard is exercised through import instead.
    pathctl("guard", dir.path())
        .arg("import")
        .arg(&file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("limit"));
    // nothing may have been written
    pathctl("guard", dir.path())
        .arg("env")
        .arg("get")
        .arg("PATHCTL_BIG_VAR")
        .assert()
        .code(4);
}

// ---------------------------------------------------------------------------
// scope / exit codes / json
// ---------------------------------------------------------------------------

#[test]
fn mutating_all_scope_is_rejected() {
    let dir = setup("scope_all");
    pathctl("scope_all", dir.path())
        .arg("add")
        .arg(r"C:\x")
        .arg("--scope")
        .arg("all")
        .assert()
        .code(2);
}

#[test]
fn unknown_scope_is_rejected() {
    let dir = setup("scope_bad");
    pathctl("scope_bad", dir.path())
        .arg("list")
        .arg("--scope")
        .arg("bogus")
        .assert()
        .code(2);
}

#[test]
fn json_output_parses() {
    let dir = setup("json");
    let entry = r"C:\pathctl-json";
    pathctl("json", dir.path()).arg("add").arg(entry).assert().success();
    let out = pathctl("json", dir.path())
        .arg("list")
        .arg("--json")
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let entries = v[0]["entries"].as_array().expect("entries array");
    assert_eq!(entries[0]["entry"], entry);
    assert_eq!(entries[0]["index"], 1);
}

#[test]
fn usage_error_exits_2() {
    let dir = setup("usage");
    pathctl("usage", dir.path()).arg("bogus-command").assert().code(2);
}

#[test]
fn completions_generate_for_each_shell() {
    let dir = setup("completions");
    for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
        let out = pathctl("completions", dir.path())
            .arg("completions")
            .arg(shell)
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell} completions must succeed");
        let text = stdout(&out);
        assert!(!text.is_empty(), "{shell} completions must not be empty");
        assert!(
            text.contains("pathctl"),
            "{shell} completions must reference the binary name"
        );
    }
}

// ---------------------------------------------------------------------------
// export / import
// ---------------------------------------------------------------------------

#[test]
fn export_import_roundtrip() {
    let dir = setup("export");
    let name = "PATHCTL_EXPORT_VAR";
    let entry = r"C:\pathctl-export";
    pathctl("export", dir.path()).arg("add").arg(entry).assert().success();
    pathctl("export", dir.path())
        .arg("env")
        .arg("set")
        .arg(name)
        .arg("exported-value")
        .assert()
        .success();
    let export_file = dir.path().join("backup.json");
    pathctl("export", dir.path())
        .arg("export")
        .arg("--output")
        .arg(&export_file)
        .assert()
        .success();

    // mutate state away
    pathctl("export", dir.path()).arg("remove").arg(entry).assert().success();
    pathctl("export", dir.path())
        .arg("env")
        .arg("delete")
        .arg(name)
        .assert()
        .success();

    // import restores (merge)
    pathctl("export", dir.path())
        .arg("import")
        .arg(&export_file)
        .assert()
        .success();
    pathctl("export", dir.path())
        .arg("env")
        .arg("get")
        .arg(name)
        .assert()
        .success()
        .stdout("exported-value\n");
    pathctl("export", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains(entry));
}

#[test]
fn export_excludes_path_from_variables() {
    let dir = setup("export_dedupe");
    let entry = r"C:\pathctl-ed";
    pathctl("export_dedupe", dir.path()).arg("add").arg(entry).assert().success();
    pathctl("export_dedupe", dir.path())
        .arg("env")
        .arg("set")
        .arg("PATHCTL_ED_VAR")
        .arg("v")
        .assert()
        .success();
    let out = pathctl("export_dedupe", dir.path())
        .arg("export")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid export JSON");
    // Path is represented exactly once, in path.user — not repeated in variables.user.
    assert!(v["path"]["user"]["value"].as_str().unwrap().contains(entry));
    let vars = v["variables"]["user"].as_array().unwrap();
    assert!(
        vars.iter()
            .all(|x| !x["name"].as_str().unwrap().eq_ignore_ascii_case("path")),
        "Path must not be duplicated in variables.user"
    );
    assert_eq!(vars.len(), 1, "only PATHCTL_ED_VAR should remain");
}

#[test]
fn export_refuses_non_windows_output_path() {
    let dir = setup("export_posix");
    pathctl("export_posix", dir.path())
        .arg("export")
        .arg("--output")
        .arg("/tmp/export.json")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("non-Windows"));
}

#[test]
fn import_rejects_bad_json() {
    let dir = setup("import_bad");
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "not json").unwrap();
    pathctl("import_bad", dir.path())
        .arg("import")
        .arg(&bad)
        .assert()
        .code(2);
}

#[test]
fn import_with_nothing_to_do_exits_4_without_prompting() {
    let dir = setup("import_noop");
    let entry = r"C:\pathctl-inoop";
    pathctl("import_noop", dir.path()).arg("add").arg(entry).assert().success();
    let export_file = dir.path().join("noop.json");
    pathctl("import_noop", dir.path())
        .arg("export")
        .arg("--output")
        .arg(&export_file)
        .assert()
        .success();
    // Reimporting the just-written state changes nothing: exit 4, and no
    // confirmation prompt (would hang without -y; succeeds here either way
    // but must not write or print completion).
    pathctl("import_noop", dir.path())
        .arg("import")
        .arg(&export_file)
        .assert()
        .code(4);
}

#[test]
fn list_raw_validates_resolved_entries() {
    let dir = setup("list_raw_vars");
    let existing = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    unsafe { std::env::set_var("PATHCTL_LIST_RAW_DIR", &existing) };
    pathctl("list_raw_vars", dir.path())
        .arg("add")
        .arg("%PATHCTL_LIST_RAW_DIR%")
        .assert()
        .success();
    // Resolvable entry: no missing/unresolvable flags even in --raw mode.
    pathctl("list_raw_vars", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains("%PATHCTL_LIST_RAW_DIR%"))
        .stdout(predicate::str::contains('[').not());
    pathctl("list_raw_vars", dir.path())
        .arg("add")
        .arg("%PATHCTL_DEFINITELY_UNRESOLVABLE_XYZ%")
        .assert()
        .success();
    pathctl("list_raw_vars", dir.path())
        .arg("list")
        .arg("--raw")
        .assert()
        .success()
        .stdout(predicate::str::contains("[%!]"));
}

#[test]
#[ignore = "requires PATHCTL_TEST_REAL_SYSTEM=1 and an elevated shell (writes HKLM environment)"]
fn system_scope_gated() {
    if std::env::var_os("PATHCTL_TEST_REAL_SYSTEM").is_none() {
        eprintln!("skipped: PATHCTL_TEST_REAL_SYSTEM not set");
        return;
    }
    fn sys(snap: &Path) -> Command {
        let mut cmd = Command::cargo_bin("pathctl").unwrap();
        cmd.env("PATHCTL_SNAPSHOT_DIR", snap)
            .arg("--no-broadcast")
            .arg("-y")
            .arg("--scope")
            .arg("system");
        cmd
    }
    let name = "PATHCTL_SYSTEM_SMOKE";
    let snap = tempfile::tempdir().unwrap();
    // best-effort cleanup of a stale variable from a previous failed run
    let _ = sys(snap.path()).arg("env").arg("delete").arg(name).output();
    let before = sys(snap.path())
        .arg("env")
        .arg("get")
        .arg(name)
        .output()
        .unwrap();
    let before_value = stdout(&before);
    sys(snap.path())
        .arg("env")
        .arg("set")
        .arg(name)
        .arg("smoke-1")
        .assert()
        .success();
    sys(snap.path())
        .arg("env")
        .arg("get")
        .arg(name)
        .assert()
        .success()
        .stdout("smoke-1\n");
    sys(snap.path()).arg("undo").assert().success();
    if before_value.is_empty() {
        sys(snap.path())
            .arg("env")
            .arg("get")
            .arg(name)
            .assert()
            .code(4);
    }
}

// ---------------------------------------------------------------------------
// Real-path smoke test (opt-in; never runs in CI)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires PATHCTL_TEST_REAL=1 and writes the real user PATH"]
fn real_path_gated() {
    if std::env::var_os("PATHCTL_TEST_REAL").is_none() {
        eprintln!("skipped: PATHCTL_TEST_REAL not set");
        return;
    }
    fn real(snap: &Path) -> Command {
        let mut cmd = Command::cargo_bin("pathctl").unwrap();
        cmd.env("PATHCTL_SNAPSHOT_DIR", snap)
            .arg("--no-broadcast")
            .arg("-y");
        cmd
    }
    let name = "PATHCTL_REAL_SMOKE";
    let snap = tempfile::tempdir().unwrap();
    let before = real(snap.path())
        .arg("env")
        .arg("get")
        .arg(name)
        .output()
        .unwrap();
    let before_value = stdout(&before);
    real(snap.path())
        .arg("env")
        .arg("set")
        .arg(name)
        .arg("smoke-1")
        .assert()
        .success();
    real(snap.path())
        .arg("env")
        .arg("get")
        .arg(name)
        .assert()
        .success()
        .stdout("smoke-1\n");
    real(snap.path()).arg("undo").assert().success();
    if before_value.is_empty() {
        real(snap.path())
            .arg("env")
            .arg("get")
            .arg(name)
            .assert()
            .code(4);
    }
}
