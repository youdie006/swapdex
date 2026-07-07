//! The persistent full-screen UI (`swapdex ui` on a real terminal), ccusage-
//! style by user request: the screen clears, the UI stays up, and everything
//! happens inside it. Switching shows its result in the status line and
//! REFRESHES the list in place; landing in a conversation (resume or new) is
//! the one action that leaves - by design, that is the goal of a switch.
//!
//! No second implementation of anything: a switch/restore runs this same
//! binary as a subprocess (`swapdex use/restore`) with its output captured
//! into the status line, and session/launch data comes from the caller
//! through [`TuiCtx`].

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::path::PathBuf;

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

pub struct Row {
    pub name: String,
    pub ident: String,
    pub tools: String,
    pub active: bool,
    pub warn: Option<&'static str>,
}

/// One line in the post-switch "open" screen (pre-rendered by the caller).
pub struct SessionEntry {
    pub line: String,
}

/// Everything the UI needs from the outside world.
pub trait TuiCtx {
    fn rows(&mut self) -> Vec<Row>;
    /// Perform the switch (subprocess); returns (success, condensed message).
    fn switch(&mut self, name: &str) -> (bool, String);
    fn restore(&mut self) -> String;
    fn delete(&mut self, name: &str) -> String;
    /// (label, session entries) for the just-switched profile.
    fn sessions(&mut self, name: &str) -> (String, Vec<SessionEntry>);
    /// Rename a profile (subprocess). Returns (ok, message).
    fn rename(&mut self, old: &str, new: &str) -> (bool, String);
    /// Save the accounts you're currently logged into as a new profile
    /// (subprocess `add <name>` - captures live logins, no sign-out). This is
    /// the onboarding action: a fresh machine is usually already logged in.
    fn save_current(&mut self, name: &str) -> (bool, String);
    /// Run `doctor` and return its output lines for a read-only panel.
    fn doctor(&mut self) -> Vec<String>;
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

/// What a text-input screen is collecting.
enum InputKind {
    Rename(String), // rename this existing profile
    SaveCurrent,    // save the current live logins as a new profile
}

enum Screen {
    Main,
    Open {
        label: String,
        entries: Vec<SessionEntry>,
    },
    Folder {
        tool: &'static str,
        input: String,
        /// The Open screen to return to on Esc (one step back, not two).
        back: (String, Vec<SessionEntry>),
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

    let outcome = 'ui: loop {
        terminal.draw(|f| {
            let [main, foot, help] = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
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

                    let items: Vec<ListItem> = rows
                        .iter()
                        .map(|r| {
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
                            let mut top = vec![
                                Span::styled(glyph, gstyle),
                                Span::styled(r.name.clone(), name_style),
                                Span::raw("  "),
                                Span::styled(r.ident.clone(), Style::default().fg(DEXGRAY)),
                            ];
                            if let Some(w) = r.warn {
                                top.push(Span::styled(
                                    format!("  ({w})"),
                                    Style::default().fg(Color::Rgb(200, 150, 90)),
                                ));
                            }
                            ListItem::new(vec![
                                Line::from(top),
                                Line::from(Span::styled(
                                    format!("    {}", r.tools),
                                    Style::default().fg(Color::DarkGray),
                                )),
                                Line::from(""),
                            ])
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
                            .highlight_style(
                                Style::default()
                                    .bg(Color::Rgb(50, 47, 68))
                                    .add_modifier(Modifier::BOLD),
                            )
                            .highlight_symbol("\u{2503} ");
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
                    let hints: &[(&str, &str)] = if rows.is_empty() {
                        &[("?", "health"), ("q", "quit")]
                    } else {
                        &[
                            ("\u{21b5}", "switch"),
                            ("o", "open"),
                            ("a", "add"),
                            ("n", "rename"),
                            ("r", "restore"),
                            ("d", "delete"),
                            ("?", "health"),
                            ("q", "quit"),
                        ]
                    };
                    f.render_widget(Paragraph::new(key_hints(hints)), help);
                }
                Screen::Open { label, entries } => {
                    let mut items: Vec<ListItem> = entries
                        .iter()
                        .map(|e| ListItem::new(Line::from(e.line.clone())))
                        .collect();
                    for (label, _) in NEW_CONV {
                        items.push(ListItem::new(Line::from(Span::styled(
                            label,
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
                    f.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            format!("  {status}"),
                            Style::default().fg(MUTED),
                        ))),
                        foot,
                    );
                    f.render_widget(
                        Paragraph::new(key_hints(&[("\u{21b5}", "open"), ("esc", "back")])),
                        help,
                    );
                }
                Screen::Folder { tool, input, .. } => {
                    let name = NEW_CONV
                        .iter()
                        .find(|(_, t)| t == tool)
                        .map(|(l, _)| *l)
                        .unwrap_or("open");
                    f.render_widget(
                        Paragraph::new(vec![
                            Line::from(""),
                            Line::from(Span::styled(
                                format!("  {name}"),
                                Style::default().add_modifier(Modifier::BOLD),
                            )),
                            Line::from(""),
                            Line::from(vec![
                                Span::styled("  folder ", Style::default().fg(MUTED)),
                                Span::styled(
                                    "[current dir]".to_string(),
                                    Style::default().fg(Color::DarkGray),
                                ),
                                Span::raw(": "),
                                Span::styled(
                                    format!("{input}\u{2588}"),
                                    Style::default().fg(VIOLET),
                                ),
                            ]),
                        ])
                        .block(list_block(" which folder? ")),
                        main,
                    );
                    f.render_widget(Paragraph::new(""), foot);
                    f.render_widget(
                        Paragraph::new(key_hints(&[
                            ("\u{21b5}", "open"),
                            ("esc", "back"),
                            ("~", "home ok, empty = current"),
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
                    let text: Vec<Line> = lines
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
                    f.render_widget(
                        Paragraph::new(text)
                            .scroll((*scroll, 0))
                            .block(list_block(" doctor - health check ")),
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
        // A left click on a menu item both selects AND activates it; treat
        // that as a synthesized Enter so the key handler below does the work.
        let mut click_activate = false;
        let key = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => k,
            Event::Mouse(m) => {
                use ratatui::crossterm::event::{MouseButton, MouseEventKind as MK};
                let list_len = match &screen {
                    Screen::Main => rows.len(),
                    Screen::Open { entries, .. } => entries.len() + NEW_CONV.len(),
                    Screen::ToolPick => 4,
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
                        let per = if is_main { 3 } else { 1 }; // Main rows are 3 lines
                        let top = main_area.y + header + 1;
                        if m.row >= top {
                            let idx = ((m.row - top) / per) as usize;
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
                        status = ctx.delete(&rows[i].name);
                        rows = ctx.rows();
                        // The list may now be EMPTY - a dangling Some(0)
                        // would make the next Enter/o index out of bounds.
                        state.select((!rows.is_empty()).then_some(0));
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
                    KeyCode::Enter if !rows.is_empty() => {
                        if let Some(i) = state.selected() {
                            let name = rows[i].name.clone();
                            let (ok, msg) = ctx.switch(&name);
                            status = msg;
                            rows = ctx.rows();
                            if ok {
                                let (label, entries) = ctx.sessions(&name);
                                open_state.select(Some(0));
                                screen = Screen::Open { label, entries };
                            }
                        }
                    }
                    KeyCode::Char('o') if !rows.is_empty() => {
                        if let Some(i) = state.selected() {
                            let name = rows[i].name.clone();
                            let (label, entries) = ctx.sessions(&name);
                            open_state.select(Some(0));
                            screen = Screen::Open { label, entries };
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
                        if let Some(i) = state.selected() {
                            screen = Screen::Input {
                                kind: InputKind::Rename(rows[i].name.clone()),
                                value: String::new(),
                            };
                        }
                    }
                    KeyCode::Char('r') => {
                        status = ctx.restore();
                        rows = ctx.rows();
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
                    _ => {}
                }
            }
            Screen::Open { entries, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    rows = ctx.rows();
                    screen = Screen::Main;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let max = entries.len() + NEW_CONV.len() - 1;
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
                    let tool = NEW_CONV[i - entries.len()].1;
                    if let Screen::Open { label, entries } = std::mem::replace(
                        &mut screen,
                        Screen::Folder {
                            tool,
                            input: String::new(),
                            back: (String::new(), Vec::new()),
                        },
                    ) {
                        if let Screen::Folder { back, .. } = &mut screen {
                            *back = (label, entries);
                        }
                    }
                }
                _ => {}
            },
            Screen::Folder { tool, input, back } => match key.code {
                KeyCode::Esc => {
                    // One step back to the Open menu, not two.
                    let (label, entries) = std::mem::take(back);
                    screen = Screen::Open { label, entries };
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Enter => {
                    let dir = if input.is_empty() {
                        None
                    } else if input == "~" {
                        dirs::home_dir()
                    } else if let Some(rest) = input.strip_prefix("~/") {
                        dirs::home_dir().map(|h| h.join(rest))
                    } else {
                        Some(PathBuf::from(input.clone()))
                    };
                    if let Some(d) = &dir {
                        if !d.is_dir() {
                            status = format!("not a directory: {}", d.display());
                            input.clear();
                            continue;
                        }
                    }
                    break 'ui Outcome::NewConv { tool, dir };
                }
                KeyCode::Char(c) => input.push(c),
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
        }
    };
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    Ok(outcome)
}
