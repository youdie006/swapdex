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
/// How long a reading stands before the dashboard asks again. Short enough that
/// a number does not sit still while the account is being spent, long enough
/// that leaving the screen open is not a stream of requests - and the read is
/// off the loop, so a refresh costs the user nothing.
const QUOTA_REFRESH_SECS: u64 = 45;

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
/// How long after the delete prompt opens a `y` is treated as pasted, not typed.
///
/// Nobody reads "delete rnd? y/n" and answers inside a quarter second. Text
/// that arrives as a block - a terminal paste, or a harness driving this pane
/// because a stale window still looks like an agent - lands in under a
/// millisecond. Two ordinary letters are not a decision to delete an account.
const PASTE_GAP_MS: u64 = 250;

/// Whether a confirmation keypress came from a person rather than a paste.
fn confirm_is_deliberate(prompt_opened_ms: u64, key_arrived_ms: u64) -> bool {
    key_arrived_ms.saturating_sub(prompt_opened_ms) >= PASTE_GAP_MS
}

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
    // An account is an email ON A TOOL. One person very often signs into Claude
    // and ChatGPT with the same address, and merging those would delete a real
    // account from the list - which it did.
    let key = |r: &Row| {
        r.ident
            .split_whitespace()
            .next()
            .filter(|e| e.contains('@'))
            .map(|e| format!("{}\u{0}{}", group_of(&r.tools), e))
    };
    // The surviving row has to be the one a switch would actually move. A slot
    // switches by pointer - which the proxy and the active marker both follow -
    // while a snapshot copies credentials and moves no pointer, so keeping the
    // snapshot row made Enter appear to do nothing.
    // A SLOT outranks a snapshot outright, not merely as a tiebreak. The slot is
    // where the account lives and what a switch moves; the snapshot is a copy
    // nothing refreshes. Ranking usability first let a stale copy that still
    // carried credentials read as "signed in" and take the row from a live slot
    // that needed one - hiding the single thing that needed doing.
    let rank = |r: &Row| {
        let usable = match (r.needs_login, r.active) {
            (false, true) => 0,
            (false, false) => 1,
            (true, _) => 2,
        };
        (u8::from(!r.is_slot), usable)
    };
    // (identity, index of the current winner, every member's index). The winner
    // takes the FIRST-seen position, so an account does not jump down the list
    // when its slot logs in; the members are kept because the losing rows' names
    // are still how their usage readings are filed.
    // The NAME is also an identity. Every command resolves an account by it, so a
    // snapshot and a slot carrying the same name on the same tool are the same
    // account by construction - and the half that is not signed in yet has no
    // email to be merged on. A real machine listed `work` twice because of it.
    let by_name = |r: &Row| format!("{}\u{0}name\u{0}{}", group_of(&r.tools), r.name);
    let matches = |row: &Row, k: &Option<String>, members: &[usize], rows: &[Row]| -> bool {
        if let (Some(a), Some(b)) = (key(row), k.as_ref()) {
            if a == *b {
                return true;
            }
        }
        members.iter().any(|i| by_name(&rows[*i]) == by_name(row))
    };
    let mut slots: Vec<(Option<String>, usize, Vec<usize>)> = Vec::new();
    let mut rows = rows;
    for i in 0..rows.len() {
        let k = key(&rows[i]);
        match slots
            .iter()
            .position(|(s, _, members)| matches(&rows[i], s, members, &rows))
        {
            Some(pos) => {
                if rank(&rows[i]) < rank(&rows[slots[pos].1]) {
                    slots[pos].1 = i;
                }
                slots[pos].2.push(i);
            }
            // Nothing it belongs with: its own group.
            None => slots.push((k, i, vec![i])),
        }
    }
    let mut out = Vec::with_capacity(slots.len());
    for (_, winner, members) in slots {
        let also: Vec<String> = members
            .iter()
            .filter(|i| **i != winner)
            .map(|i| rows[*i].name.clone())
            .collect();
        let mut row = std::mem::replace(
            &mut rows[winner],
            Row {
                name: String::new(),
                ident: String::new(),
                tools: String::new(),
                active: false,
                warn: None,
                disabled: false,
                needs_login: false,
                stale: false,
                is_slot: false,
                also: Vec::new(),
            },
        );
        // The winner is the row that SERVES; the label can still come from
        // whichever half knows who the account is. A slot that has never been
        // signed into has no email to show, and printing a nameless row would
        // trade one kind of blank for another.
        if !row.ident.contains('@') {
            if let Some(known) = members
                .iter()
                .map(|i| &rows[*i].ident)
                .find(|id| id.contains('@'))
            {
                row.ident = known.clone();
            }
        }
        row.also = also;
        out.push(row);
    }
    out
}

/// Label a row with what the account itself said, where it said anything.
///
/// A row's label otherwise comes from a file on this machine, which cannot know
/// the server disagrees with it - and that disagreement is the whole shape of
/// the mix-up where signing in as one account leaves another connected. A row
/// with no live answer keeps the label it had.
pub fn apply_live_identity(rows: &mut [Row], usage: &[(String, Usage)]) {
    for r in rows.iter_mut() {
        let names = |n: &String| *n == r.name || r.also.contains(n);
        if let Some(id) = usage
            .iter()
            .find(|(n, _)| names(n))
            .and_then(|(_, u)| u.ident.as_ref())
            .filter(|id| !id.is_empty())
        {
            r.ident = id.clone();
        }
    }
}

#[cfg(test)]
mod live_identity_tests {
    use super::*;

    fn row(name: &str, ident: &str) -> Row {
        Row {
            name: name.into(),
            ident: ident.into(),
            tools: "codex".into(),
            active: false,
            warn: None,
            disabled: false,
            needs_login: false,
            stale: false,
            is_slot: true,
            also: Vec::new(),
        }
    }

    fn said(ident: Option<&str>) -> Usage {
        Usage {
            ident: ident.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn a_row_takes_the_label_the_account_gave_itself() {
        let mut rows = vec![
            row("work", "saved@example.com"),
            row("other", "b@example.com"),
        ];
        apply_live_identity(
            &mut rows,
            &[(
                "work".into(),
                said(Some("live@example.com [pro] (saved as saved@example.com)")),
            )],
        );
        assert_eq!(
            rows[0].ident,
            "live@example.com [pro] (saved as saved@example.com)"
        );
        // A row nothing was said about keeps what it had.
        assert_eq!(rows[1].ident, "b@example.com");
    }

    #[test]
    fn a_reading_that_names_nobody_leaves_the_label_alone() {
        let mut rows = vec![row("work", "saved@example.com")];
        apply_live_identity(&mut rows, &[("work".into(), said(None))]);
        assert_eq!(rows[0].ident, "saved@example.com");
        // An empty name is not a name either - blanking the column would trade
        // a stale label for no label.
        apply_live_identity(&mut rows, &[("work".into(), said(Some("")))]);
        assert_eq!(rows[0].ident, "saved@example.com");
    }

    /// The same account saved twice is one row, and the reading may be filed
    /// under the name that was absorbed.
    #[test]
    fn a_merged_row_is_reached_by_the_name_it_absorbed() {
        let mut r = row("work", "saved@example.com");
        r.also = vec!["work-old".into()];
        let mut rows = vec![r];
        apply_live_identity(
            &mut rows,
            &[("work-old".into(), said(Some("live@example.com")))],
        );
        assert_eq!(rows[0].ident, "live@example.com");
    }
}

/// The reset slot, padded so every row's next column starts at one place.
///
/// A window nobody has used has no reset time - the five-hour window starts on
/// first use, so there is nothing to count to. Rendering that as an empty string
/// pulled everything after it left on that row alone, and `rnd`'s 7d block sat
/// several columns ahead of the rows above and below. The slot keeps its width
/// either way, and says why it is empty rather than leaving a hole.
///
/// A slot too narrow for the phrase pads instead of truncating it into nonsense.
/// Which command Enter should run for this row.
///
/// Serving reads a slot's own credential directory, so a snapshot cannot pay for
/// turns and `serve` on one answers that it has never been signed in here. Enter
/// called `serve` for every row - including the snapshot rows added so their
/// usage could be seen - so pressing it there always failed. A snapshot IS
/// switchable: `use` copies its credentials into place, which is what switching
/// a snapshot has always meant.
pub fn switch_verb(is_slot: bool) -> &'static str {
    if is_slot {
        "serve"
    } else {
        "use"
    }
}

/// What the dashboard may claim after a switch.
#[derive(Debug, PartialEq, Eq)]
pub enum SwitchClaim {
    /// The payer agrees: the running session is on this account now.
    Serving,
    /// No proxy carries this tool, so the switch reaches the NEXT session.
    Saved,
    /// A proxy is running but the payer is not who was asked for. Carries who
    /// it actually is, empty when nobody does.
    NotConfirmed(String),
}

/// Decide what a switch may be announced as.
///
/// The dashboard printed "<name> now serves the running session" after checking
/// only that a proxy was alive - never that the pointer had moved. A switch that
/// quietly did not take therefore produced the sentence saying it did, which is
/// the worst reading available: the account is believed changed, and every later
/// symptom looks like a different bug.
pub fn switch_result(asked: &str, proxy_running: bool, paying: Option<&str>) -> SwitchClaim {
    if !proxy_running {
        return SwitchClaim::Saved;
    }
    match paying {
        Some(p) if p == asked => SwitchClaim::Serving,
        Some(p) => SwitchClaim::NotConfirmed(p.to_string()),
        None => SwitchClaim::NotConfirmed(String::new()),
    }
}

/// What an empty window column should say, given whether the source publishes
/// that window at all.
///
/// "not started" is a CLAIM: the window exists and has not been touched. True
/// for Claude, whose 5h window begins on first use. False for Codex, whose
/// server never sends a session window - and there the phrase reads as "you
/// have room" beside an account the user has just been told is out.
/// Does the source behind this row publish a session (5h) window at all?
///
/// Codex's server never sends one, so "not started" - which CLAIMS the window
/// exists and is untouched - reads as "you have room" beside an account just
/// reported as out. The check compared the row's tools string to "codex"
/// exactly, and that string carries the active marker: the ACTIVE Codex account
/// reads "codex*", failed the comparison, and got Claude's phrase while the idle
/// one beside it got the right one. A display string is not a key.
pub fn publishes_session_window(tools: &str) -> bool {
    let mut named = tools
        .split(',')
        .map(|t| t.trim().trim_end_matches('*'))
        .filter(|t| !t.is_empty())
        .peekable();
    if named.peek().is_none() {
        return true; // nothing known: keep the old default
    }
    !named.all(|t| t == "codex")
}

pub fn empty_window_phrase(source_publishes: bool) -> &'static str {
    if source_publishes {
        "not started"
    } else {
        "not reported"
    }
}

