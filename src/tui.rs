//! The full-screen interactive UI (`swapdex ui` on a real terminal): every
//! profile with its account and active marker, arrow keys to move, Enter to
//! switch, plus the management actions that used to need separate commands -
//! `a`dd a new account (the sign-out-and-sign-in login flow), `r`estore,
//! `d`elete. Modeled on the workflow of llmux's picker, by direct user
//! request.
//!
//! The TUI renders and dispatches; every ACTION runs through the exact same
//! command paths as the CLI (`use_account`, `login`, `restore`, `rm`) after
//! the terminal is restored - no second implementation of switching exists.

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

const VIOLET: Color = Color::Rgb(157, 107, 255); // the brand accent (#9d6bff)

pub struct Row {
    pub name: String,
    pub ident: String,
    pub tools: String,
    pub active: bool,
    pub warn: Option<&'static str>,
}

/// What the user chose; executed by the caller AFTER the terminal is restored.
pub enum Action {
    Switch(String),
    AddAccount,
    Restore,
    Delete(String),
    Quit,
}

/// One full-screen selection pass. Returns when the user picks an action.
pub fn pick(rows: &[Row], summary: Option<&str>) -> Result<Action> {
    let mut terminal = ratatui::try_init()?;
    let mut state = ListState::default();
    // Start on the active profile (that is where the eye goes first).
    state.select(Some(rows.iter().position(|r| r.active).unwrap_or(0)));
    let mut confirm_delete: Option<usize> = None;

    let result = loop {
        terminal.draw(|f| {
            let [main, foot, help] = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(f.area());

            let items: Vec<ListItem> = rows
                .iter()
                .map(|r| {
                    let marker = if r.active { "* " } else { "  " };
                    let name_style = if r.active {
                        Style::default().fg(VIOLET).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().add_modifier(Modifier::BOLD)
                    };
                    let warn = r.warn.map(|w| format!("  ({w})")).unwrap_or_default();
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
                    " swapdex - pick a profile ",
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
                summary.unwrap_or("").to_string()
            };
            f.render_widget(
                Paragraph::new(foot_text).style(Style::default().fg(Color::DarkGray)),
                foot,
            );
            f.render_widget(
                Paragraph::new("enter switch   a add account   r restore   d delete   q quit")
                    .style(Style::default().fg(Color::DarkGray)),
                help,
            );
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(i) = confirm_delete {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        break Action::Delete(rows[i].name.clone())
                    }
                    _ => confirm_delete = None,
                }
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break Action::Quit,
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
                        break Action::Switch(rows[i].name.clone());
                    }
                }
                KeyCode::Char('a') => break Action::AddAccount,
                KeyCode::Char('r') => break Action::Restore,
                KeyCode::Char('d') if !rows.is_empty() => {
                    confirm_delete = state.selected();
                }
                _ => {}
            }
        }
    };
    ratatui::restore();
    Ok(result)
}

/// The add-account submenu (which tool), full-screen like the picker.
pub fn pick_tool() -> Result<Option<&'static str>> {
    let tools = [
        ("Claude Code", "claude-code"),
        ("Codex", "codex"),
        ("Gemini CLI", "gemini"),
        ("Antigravity", "antigravity"),
    ];
    let mut terminal = ratatui::try_init()?;
    let mut state = ListState::default();
    state.select(Some(0));
    let result = loop {
        terminal.draw(|f| {
            let [main, help] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
            let items: Vec<ListItem> = tools
                .iter()
                .map(|(label, _)| ListItem::new(Line::from(*label)))
                .collect();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(Span::styled(
                    " add a new account - which tool? ",
                    Style::default().fg(VIOLET).add_modifier(Modifier::BOLD),
                )))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> ");
            f.render_stateful_widget(list, main, &mut state);
            f.render_widget(
                Paragraph::new("enter choose   q back").style(Style::default().fg(Color::DarkGray)),
                help,
            );
        })?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break None,
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = state.selected().unwrap_or(0);
                    state.select(Some((i + 1).min(tools.len() - 1)));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = state.selected().unwrap_or(0);
                    state.select(Some(i.saturating_sub(1)));
                }
                KeyCode::Enter => break state.selected().map(|i| tools[i].1),
                _ => {}
            }
        }
    };
    ratatui::restore();
    Ok(result)
}
