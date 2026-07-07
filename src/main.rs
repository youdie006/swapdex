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
        /// Profile name (omit on a terminal to get a suggestion)
        name: Option<String>,
        #[arg(long, value_enum)]
        tool: Option<ToolSel>,
        /// Replace an existing snapshot for the tool
        #[arg(long)]
        update: bool,
    },
    /// Switch the active login to a saved profile
    Use {
        /// The saved profile to switch to (see `swapdex ls`)
        name: String,
        /// Limit the switch to one tool (default: every tool the profile has)
        #[arg(long, value_enum)]
        tool: Option<ToolSel>,
        /// Show what would change without writing
        #[arg(long)]
        dry_run: bool,
        /// After switching, open the tool right away (needs --tool)
        #[arg(long)]
        open: bool,
        /// Folder to open the conversation in (with --open; default: current dir)
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// List saved profiles (active marked from the live login)
    Ls {
        #[arg(long)]
        json: bool,
        /// Bare profile names, one per line (for scripts/completion)
        #[arg(long)]
        names: bool,
    },
    /// Show the active account per tool
    Status {
        #[arg(long)]
        json: bool,
        /// One compact line (for shell prompts / statuslines)
        #[arg(long)]
        short: bool,
    },
    /// Remove a saved profile (never touches a live login)
    Rm {
        name: String,
        /// Confirm the deletion
        #[arg(long)]
        yes: bool,
    },
    /// Guided first-time setup: save your logins, add more, learn to switch
    Setup,
    /// Log in to a tool and save the result as a profile in one step
    Login {
        name: String,
        #[arg(long, value_enum)]
        tool: Option<ToolSel>,
    },
    /// Rename a saved profile
    Rename { old: String, new: String },
    /// Put back the login that was live before the last switch
    Restore {
        /// Restore only this tool
        #[arg(long, value_enum)]
        tool: Option<commands::ToolSel>,
        /// Show what would be restored without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Sessions grouped by the account that was active when they ran
    Sessions {
        /// Emit JSON ({"available", "accounts", "total"})
        #[arg(long)]
        json: bool,
    },
    /// Interactive picker: see every profile, type a number, switch
    Ui,
    /// Local health check: store, snapshots, live logins - with a fix per finding
    Doctor,
    /// Recent local token usage per tool (5h/7d), read from session logs
    Usage {
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Run as a read-only MCP server over stdio
    Mcp,
    /// Print a shell completion script (bash, zsh, fish, ...)
    Completions { shell: clap_complete::Shell },
    /// Print the man page (roff) to stdout
    Manpage,
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
        // No subcommand: the wordmark, a short hint, and - because the person
        // typing a bare `swapdex` usually wants to know where they stand - the
        // active accounts. Best-effort: a broken store never breaks the banner,
        // and a bare banner never CREATES the store on a fresh machine.
        swapdex::banner::print_banner();
        if paths.store_dir().exists() {
            if let Some(line) = commands::short_line(&paths) {
                println!("  active: {line}");
            }
        }
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
    // Man page: pure codegen like completions (the Homebrew formula consumes
    // this at install time).
    if let Cmd::Manpage = cmd {
        let man = clap_mangen::Man::new(Cli::command());
        let mut buf = Vec::new();
        if let Err(e) = man.render(&mut buf) {
            eprintln!("swapdex: manpage render failed: {e}");
            std::process::exit(1);
        }
        use std::io::Write;
        if std::io::stdout().write_all(&buf).is_err() {
            std::process::exit(1);
        }
        return;
    }
    let result = match cmd {
        Cmd::Add { name, tool, update } => commands::add(&paths, name.as_deref(), *tool, *update),
        Cmd::Use {
            name,
            tool,
            dry_run,
            open,
            dir,
        } => {
            if *open {
                commands::use_account_open(&paths, name, *tool, dir.as_deref())
            } else {
                commands::use_account(&paths, name, *tool, *dry_run)
            }
        }
        Cmd::Ls { json, names } => commands::ls(&paths, *json, *names),
        Cmd::Status { json, short } => commands::status(&paths, *json, *short),
        Cmd::Rm { name, yes } => commands::rm(&paths, name, *yes),
        Cmd::Setup => commands::setup(&paths),
        Cmd::Login { name, tool } => commands::login(&paths, name, *tool),
        Cmd::Rename { old, new } => commands::rename(&paths, old, new),
        Cmd::Restore { tool, dry_run } => commands::restore(&paths, *tool, *dry_run),
        Cmd::Sessions { json } => commands::sessions(&paths, *json),
        Cmd::Ui => commands::ui(&paths),
        Cmd::Doctor => commands::doctor(&paths),
        Cmd::Usage { json } => commands::usage(&paths, *json),
        Cmd::Mcp => {
            swapdex::mcp::serve();
            return;
        }
        Cmd::Completions { .. } | Cmd::Manpage => unreachable!("handled above as pure codegen"),
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
