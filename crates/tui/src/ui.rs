//! Rendering of the four screens; pure functions of the app state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, Tabs, Wrap};

use crate::App;

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// Status and latest Snapshot resources.
    #[default]
    Overview,
    /// ActionRuns, with the approval inbox first.
    Actions,
    /// Issues.
    Issues,
    /// Event log tail.
    Events,
}

impl Screen {
    /// All screens in tab order.
    pub const ALL: [Screen; 4] = [
        Screen::Overview,
        Screen::Actions,
        Screen::Issues,
        Screen::Events,
    ];

    /// Tab label.
    pub fn title(self) -> &'static str {
        match self {
            Screen::Overview => "1 Overview",
            Screen::Actions => "2 Actions",
            Screen::Issues => "3 Issues",
            Screen::Events => "4 Events",
        }
    }
}

/// Semantic color for a health or status string.
fn tone(value: &str) -> Color {
    match value {
        "healthy" | "succeeded" | "running" | "resolved" => Color::Green,
        "degraded" | "waiting_for_approval" | "dispatch_frozen" | "waiting_for_human" => {
            Color::Yellow
        }
        "down" | "failed" | "verification_failed" | "fully_frozen" | "cancelled" => Color::Red,
        _ => Color::DarkGray,
    }
}

/// Draws the whole frame.
pub fn draw(frame: &mut Frame, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_header(frame, areas[0], app);
    match app.screen {
        Screen::Overview => draw_overview(frame, areas[1], app),
        Screen::Actions => draw_actions(frame, areas[1], app),
        Screen::Issues => draw_issues(frame, areas[1], app),
        Screen::Events => draw_events(frame, areas[1], app),
    }
    draw_footer(frame, areas[2], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = Screen::ALL.iter().map(|s| Line::from(s.title())).collect();
    let selected = Screen::ALL
        .iter()
        .position(|s| *s == app.screen)
        .unwrap_or(0);
    let mode = app.status.mode.clone();
    let title = Line::from(vec![
        Span::styled(
            " broccoli ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" mode "),
        Span::styled(
            mode.clone(),
            Style::default()
                .fg(tone(&mode))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(if app.status.dry_run {
            "  · dry-run"
        } else {
            "  · LIVE"
        }),
        Span::raw(format!("  · {}", app.status.team_backend)),
        Span::raw(format!("  · inbox {}", app.status.counts.actions_waiting)),
    ]);
    let tabs = Tabs::new(titles)
        .select(selected)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, area);
}

fn draw_overview(frame: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Row> = app
        .resources
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(r.id.clone()),
                Cell::from(r.kind.clone()),
                Cell::from(Span::styled(
                    r.health.clone(),
                    Style::default()
                        .fg(tone(&r.health))
                        .add_modifier(Modifier::BOLD),
                )),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(45),
            Constraint::Percentage(30),
            Constraint::Percentage(25),
        ],
    )
    .header(
        Row::new(vec!["resource", "kind", "health"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(format!(
        " latest Snapshot · {} issues · {} jobs · {} events  (s = capture) ",
        app.status.counts.issues, app.status.counts.jobs, app.status.counts.events
    )));
    frame.render_widget(table, area);
}

fn draw_actions(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    let items: Vec<ListItem> = app
        .actions
        .iter()
        .enumerate()
        .map(|(index, a)| {
            let marker = if index == app.selected { "▶ " } else { "  " };
            let line = Line::from(vec![
                Span::raw(marker),
                Span::styled(
                    format!("{:<18}", a.runbook_id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{:<28}", a.target_ids.join(","))),
                Span::styled(
                    format!("{:<22}", a.status),
                    Style::default().fg(tone(&a.status)),
                ),
                Span::raw(a.approval.clone()),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" actions  (j/k select · a approve · r reject) "),
    );
    frame.render_widget(list, columns[0]);

    let detail = app.actions.get(app.selected).map_or_else(
        || "no action selected".to_string(),
        |a| {
            format!(
                "id: {}\nreason: {}\nverification: {}",
                a.action_run_id,
                a.reason,
                a.verification_summary.as_deref().unwrap_or("—")
            )
        },
    );
    let paragraph = Paragraph::new(detail)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" detail "));
    frame.render_widget(paragraph, columns[1]);
}

fn draw_issues(frame: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Row> = app
        .issues
        .iter()
        .map(|i| {
            Row::new(vec![
                Cell::from(i.title.clone()),
                Cell::from(i.priority.clone()),
                Cell::from(Span::styled(
                    i.status.clone(),
                    Style::default().fg(tone(&i.status)),
                )),
                Cell::from(i.issue_id.clone()),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(40),
            Constraint::Percentage(12),
            Constraint::Percentage(18),
            Constraint::Percentage(30),
        ],
    )
    .header(
        Row::new(vec!["title", "priority", "status", "id"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(" issues "));
    frame.render_widget(table, area);
}

fn draw_events(frame: &mut Frame, area: Rect, app: &App) {
    let visible = area.height.saturating_sub(2) as usize;
    let start = app.events.len().saturating_sub(visible);
    let items: Vec<ListItem> = app.events[start..]
        .iter()
        .map(|e| {
            let time = e.occurred_at.get(11..19).unwrap_or("").to_string();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>5} ", e.sequence),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{time} ")),
                Span::styled(format!("{:<34}", e.kind), Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{:<16}", e.actor),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(e.summary.clone()),
            ]))
        })
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" events "));
    frame.render_widget(list, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let message = app.message.as_deref().unwrap_or(
        "1-4 screens · j/k select · a approve · r reject · s snapshot · f freeze dispatch · F freeze all · u resume · q quit",
    );
    let style = if app.message.is_some() {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(Paragraph::new(message).style(style), area);
}