/// `reset_slot`, told whether the source publishes this window.
pub fn reset_slot_reason(
    reset: Option<&str>,
    word: bool,
    width: usize,
    source_publishes: bool,
) -> String {
    let phrase = empty_window_phrase(source_publishes);
    let body = match reset {
        Some(r) if word => format!("resets {r}"),
        Some(r) => r.to_string(),
        None if width >= phrase.chars().count() => phrase.to_string(),
        None => String::new(),
    };
    let pad = width.saturating_sub(body.chars().count());
    format!("{body}{}", " ".repeat(pad))
}

pub fn reset_slot(reset: Option<&str>, word: bool, width: usize) -> String {
    const UNSTARTED: &str = "not started";
    let body = match reset {
        Some(r) if word => format!("resets {r}"),
        Some(r) => r.to_string(),
        None if width >= UNSTARTED.chars().count() => UNSTARTED.to_string(),
        None => String::new(),
    };
    let pad = width.saturating_sub(body.chars().count());
    format!("{body}{}", " ".repeat(pad))
}

/// How wide that slot has to be for every row in the frame to line up. Zero when
/// no row has a reset at all - then the column is not drawn.
pub fn reset_slot_width(resets: &[Option<String>], word: bool) -> usize {
    let longest = resets
        .iter()
        .flatten()
        .map(|r| r.chars().count() + if word { 7 } else { 0 })
        .max()
        .unwrap_or(0);
    if longest == 0 {
        0
    } else {
        // Sized for whichever phrase can appear, so rows saying different
        // things still line up.
        longest
            .max(empty_window_phrase(true).len())
            .max(empty_window_phrase(false).len())
    }
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
/// The point at which a window counts as gone. One number, because a row and the
/// fleet line both ask the question and two copies would drift.
const SPENT: f64 = 99.0;

fn account_status<'a>(r: &'a Row, u: Option<&Usage>) -> (&'a str, Color) {
    if r.disabled {
        // Out of rotation is a deliberate state, so it is said plainly and not
        // dressed as a problem.
        return ("paused", Color::Rgb(110, 108, 128));
    }
    if r.needs_login {
        // Nothing else matters until it can authenticate.
        return ("no login", Color::Rgb(200, 150, 90));
    }
    if r.stale {
        // An account that cannot serve this turn is not ready, whatever its
        // quota says - and the row's own note explains which kind of stale.
        return ("expired", Color::Rgb(200, 150, 90));
    }
    if let Some(w) = r.warn.as_deref() {
        // A snapshot problem outranks quota: the account cannot serve at all.
        return (w, Color::Rgb(200, 150, 90));
    }
    let spent = u.is_some_and(|u| {
        !u.on_credits
            && (u.five_h.is_some_and(|p| p >= SPENT) || u.seven_d.is_some_and(|p| p >= SPENT))
    });
    // Full windows, but extra usage is carrying it: the account works, and
    // saying "spent" beside one that is answering turns is simply false.
    if !spent
        && u.is_some_and(|u| {
            u.on_credits
                && (u.five_h.is_some_and(|p| p >= SPENT) || u.seven_d.is_some_and(|p| p >= SPENT))
        })
    {
        return (ON_CREDITS, Color::Rgb(200, 150, 90));
    }
    match (spent, r.active) {
        (true, _) => ("spent", Color::Rgb(196, 92, 96)),
        (false, true) => ("active", VIOLET),
        (false, false) => ("ready", Color::Rgb(120, 118, 140)),
    }
}

