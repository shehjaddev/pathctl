//! pathctl — safe PATH & environment-variable manager for Windows.

mod commands;
mod elevate;
mod notify;
mod pathops;
mod registry;
mod snapshot;
mod util;

use clap::{Parser, Subcommand};
use commands::{Global, Result};
use registry::{Registry, Scope};
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
    /// Get, set or delete environment variables (user scope by default)
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
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
    let scope = |allow_all: bool| -> Result<Vec<Scope>> {
        commands::scopes_from(cli.scope.as_deref(), allow_all)
    };
    match &cli.cmd {
        Cmd::List { raw } => {
            let scopes = scope(true)?;
            commands::list(reg, &g, &scopes, *raw)
        }
        Cmd::Check => {
            let scopes = scope(true)?;
            commands::check(reg, &g, &scopes)
        }
        Cmd::Add { dir, prepend, dedupe } => {
            let scopes = scope(false)?;
            commands::add(reg, &g, scopes[0], dir, *prepend, *dedupe)
        }
        Cmd::Remove { target } => {
            let scopes = scope(false)?;
            commands::remove(reg, &g, scopes[0], target)
        }
        Cmd::Dedupe => {
            let scopes = scope(false)?;
            commands::dedupe(reg, &g, scopes[0])
        }
        Cmd::Move { from, to } => {
            let scopes = scope(false)?;
            commands::move_entry(reg, &g, scopes[0], *from, *to)
        }
        Cmd::Undo { list, to } => {
            let scopes = scope(false)?; // undo accepts user/system only
            if *list {
                commands::undo_list(&g, Some(scopes[0]))
            } else {
                commands::undo(reg, &g, Some(scopes[0]), *to)
            }
        }
        Cmd::Diff { to } => {
            let scopes = scope(true)?;
            commands::diff(reg, &g, &scopes, *to)
        }
        Cmd::Export { output } => commands::export(reg, output.as_deref()),
        Cmd::Import { file } => commands::import(reg, &g, file),
        Cmd::Env { cmd } => {
            let scopes = scope(false)?;
            match cmd {
                EnvCmd::Get { name } => commands::env_get(reg, &g, scopes[0], name),
                EnvCmd::Set { name, value } => commands::env_set(reg, &g, scopes[0], name, value),
                EnvCmd::Delete { name } => commands::env_delete(reg, &g, scopes[0], name),
            }
        }
    }
}
