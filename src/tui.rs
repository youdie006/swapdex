//! The persistent full-screen UI (`swapdex ui` on a real terminal), ccusage-
//! style by user request: the screen clears, the UI stays up, and everything
//! happens inside it. Switching shows its result in the status line and
//! REFRESHES the list in place; landing in a conversation (resume or new) is
//! the one action that leaves - by design, that is the goal of a switch.
//!
//! No second implementation of anything: a switch runs this same binary as a
//! subprocess (`swapdex use <name>`, or `swapdex use -` for the `r` previous-
//! account toggle) with its output captured into the status line, and
//! session/launch data comes from the caller through [`TuiCtx`].

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::path::PathBuf;

/// How often the quota bars re-read the usage endpoint while the UI is open. Slow
/// enough that watching the dashboard is not a stream of requests, often enough
/// that the numbers are not from when you opened it.
const QUOTA_REFRESH_SECS: u64 = 90;

const VIOLET: Color = Color::Rgb(157, 107, 255); // the brand accent (#9d6bff)
const DEXGRAY: Color = Color::Rgb(150, 150, 160); // the dimmed "dex" half
const MUTED: Color = Color::Rgb(139, 138, 149); // subtitles / hints

/// Same rounded panel, but with an owned (dynamic) title.
fn list_block_titled(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(96, 94, 116)))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
        ))
}

/// A rounded, violet-titled panel border - the shared frame for every list.
fn list_block(title: &'static str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(96, 94, 116)))
        .title(Span::styled(
            title,
            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
        ))
}

/// The two-tone wordmark as ratatui lines (violet SWAP + dim dex), shared with
/// the CLI banner so the TUI header IS the brand mark. Empty when the terminal
/// is too short to spare the rows.
fn logo_lines() -> Vec<Line<'static>> {
    crate::banner::SWAP
        .iter()
        .zip(crate::banner::DEX.iter())
        .map(|(sw, dx)| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    *sw,
                    Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                ),
                Span::styled(*dx, Style::default().fg(DEXGRAY)),
            ])
        })
        .collect()
}

/// A key-hint footer where the keys are violet and the labels muted, so the
/// eye lands on the keys (lazygit/gitui idiom).
/// Keep a list selection in bounds after the row count changes (a switch,
/// delete, or a concurrent `swapdex rm`), so the highlight never points past
/// the end and no later `rows[i]` can panic.
fn clamp_selection(state: &mut ListState, len: usize) {
    match (state.selected(), len) {
        (_, 0) => state.select(None),
        (Some(i), n) if i >= n => state.select(Some(n - 1)),
        (None, n) if n > 0 => state.select(Some(0)),
        _ => {}
    }
}

/// Map a clicked terminal row to a list index, accounting for the list's scroll
/// `offset`: the first VISIBLE row is `offset`, not `0`. Ignoring the offset
/// meant a click on a scrolled list opened an earlier, hidden entry (potentially
/// another account's session). Caller guarantees `click_row >= top`.
fn click_row_index(offset: usize, click_row: u16, top: u16, per: u16) -> usize {
    offset + (click_row.saturating_sub(top) / per.max(1)) as usize
}

fn key_hints(pairs: &[(&'static str, &'static str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default().fg(MUTED)));
        }
        spans.push(Span::styled(
            *key,
            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, Style::default().fg(MUTED)));
    }
    Line::from(spans)
}

/// Tools in the order the Main screen groups them.
const TOOL_ORDER: &[&str] = &["claude-code", "codex", "gemini", "antigravity"];

/// The tool an account is grouped under: its first tool in canonical order, so a
/// profile holding several appears once, under the first one it has.
fn group_of(tools: &str) -> &'static str {
    TOOL_ORDER
        .iter()
        .find(|t| tools.contains(*t))
        .copied()
        .unwrap_or("other")
}

/// One account is one row, even when swapdex holds it two ways. A saved snapshot
/// and a slot for the same login are two storage details, not two accounts, and
/// showing both asks the user to know which is which. The row that can actually
/// serve wins: a slot with a login, else whatever else names that identity.
///
/// Rows with no identity to compare (no email yet) are always kept - they cannot
/// be proven to be duplicates.
pub fn dedupe_by_identity(rows: Vec<Row>) -> Vec<Row> {
    let key = |r: &Row| {
        // The identity column is "email [tier]"; the email alone identifies it.
        r.ident
            .split_whitespace()
            .next()
            .filter(|e| e.contains('@'))
            .map(str::to_string)
    };
    // Prefer, in order: signed in and active, then signed in, then the rest - so
    // the surviving row is the one that would actually take a turn.
    let rank = |r: &Row| match (r.needs_login, r.active) {
        (false, true) => 0,
        (false, false) => 1,
        (true, _) => 2,
    };
    // (identity, index of the current winner). The winner takes the FIRST-seen
    // position, so an account does not jump down the list when its slot logs in.
    let mut slots: Vec<(Option<String>, usize)> = Vec::new();
    let mut rows = rows;
    for i in 0..rows.len() {
        let k = key(&rows[i]);
        match k.as_ref().and_then(|k| {
            slots
                .iter()
                .position(|(s, _)| s.as_deref() == Some(k.as_str()))
        }) {
            Some(pos) => {
                if rank(&rows[i]) < rank(&rows[slots[pos].1]) {
                    slots[pos].1 = i;
                }
            }
            // No identity to compare on: never merged away.
            None => slots.push((k, i)),
        }
    }
    let winners: Vec<usize> = slots.into_iter().map(|(_, i)| i).collect();
    let mut out = Vec::with_capacity(winners.len());
    for i in winners {
        out.push(std::mem::replace(
            &mut rows[i],
            Row {
                name: String::new(),
                ident: String::new(),
                tools: String::new(),
                active: false,
                warn: None,
                disabled: false,
                needs_login: false,
            },
        ));
    }
    out
}

/// Sort accounts by tool group (canonical order), keeping the original order
/// inside a group, so accounts of one tool sit together under one heading.
pub fn group_sorted(mut rows: Vec<Row>) -> Vec<Row> {
    let rank = |r: &Row| {
        TOOL_ORDER
            .iter()
            .position(|t| r.tools.contains(*t))
            .unwrap_or(TOOL_ORDER.len())
    };
    rows.sort_by_key(rank);
    rows
}

/// Which rows start a new tool group (and therefore carry its heading). Index i
/// is true when row i's group differs from row i-1's.
fn group_heads(rows: &[Row]) -> Vec<bool> {
    let mut out = Vec::with_capacity(rows.len());
    let mut prev: Option<&str> = None;
    for r in rows {
        let g = group_of(&r.tools);
        out.push(prev != Some(g));
        prev = Some(g);
    }
    out
}

/// Map a clicked terminal row to a list index when items have DIFFERENT heights
/// (a group heading makes its row one line taller). Walks the visible items from
/// the scroll `offset`, summing heights, so a click always lands on the item that
/// was actually drawn there.
fn click_item_index(offset: usize, click_row: u16, top: u16, heights: &[u16]) -> usize {
    let mut y = top;
    let mut i = offset;
    while i < heights.len() {
        let h = heights[i].max(1);
        if click_row < y + h {
            return i;
        }
        y += h;
        i += 1;
    }
    heights.len().saturating_sub(1)
}

/// The column where every account's usage bar starts: two spaces past the widest
/// left side ("dot name  identity  (warn)"). One shared column means the bars
/// line up with each other AND stay beside the account they describe - right-edge
/// alignment scattered them to the far side of a wide terminal.
fn usage_bar_column(rows: &[Row]) -> usize {
    const NUM: usize = 3; // " 1 "
    const DOT: usize = 2; // the filled/hollow glyph
    const GAP: usize = 2;
    const STATUS: usize = 8; // the status word, padded
    let name_w = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    let ident_w = rows
        .iter()
        .map(|r| r.ident.chars().count())
        .max()
        .unwrap_or(0);
    NUM + DOT + name_w + GAP + ident_w + GAP + STATUS + GAP
}

/// Key hints shown on the Main screen when at least one profile exists. A pure
/// function so the `r` binding's label is unit-testable (the event loop is not
/// otherwise seamed for key-dispatch tests).
/// Every Main-screen binding, in the order the help panel lists them. The footer
/// shows only the first few; ten hints crammed into one line made each of them
/// harder to find than none at all.
const ALL_KEYS: &[KeyHint] = &[
    ("\u{21b5}", "switch to it"),
    ("1-9", "switch by number"),
    ("o", "open a chat"),
    ("r", "back to last account"),
    ("a", "add account"),
    ("q", "quit"),
    ("l", "sign in / re-login"),
    ("e", "pause / resume"),
    ("n", "rename"),
    ("d", "delete"),
    ("u", "tokens used"),
    ("%", "quota detail"),
    ("?", "health check"),
];

