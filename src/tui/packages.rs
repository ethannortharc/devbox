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

use crate::tools::registry::TOOL_SETS;
use crate::nix::sets::NIX_SETS;

/// View mode for the TUI.
#[derive(Debug, Clone, PartialEq)]
enum View {
    Sets,
    Packages { set_index: usize },
}

/// State for a set row in the TUI.
#[derive(Debug, Clone)]
struct SetRow {
    name: String,
    description: String,
    package_count: usize,
    active: bool,
    locked: bool,
}

/// Run the interactive TUI package manager.
/// Returns a list of set toggle actions (set_name, enabled).
pub fn run_packages_tui(
    active_sets: &[String],
) -> Result<Vec<(String, bool)>> {
    // Build set rows
    let mut rows: Vec<SetRow> = TOOL_SETS
        .iter()
        .map(|ts| {
            SetRow {
                name: ts.name.to_string(),
                description: ts.description.to_string(),
                package_count: ts.package_count,
                active: active_sets.contains(&ts.name.to_string()),
                locked: ts.locked,
            }
        })
        .collect();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut list_state = ListState::default();
    list_state.select(Some(0));
    let mut view = View::Sets;
    let mut pkg_state = ListState::default();
    let mut toggles: Vec<(String, bool)> = vec![];

    loop {
        terminal.draw(|f| {
            let area = centered_rect(75, 80, f.area());

            let chunks = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .split(area);

            match &view {
                View::Sets => {
                    draw_sets_view(f, chunks[0], chunks[1], &rows, &mut list_state);
                }
                View::Packages { set_index } => {
                    draw_packages_view(f, chunks[0], chunks[1], *set_index, &mut pkg_state);
                }
            }
        })?;

        if let Event::Key(key) = event::read()? {
            match &view {
                View::Sets => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = list_state.selected().unwrap_or(0);
                        let next = if i >= rows.len() - 1 { 0 } else { i + 1 };
                        list_state.select(Some(next));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = list_state.selected().unwrap_or(0);
                        let next = if i == 0 { rows.len() - 1 } else { i - 1 };
                        list_state.select(Some(next));
                    }
                    KeyCode::Char(' ') => {
                        if let Some(i) = list_state.selected() {
                            if !rows[i].locked {
                                rows[i].active = !rows[i].active;
                                toggles.push((rows[i].name.clone(), rows[i].active));
                            }
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(i) = list_state.selected() {
                            pkg_state.select(Some(0));
                            view = View::Packages { set_index: i };
                        }
                    }
                    _ => {}
                },
                View::Packages { set_index } => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        view = View::Sets;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let set_name = &rows[*set_index].name;
                        let pkg_count = NIX_SETS
                            .iter()
                            .find(|s| s.name == set_name)
                            .map(|s| s.packages.len())
                            .unwrap_or(0);
                        let i = pkg_state.selected().unwrap_or(0);
                        let next = if pkg_count == 0 || i >= pkg_count - 1 { 0 } else { i + 1 };
                        pkg_state.select(Some(next));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let set_name = &rows[*set_index].name;
                        let pkg_count = NIX_SETS
                            .iter()
                            .find(|s| s.name == set_name)
                            .map(|s| s.packages.len())
                            .unwrap_or(0);
                        let i = pkg_state.selected().unwrap_or(0);
                        let next = if i == 0 { pkg_count.saturating_sub(1) } else { i - 1 };
                        pkg_state.select(Some(next));
                    }
                    _ => {}
                },
            }
        }
    }

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;

    Ok(toggles)
}

fn draw_sets_view(
    f: &mut ratatui::Frame,
    main_area: Rect,
    help_area: Rect,
    rows: &[SetRow],
    state: &mut ListState,
) {
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let marker = if r.locked {
                "■"
            } else if r.active {
                "●"
            } else {
                "○"
            };
            let status_color = if r.active { Color::Green } else { Color::DarkGray };
            let status_text = if r.active { "active" } else { "off" };

            let line = Line::from(vec![
                Span::styled(
                    format!(" {marker} "),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!("{:<16}", r.name),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:>3} pkgs   ", r.package_count),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("{:<8}", status_text),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    &r.description,
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(" devbox packages — Tool Sets ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(list, main_area, state);

    let help = Paragraph::new(Line::from(vec![
        Span::styled(" Space ", Style::default().fg(Color::Yellow)),
        Span::raw("Toggle  "),
        Span::styled("Enter ", Style::default().fg(Color::Yellow)),
        Span::raw("Browse packages  "),
        Span::styled("q/Esc ", Style::default().fg(Color::Yellow)),
        Span::raw("Quit"),
    ]))
    .block(Block::default().borders(Borders::ALL));

    f.render_widget(help, help_area);
}

fn draw_packages_view(
    f: &mut ratatui::Frame,
    main_area: Rect,
    help_area: Rect,
    set_index: usize,
    state: &mut ListState,
) {
    let set_name = TOOL_SETS.get(set_index).map(|s| s.name).unwrap_or("unknown");

    let packages: Vec<&str> = NIX_SETS
        .iter()
        .find(|s| s.name == set_name)
        .map(|s| s.packages.to_vec())
        .unwrap_or_default();

    let items: Vec<ListItem> = packages
        .iter()
        .map(|pkg| {
            let line = Line::from(vec![
                Span::styled(
                    "  ● ",
                    Style::default().fg(Color::Green),
                ),
                Span::styled(
                    format!("{pkg}"),
                    Style::default().fg(Color::Cyan),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let title = format!(" devbox packages > {} ({} packages) ", set_name, packages.len());
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(list, main_area, state);

    let help = Paragraph::new(Line::from(vec![
        Span::styled(" ↑↓ ", Style::default().fg(Color::Yellow)),
        Span::raw("Navigate  "),
        Span::styled("Esc ", Style::default().fg(Color::Yellow)),
        Span::raw("Back to sets  "),
        Span::styled("q ", Style::default().fg(Color::Yellow)),
        Span::raw("Quit"),
    ]))
    .block(Block::default().borders(Borders::ALL));

    f.render_widget(help, help_area);
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
