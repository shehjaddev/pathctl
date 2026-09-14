//! pathctl — safe PATH & environment-variable manager for Windows.

mod commands;
mod elevate;
mod notify;
mod pathops;
mod registry;
mod snapshot;
mod util;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use commands::{Global, Result};
use registry::Registry;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "pathctl",
    version,
    about = "Safe PATH & environment-variable manager for Windows: list, dedupe, reorder, edit with undo/diff/JSON"
)]
struct Cli {
    /// Scope to operate on: user (default), system, or all (read commands only)
    #[arg(long, global = true, value_name = "user|system|all")]
    scope: Option<String>,

    /// Machine-readable JSON output
    #[arg(long, global = true)]
    json: bool,

    /// Print the would-be change and exit 0 without writing anything
    #[arg(long, global = true)]
    dry_run: bool,

    /// Skip confirmation prompts
    #[arg(short = 'y', long, global = true)]
    yes: bool,

    /// Do not broadcast WM_SETTINGCHANGE after writes (batch scripts)
    #[arg(long, global = true)]
    no_broadcast: bool,

    /// Relaunch elevated (UAC) when a system-scope write needs admin
    #[arg(long, global = true)]
    elevate: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List PATH entries; flags: ! missing dir, d duplicate, % unresolvable variable
    List {
        /// Show registry literals instead of expanded values
        #[arg(long)]
        raw: bool,
    },
    /// Analyze PATH: duplicates, missing dirs, long and unresolvable entries (exit 1 if findings)
    Check,
    /// Add a directory to PATH
    Add {
        dir: String,
        /// Insert at the front instead of appending
        #[arg(long)]
        prepend: bool,
        /// No-op if the directory is already present (case-insensitive)
        #[arg(long)]
        dedupe: bool,
    },
    /// Remove a directory (path or #index)
    Remove { target: String },
    /// Drop duplicate entries, keeping the first
    Dedupe,
    /// Remove entries whose directories no longer exist
    Prune,
    /// Move an entry to a new position (1-based indices)
    Move { from: usize, to: usize },
    /// Undo a previous mutation; --list shows snapshot history
    Undo {
        /// List snapshots instead of restoring
        #[arg(long)]
        list: bool,
        /// Restore a specific snapshot by id (see `undo --list`)
        #[arg(long)]
        to: Option<usize>,
        /// Only consider path or environment-variable snapshots
        #[arg(long, value_enum)]
        kind: Option<UndoKind>,
    },
    /// Diff current PATH against a snapshot (exit 1 if it differs)
    Diff {
        /// Snapshot id to diff against (default: newest)
        #[arg(long)]
        to: Option<usize>,
    },
    /// Export PATH (user + system) and user variables to JSON
    Export {
        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
    },
    /// Import from a pathctl export (merge; never truncates)
    Import { file: std::path::PathBuf },
    /// Generate shell completion scripts (bash, elvish, fish, powershell, zsh)
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Get, set or delete environment variables (user scope by default)
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
}

/// Snapshot domain filter for `undo`.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum UndoKind {
    /// PATH snapshots only
    Path,
    /// Environment-variable snapshots only
    Env,
}

#[derive(Subcommand)]
enum EnvCmd {
    Get { name: String },
    Set { name: String, value: String },
    Delete { name: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let g = Global {
        json: cli.json,
        dry_run: cli.dry_run,
        yes: cli.yes,
        no_broadcast: cli.no_broadcast,
        elevate: cli.elevate,
    };
    let reg = Registry::new();
    match run(cli, g, &reg) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

fn run(cli: Cli, g: Global, reg: &Registry) -> Result<u8> {
    let read_scopes = |allow_all: bool| commands::scopes_from(cli.scope.as_deref(), allow_all);
    match &cli.cmd {
        Cmd::List { raw } => {
            let scopes = read_scopes(true)?;
            commands::list(reg, &g, &scopes, *raw)
        }
        Cmd::Check => {
            let scopes = read_scopes(true)?;
            commands::check(reg, &g, &scopes)
        }
        Cmd::Add { dir, prepend, dedupe } => {
            let scope = commands::mutation_scope(cli.scope.as_deref())?;
            commands::add(reg, &g, scope, dir, *prepend, *dedupe)
        }
        Cmd::Remove { target } => {
            let scope = commands::mutation_scope(cli.scope.as_deref())?;
            commands::remove(reg, &g, scope, target)
        }
        Cmd::Dedupe => {
            let scope = commands::mutation_scope(cli.scope.as_deref())?;
            commands::dedupe(reg, &g, scope)
        }
        Cmd::Prune => {
            let scope = commands::mutation_scope(cli.scope.as_deref())?;
            commands::prune(reg, &g, scope)
        }
        Cmd::Move { from, to } => {
            let scope = commands::mutation_scope(cli.scope.as_deref())?;
            commands::move_entry(reg, &g, scope, *from, *to)
        }
        Cmd::Undo { list, to, kind } => {
            let scope = commands::mutation_scope(cli.scope.as_deref())?;
            let kind = kind.map(|k| match k {
                UndoKind::Path => commands::SnapshotKind::Path,
                UndoKind::Env => commands::SnapshotKind::Var,
            });
            if *list {
                commands::undo_list(&g, Some(scope), kind)
            } else {
                commands::undo(reg, &g, Some(scope), kind, *to)
            }
        }
        Cmd::Diff { to } => {
            let scopes = read_scopes(true)?;
            commands::diff(reg, &g, &scopes, *to)
        }
        Cmd::Export { output } => {
            let scopes = commands::backup_scopes(cli.scope.as_deref())?;
            commands::export(reg, &scopes, output.as_deref())
        }
        Cmd::Import { file } => {
            let scopes = commands::backup_scopes(cli.scope.as_deref())?;
            commands::import(reg, &g, &scopes, file)
        }
        Cmd::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(*shell, &mut cmd, "pathctl", &mut std::io::stdout());
            Ok(0)
        }
        Cmd::Env { cmd } => {
            let scope = commands::mutation_scope(cli.scope.as_deref())?;
            match cmd {
                EnvCmd::Get { name } => commands::env_get(reg, &g, scope, name),
                EnvCmd::Set { name, value } => commands::env_set(reg, &g, scope, name, value),
                EnvCmd::Delete { name } => commands::env_delete(reg, &g, scope, name),
            }
        }
    }
}