/// What an account is doing right now, for the status column. Following the shape
/// teamclaude uses: one word per row that answers "can this account serve me?"
/// without reading the bars.
fn account_status(r: &Row, u: Option<&Usage>) -> (&'static str, Color) {
    const SPENT: f64 = 99.0;
    if r.disabled {
        // Out of rotation is a deliberate state, so it is said plainly and not
        // dressed as a problem.
        return ("paused", Color::Rgb(110, 108, 128));
    }
    if r.needs_login {
        // Nothing else matters until it can authenticate.
        return ("no login", Color::Rgb(200, 150, 90));
    }
    if let Some(w) = r.warn {
        // A snapshot problem outranks quota: the account cannot serve at all.
        return (w, Color::Rgb(200, 150, 90));
    }
    let spent = u.is_some_and(|u| {
        u.five_h.is_some_and(|p| p >= SPENT) || u.seven_d.is_some_and(|p| p >= SPENT)
    });
    match (spent, r.active) {
        (true, _) => ("spent", Color::Rgb(196, 92, 96)),
        (false, true) => ("active", VIOLET),
        (false, false) => ("ready", Color::Rgb(120, 118, 140)),
    }
}

/// A note for figures that are a snapshot: "as of 2h ago", so a stale number is
/// never mistaken for a live one. Empty when the data IS live, or fresh enough
/// that the distinction does not matter.
fn observed_note(observed_at: Option<i64>) -> String {
    const FRESH: i64 = 15 * 60;
    let Some(t) = observed_at else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age = now - t;
    if age < FRESH {
        return String::new();
    }
    format!("as of {}", fmt_reset(age))
}

/// A key and what it does.
type KeyHint = (&'static str, &'static str);

/// The two hint rows: the keys you reach for first on top, the rest below. Every
/// key is named - a key you cannot see is a key you do not have.
fn hint_rows() -> (&'static [KeyHint], &'static [KeyHint]) {
    ALL_KEYS.split_at(5)
}

/// One account's quota picture: session (5h) and weekly (7d) utilization with
/// their reset countdowns. `None` for a window the endpoint did not report.
#[derive(Clone, Copy, Default)]
pub struct Usage {
    pub five_h: Option<f64>,
    pub five_h_reset: Option<i64>,
    pub seven_d: Option<f64>,
    pub seven_d_reset: Option<i64>,
    /// For figures that are a SNAPSHOT rather than a live read (Codex has no
    /// endpoint to ask): unix seconds when they were recorded. `None` means the
    /// numbers are current as of this refresh.
    pub observed_at: Option<i64>,
}

pub struct Row {
    pub name: String,
    pub ident: String,
    pub tools: String,
    pub active: bool,
    pub warn: Option<&'static str>,
    /// Kept out of automatic rotation (still switchable by hand).
    pub disabled: bool,
    /// A slot account with no readable login yet - it cannot serve a turn until
    /// its tool signs in there.
    pub needs_login: bool,
}

/// One line in the post-switch "open" screen (pre-rendered by the caller).
pub struct SessionEntry {
    pub line: String,
}

/// Everything the UI needs from the outside world.
pub trait TuiCtx {
    fn rows(&mut self) -> Vec<Row>;
    /// Perform the switch (subprocess); returns (success, condensed message).
    /// `"-"` toggles to the previously-used account (the `r` key).
    fn switch(&mut self, name: &str) -> (bool, String);
    fn delete(&mut self, name: &str) -> String;
    /// Take an account out of automatic rotation, or put it back. Returns the
    /// message to show. Default no-op so test contexts need not implement it.
    fn toggle_rotation(&mut self, _name: &str) -> String {
        String::new()
    }
    /// (label, session entries) for the just-switched profile.
    /// (label, session entries, the profile's tools) for the just-switched
    /// profile. The tools drive which "open a NEW ..." entries to show.
    fn sessions(&mut self, name: &str) -> (String, Vec<SessionEntry>, Vec<&'static str>);
    /// Rename a profile (subprocess). Returns (ok, message).
    fn rename(&mut self, old: &str, new: &str) -> (bool, String);
    /// Save the accounts you're currently logged into as a new profile
    /// (subprocess `add <name>` - captures live logins, no sign-out). This is
    /// the onboarding action: a fresh machine is usually already logged in.
    fn save_current(&mut self, name: &str) -> (bool, String);
    /// Run `doctor` and return its output lines for a read-only panel.
    fn doctor(&mut self) -> Vec<String>;
    /// Run `usage` and return its lines (consumed tokens per account).
    fn usage(&mut self) -> Vec<String>;
    /// Run `quota` and return its lines (remaining quota per Claude account -
    /// the one opt-in network read).
    fn quota(&mut self) -> Vec<String>;
    /// Per-account session (5h) and weekly (7d) utilization from the live quota
    /// endpoint, for the inline bars. Network; called lazily. Default empty so
    /// test contexts need not implement it.
    fn quota_pct(&mut self) -> Vec<(String, Usage)> {
        Vec::new()
    }
    /// Is a `swapdex proxy` running right now? When it is, a switch takes effect
    /// in the session that is ALREADY open, so Enter has no reason to leave the
    /// screen to start a new conversation. Default false so test contexts need
    /// not implement it.
    fn proxy_running(&mut self) -> bool {
        false
    }
    /// Is `sessionwiki` installed? When not, the session menu is native and a
    /// one-line hint points at what installing it would add.
    fn sessionwiki_present(&mut self) -> bool;
    /// Display names of the tools you're logged into RIGHT NOW (for the
    /// empty-state onboarding: "save these as a profile").
    fn live_tools(&mut self) -> Vec<String>;
}

/// What finally leaves the UI. Executed by the caller AFTER the terminal is
/// restored.
pub enum Outcome {
    Quit,
    /// Open the i-th session from the last `sessions()` call.
    OpenSession(usize),
    /// Open a fresh conversation in `dir` (None = current directory).
    NewConv {
        tool: &'static str,
        dir: Option<PathBuf>,
    },
    /// Run the add-a-new-account login flow (needs the real terminal).
    AddAccount(&'static str),
    /// Sign this account in by launching its own slot: the tool's own login runs
    /// there, which is the only thing that can create a slot's credential.
    SignIn(String),
}

const NEW_CONV: [(&str, &str); 4] = [
    ("open a NEW Claude Code conversation", "claude-code"),
    ("open a NEW Codex conversation", "codex"),
    ("open a NEW Gemini conversation", "gemini"),
    ("open a NEW Antigravity conversation", "antigravity"),
];

/// The "open a NEW <tool> conversation" entries for the tools a profile
/// actually has - so a Claude-only account doesn't offer Codex/Gemini/etc.
fn new_conv_for(tools: &[&str]) -> Vec<(&'static str, &'static str)> {
    NEW_CONV
        .iter()
        .filter(|(_, t)| tools.contains(t))
        .map(|&(l, t)| (l, t))
        .collect()
}

/// What a text-input screen is collecting.
enum InputKind {
    Rename(String), // rename this existing profile
    SaveCurrent,    // save the current live logins as a new profile
}

/// One row in the folder browser.
enum FolderRow {
    OpenHere,      // launch the conversation in the current dir
    Up,            // go to the parent dir
    Home,          // jump to $HOME
    Into(PathBuf), // descend into this subdirectory
}

/// The browser rows for `cwd`: "open here", parent (if any), home, then the
/// visible subdirectories (alphabetical, dotfiles hidden, unreadable skipped).
fn folder_rows(cwd: &std::path::Path) -> Vec<FolderRow> {
    let mut rows = vec![FolderRow::OpenHere];
    if cwd.parent().is_some() {
        rows.push(FolderRow::Up);
    }
    if dirs::home_dir().is_some_and(|h| h != cwd) {
        rows.push(FolderRow::Home);
    }
    let mut subs: Vec<PathBuf> = std::fs::read_dir(cwd)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
        })
        .collect();
    subs.sort();
    rows.extend(subs.into_iter().map(FolderRow::Into));
    rows
}

/// A path with $HOME collapsed to `~`, for a compact browser title.
fn tildify(p: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    p.display().to_string()
}

enum Screen {
    Main,
    Open {
        label: String,
        entries: Vec<SessionEntry>,
        new_conv: Vec<(&'static str, &'static str)>,
    },
    /// A folder BROWSER (no typing): navigate into subdirs, `..` to go up,
    /// and pick "open here" - conversations are per-directory.
    Folder {
        tool: &'static str,
        cwd: PathBuf,
        rows: Vec<FolderRow>,
        /// The Open screen to return to on Esc (one step back, not two).
        back: (String, Vec<SessionEntry>, Vec<(&'static str, &'static str)>),
    },
    ToolPick,
    /// A single-line text prompt (rename / save-current / new-account name).
    Input {
        kind: InputKind,
        value: String,
    },
    /// Read-only `doctor` output. `pending` = the (slow, tool-probing) check
    /// has not run yet; we draw a "checking..." frame first so the UI never
    /// looks frozen.
    Doctor {
        lines: Vec<String>,
        scroll: u16,
        pending: bool,
    },
    /// Read-only `usage` output (consumed tokens per account, local).
    Usage {
        lines: Vec<String>,
        scroll: u16,
        pending: bool,
    },
    /// Read-only `quota` output (remaining quota per Claude account). `pending`
    /// draws a "fetching..." frame first because this one hits the network.
    Quota {
        lines: Vec<String>,
        scroll: u16,
        pending: bool,
    },
}