/// The row the cursor is on has to be findable at a glance. A bold-only
/// highlight was nearly invisible - the letters change weight and nothing else -
/// but painting the whole row would cover the quota bars, whose colour IS their
/// value. So the fill stops where the bars begin: `upto` is the number of leading
/// spans (marker, number, glyph, name, identity, status) that may be tinted.
///
/// Returns the styled spans, so the caller cannot forget to use the result.
fn mark_selected(mut spans: Vec<Span<'static>>, upto: usize, selected: bool) -> Vec<Span<'static>> {
    if !selected {
        return spans;
    }
    for sp in spans.iter_mut().take(upto) {
        // Keep each span's own colour - the status word stays its status colour -
        // and add the band behind it.
        sp.style = sp.style.bg(SELECT_BG).add_modifier(Modifier::BOLD);
    }
    spans
}

/// The band behind the selected row: the page's violet, dark enough that white
/// text and the status colours all stay legible on it.
const SELECT_BG: Color = Color::Rgb(48, 42, 78);

/// A box of `w` x `h` in the middle of `area`, clamped to fit.
fn centered(area: ratatui::layout::Rect, w: u16, h: u16) -> ratatui::layout::Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    ratatui::layout::Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Give the terminal back for the duration of `f`, then take it again.
///
/// A child process that draws its own interface - a sign-in prompt - cannot share
/// a screen with a live TUI: the alternate screen and raw mode have to be handed
/// back or its output lands in a buffer nobody sees and its keystrokes never
/// arrive.
fn suspended<T>(terminal: &mut ratatui::DefaultTerminal, f: impl FnOnce() -> T) -> T {
    // Mouse capture has to go too, and it is NOT part of restore(): while it is on
    // the terminal reports clicks and selections as escape sequences instead of
    // acting on them, so a mouse paste into the child never arrives as text. A
    // sign-in prompt asks for a pasted code, which made this the difference
    // between "the prompt takes no input" and a working sign-in.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    let out = f();
    if let Ok(t) = ratatui::try_init() {
        *terminal = t;
    }
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    );
    let _ = terminal.clear();
    out
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
#[derive(Clone, Default)]
pub struct Usage {
    pub five_h: Option<f64>,
    pub five_h_reset: Option<i64>,
    pub seven_d: Option<f64>,
    pub seven_d_reset: Option<i64>,
    /// For figures that are a SNAPSHOT rather than a live read (Codex has no
    /// endpoint to ask): unix seconds when they were recorded. `None` means the
    /// numbers are current as of this refresh.
    pub observed_at: Option<i64>,
    /// Why there are no numbers, when there are none. Empty tracks alone cannot
    /// distinguish "never read" from "could not be read" from "nothing left".
    pub note: Option<String>,
    /// The account keeps serving past a full window, billed to extra usage. A
    /// window at 100% is then not the end of it: the account was answering turns
    /// all afternoon while the row called it spent.
    pub on_credits: bool,
    /// Who the account said it was, when the reading came from somewhere that
    /// says. It travels with the numbers because it arrived with them, on the
    /// same response - and a row labelled from a local file has no way to know
    /// the server disagrees.
    pub ident: Option<String>,
}

/// The word for an account whose windows are full but whose credits are not.
pub const ON_CREDITS: &str = "credits";

/// The trailing column for a row's figures: the reason there are none if there
/// is one, else how old they are.
///
/// `checking` says a live read is in flight and these numbers came from the last
/// one. Remembered numbers are drawn immediately so the gauges are never blank,
/// but under fifteen minutes old they carry no age - which made a number from
/// before the last hour of work look exactly like a current one. Whatever was
/// spent since is missing from it, and nothing on screen said so.
fn trailing_note(u: &Usage, checking: bool) -> String {
    match &u.note {
        Some(n) if !n.is_empty() => n.clone(),
        _ => {
            let age = observed_note(u.observed_at);
            match (checking, u.observed_at.is_some()) {
                (true, true) if age.is_empty() => "checking\u{2026}".to_string(),
                (true, true) => format!("{age}, checking\u{2026}"),
                _ => age,
            }
        }
    }
}

pub struct Row {
    pub name: String,
    pub ident: String,
    pub tools: String,
    pub active: bool,
    pub warn: Option<String>,
    /// Kept out of automatic rotation (still switchable by hand).
    pub disabled: bool,
    /// A slot account with no readable login yet - it cannot serve a turn until
    /// its tool signs in there.
    pub needs_login: bool,
    /// Signed in, but the access token has lapsed. It may renew itself on the
    /// next turn and it may need a fresh sign-in; either way it cannot serve
    /// right now, so saying "ready" would be a promise the row cannot keep.
    pub stale: bool,
    /// Backed by a permanent slot. Switching one moves the default pointer, which
    /// is what the proxy and the active marker follow; switching a snapshot copies
    /// credentials and moves no pointer at all.
    pub is_slot: bool,
    /// Names of rows merged into this one - the same account saved twice. A usage
    /// reading is filed under the name it was taken for, which may be one of
    /// these, so they have to stay reachable after the merge.
    pub also: Vec<String>,
}

/// This row's reading, under its own name or under any name it absorbed.
///
/// An entry that carries NUMBERS is preferred over one that carries only a reason
/// there are none. The merged names are one account with one quota, so a snapshot
/// whose token expired says nothing about the slot that answered for the same
/// login - and showing its reason instead of the slot's figures loses the only
/// real information present.
/// Fold a finished usage reading into what the dashboard already shows.
///
/// The map used to be replaced with whatever came back, and stamped as freshly
/// read. An empty round - the usage endpoint throttles, for minutes at a time -
/// therefore blanked every gauge and reset the staleness timer, so the numbers
/// stayed gone while the cache on disk still held good ones. Leave the dashboard
/// open long enough and one empty round wipes it.
///
/// Returns the map to show and whether a reading actually landed.
pub fn merge_reading(
    current: Option<std::collections::HashMap<String, Usage>>,
    incoming: Vec<(String, Usage)>,
) -> (Option<std::collections::HashMap<String, Usage>>, bool) {
    if incoming.is_empty() {
        return (current, false);
    }
    (Some(incoming.into_iter().collect()), true)
}

pub fn usage_for<'a>(
    map: &'a std::collections::HashMap<String, Usage>,
    r: &Row,
) -> Option<&'a Usage> {
    let has_numbers = |u: &&Usage| u.five_h.is_some() || u.seven_d.is_some();
    let names = || std::iter::once(&r.name).chain(r.also.iter());
    names()
        .filter_map(|n| map.get(n))
        .find(has_numbers)
        .or_else(|| names().find_map(|n| map.get(n)))
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
    fn switch(&mut self, name: &str, is_slot: bool) -> (bool, String);
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
    /// Sign this account in and RETURN. Adding several accounts is what the
    /// dashboard is for, and handing the sign-in back to the shell tore the
    /// dashboard down after each one.
    fn sign_in(&mut self, name: &str) -> (bool, String);
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
    /// What was read last time, from disk. Drawn immediately so the bars are
    /// there on the first frame instead of six seconds later; each carries its
    /// own age, so a remembered number is never passed off as current.
    fn cached_quota(&mut self) -> Vec<(String, Usage)> {
        Vec::new()
    }
    /// Start a reading and hand back the channel it will arrive on. The default
    /// answers immediately from `quota_pct`, so a context that has nothing to
    /// fetch needs no threads.
    fn quota_pct_async(&mut self) -> std::sync::mpsc::Receiver<Vec<(String, Usage)>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = tx.send(self.quota_pct());
        rx
    }
    /// Is a `swapdex proxy` running right now? When it is, a switch takes effect
    /// in the session that is ALREADY open, so Enter has no reason to leave the
    /// screen to start a new conversation. Default false so test contexts need
    /// not implement it.
    fn proxy_running(&mut self) -> bool {
        false
    }
    /// Who is paying for turns right now, so a switch is confirmed rather than
    /// assumed. `None` when nobody is. Default None so test contexts need not
    /// implement it.
    fn paying_account(&mut self) -> Option<String> {
        None
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
    // An account with no saved profile has no tool list to filter by - and every
    // slot account is one, so filtering left them with no way to start anything.
    // Offer the two swapdex can launch into an account's own home instead.
    if tools.is_empty() {
        return NEW_CONV[..2].to_vec();
    }
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
fn quota_bar(pct: Option<f64>, width: usize) -> Vec<Span<'static>> {
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
    // The gauge carries ONE reading: how much is left.
    //
    // The bar's fill and its number both measure what is LEFT. They used to
    // disagree - the fill rose as an account was spent while the number counted
    // down - so an untouched account showed "100% left" across an empty bar. A
    // fuel gauge that empties as you fill the tank is not a gauge.
    //
    // The word matters too: "2%" on a window that had just reset read as almost
    // nothing left when it meant the opposite.
    //
    // The reset time is NOT in here. Crammed in beside the percentage it read
    // as a second quantity of quota ("62% left 6d"), and it forced the bar wide
    // enough to hold a sentence. It sits outside the gauge now.
    let left = 100.0 - pct;
    let mut label = format!("{left:.0}%");
    for candidate in [format!("{left:.0}% left"), format!("{left:.0}%")] {
        if candidate.chars().count() <= width {
            label = candidate;
            break;
        }
    }
    let lw = label.chars().count().min(width);
    let left_pad = (width - lw) / 2;
    let text: String = " ".repeat(left_pad)
        + &label.chars().take(lw).collect::<String>()
        + &" ".repeat(width - left_pad - lw);
    // Split where the fill ends: the label reads on both grounds because each
    // half carries its own foreground colour.
    //
    // The fill measures what is LEFT, the same thing the number says. It used to
    // measure what was SPENT, so the two halves of one gauge said opposite
    // things: an untouched account read "100% left" across an empty bar. A fuel
    // gauge that empties as you fill the tank is not a gauge.
    let filled = ((left / 100.0) * width as f64)
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
/// The same instant as a clock time on the wall. `swapdex quota` counts down
/// because it prints once and is read at once; this is the dashboard, where a
/// time that does not move is easier to hold in your head than a number that
/// ticks - and a time cannot be mistaken for a quantity of quota.
fn fmt_reset_clock(resets_at_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // The endpoint reports either an absolute epoch or a relative span; a small
    // number is seconds from now, the same reading `fmt_reset` takes.
    let at = if resets_at_secs > now {
        resets_at_secs
    } else if resets_at_secs > 0 && resets_at_secs < 60 * 60 * 24 * 30 {
        now + resets_at_secs
    } else {
        return String::new();
    };
    crate::proxy::pick::reset_clock(at, now, crate::proxy::tz_offset())
}

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
/// Milestone timing, printed only when `SWAPDEX_TIMING` is set.
///
/// Marks are cheap and the variable is off by default, so they can sit on the
/// path a complaint is about - which is the only place they are any use.
///
/// A startup delay someone reports and nobody can reproduce is a delay in THEIR
/// environment, and no amount of measuring elsewhere will find it. This makes the
/// program say where its own time went, on the machine that is slow.
struct Timing {
    start: std::time::Instant,
    on: bool,
    /// Marks are held until the screen is handed back. Printing them as they
    /// happen wrote them ONTO the dashboard - by the second mark the terminal is
    /// in the alternate screen, so a line lands beside an account and the marks
    /// before it are wiped by the screen switch. The instrument was destroying
    /// the reading it was taking.
    marks: std::cell::RefCell<Vec<String>>,
}

impl Timing {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            on: std::env::var_os("SWAPDEX_TIMING").is_some(),
            marks: std::cell::RefCell::new(Vec::new()),
        }
    }
    fn mark(&self, what: &str) {
        if self.on {
            self.marks.borrow_mut().push(format!(
                "[timing] {:>6} ms  {what}",
                self.start.elapsed().as_millis()
            ));
        }
    }
    /// Print what was collected, once the terminal is a terminal again.
    fn report(&self) {
        for line in self.marks.borrow().iter() {
            eprintln!("{line}");
        }
    }
}

