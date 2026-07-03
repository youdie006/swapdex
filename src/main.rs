use clap::Parser;

#[derive(Parser)]
#[command(
    name = "swapdex",
    version,
    about = "Switch Claude Code / Codex login accounts, locally."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// List saved account profiles
    Ls,
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Ls) | None => {
            eprintln!("swapdex: no accounts yet (scaffold)");
        }
    }
}