/// A 10-wide utilization bar for a 0..100 percent (filled blocks over dim ones).
/// A quota window drawn as a filled block with its number written INSIDE it -
/// the percentage (and the reset countdown when it fits) sits centred on the bar
/// rather than beside it, so two windows fit on one row and each number is
/// unambiguously attached to its own bar. Returns the spans to render.
fn quota_bar(pct: Option<f64>, reset_secs: Option<i64>, width: usize) -> Vec<Span<'static>> {
    // Filled by BACKGROUND colour, not block characters: with blocks, the cells
    // the label occupies had no block in them, so the number sat in a visible gap
    // in the fill. Colouring the cells instead means the fill runs unbroken
    // underneath the label, and a full window looks full even when the label
    // spans the whole bar.
    let empty_bg = Color::Rgb(52, 50, 64);
    let Some(pct) = pct else {
        return vec![Span::styled(
            " ".repeat(width),
            Style::default().bg(empty_bg),
        )];
    };
    let pct = pct.clamp(0.0, 100.0);
    // "62%", plus the reset countdown when the width can carry it.
    let short = format!("{pct:.0}%");
    let label = match reset_secs.map(fmt_reset) {
        Some(r) if !r.is_empty() && short.chars().count() + 1 + r.chars().count() <= width => {
            format!("{short} {r}")
        }
        _ => short,
    };
    let lw = label.chars().count().min(width);
    let left_pad = (width - lw) / 2;
    let text: String = " ".repeat(left_pad)
        + &label.chars().take(lw).collect::<String>()
        + &" ".repeat(width - left_pad - lw);
    // Split where the fill ends: the label reads on both grounds because each
    // half carries its own foreground colour.
    let filled = ((pct / 100.0) * width as f64)
        .round()
        .clamp(0.0, width as f64) as usize;
    let head: String = text.chars().take(filled).collect();
    let tail: String = text.chars().skip(filled).collect();
    vec![
        Span::styled(
            head,
            Style::default()
                .bg(quota_fill(pct))
                .fg(Color::Rgb(24, 20, 34)) // near-black with a violet cast
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(tail, Style::default().bg(empty_bg).fg(DEXGRAY)),
    ]
}

/// Fill colour: swapdex's own violet, deepening as the window fills, with red
/// reserved for "about to be spent". A traffic-light green/amber/red would read
/// as a generic dashboard; this keeps the one brand accent doing the work and
/// spends a second colour only where it means something.
fn quota_fill(pct: f64) -> Color {
    if pct >= 90.0 {
        Color::Rgb(196, 92, 96) // the one warning tone
    } else if pct >= 65.0 {
        Color::Rgb(157, 107, 255) // full brand violet - hard to miss
    } else if pct >= 30.0 {
        Color::Rgb(124, 92, 196) // brand violet, held back
    } else {
        Color::Rgb(88, 74, 138) // quiet: plenty left, nothing to look at
    }
}

/// A reset countdown, shortest useful form: `48m`, `2h14m`, `3d4h`. Empty when
/// the window has already reset (nothing to count down to).
fn fmt_reset(resets_at_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // The endpoint reports either an absolute epoch or a relative span; treat a
    // small number as "seconds from now" so both shapes render.
    let left = if resets_at_secs > now {
        resets_at_secs - now
    } else if resets_at_secs > 0 && resets_at_secs < 60 * 60 * 24 * 30 {
        resets_at_secs
    } else {
        return String::new();
    };
    let mins = left / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let (h, m) = (mins / 60, mins % 60);
    if h < 24 {
        return if m > 0 {
            format!("{h}h{m}m")
        } else {
            format!("{h}h")
        };
    }
    let (d, rh) = (h / 24, h % 24);
    if rh > 0 {
        format!("{d}d{rh}h")
    } else {
        format!("{d}d")
    }
}

