use clap::Parser;
use swapdex::commands::{self, ToolSel};
use swapdex::paths::Paths;

#[derive(Parser)]
#[command(
    name = "swapdex",
    version,
    about = "Switch Claude Code / Codex login accounts, locally and safely."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Save the current live login as a named profile
    Add {
        name: String,
        #[arg(long)]
        tool: Option<String>,
        /// Replace an existing snapshot for the tool
        #[arg(long)]
        update: bool,
    },
    /// Switch the active login to a saved profile
    Use {
        name: String,
        #[arg(long)]
        tool: Option<String>,
        /// Show what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// List saved profiles (active marked from the live login)
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Show the active account per tool
    Status,
    /// Remove a saved profile (never touches a live login)
    Rm { name: String },
    /// Sessions grouped by the account that was active when they ran
    Sessions,
}

fn main() {
    let cli = Cli::parse();
    let paths = match Paths::resolve() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("swapdex: {}", swapdex::util::redact_path(&format!("{e:#}")));
            std::process::exit(1);
        }
    };
    let result = match &cli.cmd {
        Cmd::Add { name, tool, update } => {
            commands::add(&paths, name, &ToolSel::parse(tool.as_deref()), *update)
        }
        Cmd::Use {
            name,
            tool,
            dry_run,
        } => commands::use_account(&paths, name, &ToolSel::parse(tool.as_deref()), *dry_run),
        Cmd::Ls { json } => commands::ls(&paths, *json),
        Cmd::Status => commands::status(&paths),
        Cmd::Rm { name } => commands::rm(&paths, name),
        Cmd::Sessions => commands::sessions(&paths),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            // Redact home paths from any error before it reaches a terminal/log.
            eprintln!("swapdex: {}", swapdex::util::redact_path(&format!("{e:#}")));
            std::process::exit(1);
        }
    }
}