pub fn run(ctx: &mut dyn TuiCtx) -> Result<Outcome> {
    let timing = Timing::new();
    timing.mark("start");
    let mut terminal = ratatui::try_init()?;
    timing.mark("terminal ready");
    // Mouse: scroll to move, click to select/switch - the "manage by clicking"
    // the picker was asked for. Best-effort; key control is unaffected if the
    // terminal refuses.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    );
    let mut rows = ctx.rows();
    timing.mark("accounts read");
    let mut state = ListState::default();
    state.select(Some(rows.iter().position(|r| r.active).unwrap_or(0)));
    let mut open_state = ListState::default();
    let mut status = String::new();
    let mut confirm_delete: Option<(usize, std::time::Instant)> = None;
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
    // Start from what was read last time: the live read takes seconds, and an
    // empty gauge for that long says "no quota" when the truth is "not asked yet".
    let cached = ctx.cached_quota();
    timing.mark("remembered usage read");
    let mut quota_pct: Option<std::collections::HashMap<String, Usage>> =
        (!cached.is_empty()).then(|| cached.into_iter().collect());
    let mut first_frame = true;
    let mut fetch_marked = false;
    let mut first_key_marked = false;
    // A reading in flight, if one is.
    let mut quota_rx: Option<std::sync::mpsc::Receiver<Vec<(String, Usage)>>> = None;
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
                    // Reset slots are sized across every row, not per row: a
                    // window nobody has used has no reset, and letting that row
                    // draw a shorter slot broke the column down the list.
                    let (all5, all7): (Vec<Option<String>>, Vec<Option<String>>) = rows
                        .iter()
                        .map(|r| {
                            let u = quota_pct
                                .as_ref()
                                .and_then(|q| usage_for(q, r).cloned())
                                .unwrap_or_default();
                            let f = |at: Option<i64>| {
                                at.map(fmt_reset_clock).filter(|s| !s.is_empty())
                            };
                            (f(u.five_h_reset), f(u.seven_d_reset))
                        })
                        .unzip();
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
                            let u_now = quota_pct.as_ref().and_then(|q| usage_for(q, r));
                            let (st, st_color) = account_status(r, u_now);
                            // Draw the selection marker on the ACCOUNT line
                            // ourselves: the widget puts highlight_symbol on an
                            // item's FIRST line, which for a group's first account
                            // is the heading - so the cursor appeared to sit on the
                            // heading while the selection was really the account.
                            let selected = state.selected() == Some(ri);
                            // How many leading spans are text rather than gauge
                            // fill - assigned where the bars are appended, which
                            // every row does.
                            let text_cols;
                            let mut top = vec![
                                Span::styled(
                                    // A solid half-block, not a thin rule: this is
                                    // the one mark that says "you are here".
                                    if selected { "\u{258c} " } else { "  " },
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
                                    .and_then(|q| usage_for(q, r).cloned())
                                    .unwrap_or_default();
                                // Everything up to here may be tinted; the bars
                                // themselves carry the reading in their fill.
                                text_cols = top.len();
                                let left_w: usize =
                                    top.iter().map(|s| s.content.chars().count()).sum();
                                let inner = (body.width as usize).saturating_sub(4);
                                // "5h " + gauge + reset + "  7d " + gauge + reset.
                                // The gauge holds only the reading; the reset
                                // time sits BESIDE it, where there is room, so
                                // the bar stays narrow and the two numbers can
                                // never be mistaken for each other.
                                let avail = inner.saturating_sub(bar_col);
                                let stamp = |at: Option<i64>| -> String {
                                    at.map(fmt_reset_clock)
                                        .filter(|r| !r.is_empty())
                                        .unwrap_or_default()
                                };
                                let (r5, r7) = (stamp(u.five_h_reset), stamp(u.seven_d_reset));
                                // Longest first: the gauges are the point, the
                                // times ride along when the terminal has room,
                                // and a narrow window keeps the readings.
                                let bw = if avail >= 34 { 12 } else { 7 };
                                // TWO spaces, not one. The gauge ends in a dark
                                // track cell, so a single space left the time
                                // butted against it and reading as part of the
                                // bar - which is the confusion moving it
                                // outside was meant to end.
                                // Two leading spaces, then a slot sized across the
                                // WHOLE frame. The gauge ends in a dark track
                                // cell, so one space left the time butted against
                                // it; and a row whose window has no reset must
                                // still hold the column, or everything after it
                                // slides left on that row alone.
                                // Codex's server never publishes a session
                                // window, so an empty 5h cell there means "not
                                // reported", not "not started" - the latter
                                // reads as room the account may not have.
                                let publishes_5h = publishes_session_window(&r.tools);
                                let slot = |r: &str, word: bool, w: usize| -> String {
                                    if w == 0 {
                                        String::new()
                                    } else {
                                        let v = (!r.is_empty()).then_some(r);
                                        format!("  {}", reset_slot_reason(v, word, w, publishes_5h))
                                    }
                                };
                                let (t5, t7) = {
                                    let mk = |word: bool| {
                                        (
                                            slot(&r5, word, reset_slot_width(&all5, word)),
                                            slot(&r7, word, reset_slot_width(&all7, word)),
                                        )
                                    };
                                    let full = mk(true);
                                    let bare = mk(false);
                                    let fits = |a: &str, b: &str| {
                                        3 + bw + a.chars().count()
                                            + 5 + bw + b.chars().count()
                                            <= avail
                                    };
                                    if fits(&full.0, &full.1) {
                                        full
                                    } else if fits(&bare.0, &bare.1) {
                                        bare
                                    } else {
                                        (String::new(), String::new())
                                    }
                                };
                                let needed = 3 + bw + t5.chars().count()
                                    + 5 + bw + t7.chars().count();
                                let start = bar_col.min(inner.saturating_sub(needed));
                                top.push(
                                    Span::raw(" ".repeat(start.saturating_sub(left_w).max(1))),
                                );
                                top.push(Span::styled("5h ", Style::default().fg(MUTED)));
                                top.extend(quota_bar(u.five_h, bw));
                                top.push(Span::styled(t5, Style::default().fg(MUTED)));
                                top.push(Span::styled("  7d ", Style::default().fg(MUTED)));
                                top.extend(quota_bar(u.seven_d, bw));
                                top.push(Span::styled(t7, Style::default().fg(MUTED)));
                                // Snapshot figures say when they were taken, so an
                                // old number is never read as a current one.
                                let note = trailing_note(&u, quota_rx.is_some());
                                if !note.is_empty() {
                                    top.push(Span::styled(
                                        format!("  {note}"),
                                        Style::default().fg(Color::Rgb(96, 94, 116)),
                                    ));
                                }
                            }
                            let top = mark_selected(top, text_cols, selected);
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
                                // What the whole group adds up to, on the line
                                // that already exists. Adding the percentages up
                                // in your head is work a dashboard should have
                                // done, and a separate row would cost a line per
                                // tool on a screen whose point is seeing accounts
                                // together.
                                let fleet = fleet_of(
                                    rows.iter().filter(|x| group_of(&x.tools) == g),
                                    |x| quota_pct.as_ref().and_then(|q| usage_for(q, x)),
                                );
                                let summary = fleet_line(&fleet);
                                let rule_w = bar_col.saturating_sub(
                                    title.chars().count() + 3 + summary.chars().count(),
                                );
                                lines.push(Line::from(vec![
                                    Span::styled(
                                        format!("  {title}"),
                                        Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                                    ),
                                    Span::styled(
                                        "\u{2500}".repeat(rule_w.clamp(2, 40)),
                                        Style::default().fg(Color::Rgb(72, 70, 88)),
                                    ),
                                    Span::styled(
                                        summary,
                                        Style::default().fg(Color::Rgb(140, 138, 160)),
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
                    let foot_line = if let Some((i, _)) = confirm_delete {
                        // Say what this actually does. "Delete" over an account
                        // whose folder and login both survive invites someone to
                        // decline a harmless action - or to expect a folder gone
                        // that is still there.
                        Line::from(Span::styled(
                            format!(
                                "  stop managing '{}'? its login and folder stay.  y / N",
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
                    // Starting something new comes first. Appended after the
                    // recent sessions it sat below the fold on any account with a
                    // few of them, so the screen looked like it could only reopen
                    // the past.
                    let mut items: Vec<ListItem> = new_conv
                        .iter()
                        .map(|(nlabel, _)| {
                            ListItem::new(Line::from(Span::styled(
                                *nlabel,
                                Style::default().fg(VIOLET),
                            )))
                        })
                        .collect();
                    items.extend(
                        entries
                            .iter()
                            .map(|e| ListItem::new(Line::from(e.line.clone()))),
                    );
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
                            format!(" rename {old} "),
                            "type a new name, then press Enter".to_string(),
                        ),
                        InputKind::SaveCurrent => (
                            " save the accounts you are signed into ".to_string(),
                            "name them, then press Enter".to_string(),
                        ),
                    };
                    // A small dialog, not a full-screen box. It used to draw one
                    // dim line inside thirty empty rows, which does not read as
                    // "type here" at all - so people pressed Enter to get out of
                    // it and reported that nothing happened.
                    let dialog = centered(main, 60.min(main.width.saturating_sub(4)), 5);
                    f.render_widget(ratatui::widgets::Clear, dialog);
                    f.render_widget(
                        Paragraph::new(vec![
                            Line::from(vec![Span::styled(
                                format!("  {prompt}"),
                                Style::default().fg(Color::White),
                            )]),
                            Line::from(""),
                            Line::from(vec![
                                Span::raw("  "),
                                Span::styled(
                                    format!("{value}\u{2588}"),
                                    Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                                ),
                            ]),
                        ])
                        .block(list_block_titled(&title)),
                        dialog,
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
        if first_frame {
            timing.mark("first frame drawn");
            first_frame = false;
        }
        if quota_rx.is_some() && !fetch_marked {
            timing.mark("usage read started (off the loop)");
            fetch_marked = true;
        }
        // Collect a finished reading without waiting for one.
        if let Some(rx) = quota_rx.as_ref() {
            if let Ok(got) = rx.try_recv() {
                // A reading can arrive knowing whose account it is, and a row
                // labelled from a local file cannot know the server disagrees.
                apply_live_identity(&mut rows, &got);
                // An empty round is not a reading: keep the numbers already on
                // screen rather than blanking every gauge, and do not stamp it
                // as fresh - the next tick should try again soon.
                let (next, landed) = merge_reading(quota_pct.take(), got);
                quota_pct = next;
                if landed {
                    quota_fetched = Some(std::time::Instant::now());
                }
                quota_rx = None;
            }
        }
        let stale_quota = quota_fetched.is_none_or(|t: std::time::Instant| {
            t.elapsed() >= std::time::Duration::from_secs(QUOTA_REFRESH_SECS)
        });
        if matches!(screen, Screen::Main) && stale_quota && quota_rx.is_none() && !rows.is_empty() {
            // OFF the loop. Reading every account's usage is several network
            // round trips with backoff, and doing it here froze the dashboard
            // for seconds on open - no keys, no cursor - which reads as the tool
            // being broken. The bars fill in when the answer arrives.
            quota_rx = Some(ctx.quota_pct_async());
        }
        // A left click on a menu item both selects AND activates it; treat
        // that as a synthesized Enter so the key handler below does the work.
        let mut click_activate = false;
        // Wait for input, but not forever: without a timeout the loop blocks until
        // a keypress, so a dashboard left alone would never refresh its numbers.
        if !event::poll(std::time::Duration::from_millis(500))? {
            continue;
        }
        if !first_key_marked {
            timing.mark("first input event received");
            first_key_marked = true;
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
                if let Some((i, opened)) = confirm_delete {
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        // A `y` that lands in the same instant as the `d` that
                        // opened this prompt was not typed - refuse it and leave
                        // the account alone.
                        if !confirm_is_deliberate(0, opened.elapsed().as_millis() as u64) {
                            status =
                                "that arrived too fast to be typed - nothing was deleted".into();
                            confirm_delete = None;
                            continue;
                        }
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
                            let (ok, msg) = ctx.switch(
                                &name,
                                state
                                    .selected()
                                    .and_then(|i| rows.get(i))
                                    .is_some_and(|r| r.is_slot),
                            );
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
                            let (ok, msg) = ctx.switch(
                                &name,
                                state
                                    .selected()
                                    .and_then(|i| rows.get(i))
                                    .is_some_and(|r| r.is_slot),
                            );
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
                            let (ok, msg) = ctx.switch(
                                &name,
                                state
                                    .selected()
                                    .and_then(|i| rows.get(i))
                                    .is_some_and(|r| r.is_slot),
                            );
                            status = msg;
                            rows = ctx.rows();
                            clamp_selection(&mut state, rows.len());
                            // `o` means "open a conversation", and it has to keep
                            // meaning that: with a proxy running this went straight
                            // back to the list with a note, so the one way to start
                            // a new chat disappeared exactly when proxy mode became
                            // the normal setup. The proxy is worth saying - the
                            // switch already reached the session you have open -
                            // but it is a note on the screen, not a reason to skip
                            // showing it.
                            if ok {
                                if ctx.proxy_running() {
                                    status = format!("{name} also serves the session already open");
                                }
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
                        let (_ok, msg) = ctx.switch("-", true);
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
                            // Hand the terminal to the tool's own login, then
                            // come back and redraw - the account list is where
                            // the result belongs.
                            let (_ok, msg) = suspended(&mut terminal, || ctx.sign_in(&name));
                            status = msg;
                            rows = ctx.rows();
                            clamp_selection(&mut state, rows.len());
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
                        confirm_delete = state.selected().map(|i| (i, std::time::Instant::now()));
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
                    // The new-conversation entries are drawn FIRST, so the index
                    // has to be read the same way round - reading it the old way
                    // after the reorder would open a session when the user asked
                    // for a new chat.
                    let i = open_state.selected().unwrap_or(0);
                    let Some(&(_, tool)) = new_conv.get(i) else {
                        break 'ui Outcome::OpenSession(i - new_conv.len());
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
    timing.report();
    Ok(outcome)
}

/// The one line that answers "how much have I got, all in?" before the per-row
/// detail does.
///
/// Reading a column of percentages and adding them up in your head is the thing
/// a dashboard should have already done. teamclaude puts a FLEET row above its
/// accounts for the same reason.
#[derive(Debug, Default, PartialEq)]
pub struct Fleet {
    /// Accounts that could serve a turn right now.
    pub ready: usize,
    /// Accounts in the group at all.
    pub total: usize,
    /// Headroom summed across the ready ones, as a share of what they could hold
    /// if every one were untouched. `None` when nothing was measured - which is
    /// not the same as nothing left.
    pub left_pct: Option<f64>,
    /// The soonest moment any spent account comes back, unix seconds.
    pub next_reset: Option<i64>,
}

/// The summary as it appears beside a group heading. Empty when there is nothing
/// worth saying - one account is not a fleet, and a line that restates the row
/// below it is noise.
pub fn fleet_line(f: &Fleet) -> String {
    if f.total < 2 {
        return String::new();
    }
    let mut s = format!("  {}/{} ready", f.ready, f.total);
    if let Some(left) = f.left_pct {
        s.push_str(&format!(" · {left:.0}% left"));
    }
    s
}

/// Summarise one tool's rows. `usage` looks a row's figures up by name.
pub fn fleet_of<'a>(
    rows: impl Iterator<Item = &'a Row>,
    usage: impl Fn(&Row) -> Option<&'a Usage>,
) -> Fleet {
    let (mut f, mut measured, mut sum) = (Fleet::default(), 0usize, 0.0f64);
    for r in rows {
        f.total += 1;
        let u = usage(r);
        // "Ready" is about serving, so a row with no login is never counted -
        // an account nobody can use is not capacity.
        let spent = u.is_some_and(|u| {
            !u.on_credits
                && (u.five_h.is_some_and(|p| p >= SPENT) || u.seven_d.is_some_and(|p| p >= SPENT))
        });
        if !r.needs_login && !spent {
            f.ready += 1;
        }
        if let Some(u) = u {
            if let Some(worst) = [u.five_h, u.seven_d]
                .into_iter()
                .flatten()
                .fold(None::<f64>, |acc, p| Some(acc.map_or(p, |a: f64| a.max(p))))
            {
                measured += 1;
                sum += (100.0 - worst).clamp(0.0, 100.0);
            }
            for reset in [u.five_h_reset, u.seven_d_reset].into_iter().flatten() {
                f.next_reset = Some(f.next_reset.map_or(reset, |cur: i64| cur.min(reset)));
            }
        }
    }
    if measured > 0 {
        f.left_pct = Some(sum / measured as f64);
    }
    f
}

#[cfg(test)]
mod fleet_tests {
    use super::*;

    fn row(name: &str, needs_login: bool) -> Row {
        Row {
            name: name.into(),
            ident: String::new(),
            tools: "claude-code".into(),
            active: false,
            warn: None,
            disabled: false,
            needs_login,
            stale: false,
            is_slot: true,
            also: Vec::new(),
        }
    }
    fn used(five_h: f64, reset: i64) -> Usage {
        Usage {
            five_h: Some(five_h),
            five_h_reset: Some(reset),
            ..Default::default()
        }
    }

    #[test]
    fn it_counts_what_could_actually_serve() {
        let rows = [
            row("fresh", false),
            row("spent", false),
            row("nologin", true),
        ];
        let u = |r: &Row| match r.name.as_str() {
            "fresh" => Some(Box::leak(Box::new(used(10.0, 500))) as &Usage),
            "spent" => Some(Box::leak(Box::new(used(100.0, 200))) as &Usage),
            _ => None,
        };
        let f = fleet_of(rows.iter(), u);
        assert_eq!(f.total, 3);
        assert_eq!(
            f.ready, 1,
            "the spent one and the signed-out one are not capacity"
        );
        assert_eq!(f.left_pct, Some(45.0), "90 and 0 across the two measured");
        assert_eq!(f.next_reset, Some(200), "the soonest anything comes back");
    }

    /// Nothing measured is not "nothing left" - an empty gauge and an empty
    /// account must not read the same.
    #[test]
    fn unmeasured_is_not_reported_as_empty() {
        let rows = [row("a", false)];
        let f = fleet_of(rows.iter(), |_| None);
        assert_eq!(f.left_pct, None);
        assert_eq!(f.ready, 1, "unmeasured but signed in - it can still serve");
    }

    /// An account past its window but carrying credits is still capacity.
    #[test]
    fn credits_still_count_as_ready() {
        let rows = [row("credits", false)];
        let f = fleet_of(rows.iter(), |_| {
            Some(Box::leak(Box::new(Usage {
                five_h: Some(100.0),
                on_credits: true,
                ..Default::default()
            })))
        });
        assert_eq!(f.ready, 1);
    }
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
            is_slot: false,
            stale: false,
            also: Vec::new(),
        };
        // A snapshot and a slot for the same login: the slot with a login wins.
        let out = dedupe_by_identity(vec![
            row("rnd", "rnd@x.co [team]", true, false),
            row("rnd-slot", "rnd@x.co", false, true),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "rnd-slot");
        // Equally usable: the SLOT still wins, because switching one moves the
        // pointer the proxy and the marker follow, while switching a snapshot
        // moves nothing they can see - Enter would look like it did nothing.
        let mut snap = row("rnd", "rnd@x.co [team]", false, false);
        let mut slot = row("rnd-slot", "rnd@x.co", false, false);
        snap.is_slot = false;
        slot.is_slot = true;
        let out = dedupe_by_identity(vec![snap, slot]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "rnd-slot", "the switchable row survives");
        // Different logins are left alone.
        let out = dedupe_by_identity(vec![
            row("a", "a@x.co", false, false),
            row("b", "b@x.co", false, false),
        ]);
        assert_eq!(out.len(), 2);
        // The SAME address on two different tools is two accounts - signing into
        // Claude and ChatGPT with one email is normal, and merging them would
        // delete a real account from the list.
        let mut claude = row("claude", "me@x.co [max]", false, false);
        let mut codex = row("codex", "me@x.co [chatgpt]", false, true);
        claude.tools = "claude-code".into();
        codex.tools = "codex".into();
        let out = dedupe_by_identity(vec![claude, codex]);
        assert_eq!(
            out.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["claude", "codex"],
            "one email on two tools stays two accounts"
        );
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

    // Merging two rows into one hides a name, and usage is filed under whichever
    // name the reading was taken for. The snapshot 'rnd' and the slot 'rnd-slot'
    // are one account: a reading taken for either has to reach the surviving row,
    // or a measured account displays as blank - which reads as "nothing left".
    #[test]
    fn a_merged_row_still_finds_usage_filed_under_the_name_it_absorbed() {
        let row = |name: &str, is_slot: bool| Row {
            name: name.into(),
            ident: "rnd@x.co".into(),
            tools: "claude-code".into(),
            active: false,
            warn: None,
            disabled: false,
            needs_login: false,
            stale: false,
            is_slot,
            also: Vec::new(),
        };
        let out = dedupe_by_identity(vec![row("rnd", false), row("rnd-slot", true)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "rnd-slot");
        assert_eq!(out[0].also, vec!["rnd".to_string()], "the absorbed name");

        let measured = |pct: f64| Usage {
            five_h: Some(pct),
            ..Default::default()
        };
        let reason = |why: &str| Usage {
            note: Some(why.into()),
            ..Default::default()
        };
        let mut usage = std::collections::HashMap::new();
        usage.insert("rnd".to_string(), measured(42.0));
        assert_eq!(
            usage_for(&usage, &out[0]).and_then(|u| u.five_h),
            Some(42.0),
            "a reading taken for the snapshot belongs to the same account"
        );
        // The row's own name wins when both were actually measured.
        usage.insert("rnd-slot".to_string(), measured(7.0));
        assert_eq!(usage_for(&usage, &out[0]).and_then(|u| u.five_h), Some(7.0));
        // But a name that only carries a REASON does not outrank a name that
        // carries numbers: the account has one quota, and 'the snapshot's token
        // expired' says nothing about the slot that answered for the same login.
        usage.insert("rnd-slot".to_string(), reason("saved token expired"));
        assert_eq!(
            usage_for(&usage, &out[0]).and_then(|u| u.five_h),
            Some(42.0),
            "numbers anywhere beat a reason here"
        );
        // With no numbers anywhere, the row's own reason is the one to show.
        usage.insert("rnd".to_string(), reason("endpoint busy"));
        assert_eq!(
            usage_for(&usage, &out[0]).and_then(|u| u.note.clone()),
            Some("saved token expired".into())
        );
        // And an account with no reading anywhere stays absent, not zero.
        let lonely = row("other", true);
        assert!(usage_for(&usage, &lonely).is_none());
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
            stale: false,
            is_slot: false,
            also: Vec::new(),
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

    /// The row has to be named after the thing that actually serves. A saved
    /// snapshot is a dead copy - nothing refreshes it, and the slot answers for
    /// the account - but it carries credentials, so it read as "signed in" and
    /// outranked a slot that needed one. On a real machine that showed `claude`
    /// (a snapshot from weeks ago) while the live slot `personal` sat unnamed
    /// and unsigned-in, so the one thing needing attention was the one hidden.
    #[test]
    fn a_live_slot_is_named_even_when_a_snapshot_of_it_looks_signed_in() {
        let row = |name: &str, ident: &str, is_slot: bool, needs_login: bool| Row {
            name: name.into(),
            ident: ident.into(),
            tools: "claude-code".into(),
            active: false,
            warn: None,
            disabled: false,
            needs_login,
            stale: false,
            is_slot,
            also: Vec::new(),
        };
        let out = dedupe_by_identity(vec![
            row("claude", "you@gmail.com [max]", false, false),
            row("personal", "you@gmail.com", true, true),
        ]);
        assert_eq!(out.len(), 1, "still one account, one row");
        assert_eq!(out[0].name, "personal", "named after what serves");
        assert!(
            out[0].needs_login,
            "and it says the account needs a login, which the snapshot was hiding"
        );
        assert_eq!(out[0].also, vec!["claude".to_string()], "nothing is lost");
    }

    /// One account, one row. A saved snapshot and a slot can carry the same
    /// name - that name IS the account, since every command resolves by it - and
    /// the merge keyed only on the email, which the not-yet-signed-in half does
    /// not have. So a real machine listed `work` twice: once with its ChatGPT
    /// address, once as "no login", and nothing said they were the same thing.
    #[test]
    fn one_name_on_one_tool_is_one_row() {
        let row = |name: &str, ident: &str, is_slot: bool, needs_login: bool| Row {
            name: name.into(),
            ident: ident.into(),
            tools: "codex".into(),
            active: false,
            warn: None,
            disabled: false,
            needs_login,
            stale: false,
            is_slot,
            also: Vec::new(),
        };
        let out = dedupe_by_identity(vec![
            row("work", "polarisairnd@gmail.com [chatgpt]", false, false),
            row("work", "", true, true),
        ]);
        let names: Vec<&str> = out.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(out.len(), 1, "one account, one row: {names:?}");
        assert!(
            out[0].ident.contains("polarisairnd@gmail.com"),
            "the half that knows who it is wins the label: {:?}",
            out[0].ident
        );

        // Same name on DIFFERENT tools is two accounts, and stays two rows.
        let mut a = row("work", "same@x.com", true, false);
        a.tools = "claude-code".into();
        let b = row("work", "same@x.com", true, false);
        assert_eq!(dedupe_by_identity(vec![a, b]).len(), 2);
    }

    /// A window at 100% is not the end of an account that can bill credits -
    /// Anthropic keeps serving past the cap. Calling it "spent" beside an account
    /// answering turns all afternoon is simply false, and the same verdict was
    /// steering the proxy away from it.
    #[test]
    fn a_full_window_carried_by_credits_is_not_called_spent() {
        let row = Row {
            name: "bsgong".into(),
            ident: "e@x".into(),
            tools: "claude-code".into(),
            active: true,
            warn: None,
            disabled: false,
            needs_login: false,
            stale: false,
            is_slot: true,
            also: Vec::new(),
        };
        let full = |on_credits| Usage {
            five_h: Some(100.0),
            seven_d: Some(55.0),
            on_credits,
            ..Default::default()
        };
        assert_eq!(account_status(&row, Some(&full(false))).0, "spent");
        assert_eq!(account_status(&row, Some(&full(true))).0, ON_CREDITS);
        // And an account nowhere near its cap is unaffected either way.
        let fresh = Usage {
            five_h: Some(10.0),
            seven_d: Some(20.0),
            on_credits: true,
            ..Default::default()
        };
        assert_eq!(account_status(&row, Some(&fresh)).0, "active");
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
            stale: false,
            is_slot: false,
            also: Vec::new(),
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
            account_status(&mk(true, Some("stale".to_string()), false), Some(&fresh)).0,
            "stale",
            "an unusable snapshot outranks quota"
        );
        assert_eq!(
            account_status(&mk(true, Some("stale".to_string()), true), Some(&spent)).0,
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
            stale: false,
            is_slot: false,
            also: Vec::new(),
        };
        assert_eq!(account_status(&needs, Some(&fresh)).0, "no login");
        // No quota data is not "spent".
        assert_eq!(account_status(&mk(false, None, false), None).0, "ready");
    }

    // The number counts what is LEFT, the way Claude's own status does and the
    // way `swapdex quota` does. It used to count what was spent, with no word to
    // say which - so a window that had just reset showed "2%" and read as almost
    // nothing left when it meant almost nothing used.
    /// The number and the fill must move together. The bar filled by what was
    /// SPENT while the number said what was LEFT, so an untouched account read
    /// "100% left" across an empty bar and a spent one read "0% left" across a
    /// full one - every gauge saying the opposite of its own caption.
    #[test]
    fn the_fill_matches_the_number_beside_it() {
        let filled_cells = |used: f64| -> usize {
            quota_bar(Some(used), 20)
                .first()
                .map(|s| s.content.chars().count())
                .unwrap_or(0)
        };
        assert_eq!(filled_cells(0.0), 20, "nothing used -> the bar is full");
        assert_eq!(filled_cells(100.0), 0, "all used -> the bar is empty");
        assert_eq!(filled_cells(50.0), 10, "half used -> half full");
        assert!(
            filled_cells(10.0) > filled_cells(90.0),
            "spending more leaves less showing"
        );
    }

    /// And the warning tone belongs to the account that is nearly OUT, not the
    /// one that has barely started.
    #[test]
    fn the_alarming_colour_marks_a_nearly_empty_window() {
        assert_eq!(quota_fill(95.0), quota_fill(100.0), "both nearly spent");
        assert_ne!(
            quota_fill(95.0),
            quota_fill(5.0),
            "a fresh window is not drawn like a spent one"
        );
    }

    /// A window nobody has used has no reset - the five-hour window starts on
    /// first use. Rendering that as an empty string pulled everything after it
    /// left on that row alone, so `rnd`'s 7d block sat several columns ahead of
    /// the rows above and below it.
    #[test]
    fn a_missing_reset_keeps_its_place_in_the_row() {
        let resets = vec![
            Some("2:10pm".to_string()),
            None,
            Some("Mon 12:59pm".to_string()),
        ];
        let w = reset_slot_width(&resets, true);
        let cells: Vec<String> = resets
            .iter()
            .map(|r| reset_slot(r.as_deref(), true, w))
            .collect();
        let widths: Vec<usize> = cells.iter().map(|c| c.chars().count()).collect();
        assert!(
            widths.windows(2).all(|x| x[0] == x[1]),
            "every slot is one width: {widths:?} in {cells:?}"
        );
        assert!(cells[0].starts_with("resets 2:10pm"), "{:?}", cells[0]);
        assert!(
            cells[1].starts_with("not started"),
            "an unused window says so rather than leaving a hole: {:?}",
            cells[1]
        );
    }

    /// No row has a reset: the column is not drawn at all rather than filling
    /// the row with phrases nobody asked for.
    #[test]
    fn a_frame_with_no_resets_draws_no_slot() {
        assert_eq!(reset_slot_width(&[None, None], true), 0);
    }

    /// Too narrow for the phrase: pad, never truncate it into nonsense.
    #[test]
    fn a_slot_too_narrow_for_the_phrase_stays_blank() {
        assert_eq!(reset_slot(None, true, 6), "      ");
    }

    /// The gauge holds ONE reading and nothing else. The reset time lives
    /// beside it now: crammed inside, "62% left 6d" read as two quantities of
    /// quota - 병승 asked whether the 6d was remaining allowance - and it forced
    /// the bar wide enough to hold a sentence.
    #[test]
    fn the_gauge_carries_only_what_is_left() {
        for w in [7usize, 12, 20] {
            let text: String = quota_bar(Some(38.0), w)
                .iter()
                .map(|s| s.content.to_string())
                .collect();
            assert_eq!(text.chars().count(), w, "the bar is exactly its width");
            let t = text.trim();
            assert!(t.starts_with("62%"), "the reading always survives: {t:?}");
            for stray in ["resets", "am", "pm", "6d", "2h"] {
                assert!(
                    !t.contains(stray),
                    "width {w} put a time in the gauge: {t:?}"
                );
            }
        }
    }

    #[test]
    fn quota_bar_writes_what_is_left_inside_the_bar() {
        let spans = quota_bar(Some(62.0), 14);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text.chars().count(), 14, "the bar is exactly its width");
        assert!(text.contains("38% left"), "62% spent is 38% left: {text:?}");
        // A window barely touched reads as nearly full, not nearly empty.
        let fresh: String = quota_bar(Some(2.0), 14)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(fresh.contains("98% left"), "{fresh:?}");
        // The fill tracks what is LEFT, like the number: 38% of 14 -> 5 cells.
        // This assertion used to pin the opposite, on the reasoning that a bar
        // filling as an account is used is the picture people expect - but the
        // number beside it counts DOWN, so the two halves of one gauge said
        // opposite things and an untouched account showed an empty bar.
        assert_eq!(spans[0].content.chars().count(), 5);
        assert_eq!(spans[1].content.chars().count(), 9);
        // A spent window has nothing filled, and still says so in words.
        let full = quota_bar(Some(100.0), 14);
        assert_eq!(
            full[0].content.chars().count(),
            0,
            "nothing left, nothing filled: {:?}",
            full[1].content
        );
        assert!(
            full[1].content.contains("0% left"),
            "a spent window says so plainly: {:?}",
            full[1].content
        );
        // Room for the word, and nothing else joins it - the reset lives beside
        // the gauge, not in it.
        let wide: String = quota_bar(Some(10.0), 17)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(wide.contains("90% left"), "{wide:?}");
        assert!(
            !wide.contains("am") && !wide.contains("pm"),
            "no time inside the gauge: {wide:?}"
        );
        let narrow: String = quota_bar(Some(10.0), 5)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        // Too narrow for the word: the number alone, not a cut-off word.
        assert!(
            narrow.contains("90%") && !narrow.contains("1h") && !narrow.contains("l"),
            "{narrow:?}"
        );
        // No data: an empty track of the right width, no number invented.
        let none = quota_bar(None, 6);
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

    // An account with no bars is the one thing the dashboard cannot explain by
    // drawing: empty tracks look identical whether the account was never read,
    // could not be read, or has genuinely nothing left. Whatever the reader was
    // told goes in the trailing column, and it outranks the age caveat - a reading
    // that failed has no age worth reporting.
    // The cursor has to be visible without covering the gauges: the bars carry
    // their own background, and that background IS the reading.
    // Starting a new conversation is the point of the screen, so it must be
    // reachable: at the TOP, and offered even for an account with no saved
    // profile - every slot account is exactly that, and they had no entry at all.
    // Adding several accounts is what the dashboard is for. Signing one in used to
    // hand the terminal to the tool and never come back, so each account meant
    // relaunching the dashboard - the list is exactly where the result belongs.
    #[test]
    fn signing_in_keeps_the_dashboard_alive() {
        // The trait's method returns a result rather than an Outcome, which is
        // what makes staying possible: an Outcome ends the loop by construction.
        fn asserts_it_returns<C: TuiCtx>(ctx: &mut C, name: &str) -> (bool, String) {
            ctx.sign_in(name)
        }
        struct Fake {
            called: Vec<String>,
        }
        impl TuiCtx for Fake {
            fn rows(&mut self) -> Vec<Row> {
                Vec::new()
            }
            fn switch(&mut self, _: &str, _: bool) -> (bool, String) {
                (true, String::new())
            }
            fn delete(&mut self, _: &str) -> String {
                String::new()
            }
            fn sessions(&mut self, _: &str) -> (String, Vec<SessionEntry>, Vec<&'static str>) {
                (String::new(), Vec::new(), Vec::new())
            }
            fn rename(&mut self, _: &str, _: &str) -> (bool, String) {
                (true, String::new())
            }
            fn sign_in(&mut self, name: &str) -> (bool, String) {
                self.called.push(name.to_string());
                (true, format!("'{name}' is signed in"))
            }
            fn save_current(&mut self, _: &str) -> (bool, String) {
                (true, String::new())
            }
            fn doctor(&mut self) -> Vec<String> {
                Vec::new()
            }
            fn usage(&mut self) -> Vec<String> {
                Vec::new()
            }
            fn quota(&mut self) -> Vec<String> {
                Vec::new()
            }
            fn sessionwiki_present(&mut self) -> bool {
                false
            }
            fn live_tools(&mut self) -> Vec<String> {
                Vec::new()
            }
        }
        let mut f = Fake { called: Vec::new() };
        let (ok, msg) = asserts_it_returns(&mut f, "work");
        assert!(ok);
        assert_eq!(f.called, vec!["work".to_string()], "it reached the account");
        assert!(msg.contains("signed in"), "and reports back: {msg}");
        // A second one needs no relaunch - that is the whole point.
        asserts_it_returns(&mut f, "home");
        assert_eq!(f.called.len(), 2);
    }

    #[test]
    fn the_new_conversation_entries_lead_and_never_vanish() {
        assert_eq!(
            new_conv_for(&["claude-code"]),
            vec![("open a NEW Claude Code conversation", "claude-code")],
            "only the tools the account actually has"
        );
        assert_eq!(
            new_conv_for(&["codex"]),
            vec![("open a NEW Codex conversation", "codex")]
        );
        // An account whose tools could not be determined still gets somewhere to
        // go, rather than a screen that can only reopen the past.
        assert_eq!(
            new_conv_for(&[]),
            NEW_CONV[..2].to_vec(),
            "the two tools swapdex can launch, rather than nothing"
        );
    }

    #[test]
    fn the_selected_row_is_banded_only_up_to_the_bars() {
        let spans = || {
            vec![
                Span::raw("  "),
                Span::styled("work", Style::default().fg(Color::White)),
                Span::styled("ready", Style::default().fg(Color::Green)),
                // A bar span, with the fill colour that means "82% used".
                Span::styled("  ", Style::default().bg(Color::Rgb(1, 2, 3))),
            ]
        };
        let out = mark_selected(spans(), 3, true);
        assert!(
            out[..3].iter().all(|s| s.style.bg == Some(SELECT_BG)),
            "the row reads as selected across its text columns"
        );
        assert!(
            out[..3]
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "and weight still helps"
        );
        assert_eq!(
            out[1].style.fg,
            Some(Color::White),
            "each column keeps its own colour"
        );
        assert_eq!(out[2].style.fg, Some(Color::Green), "including the status");
        assert_eq!(
            out[3].style.bg,
            Some(Color::Rgb(1, 2, 3)),
            "the bar's fill is untouched - it is the number, not decoration"
        );
        // An unselected row is returned exactly as it came.
        let plain = mark_selected(spans(), 3, false);
        assert!(plain.iter().all(|s| s.style.bg != Some(SELECT_BG)));
    }

    // A row that said "ready" beside a note saying its token had expired was
    // promising something it could not keep - and for someone working from the
    // dashboard, that status IS the answer to "can I use this now".
    #[test]
    fn an_account_that_cannot_serve_is_not_called_ready() {
        let base = Row {
            name: "work".into(),
            ident: "w@x.co".into(),
            tools: "claude-code".into(),
            active: false,
            warn: None,
            disabled: false,
            needs_login: false,
            is_slot: true,
            also: Vec::new(),
            stale: false,
        };
        assert_eq!(account_status(&base, None).0, "ready");

        let stale = Row {
            stale: true,
            ..Row {
                name: base.name.clone(),
                ident: base.ident.clone(),
                tools: base.tools.clone(),
                ..base
            }
        };
        assert_eq!(account_status(&stale, None).0, "expired");
        // Never signed in outranks it: there is nothing to renew.
        let fresh = Row {
            needs_login: true,
            ..stale
        };
        assert_eq!(account_status(&fresh, None).0, "no login");
    }

    #[test]
    fn a_row_with_no_numbers_says_why() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let u = |note: Option<&str>, observed: Option<i64>| Usage {
            five_h: None,
            five_h_reset: None,
            seven_d: None,
            seven_d_reset: None,
            observed_at: observed,
            note: note.map(str::to_string),
            on_credits: false,
            ident: None,
        };
        assert_eq!(
            trailing_note(&u(Some("token expired"), None), false),
            "token expired"
        );
        assert_eq!(
            trailing_note(&u(Some("endpoint busy"), Some(now - 3 * 3600)), false),
            "endpoint busy",
            "the reason outranks the age"
        );
        assert_eq!(
            trailing_note(&u(None, Some(now - 2 * 3600)), false),
            "as of 2h"
        );
        assert_eq!(trailing_note(&u(None, None), false), "");

        // A remembered number under fifteen minutes old carries no age, so while
        // a live read is in flight it would look exactly like a current one -
        // and everything spent since is missing from it.
        assert_eq!(
            trailing_note(&u(None, Some(now - 60)), true),
            "checking\u{2026}"
        );
        assert_eq!(
            trailing_note(&u(None, Some(now - 2 * 3600)), true),
            "as of 2h, checking\u{2026}"
        );
        // A live number has nothing to disclose, in flight or not.
        assert_eq!(trailing_note(&u(None, None), true), "");
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
            stale: false,
            is_slot: false,
            also: Vec::new(),
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

#[cfg(test)]
mod empty_window_tests {
    use super::*;

    /// "not started" is a claim: the window exists and you have not touched it.
    /// True for Claude, where the 5h window begins on first use. False for
    /// Codex, whose server never publishes a session window at all - there the
    /// phrase reads as "you have room" beside an account the user has just been
    /// told is out, which is the reverse of the truth.
    #[test]
    fn an_unpublished_window_is_not_called_unused() {
        assert_eq!(empty_window_phrase(true), "not started");
        assert_eq!(empty_window_phrase(false), "not reported");
        assert_ne!(
            empty_window_phrase(false),
            "not started",
            "silence from the server must not be reported as an unused window"
        );
    }

    /// The column is sized for whichever phrase can appear, so a row saying one
    /// and a row saying the other still line up.
    #[test]
    fn the_column_fits_either_phrase() {
        let w = reset_slot_width(&[Some("1:00pm".to_string())], false);
        assert!(w >= empty_window_phrase(false).len(), "width {w}");
        let cell = reset_slot_reason(None, false, w, false);
        assert!(cell.starts_with("not reported"), "{cell:?}");
        assert_eq!(cell.chars().count(), w, "padded to the column: {cell:?}");
    }
}

#[cfg(test)]
mod paste_guard_tests {
    use super::*;

    /// A destructive confirmation must not be satisfiable by pasted text.
    ///
    /// `d` opens the delete prompt and `y` completes it, so any text arriving
    /// as keystrokes with a `d` followed by a `y` deletes an account. A stale
    /// pane can receive injected text - a harness driving a sibling session, a
    /// terminal paste - and two ordinary letters are not a decision.
    ///
    /// Keys typed by a person arrive with human gaps between them. Two keys in
    /// the same instant are a paste, and a paste never confirms a deletion.
    #[test]
    fn a_confirmation_arriving_instantly_after_the_prompt_is_refused() {
        // Typed: the prompt opened, then a human took a moment to answer.
        assert!(confirm_is_deliberate(0, PASTE_GAP_MS));
        assert!(confirm_is_deliberate(0, 5_000));
        // Pasted: both keys landed in the same instant.
        assert!(!confirm_is_deliberate(0, 0));
        assert!(!confirm_is_deliberate(0, PASTE_GAP_MS - 1));
    }
}

#[cfg(test)]
mod empty_reading_tests {
    use super::*;

    /// A reading that came back with nothing must not erase the numbers on screen.
    ///
    /// The dashboard replaced its whole quota map with whatever a fetch
    /// returned, and stamped it as freshly read. When a round came back empty -
    /// the usage endpoint throttles, and it does so for minutes at a time - the
    /// gauges went blank and stayed blank, while the cache on disk still held
    /// good numbers. Leave the dashboard open and eventually one empty round
    /// wipes it: that is the "the usage disappears after a while" report.
    #[test]
    fn an_empty_reading_leaves_the_numbers_that_are_already_there() {
        let have: std::collections::HashMap<String, Usage> = [(
            "rnd".to_string(),
            Usage {
                five_h: Some(4.0),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect();

        // Nothing came back: keep what is on screen.
        let (map, landed) = merge_reading(Some(have.clone()), Vec::new());
        assert!(!landed, "an empty round is not a reading");
        assert_eq!(
            map.as_ref()
                .and_then(|m| m.get("rnd"))
                .and_then(|u| u.five_h),
            Some(4.0),
            "the numbers survived"
        );

        // A real reading replaces it.
        let fresh = vec![(
            "rnd".to_string(),
            Usage {
                five_h: Some(9.0),
                ..Default::default()
            },
        )];
        let (map, landed) = merge_reading(Some(have), fresh);
        assert!(landed, "a reading with rows in it landed");
        assert_eq!(
            map.and_then(|m| m.get("rnd").and_then(|u| u.five_h)),
            Some(9.0)
        );
    }
}

#[cfg(test)]
mod publishes_window_tests {
    use super::*;

    /// Whether the source publishes a 5h window must not depend on a marker.
    ///
    /// Codex's server never sends a session window, so "not started" - which
    /// claims the window exists and is untouched - reads as "you have room"
    /// beside an account just reported as out. The check compared the row's
    /// tools string to "codex" exactly, and that string carries the active
    /// marker: the ACTIVE Codex account reads "codex*", failed the comparison,
    /// and got Claude's phrase while the idle one next to it got the right one.
    #[test]
    fn the_active_marker_does_not_change_what_a_source_publishes() {
        assert!(!publishes_session_window("codex"));
        assert!(
            !publishes_session_window("codex*"),
            "the marker is not part of the tool"
        );
        assert!(!publishes_session_window("codex, codex*"));
        assert!(publishes_session_window("claude-code"));
        assert!(publishes_session_window("claude-code*"));
        // A row carrying both publishes one, so the phrase stays Claude's.
        assert!(publishes_session_window("claude-code*, codex"));
        // Nothing known: keep the old default rather than claiming Codex.
        assert!(publishes_session_window(""));
    }
}

#[cfg(test)]
mod switch_claim_tests {
    use super::*;

    /// The dashboard must not announce a switch it has not confirmed.
    ///
    /// On success it printed "<name> now serves the running session" after
    /// checking only that a proxy was alive - never that the pointer had
    /// actually moved. A switch that quietly did not take therefore produced
    /// the sentence that says it did, which is the worst possible reading: the
    /// user believes the account changed and every later symptom looks like a
    /// different bug.
    #[test]
    fn a_switch_is_only_announced_when_the_payer_agrees() {
        assert_eq!(
            switch_result("rnd", true, Some("rnd")),
            SwitchClaim::Serving,
            "the payer agrees: say so"
        );
        assert_eq!(
            switch_result("rnd", true, Some("kong")),
            SwitchClaim::NotConfirmed("kong".into()),
            "someone else is paying - do not claim the switch landed"
        );
        assert_eq!(
            switch_result("rnd", true, None),
            SwitchClaim::NotConfirmed(String::new()),
            "nobody is paying - still not a confirmed switch"
        );
        assert_eq!(
            switch_result("rnd", false, None),
            SwitchClaim::Saved,
            "no proxy: the switch is real but reaches the next session"
        );
    }
}

#[cfg(test)]
mod switch_command_tests {
    use super::*;

    /// Enter has to run the command that works for THIS account.
    ///
    /// Serving reads a slot's own credential directory, so a snapshot cannot pay
    /// for turns - `serve` on one answers "'work' is saved but has never been
    /// signed in on this machine". Enter called `serve` for every row, including
    /// the snapshot rows added so their usage could be seen, so pressing it
    /// there always failed. A snapshot IS switchable: `use` copies its
    /// credentials into place, which is what the row's own doc says switching a
    /// snapshot means.
    #[test]
    fn enter_serves_a_slot_and_uses_a_snapshot() {
        assert_eq!(switch_verb(true), "serve", "a slot can pay for turns");
        assert_eq!(switch_verb(false), "use", "a snapshot is copied into place");
    }
}
