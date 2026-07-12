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
                            ("u", "usage"),
                            ("%", "quota"),
                            ("r", "restore"),
                            ("d", "delete"),
                            ("?", "health"),
                            ("q", "quit"),
                        ]
                    };
                    f.render_widget(Paragraph::new(key_hints(hints)), help);
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
        // A left click on a menu item both selects AND activates it; treat
        // that as a synthesized Enter so the key handler below does the work.
        let mut click_activate = false;
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
                                let (label, entries, tools) = ctx.sessions(&name);
                                let new_conv = new_conv_for(&tools);
                                open_state.select(Some(0));
                                // A single-tool profile: skip the menu entirely
                                // when there are no sessions to pick - go
                                // straight to that tool's folder browser.
                                if entries.is_empty() && new_conv.len() == 1 {
                                    let tool = new_conv[0].1;
                                    let cwd = std::env::current_dir()
                                        .ok()
                                        .or_else(dirs::home_dir)
                                        .unwrap_or_else(|| PathBuf::from("/"));
                                    let frows = folder_rows(&cwd);
                                    screen = Screen::Folder {
                                        tool,
                                        cwd,
                                        rows: frows,
                                        back: (label, entries, new_conv),
                                    };
                                } else {
                                    screen = Screen::Open {
                                        label,
                                        entries,
                                        new_conv,
                                    };
                                }
                            }
                        }
                    }
                    KeyCode::Char('o') if !rows.is_empty() => {
                        if let Some(i) = state.selected() {
                            let name = rows[i].name.clone();
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
}
