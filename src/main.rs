use clap::{CommandFactory, Parser};
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
    cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Save the current live login as a named profile
    Add {
        name: String,
        #[arg(long, value_enum)]
        tool: Option<ToolSel>,
        /// Replace an existing snapshot for the tool
        #[arg(long)]
        update: bool,
    },
    /// Switch the active login to a saved profile
    Use {
        name: String,
        #[arg(long, value_enum)]
        tool: Option<ToolSel>,
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
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Remove a saved profile (never touches a live login)
    Rm {
        name: String,
        /// Confirm the deletion
        #[arg(long)]
        yes: bool,
    },
    /// Rename a saved profile
    Rename { old: String, new: String },
    /// Sessions grouped by the account that was active when they ran
    Sessions,
    /// Run as a read-only MCP server over stdio
    Mcp,
    /// Print a shell completion script (bash, zsh, fish, ...)
    Completions { shell: clap_complete::Shell },
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
    let Some(cmd) = &cli.cmd else {
        // No subcommand: print the ASCII wordmark + a short hint.
        swapdex::banner::print_banner();
        return;
    };
    // Completions generate swapdex's OWN tab-completion - they do NOT wrap or
    // intercept `claude`/`codex` (that is llmux's territory, deliberately out of
    // scope). Pure codegen, no account access.
    if let Cmd::Completions { shell } = cmd {
        clap_complete::generate(
            *shell,
            &mut Cli::command(),
            "swapdex",
            &mut std::io::stdout(),
        );
        return;
    }
    let result = match cmd {
        Cmd::Add { name, tool, update } => commands::add(&paths, name, *tool, *update),
        Cmd::Use {
            name,
            tool,
            dry_run,
        } => commands::use_account(&paths, name, *tool, *dry_run),
        Cmd::Ls { json } => commands::ls(&paths, *json),
        Cmd::Status { json } => commands::status(&paths, *json),
        Cmd::Rm { name, yes } => commands::rm(&paths, name, *yes),
        Cmd::Rename { old, new } => commands::rename(&paths, old, new),
        Cmd::Sessions => commands::sessions(&paths),
        Cmd::Mcp => {
            swapdex::mcp::serve();
            return;
        }
        Cmd::Completions { .. } => unreachable!("handled before path resolution"),
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