/// The persistent loop. Enters the alternate screen once and stays there
/// until an [`Outcome`] leaves it.
pub fn run(ctx: &mut dyn TuiCtx) -> Result<Outcome> {
    let mut terminal = ratatui::try_init()?;
    // Mouse: scroll to move, click to select/switch - the "manage by clicking"
    // the picker was asked for. Best-effort; key control is unaffected if the
    // terminal refuses.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    );
    let mut rows = ctx.rows();
    let mut state = ListState::default();
    state.select(Some(rows.iter().position(|r| r.active).unwrap_or(0)));
    let mut open_state = ListState::default();
    let mut status = String::new();
    let mut confirm_delete: Option<usize> = None;
    // Checked once: drives the "install sessionwiki for more" hint in the
    // native session menu.
    let wiki_present = ctx.sessionwiki_present();
    let mut screen = Screen::Main;
    // Cached only while the list is empty (onboarding); cheap to recompute.
    let mut onboard_live: Vec<String> = if rows.is_empty() {
        ctx.live_tools()
    } else {
        Vec::new()
    };
    // The list-body Rect from the last draw, so a mouse click can map its row
    // to a selection index.
    let mut main_area = Rect::default();
    // Per-account 5h utilization percent for the inline right-aligned bars.
    // Network, so fetched once lazily after the first frame (the UI opens
    // instantly; bars fill in). None = not fetched yet.
    let mut quota_pct: Option<std::collections::HashMap<String, Usage>> = None;
    // When the bars were last refreshed, so they can be kept current.
    let mut quota_fetched: Option<std::time::Instant> = None;

    let outcome = 'ui: loop {
        terminal.draw(|f| {
            // Two hint rows: ten keys on one line were unreadable, and hiding
            // most of them behind '?' was worse - you cannot use a key you cannot
            // see. Two rows fit them all WITH labels that say what they do.
            let [main, foot, help] = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(2),
            ])
            .areas(f.area());
            main_area = main;
            match &screen {
                Screen::Main => {
                    // A tall terminal gets the full wordmark header; a short
                    // one drops it so the list keeps its room.
                    let show_logo = main.height >= 14;
                    let head_h = if show_logo { 8 } else { 0 };
                    let [header, body] =
                        Layout::vertical([Constraint::Length(head_h), Constraint::Min(3)])
                            .areas(main);
                    if show_logo {
                        let mut lines = logo_lines();
                        lines.push(Line::from(""));
                        lines.push(Line::from(Span::styled(
                            "  Claude Code \u{b7} Codex \u{b7} Gemini \u{b7} Antigravity - one command, all local",
                            Style::default().fg(MUTED),
                        )));
                        f.render_widget(Paragraph::new(lines), header);
                    }

                    // The column every usage bar starts at: past the widest
                    // "dot name  identity (warn)" so the bars form one vertical
                    // line right beside the accounts.
                    // Pad name and identity to the widest, so status and bars form
                    // straight columns down the list.
                    let name_w = rows.iter().map(|r| r.name.chars().count()).max().unwrap_or(0);
                    let ident_w = rows
                        .iter()
                        .map(|r| r.ident.chars().count())
                        .max()
                        .unwrap_or(0);
                    let bar_col = usage_bar_column(&rows);
                    let heads = group_heads(&rows);
                    let items: Vec<ListItem> = rows
                        .iter()
                        .enumerate()
                        .map(|(ri, r)| {
                            // Filled dot = the active profile, hollow = the
                            // rest - the eye finds the live account fast.
                            let (glyph, gstyle) = if r.active {
                                ("\u{25cf} ", Style::default().fg(VIOLET))
                            } else {
                                ("\u{25cb} ", Style::default().fg(Color::DarkGray))
                            };
                            let name_style = if r.active {
                                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().add_modifier(Modifier::BOLD)
                            };
                            // Number the rows so an account can be reached by
                            // typing its digit, not only by arrowing to it.
                            let u_now = quota_pct.as_ref().and_then(|q| q.get(&r.name));
                            let (st, st_color) = account_status(r, u_now);
                            // Draw the selection marker on the ACCOUNT line
                            // ourselves: the widget puts highlight_symbol on an
                            // item's FIRST line, which for a group's first account
                            // is the heading - so the cursor appeared to sit on the
                            // heading while the selection was really the account.
                            let selected = state.selected() == Some(ri);
                            let mut top = vec![
                                Span::styled(
                                    if selected { "\u{2503} " } else { "  " },
                                    Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    format!("{:>2} ", ri + 1),
                                    Style::default().fg(Color::Rgb(96, 94, 116)),
                                ),
                                Span::styled(glyph, gstyle),
                                Span::styled(format!("{:<name_w$}", r.name), name_style),
                                Span::raw("  "),
                                Span::styled(
                                    format!("{:<ident_w$}", r.ident),
                                    Style::default().fg(DEXGRAY),
                                ),
                                Span::raw("  "),
                                Span::styled(format!("{st:<8}"), Style::default().fg(st_color)),
                            ];
                            // Session (5h) and weekly (7d) windows, each drawn
                            // as a bar with its own number inside it, both at one
                            // shared column so they line up beside the accounts.
                            // Both windows for every account: the number lives
                            // inside its own bar, so the two gauges sit side by
                            // side and an account with nothing to report still
                            // draws its tracks (keeping the columns aligned).
                            {
                                let u = quota_pct
                                    .as_ref()
                                    .and_then(|q| q.get(&r.name).copied())
                                    .unwrap_or_default();
                                let left_w: usize =
                                    top.iter().map(|s| s.content.chars().count()).sum();
                                let inner = (body.width as usize).saturating_sub(4);
                                // "5h " + bar + "  7d " + bar; the wider bar has
                                // room for the countdown inside it.
                                let bw = if inner.saturating_sub(bar_col) >= 34 {
                                    12
                                } else {
                                    7
                                };
                                let needed = 3 + bw + 5 + bw;
                                let start = bar_col.min(inner.saturating_sub(needed));
                                top.push(
                                    Span::raw(" ".repeat(start.saturating_sub(left_w).max(1))),
                                );
                                top.push(Span::styled("5h ", Style::default().fg(MUTED)));
                                top.extend(quota_bar(u.five_h, u.five_h_reset, bw));
                                top.push(Span::styled("  7d ", Style::default().fg(MUTED)));
                                top.extend(quota_bar(u.seven_d, u.seven_d_reset, bw));
                                // Snapshot figures say when they were taken, so an
                                // old number is never read as a current one.
                                let note = observed_note(u.observed_at);
                                if !note.is_empty() {
                                    top.push(Span::styled(
                                        format!("  {note}"),
                                        Style::default().fg(Color::Rgb(96, 94, 116)),
                                    ));
                                }
                            }
                            // One heading per tool, on its first account, so the
                            // Claude accounts and the Codex accounts read as two
                            // groups instead of one undifferentiated list.
                            let mut lines = Vec::with_capacity(4);
                            if heads.get(ri).copied().unwrap_or(false) {
                                // A heading pressed against the previous group's
                                // last row read as part of it; give it air above.
                                if ri > 0 {
                                    lines.push(Line::from(""));
                                }
                                let g = group_of(&r.tools);
                                // A terminal cannot change type size, so weight and
                                // letter-spacing carry it: uppercase, spaced, bold.
                                // That reads as a section title next to the tight
                                // account rows below it.
                                let title: String = g
                                    .to_uppercase()
                                    .chars()
                                    .flat_map(|c| [c, ' '])
                                    .collect();
                                let rule_w = bar_col.saturating_sub(title.chars().count() + 3);
                                lines.push(Line::from(vec![
                                    Span::styled(
                                        format!("  {title}"),
                                        Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                                    ),
                                    Span::styled(
                                        "\u{2500}".repeat(rule_w.clamp(2, 40)),
                                        Style::default().fg(Color::Rgb(72, 70, 88)),
                                    ),
                                ]));
                                // And air BELOW it: a heading touching its first
                                // account reads as that account's own line.
                                lines.push(Line::from(""));
                            }
                            // One line per account. The tools line is gone: the
                            // group heading already says which tool these are, and
                            // three lines each meant five accounts filled a screen
                            // - the point of a dashboard is seeing them together.
                            lines.push(Line::from(top));
                            ListItem::new(lines)
                        })
                        .collect();
                    if rows.is_empty() {
                        // Onboarding. A fresh machine is usually ALREADY logged
                        // into some tools - the fastest first step is to save
                        // those, so lead with it when they exist.
                        let mut lines = vec![
                            Line::from(""),
                            Line::from(Span::styled(
                                "  Welcome to swapdex.",
                                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                            )),
                            Line::from(""),
                        ];
                        if onboard_live.is_empty() {
                            lines.push(Line::from(Span::styled(
                                "  You're not logged into any tool yet. Sign in to Claude Code,",
                                Style::default().fg(MUTED),
                            )));
                            lines.push(Line::from(Span::styled(
                                "  Codex, Gemini, or Antigravity first, then come back.",
                                Style::default().fg(MUTED),
                            )));
                            lines.push(Line::from(""));
                            lines.push(key_hints(&[
                                ("a", "log in to a new account"),
                                ("q", "quit"),
                            ]));
                        } else {
                            lines.push(Line::from(vec![
                                Span::styled("  You're logged into ", Style::default().fg(MUTED)),
                                Span::styled(
                                    onboard_live.join(", "),
                                    Style::default().fg(Color::Reset).add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(".", Style::default().fg(MUTED)),
                            ]));
                            lines.push(Line::from(""));
                            lines.push(key_hints(&[
                                ("s", "save these as your first profile"),
                                ("a", "add a different account"),
                                ("q", "quit"),
                            ]));
                        }
                        f.render_widget(
                            Paragraph::new(lines).block(list_block(" welcome ")),
                            body,
                        );
                    } else {
                        let list = List::new(items)
                            .block(list_block(" accounts "))
                            // No highlight background (it would paint over the
                            // gauge fills, which ARE backgrounds) and no
                            // highlight_symbol: the marker is drawn on the account
                            // line itself, since the widget would put it on the
                            // group heading instead.
                            .highlight_style(Style::default().add_modifier(Modifier::BOLD));
                        f.render_stateful_widget(list, body, &mut state);
                    }
                    let foot_line = if let Some(i) = confirm_delete {
                        Line::from(Span::styled(
                            format!(
                                "  delete saved profile '{}'? the live login stays.  y / N",
                                rows[i].name
                            ),
                            Style::default().fg(Color::Rgb(200, 150, 90)),
                        ))
                    } else {
                        Line::from(Span::styled(
                            format!("  {}", status),
                            Style::default().fg(MUTED),
                        ))
                    };
                    f.render_widget(Paragraph::new(foot_line), foot);
                    if rows.is_empty() {
                        f.render_widget(
                            Paragraph::new(key_hints(&[("?", "health"), ("q", "quit")])),
                            help,
                        );
                    } else {
                        let (a, b) = hint_rows();
                        f.render_widget(Paragraph::new(vec![key_hints(a), key_hints(b)]), help);
                    }
                }
                Screen::Open { label, entries, new_conv } => {
                    let mut items: Vec<ListItem> = entries
                        .iter()
                        .map(|e| ListItem::new(Line::from(e.line.clone())))
                        .collect();
                    for (nlabel, _) in new_conv {
                        items.push(ListItem::new(Line::from(Span::styled(
                            *nlabel,
                            Style::default().fg(VIOLET),
                        ))));
                    }
                    let list = List::new(items)
                        .block(list_block_titled(&format!(" {label} ")))
                        .highlight_style(
                            Style::default()
                                .bg(Color::Rgb(50, 47, 68))
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("\u{2503} ");
                    f.render_stateful_widget(list, main, &mut open_state);
                    // Native session menu (no sessionwiki): a one-line nudge at
                    // what installing it adds. With sessionwiki, show the switch
                    // status instead.
                    let foot_line = if wiki_present {
                        Span::styled(format!("  {status}"), Style::default().fg(MUTED))
                    } else {
                        Span::styled(
                            "  tip: install sessionwiki to search these, trace a file to its \
                             session, and group by account",
                            Style::default().fg(MUTED),
                        )
                    };
                    f.render_widget(Paragraph::new(Line::from(foot_line)), foot);
                    f.render_widget(
                        Paragraph::new(key_hints(&[("\u{21b5}", "open"), ("esc", "back")])),
                        help,
                    );
                }
                Screen::Folder { tool, cwd, rows: frows, .. } => {
                    let name = NEW_CONV
                        .iter()
                        .find(|(_, t)| t == tool)
                        .map(|(l, _)| *l)
                        .unwrap_or("open");
                    let items: Vec<ListItem> = frows
                        .iter()
                        .map(|r| match r {
                            FolderRow::OpenHere => ListItem::new(Line::from(Span::styled(
                                "\u{25b8} open here",
                                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                            ))),
                            FolderRow::Up => ListItem::new(Line::from(Span::styled(
                                "\u{2191} ..",
                                Style::default().fg(DEXGRAY),
                            ))),
                            FolderRow::Home => ListItem::new(Line::from(Span::styled(
                                "\u{2302} ~  (home)",
                                Style::default().fg(DEXGRAY),
                            ))),
                            FolderRow::Into(p) => {
                                let leaf = p
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("?");
                                ListItem::new(Line::from(vec![
                                    Span::styled("  ", Style::default()),
                                    Span::styled(
                                        format!("{leaf}/"),
                                        Style::default().fg(Color::Reset),
                                    ),
                                ]))
                            }
                        })
                        .collect();
                    let list = List::new(items)
                        .block(list_block_titled(&format!(
                            " {name}  \u{2014}  {} ",
                            tildify(cwd)
                        )))
                        .highlight_style(
                            Style::default()
                                .bg(Color::Rgb(50, 47, 68))
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("\u{2503} ");
                    f.render_stateful_widget(list, main, &mut open_state);
                    f.render_widget(Paragraph::new(""), foot);
                    f.render_widget(
                        Paragraph::new(key_hints(&[
                            ("\u{21b5}", "enter / open here"),
                            ("\u{2191}\u{2193}", "move"),
                            ("esc", "back"),
                        ])),
                        help,
                    );
                }
                Screen::ToolPick => {
                    let items: Vec<ListItem> =
                        ["Claude Code", "Codex", "Gemini CLI", "Antigravity"]
                            .iter()
                            .map(|l| ListItem::new(Line::from(*l)))
                            .collect();
                    let list = List::new(items)
                        .block(list_block(" add an account - which tool? "))
                        .highlight_style(
                            Style::default()
                                .bg(Color::Rgb(50, 47, 68))
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol("\u{2503} ");
                    f.render_stateful_widget(list, main, &mut open_state);
                    f.render_widget(Paragraph::new(""), foot);
                    f.render_widget(
                        Paragraph::new(key_hints(&[("\u{21b5}", "choose"), ("esc", "back")])),
                        help,
                    );
                }
                Screen::Input { kind, value } => {
                    let (title, prompt) = match kind {
                        InputKind::Rename(old) => (
                            " rename profile ".to_string(),
                            format!("new name for '{old}'"),
                        ),
                        InputKind::SaveCurrent => (
                            " save current logins ".to_string(),
                            "name for this profile".to_string(),
                        ),
                    };
                    f.render_widget(
                        Paragraph::new(vec![
                            Line::from(""),
                            Line::from(vec![
                                Span::styled(format!("  {prompt}"), Style::default().fg(MUTED)),
                                Span::raw(": "),
                                Span::styled(
                                    format!("{value}\u{2588}"),
                                    Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                                ),
                            ]),
                        ])
                        .block(list_block_titled(&title)),
                        main,
                    );
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            format!("  {status}"),
                            Style::default().fg(MUTED),
                        ))),
                        foot,
                    );
                    f.render_widget(
                        Paragraph::new(key_hints(&[("\u{21b5}", "confirm"), ("esc", "cancel")])),
                        help,
                    );
                }
                Screen::Doctor { lines, scroll, .. } => {
                    // '?' is the door to the full key list, so name the keys here
                    // rather than crowding them into the footer.
                    let mut text: Vec<Line> = vec![Line::from(Span::styled(
                        "  keys",
                        Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                    ))];
                    for chunk in ALL_KEYS.chunks(2) {
                        let mut spans = vec![Span::raw("  ")];
                        for (k, label) in chunk {
                            spans.push(Span::styled(
                                format!("{k:>2} "),
                                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                            ));
                            spans.push(Span::styled(
                                format!("{label:<22}"),
                                Style::default().fg(MUTED),
                            ));
                        }
                        text.push(Line::from(spans));
                    }
                    text.push(Line::from(""));
                    text.push(Line::from(Span::styled(
                        "  health",
                        Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                    )));
                    let checks: Vec<Line> = lines
                        .iter()
                        .map(|l| {
                            // Colour the verdict word so problems stand out.
                            let style = if l.contains("problem") {
                                Style::default().fg(Color::Rgb(210, 140, 90))
                            } else if l.contains(" ok ") || l.contains("healthy") {
                                Style::default().fg(Color::Rgb(120, 190, 140))
                            } else {
                                Style::default().fg(DEXGRAY)
                            };
                            Line::from(Span::styled(format!("  {l}"), style))
                        })
                        .collect();
                    text.extend(checks);
                    f.render_widget(
                        Paragraph::new(text)
                            .scroll((*scroll, 0))
                            .block(list_block(" keys and health ")),
                        main,
                    );
                    f.render_widget(Paragraph::new(""), foot);
                    f.render_widget(
                        Paragraph::new(key_hints(&[
                            ("\u{2191}\u{2193}", "scroll"),
                            ("esc", "back"),
                        ])),
                        help,
                    );
                }
                Screen::Usage { lines, scroll, .. } => {
                    let text: Vec<Line> = lines
                        .iter()
                        .map(|l| {
                            let style = if l.trim_start().starts_with('@') {
                                Style::default().fg(VIOLET)
                            } else if l.contains("note:") || l.contains("(") {
                                Style::default().fg(MUTED)
                            } else {
                                Style::default().fg(DEXGRAY)
                            };
                            Line::from(Span::styled(format!("  {l}"), style))
                        })
                        .collect();
                    f.render_widget(
                        Paragraph::new(text)
                            .scroll((*scroll, 0))
                            .block(list_block(" usage - tokens used (local, this machine) ")),
                        main,
                    );
                    f.render_widget(Paragraph::new(""), foot);
                    f.render_widget(
                        Paragraph::new(key_hints(&[
                            ("\u{2191}\u{2193}", "scroll"),
                            ("esc", "back"),
                        ])),
                        help,
                    );
                }
                Screen::Quota { lines, scroll, .. } => {
                    let text: Vec<Line> = lines
                        .iter()
                        .map(|l| {
                            let style = if l.contains("% left") {
                                Style::default().fg(VIOLET)
                            } else if l.contains("expired")
                                || l.contains("rejected")
                                || l.contains("unexpected")
                                || l.contains("could not reach")
                            {
                                Style::default().fg(Color::Rgb(200, 150, 90))
                            } else if l.starts_with(' ') || l.contains("network") || l.contains("(") {
                                Style::default().fg(MUTED)
                            } else {
                                Style::default().fg(DEXGRAY)
                            };
                            Line::from(Span::styled(format!("  {l}"), style))
                        })
                        .collect();
                    f.render_widget(
                        Paragraph::new(text)
                            .scroll((*scroll, 0))
                            .block(list_block(" quota - remaining (live from Anthropic) ")),
                        main,
                    );
                    f.render_widget(Paragraph::new(""), foot);
                    f.render_widget(
                        Paragraph::new(key_hints(&[
                            ("\u{2191}\u{2193}", "scroll"),
                            ("esc", "back"),
                        ])),
                        help,
                    );
                }
            }
        })?;

        // A pending health check runs AFTER its "checking..." frame is drawn,
        // so the UI shows feedback instead of freezing on the old screen.
        if let Screen::Doctor { pending: true, .. } = &screen {
            let lines = ctx.doctor();
            screen = Screen::Doctor {
                lines,
                scroll: 0,
                pending: false,
            };
            continue;
        }
        if let Screen::Usage { pending: true, .. } = &screen {
            let lines = ctx.usage();
            screen = Screen::Usage {
                lines,
                scroll: 0,
                pending: false,
            };
            continue;
        }
        if let Screen::Quota { pending: true, .. } = &screen {
            let lines = ctx.quota();
            screen = Screen::Quota {
                lines,
                scroll: 0,
                pending: false,
            };
            continue;
        }
        // Fill the inline quota bars after the first frame (so the UI opens
        // instantly), then keep them current: a dashboard showing what was true
        // when you opened it is misleading the longer you leave it up. The read is
        // the same zero-spend usage endpoint `swapdex quota` uses, once per
        // account, so it is refreshed on a slow cadence rather than every frame.
        let stale_quota = quota_pct.is_none()
            || quota_fetched.is_some_and(|t: std::time::Instant| {
                t.elapsed() >= std::time::Duration::from_secs(QUOTA_REFRESH_SECS)
            });
        if matches!(screen, Screen::Main) && stale_quota && !rows.is_empty() {
            quota_pct = Some(ctx.quota_pct().into_iter().collect());
            quota_fetched = Some(std::time::Instant::now());
            continue;
        }
        // A left click on a menu item both selects AND activates it; treat
        // that as a synthesized Enter so the key handler below does the work.
        let mut click_activate = false;
        // Wait for input, but not forever: without a timeout the loop blocks until
        // a keypress, so a dashboard left alone would never refresh its numbers.
        if !event::poll(std::time::Duration::from_millis(500))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => k,
            Event::Mouse(m) => {
                use ratatui::crossterm::event::{MouseButton, MouseEventKind as MK};
                // Text panels (doctor/usage/quota) scroll their content with
                // the wheel - the list logic below is for menu screens only.
                if let Screen::Doctor { lines, scroll, .. }
                | Screen::Usage { lines, scroll, .. }
                | Screen::Quota { lines, scroll, .. } = &mut screen
                {
                    let max = (lines.len() as u16).saturating_sub(1);
                    match m.kind {
                        MK::ScrollDown => *scroll = (*scroll + 1).min(max),
                        MK::ScrollUp => *scroll = scroll.saturating_sub(1),
                        _ => {}
                    }
                    continue;
                }
                let list_len = match &screen {
                    Screen::Main => rows.len(),
                    Screen::Open {
                        entries, new_conv, ..
                    } => entries.len() + new_conv.len(),
                    Screen::ToolPick => 4,
                    Screen::Folder { rows: frows, .. } => frows.len(),
                    _ => 0,
                };
                let is_main = matches!(screen, Screen::Main);
                let sel = if is_main { &mut state } else { &mut open_state };
                match m.kind {
                    MK::ScrollDown if list_len > 0 => {
                        let i = sel.selected().unwrap_or(0);
                        sel.select(Some((i + 1).min(list_len - 1)));
                    }
                    MK::ScrollUp if list_len > 0 => {
                        let i = sel.selected().unwrap_or(0);
                        sel.select(Some(i.saturating_sub(1)));
                    }
                    MK::Down(MouseButton::Left) if list_len > 0 => {
                        // The list box's first row = main.y + logo-header + border.
                        let header = if is_main && main_area.height >= 14 {
                            8u16
                        } else {
                            0
                        };
                        let per = if is_main { 3 } else { 1 }; // menu rows are 1 line
                        let top = main_area.y + header + 1;
                        // Bottom of the list box's INNER area (above its border).
                        // A click below it (the foot/help rows) must not map to a
                        // hidden entry and synthesize an Enter.
                        let bottom = main_area.y + main_area.height.saturating_sub(1);
                        if m.row >= top && m.row < bottom {
                            // offset(): a scrolled list's first visible row is
                            // sel.offset(), not 0 - without it a click opened a
                            // hidden earlier entry (maybe another account).
                            // Main rows are 3 lines, 4 when they carry a tool
                            // heading, so the mapping must walk real heights -
                            // a fixed stride would select the wrong account.
                            let idx = if is_main {
                                let heights: Vec<u16> = group_heads(&rows)
                                    .iter()
                                    .enumerate()
                                    .map(|(i, h)| match (*h, i) {
                                        (true, 0) => 3,  // heading + blank + row
                                        (true, _) => 4,  // blank + heading + blank + row
                                        (false, _) => 1, // row
                                    })
                                    .collect();
                                click_item_index(sel.offset(), m.row, top, &heights)
                            } else {
                                click_row_index(sel.offset(), m.row, top, per)
                            };
                            if idx < list_len {
                                sel.select(Some(idx));
                                // Click activates a MENU item; on Main it only
                                // selects (Enter switches) so a stray click
                                // never switches accounts by surprise.
                                click_activate = !is_main;
                            }
                        }
                    }
                    _ => {}
                }
                if click_activate {
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        // Ctrl+C quits from ANY screen - raw mode swallows the signal, and it
        // is the first key a user in trouble reaches for.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            break 'ui Outcome::Quit;
        }
        match &mut screen {
            Screen::Main => {
                if let Some(i) = confirm_delete {
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        if let Some(row) = rows.get(i) {
                            status = ctx.delete(&row.name);
                            rows = ctx.rows();
                        }
                        // The list may now be EMPTY - a dangling Some(0)
                        // would make the next Enter/o index out of bounds.
                        clamp_selection(&mut state, rows.len());
                        // Deleting the last profile drops to the welcome screen,
                        // which reads onboard_live - recompute it (the live login
                        // is untouched by a delete) so it does not falsely claim
                        // you are logged out. Mirrors the save/rename path.
                        onboard_live = if rows.is_empty() {
                            ctx.live_tools()
                        } else {
                            Vec::new()
                        };
                    }
                    confirm_delete = None;
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break 'ui Outcome::Quit,
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some((i + 1).min(rows.len().saturating_sub(1))));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = state.selected().unwrap_or(0);
                        state.select(Some(i.saturating_sub(1)));
                    }
                    // A digit jumps straight to that numbered account and
                    // switches - the fastest path when you can see the list.
                    KeyCode::Char(c) if c.is_ascii_digit() && c != '0' && !rows.is_empty() => {
                        let idx = (c as usize) - ('1' as usize);
                        if let Some(name) = rows.get(idx).map(|r| r.name.clone()) {
                            state.select(Some(idx));
                            let (ok, msg) = ctx.switch(&name);
                            rows = ctx.rows();
                            clamp_selection(&mut state, rows.len());
                            status = if !ok {
                                msg
                            } else if ctx.proxy_running() {
                                format!("{name} now serves the running session")
                            } else {
                                msg
                            };
                        }
                    }
                    KeyCode::Enter if !rows.is_empty() => {
                        // rows.get, not rows[i]: the stored selection can point
                        // past the list if a concurrent `swapdex rm` shrank it.
                        if let Some(name) = state
                            .selected()
                            .and_then(|i| rows.get(i))
                            .map(|r| r.name.clone())
                        {
                            let (ok, msg) = ctx.switch(&name);
                            rows = ctx.rows();
                            clamp_selection(&mut state, rows.len());
                            // Enter switches and STAYS here. Leaving for a session
                            // menu on every switch was in the way; `o` is the key
                            // that opens a conversation, and with a proxy running
                            // the switch has already reached the open session.
                            status = if !ok {
                                msg
                            } else if ctx.proxy_running() {
                                format!("{name} now serves the running session")
                            } else {
                                msg
                            };
                        }
                    }
                    KeyCode::Char('o') if !rows.is_empty() => {
                        if let Some(name) = state
                            .selected()
                            .and_then(|i| rows.get(i))
                            .map(|r| r.name.clone())
                        {
                            // Switch FIRST, like Enter: opening a NEW conversation
                            // (or resuming) launches under whatever account is
                            // live, so without switching, `o` on a non-active
                            // profile would open the wrong account. `o` differs
                            // from Enter only in always showing the full menu
                            // (Enter shortcuts a single-tool profile to the folder).
                            let (ok, msg) = ctx.switch(&name);
                            status = msg;
                            rows = ctx.rows();
                            clamp_selection(&mut state, rows.len());
                            // With a proxy running the switch lands in the session
                            // that is already open, so stay here: opening a new
                            // conversation would be the wrong next step.
                            if ok && ctx.proxy_running() {
                                status = format!("{name} now serves the running session (proxy)");
                            } else if ok {
                                let (label, entries, tools) = ctx.sessions(&name);
                                let new_conv = new_conv_for(&tools);
                                open_state.select(Some(0));
                                screen = Screen::Open {
                                    label,
                                    entries,
                                    new_conv,
                                };
                            }
                        }
                    }
                    KeyCode::Char('a') => {
                        open_state.select(Some(0));
                        screen = Screen::ToolPick;
                    }
                    KeyCode::Char('s') if rows.is_empty() && !onboard_live.is_empty() => {
                        // Onboarding: save the accounts you're already logged
                        // into as your first profile.
                        screen = Screen::Input {
                            kind: InputKind::SaveCurrent,
                            value: String::new(),
                        };
                    }
                    KeyCode::Char('n') if !rows.is_empty() => {
                        if let Some(name) = state
                            .selected()
                            .and_then(|i| rows.get(i))
                            .map(|r| r.name.clone())
                        {
                            screen = Screen::Input {
                                kind: InputKind::Rename(name),
                                value: String::new(),
                            };
                        }
                    }
                    KeyCode::Char('r') => {
                        // `r` toggles to the previously-used account (`use -`),
                        // not `restore`: restore returns the pre-switch login,
                        // which in hub-and-spoke use is always one fixed base,
                        // never the account you actually used before.
                        let (_ok, msg) = ctx.switch("-");
                        status = msg;
                        rows = ctx.rows();
                    }
                    // Pause an account: keep it for manual switching but stop the
                    // proxy from choosing it. Useful for an account that is shared,
                    // billed elsewhere, or being saved for later.
                    // Sign an account in (or back in): launch its slot so the
                    // tool's own login runs there. swapdex never writes a
                    // credential itself, so this is the only way to create one.
                    KeyCode::Char('l') if !rows.is_empty() => {
                        if let Some(name) = state
                            .selected()
                            .and_then(|i| rows.get(i))
                            .map(|r| r.name.clone())
                        {
                            break 'ui Outcome::SignIn(name);
                        }
                    }
                    KeyCode::Char('e') if !rows.is_empty() => {
                        if let Some(name) = state
                            .selected()
                            .and_then(|i| rows.get(i))
                            .map(|r| r.name.clone())
                        {
                            status = ctx.toggle_rotation(&name);
                            rows = ctx.rows();
                            clamp_selection(&mut state, rows.len());
                        }
                    }
                    KeyCode::Char('d') if !rows.is_empty() => {
                        confirm_delete = state.selected();
                    }
                    KeyCode::Char('?') => {
                        screen = Screen::Doctor {
                            lines: vec!["running health check...".into()],
                            scroll: 0,
                            pending: true,
                        };
                    }
                    KeyCode::Char('u') if !rows.is_empty() => {
                        screen = Screen::Usage {
                            lines: vec!["computing usage...".into()],
                            scroll: 0,
                            pending: true,
                        };
                    }
                    // Ungated like doctor's '?': quota also covers a live
                    // login that is not saved as any profile yet.
                    KeyCode::Char('%') => {
                        screen = Screen::Quota {
                            lines: vec!["fetching remaining quota from Anthropic...".into()],
                            scroll: 0,
                            pending: true,
                        };
                    }
                    _ => {}
                }
            }
            Screen::Open {
                entries, new_conv, ..
            } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    rows = ctx.rows();
                    screen = Screen::Main;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    // saturating: both lists can be empty (a profile whose
                    // store entry vanished mid-session) - `- 1` would panic.
                    let max = (entries.len() + new_conv.len()).saturating_sub(1);
                    let i = open_state.selected().unwrap_or(0);
                    open_state.select(Some((i + 1).min(max)));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = open_state.selected().unwrap_or(0);
                    open_state.select(Some(i.saturating_sub(1)));
                }
                KeyCode::Enter => {
                    let i = open_state.selected().unwrap_or(0);
                    if i < entries.len() {
                        break 'ui Outcome::OpenSession(i);
                    }
                    let Some(&(_, tool)) = new_conv.get(i - entries.len()) else {
                        continue;
                    };
                    let cwd = std::env::current_dir()
                        .ok()
                        .or_else(dirs::home_dir)
                        .unwrap_or_else(|| PathBuf::from("/"));
                    let frows = folder_rows(&cwd);
                    open_state.select(Some(0));
                    if let Screen::Open {
                        label,
                        entries,
                        new_conv: nc,
                    } = std::mem::replace(
                        &mut screen,
                        Screen::Folder {
                            tool,
                            cwd,
                            rows: frows,
                            back: (String::new(), Vec::new(), Vec::new()),
                        },
                    ) {
                        if let Screen::Folder { back, .. } = &mut screen {
                            *back = (label, entries, nc);
                        }
                    }
                }
                _ => {}
            },
            Screen::Folder {
                tool,
                cwd,
                rows: frows,
                back,
            } => match key.code {
                KeyCode::Esc => {
                    // One step back to the Open menu, not two.
                    let (label, entries, new_conv) = std::mem::take(back);
                    screen = Screen::Open {
                        label,
                        entries,
                        new_conv,
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = open_state.selected().unwrap_or(0);
                    open_state.select(Some((i + 1).min(frows.len().saturating_sub(1))));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = open_state.selected().unwrap_or(0);
                    open_state.select(Some(i.saturating_sub(1)));
                }
                // Left / Backspace = go up a level (a natural browser gesture).
                KeyCode::Left | KeyCode::Backspace => {
                    if let Some(parent) = cwd.parent() {
                        *cwd = parent.to_path_buf();
                        *frows = folder_rows(cwd);
                        open_state.select(Some(0));
                    }
                }
                KeyCode::Enter | KeyCode::Right => {
                    let i = open_state.selected().unwrap_or(0);
                    match frows.get(i) {
                        Some(FolderRow::OpenHere) => {
                            break 'ui Outcome::NewConv {
                                tool,
                                dir: Some(cwd.clone()),
                            };
                        }
                        Some(FolderRow::Up) => {
                            if let Some(parent) = cwd.parent() {
                                *cwd = parent.to_path_buf();
                                *frows = folder_rows(cwd);
                                open_state.select(Some(0));
                            }
                        }
                        Some(FolderRow::Home) => {
                            if let Some(h) = dirs::home_dir() {
                                *cwd = h;
                                *frows = folder_rows(cwd);
                                open_state.select(Some(0));
                            }
                        }
                        Some(FolderRow::Into(p)) => {
                            *cwd = p.clone();
                            *frows = folder_rows(cwd);
                            open_state.select(Some(0));
                        }
                        None => {}
                    }
                }
                _ => {}
            },
            Screen::ToolPick => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => screen = Screen::Main,
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = open_state.selected().unwrap_or(0);
                    open_state.select(Some((i + 1).min(3)));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = open_state.selected().unwrap_or(0);
                    open_state.select(Some(i.saturating_sub(1)));
                }
                KeyCode::Enter => {
                    let tool = ["claude-code", "codex", "gemini", "antigravity"]
                        [open_state.selected().unwrap_or(0)];
                    break 'ui Outcome::AddAccount(tool);
                }
                _ => {}
            },
            Screen::Input { kind, value } => match key.code {
                KeyCode::Esc => screen = Screen::Main,
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(c) => value.push(c),
                KeyCode::Enter => {
                    let name = value.trim().to_string();
                    if name.is_empty() {
                        screen = Screen::Main;
                    } else {
                        let (ok, msg) = match kind {
                            InputKind::Rename(old) => ctx.rename(old, &name),
                            InputKind::SaveCurrent => ctx.save_current(&name),
                        };
                        status = msg;
                        rows = ctx.rows();
                        onboard_live = if rows.is_empty() {
                            ctx.live_tools()
                        } else {
                            Vec::new()
                        };
                        if ok {
                            state.select(rows.iter().position(|r| r.name == name).or(Some(0)));
                        }
                        screen = Screen::Main;
                    }
                }
                _ => {}
            },
            Screen::Doctor { lines, scroll, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => screen = Screen::Main,
                KeyCode::Down | KeyCode::Char('j') => {
                    let max = (lines.len() as u16).saturating_sub(1);
                    *scroll = (*scroll + 1).min(max);
                }
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                _ => {}
            },
            Screen::Usage { lines, scroll, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => screen = Screen::Main,
                KeyCode::Down | KeyCode::Char('j') => {
                    let max = (lines.len() as u16).saturating_sub(1);
                    *scroll = (*scroll + 1).min(max);
                }
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                _ => {}
            },
            Screen::Quota { lines, scroll, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => screen = Screen::Main,
                KeyCode::Down | KeyCode::Char('j') => {
                    let max = (lines.len() as u16).saturating_sub(1);
                    *scroll = (*scroll + 1).min(max);
                }
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                _ => {}
            },
        }
    };
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_selection_keeps_index_in_bounds() {
        let mut s = ListState::default();
        s.select(Some(3));
        clamp_selection(&mut s, 2); // list shrank to 2
        assert_eq!(s.selected(), Some(1), "clamped to last");
        clamp_selection(&mut s, 0); // now empty
        assert_eq!(s.selected(), None, "no selection on empty list");
        clamp_selection(&mut s, 3); // grew from empty
        assert_eq!(s.selected(), Some(0), "reselect top when non-empty");
        s.select(Some(1));
        clamp_selection(&mut s, 5); // still in range
        assert_eq!(s.selected(), Some(1), "untouched when in range");
    }

    #[test]
    fn click_row_index_accounts_for_scroll_offset() {
        // top=5, per=1 (a 1-line menu row). No scroll: clicking the first inner
        // row selects item 0; clicking two rows down selects item 2.
        assert_eq!(click_row_index(0, 5, 5, 1), 0);
        assert_eq!(click_row_index(0, 7, 5, 1), 2);
        // Scrolled down by 3: the first VISIBLE row is item 3, not 0 - the old
        // math returned 0 here and opened a hidden earlier session.
        assert_eq!(click_row_index(3, 5, 5, 1), 3);
        assert_eq!(click_row_index(3, 7, 5, 1), 5);
        // 3-line Main rows (per=3): two text rows down is still the same entry.
        assert_eq!(click_row_index(0, 6, 5, 3), 0);
        assert_eq!(click_row_index(0, 8, 5, 3), 1);
    }

    #[test]
    fn new_conv_only_offers_the_profiles_tools() {
        // A Claude-only profile offers just Claude, not all four.
        let one = super::new_conv_for(&["claude-code"]);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].1, "claude-code");
        // A two-tool profile offers exactly those two, in canonical order.
        let two = super::new_conv_for(&["gemini", "codex"]);
        let tools: Vec<&str> = two.iter().map(|(_, t)| *t).collect();
        assert_eq!(tools, vec!["codex", "gemini"]);
    }

    #[test]
    fn folder_rows_lead_with_open_here_and_hide_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("visible")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("afile"), b"x").unwrap();
        let rows = folder_rows(dir.path());
        assert!(matches!(rows[0], FolderRow::OpenHere), "open-here is first");
        assert!(
            rows.iter().any(|r| matches!(r, FolderRow::Up)),
            "parent exists -> Up row present"
        );
        let into: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                FolderRow::Into(p) => p.file_name().and_then(|n| n.to_str()),
                _ => None,
            })
            .collect();
        assert_eq!(into, vec!["visible"], "only non-dot subdirs, no files");
    }

    #[test]
    fn tildify_collapses_home() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(tildify(&home), "~");
            assert_eq!(tildify(&home.join("proj")), "~/proj");
        }
        assert_eq!(tildify(std::path::Path::new("/etc")), "/etc");
    }

    // Every bar starts at one shared column, past the WIDEST row - so the bars
    // line up and sit beside the accounts instead of at the terminal's edge.
    // The number is written INSIDE the bar and stays legible: the label is
    // centred, the fill splits it, and a countdown joins only when it fits.
    // Accounts group by tool, in canonical order, with a heading on the first row
    // of each group - so Claude accounts and Codex accounts read as two sections.
    // The same login held two ways is one account, not two: the row that can
    // actually serve survives, and rows with no identity are never merged away.
    #[test]
    fn duplicate_identities_collapse_to_the_row_that_can_serve() {
        let row = |name: &str, ident: &str, needs_login: bool, active: bool| Row {
            name: name.into(),
            ident: ident.into(),
            tools: "claude-code".into(),
            active,
            warn: None,
            disabled: false,
            needs_login,
        };
        // A snapshot and a slot for the same login: the slot with a login wins.
        let out = dedupe_by_identity(vec![
            row("rnd", "rnd@x.co [team]", true, false),
            row("rnd-slot", "rnd@x.co", false, true),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "rnd-slot");
        // Different logins are left alone.
        let out = dedupe_by_identity(vec![
            row("a", "a@x.co", false, false),
            row("b", "b@x.co", false, false),
        ]);
        assert_eq!(out.len(), 2);
        // No identity yet: cannot be proven a duplicate, so it stays.
        let out = dedupe_by_identity(vec![
            row("fresh", "", true, false),
            row("also", "", true, false),
            row("known", "k@x.co", false, false),
        ]);
        assert_eq!(out.len(), 3);
        // The FIRST position is kept when a later row wins, so the list does not
        // jump around as logins come and go.
        let out = dedupe_by_identity(vec![
            row("first", "same@x.co", true, false),
            row("other", "b@x.co", false, false),
            row("better", "same@x.co", false, false),
        ]);
        assert_eq!(
            out.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["better", "other"]
        );
    }

    #[test]
    fn accounts_group_by_tool_with_one_heading_each() {
        let row = |name: &str, tools: &str| Row {
            name: name.into(),
            ident: "e@x".into(),
            tools: tools.into(),
            active: false,
            warn: None,
            disabled: false,
            needs_login: false,
        };
        let sorted = group_sorted(vec![
            row("codex", "codex*"),
            row("rnd", "claude-code*"),
            row("work", "codex"),
            row("bsgong", "claude-code"),
        ]);
        let names: Vec<&str> = sorted.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["rnd", "bsgong", "codex", "work"],
            "claude accounts first, then codex, original order kept inside a group"
        );
        assert_eq!(
            group_heads(&sorted),
            vec![true, false, true, false],
            "one heading per group, on its first account"
        );
        assert_eq!(group_of("claude-code*"), "claude-code");
        assert_eq!(group_of("codex"), "codex");
        assert_eq!(group_of("mystery"), "other", "an unknown tool still groups");
    }

    // A heading makes its row taller, so clicks must walk real heights: a fixed
    // stride would select the wrong account below the first group.
    #[test]
    fn clicks_map_through_variable_row_heights() {
        // rows: [heading+3, 3, heading+3] -> heights 4,3,4 starting at y=5
        let heights = [4u16, 3, 4];
        assert_eq!(click_item_index(0, 5, 5, &heights), 0);
        assert_eq!(click_item_index(0, 8, 5, &heights), 0, "still inside row 0");
        assert_eq!(click_item_index(0, 9, 5, &heights), 1);
        assert_eq!(click_item_index(0, 12, 5, &heights), 2);
        // Scrolled: the first visible item is index 1.
        assert_eq!(click_item_index(1, 5, 5, &heights), 1);
        assert_eq!(click_item_index(1, 8, 5, &heights), 2);
        // The real shape: a plain row is 1 line; the first row of a group carries
        // its heading and a blank (3), and a later group also gets a blank above
        // its heading (4).
        let real = [3u16, 1, 4, 1];
        assert_eq!(click_item_index(0, 5, 5, &real), 0, "the heading");
        assert_eq!(click_item_index(0, 7, 5, &real), 0, "its account line");
        assert_eq!(click_item_index(0, 8, 5, &real), 1);
        assert_eq!(click_item_index(0, 9, 5, &real), 2, "second group starts");
        assert_eq!(click_item_index(0, 12, 5, &real), 2, "still its account");
        assert_eq!(click_item_index(0, 13, 5, &real), 3);
        // Past the end clamps to the last item rather than panicking.
        assert_eq!(click_item_index(0, 200, 5, &heights), 2);
    }

    // The status word answers "can this account serve me?" before the bars do.
    #[test]
    fn status_says_what_the_account_can_do() {
        let mk = |active: bool, warn, disabled| Row {
            name: "a".into(),
            ident: "e@x".into(),
            tools: "claude-code".into(),
            active,
            warn,
            disabled,
            needs_login: false,
        };
        let spent = Usage {
            five_h: Some(100.0),
            ..Default::default()
        };
        let fresh = Usage {
            five_h: Some(12.0),
            seven_d: Some(30.0),
            ..Default::default()
        };
        assert_eq!(
            account_status(&mk(true, None, false), Some(&fresh)).0,
            "active"
        );
        assert_eq!(
            account_status(&mk(false, None, false), Some(&fresh)).0,
            "ready"
        );
        assert_eq!(
            account_status(&mk(true, None, false), Some(&spent)).0,
            "spent",
            "an exhausted window outranks being active"
        );
        assert_eq!(
            account_status(&mk(true, Some("stale"), false), Some(&fresh)).0,
            "stale",
            "an unusable snapshot outranks quota"
        );
        assert_eq!(
            account_status(&mk(true, Some("stale"), true), Some(&spent)).0,
            "paused",
            "a deliberate pause is stated plainly, not as a problem"
        );
        // An account that cannot authenticate says so before anything else: quota
        // and activity are meaningless until it can.
        let needs = Row {
            name: "a".into(),
            ident: "e@x".into(),
            tools: "claude-code".into(),
            active: true,
            warn: None,
            disabled: false,
            needs_login: true,
        };
        assert_eq!(account_status(&needs, Some(&fresh)).0, "no login");
        // No quota data is not "spent".
        assert_eq!(account_status(&mk(false, None, false), None).0, "ready");
    }

    #[test]
    fn quota_bar_writes_the_number_inside_the_bar() {
        let spans = quota_bar(Some(62.0), None, 11);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text.chars().count(), 11, "the bar is exactly its width");
        assert!(
            text.contains("62%"),
            "the number is inside the bar: {text:?}"
        );
        // 62% of 11 -> 7 filled cells: the fill is the SPAN split, so the label
        // never leaves a gap in it.
        assert_eq!(spans[0].content.chars().count(), 7);
        assert_eq!(spans[1].content.chars().count(), 4);
        // A full window is filled edge to edge even though the label spans it.
        let full = quota_bar(Some(100.0), Some(3600), 11);
        assert_eq!(
            full[1].content.chars().count(),
            0,
            "nothing unfilled at 100%: {:?}",
            full[0].content
        );
        assert!(full[0].content.contains("100%"));
        // The countdown joins when it fits, and is dropped when it does not.
        let wide: String = quota_bar(Some(10.0), Some(3600), 14)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(wide.contains("10%") && wide.contains("1h"), "{wide:?}");
        let narrow: String = quota_bar(Some(10.0), Some(3600), 5)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            narrow.contains("10%") && !narrow.contains("1h"),
            "{narrow:?}"
        );
        // No data: an empty track of the right width, no number invented.
        let none = quota_bar(None, None, 6);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].content.chars().count(), 6);
        assert_eq!(none[0].content.trim(), "");
    }

    // Snapshot figures disclose their age; live ones have nothing to disclose,
    // and a recent snapshot does not need the caveat either.
    #[test]
    fn observed_note_only_appears_for_stale_snapshots() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(observed_note(None), "", "a live read says nothing");
        assert_eq!(
            observed_note(Some(now - 60)),
            "",
            "a minute old is still current enough"
        );
        assert_eq!(observed_note(Some(now - 2 * 3600)), "as of 2h");
        assert_eq!(observed_note(Some(now - 3 * 86400)), "as of 3d");
    }

    #[test]
    fn fmt_reset_shortens_to_the_useful_unit() {
        assert_eq!(fmt_reset(48 * 60), "48m");
        assert_eq!(fmt_reset(2 * 3600 + 14 * 60), "2h14m");
        assert_eq!(fmt_reset(3 * 3600), "3h");
        assert_eq!(fmt_reset(3 * 86400 + 4 * 3600), "3d4h");
        assert_eq!(fmt_reset(0), "", "already reset: nothing to count down");
    }

    #[test]
    fn usage_bar_column_clears_the_widest_row() {
        let row = |name: &str, ident: &str| Row {
            name: name.into(),
            ident: ident.into(),
            tools: String::new(),
            active: false,
            warn: None,
            disabled: false,
            needs_login: false,
        };
        // " N " + dot(2) + name + 2 + ident + 2 + status(8) + 2, using the WIDEST
        // name and identity so every bar starts at the same column.
        let rows = vec![
            row("rnd", "rnd@x.co"),
            row("bsgong", "bsgong@polarisai.co.kr"),
        ];
        assert_eq!(usage_bar_column(&rows), 3 + 2 + 6 + 2 + 22 + 2 + 8 + 2);
        // One narrow row: the column shrinks with it.
        assert_eq!(
            usage_bar_column(&[row("a", "b")]),
            3 + 2 + 1 + 2 + 1 + 2 + 8 + 2
        );
        // With no rows the name/identity widths are zero, leaving the fixed
        // columns: number, glyph, two gaps, the status word, and the trailing gap.
        assert_eq!(usage_bar_column(&[]), 3 + 2 + 2 + 2 + 8 + 2);
    }

    #[test]
    fn r_key_is_previous_not_restore() {
        // `r` toggles to the previously-used account (`use -`), not `restore`.
        // Restore always returned the pre-switch login, which in hub-and-spoke
        // use is always the one base account - never the account you last used.
        assert!(
            ALL_KEYS
                .iter()
                .any(|(k, label)| *k == "r" && label.contains("last account")),
            "the r key goes back to the previous account"
        );
        assert!(
            !ALL_KEYS.iter().any(|(_, label)| label.contains("restore")),
            "'restore' is no longer a Main-screen binding"
        );
        // Nothing is hidden behind '?': both rows together are the whole set, and
        // every label explains what its key does.
        let (a, b) = hint_rows();
        assert_eq!(a.len() + b.len(), ALL_KEYS.len(), "no key is dropped");
        assert!(!a.is_empty() && !b.is_empty(), "both rows carry keys");
        for (k, label) in ALL_KEYS {
            assert!(
                label.len() >= 4,
                "key {k:?} needs a label that explains it: {label:?}"
            );
        }
    }
}
