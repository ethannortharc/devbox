use std::io;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

use super::LAYOUTS;

/// Show an interactive layout picker TUI.
/// Returns the selected layout name, or None if cancelled.
pub fn pick_layout() -> Result<Option<String>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = ListState::default();
    state.select(Some(0));
    let mut set_default = false;

    let result = loop {
        terminal.draw(|f| {
            let area = centered_rect(60, 70, f.area());

            let chunks = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .split(area);

            // Layout list
            let items: Vec<ListItem> = LAYOUTS
                .iter()
                .map(|l| {
                    let line = Line::from(vec![
                        Span::styled(
                            format!("{:<16}", l.name),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(l.description),
                    ]);
                    ListItem::new(line)
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::default()
                        .title(" Choose your workspace layout ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Blue)),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▸ ");

            f.render_stateful_widget(list, chunks[0], &mut state);

            // Help bar
            let help = Paragraph::new(Line::from(vec![
                Span::styled(" ↑↓ ", Style::default().fg(Color::Yellow)),
                Span::raw("Select  "),
                Span::styled("Enter ", Style::default().fg(Color::Yellow)),
                Span::raw("Launch  "),
                Span::styled("d ", Style::default().fg(Color::Yellow)),
                Span::raw("Set as default  "),
                Span::styled("q/Esc ", Style::default().fg(Color::Yellow)),
                Span::raw("Cancel"),
            ]))
            .block(Block::default().borders(Borders::ALL));

            f.render_widget(help, chunks[1]);
        })?;

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break None,
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = state.selected().unwrap_or(0);
                    let next = if i >= LAYOUTS.len() - 1 { 0 } else { i + 1 };
                    state.select(Some(next));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = state.selected().unwrap_or(0);
                    let next = if i == 0 { LAYOUTS.len() - 1 } else { i - 1 };
                    state.select(Some(next));
                }
                KeyCode::Char('d') => {
                    set_default = true;
                    if let Some(i) = state.selected() {
                        break Some(LAYOUTS[i].name.to_string());
                    }
                }
                KeyCode::Enter => {
                    if let Some(i) = state.selected() {
                        break Some(LAYOUTS[i].name.to_string());
                    }
                }
                _ => {}
            }
        }
    };

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    if set_default {
        if let Some(ref name) = result {
            println!("Set '{name}' as default layout.");
        }
    }

    Ok(result)
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(v[1])[1]
}
