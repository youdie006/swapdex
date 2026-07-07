//! The ASCII wordmark the CLI prints when run with no subcommand. It is the
//! same art shown in the README banner, so the banner is literally what you see
//! in your own terminal. ANSI colour is used only when stdout is a TTY and
//! NO_COLOR is unset, so a piped invocation stays plain text.

// "SWAP" and "dex" in the ansi_shadow block font, kept separate so SWAP can be
// coloured violet and dex dimmed - the two-tone family wordmark.
const SWAP: [&str; 6] = [
    "███████╗██╗    ██╗ █████╗ ██████╗ ",
    "██╔════╝██║    ██║██╔══██╗██╔══██╗",
    "███████╗██║ █╗ ██║███████║██████╔╝",
    "╚════██║██║███╗██║██╔══██║██╔═══╝ ",
    "███████║╚███╔███╔╝██║  ██║██║     ",
    "╚══════╝ ╚══╝╚══╝ ╚═╝  ╚═╝╚═╝     ",
];
const DEX: [&str; 6] = [
    "██████╗ ███████╗██╗  ██╗",
    "██╔══██╗██╔════╝╚██╗██╔╝",
    "██║  ██║█████╗   ╚███╔╝ ",
    "██║  ██║██╔══╝   ██╔██╗ ",
    "██████╔╝███████╗██╔╝ ██╗",
    "╚═════╝ ╚══════╝╚═╝  ╚═╝",
];

const VIOLET: &str = "\x1b[38;2;157;107;255m"; // #9d6bff
const DIM: &str = "\x1b[38;2;176;176;186m";
const MUTED: &str = "\x1b[38;2;139;138;149m";
const RESET: &str = "\x1b[0m";

/// Print the wordmark + a short command hint. Called on no-subcommand.
pub fn print_banner() {
    let color = crate::util::color_enabled();
    let (v, d, m, r) = if color {
        (VIOLET, DIM, MUTED, RESET)
    } else {
        ("", "", "", "")
    };
    let mut s = String::from("\n");
    for i in 0..SWAP.len() {
        s.push_str(&format!("  {v}{}{d}{}{r}\n", SWAP[i], DEX[i]));
    }
    s.push_str(&format!(
        "\n  {m}Switch Claude Code / Codex / Gemini / Antigravity accounts - locally, one command.{r}\n\n"
    ));
    s.push_str(&format!(
        "  {v}${r} swapdex setup          guided first-time setup\n"
    ));
    s.push_str(&format!(
        "  {v}${r} swapdex login <name>   log in + save an account\n"
    ));
    s.push_str(&format!(
        "  {v}${r} swapdex use <name>     switch to a saved account\n"
    ));
    s.push_str(&format!(
        "  {v}${r} swapdex ls | status    see your accounts\n\n"
    ));
    s.push_str(&format!(
        "  {m}New here? Run `swapdex setup`.  Switch: `swapdex ui`.  All commands: `swapdex --help`.{r}\n"
    ));
    print!("{s}");
}
