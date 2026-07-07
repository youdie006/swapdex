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
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::path::PathBuf;

const VIOLET: Color = Color::Rgb(157, 107, 255); // the brand accent (#9d6bff)

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

enum Screen {
    Main,
    Open {
        label: String,
        entries: Vec<SessionEntry>,
    },
    Folder {
        tool: &'static str,
        input: String,
    },
    ToolPick,
}

/// The persistent loop. Enters the alternate screen once and stays there
/// until an [`Outcome`] leaves it.
pub fn run(ctx: &mut dyn TuiCtx) -> Result<Outcome> {
    let mut terminal = ratatui::try_init()?;
    let mut rows = ctx.rows();
    let mut state = ListState::default();
    state.select(Some(rows.iter().position(|r| r.active).unwrap_or(0)));
    let mut open_state = ListState::default();
    let mut status = String::new();
    let mut confirm_delete: Option<usize> = None;
    let mut screen = Screen::Main;

    let outcome = 'ui: loop {
        terminal.draw(|f| {
            let [main, foot, help] = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(f.area());
            match &screen {
                Screen::Main => {
                    let items: Vec<ListItem> = rows
                        .iter()
                        .map(|r| {
                            let marker = if r.active { "* " } else { "  " };
                            let name_style = if r.active {
                                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().add_modifier(Modifier::BOLD)
                            };
                            let warn =
                                r.warn.map(|w| format!("  ({w})")).unwrap_or_default();
                            ListItem::new(vec![
                                Line::from(vec![
                                    Span::raw(marker),
                                    Span::styled(r.name.clone(), name_style),
                                    Span::raw("  "),
                                    Span::raw(r.ident.clone()),
                                ]),
                                Line::from(Span::styled(
                                    format!("    {}{warn}", r.tools),
                                    Style::default().fg(Color::DarkGray),
                                )),
                            ])
                        })
                        .collect();
                    let list = List::new(items)
                        .block(Block::default().borders(Borders::ALL).title(Span::styled(
                            " swapdex ",
                            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                        )))
                        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                        .highlight_symbol("> ");
                    f.render_stateful_widget(list, main, &mut state);
                    let foot_text = if let Some(i) = confirm_delete {
                        format!(
                            "delete saved profile '{}'? the live login stays. y/N",
                            rows[i].name
                        )
                    } else {
                        status.clone()
                    };
                    f.render_widget(
                        Paragraph::new(foot_text).style(Style::default().fg(Color::DarkGray)),
                        foot,
                    );
                    f.render_widget(
                        Paragraph::new(
                            "enter switch   o open conversation   a add account   r restore   d delete   q quit",
                        )
                        .style(Style::default().fg(Color::DarkGray)),
                        help,
                    );
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
                        .block(Block::default().borders(Borders::ALL).title(Span::styled(
                            format!(" {label} "),
                            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                        )))
                        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                        .highlight_symbol("> ");
                    f.render_stateful_widget(list, main, &mut open_state);
                    f.render_widget(
                        Paragraph::new(status.clone())
                            .style(Style::default().fg(Color::DarkGray)),
                        foot,
                    );
                    f.render_widget(
                        Paragraph::new("enter open   esc back")
                            .style(Style::default().fg(Color::DarkGray)),
                        help,
                    );
                }
                Screen::Folder { tool, input } => {
                    let name = NEW_CONV
                        .iter()
                        .find(|(_, t)| t == tool)
                        .map(|(l, _)| *l)
                        .unwrap_or("open");
                    f.render_widget(
                        Paragraph::new(format!("{name}\n\nfolder [current dir]: {input}_"))
                            .block(Block::default().borders(Borders::ALL).title(Span::styled(
                                " which folder? ",
                                Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                            ))),
                        main,
                    );
                    f.render_widget(Paragraph::new(""), foot);
                    f.render_widget(
                        Paragraph::new("enter open   esc back   (empty = current dir, ~ ok)")
                            .style(Style::default().fg(Color::DarkGray)),
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
                        .block(Block::default().borders(Borders::ALL).title(Span::styled(
                            " add a new account - which tool? ",
                            Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                        )))
                        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                        .highlight_symbol("> ");
                    f.render_stateful_widget(list, main, &mut open_state);
                    f.render_widget(Paragraph::new(""), foot);
                    f.render_widget(
                        Paragraph::new("enter choose   esc back")
                            .style(Style::default().fg(Color::DarkGray)),
                        help,
                    );
                }
            }
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match &mut screen {
            Screen::Main => {
                if let Some(i) = confirm_delete {
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        status = ctx.delete(&rows[i].name);
                        rows = ctx.rows();
                        state.select(Some(0));
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
                    KeyCode::Enter => {
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
                    KeyCode::Char('o') => {
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
                    KeyCode::Char('r') => {
                        status = ctx.restore();
                        rows = ctx.rows();
                    }
                    KeyCode::Char('d') if !rows.is_empty() => {
                        confirm_delete = state.selected();
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
                    screen = Screen::Folder {
                        tool,
                        input: String::new(),
                    };
                }
                _ => {}
            },
            Screen::Folder { tool, input } => match key.code {
                KeyCode::Esc => {
                    rows = ctx.rows();
                    screen = Screen::Main;
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Enter => {
                    let dir = if input.is_empty() {
                        None
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
        }
    };
    ratatui::restore();
    Ok(outcome)
}
