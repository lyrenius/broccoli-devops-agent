//! Events: the append-only log every component writes to, live.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::app::App;
use crate::format::{actor_color, pad, short, time};
use crate::screens::step;
use crate::ui::{Doc, badge, dim, inner_width, panel, render_doc, sep};

/// Footer hints.
pub const HINTS: &str =
    "j/k move · G newest and follow · PgUp/PgDn page · Enter/t trace of the event's Issue";

/// Events state.
#[derive(Debug)]
pub struct EventsState {
    /// Selection.
    pub list: ListState,
    /// Whether the newest event stays selected as events arrive.
    pub follow: bool,
}

impl Default for EventsState {
    fn default() -> Self {
        Self {
            list: ListState::default(),
            follow: true,
        }
    }
}

/// Keeps the newest event selected while following.
pub fn on_events(app: &mut App) {
    if app.events_view.follow && !app.events.is_empty() {
        app.events_view.list.select(Some(app.events.len() - 1));
    }
}

/// Events keys.
pub fn key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> bool {
    let len = app.events.len();
    let view = &mut app.events_view;
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            view.list.select(step(view.list.selected(), len, 1));
            view.follow = view.list.selected() == Some(len.saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => {
            view.list.select(step(view.list.selected(), len, -1));
            view.follow = false;
        }
        KeyCode::PageDown => {
            view.list.select(step(view.list.selected(), len, 20));
            view.follow = view.list.selected() == Some(len.saturating_sub(1));
        }
        KeyCode::PageUp => {
            view.list.select(step(view.list.selected(), len, -20));
            view.follow = false;
        }
        KeyCode::Char('g') | KeyCode::Home => {
            view.list.select(step(None, len, 0));
            view.follow = false;
        }
        KeyCode::Char('G') | KeyCode::End => {
            view.list.select(step(None, len, len as isize));
            view.follow = true;
        }
        KeyCode::Enter | KeyCode::Char('t') => {
            let selected = view
                .list
                .selected()
                .and_then(|i| app.events.get(i))
                .map(|e| (e.issue_id.clone(), e.job_id.clone()));
            match selected {
                Some((Some(issue), job)) => app.open_trace(issue, job),
                _ => app.message = Some("this event belongs to no Issue".to_string()),
            }
        }
        _ => return false,
    }
    true
}

/// Draws the Events screen.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::vertical([Constraint::Min(4), Constraint::Length(6)]).split(area);
    let width = inner_width(rows[0]);
    if app.events.is_empty() {
        app.events_view.list.select(None);
    } else if app
        .events_view
        .list
        .selected()
        .is_none_or(|i| i >= app.events.len())
    {
        app.events_view.list.select(Some(app.events.len() - 1));
    }
    let items: Vec<ListItem> = app
        .events
        .iter()
        .map(|e| {
            let rest = width.saturating_sub(6 + 9 + 34 + 17 + 2);
            ListItem::new(Line::from(vec![
                dim(format!("{:>5} ", e.sequence)),
                Span::raw(format!("{} ", time(&e.occurred_at))),
                Span::styled(pad(&e.kind, 33), Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(
                    pad(&e.actor, 16),
                    Style::default().fg(actor_color(&e.actor)),
                ),
                Span::raw(" "),
                Span::raw(crate::format::truncate(&e.summary.replace('\n', " "), rest)),
            ]))
        })
        .collect();
    let title = Line::from(vec![
        Span::raw(" Events "),
        if app.events_connected {
            badge("live", Color::Green)
        } else {
            badge("not connected", Color::Red)
        },
        Span::raw(format!(
            " {} in view{} ",
            app.events.len(),
            if app.events_view.follow {
                " · following"
            } else {
                ""
            }
        )),
    ]);
    let list = List::new(items)
        .block(panel(title))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, rows[0], &mut app.events_view.list);

    let mut doc = Doc::new(inner_width(rows[1]));
    match app
        .events_view
        .list
        .selected()
        .and_then(|i| app.events.get(i))
    {
        None => doc.note(if app.events.is_empty() {
            "No events yet."
        } else {
            "Model output is recorded as data, never as authority."
        }),
        Some(e) => {
            let mut ids = Vec::new();
            if let Some(issue) = &e.issue_id {
                ids.push(format!("issue {}", short(issue)));
            }
            if let Some(job) = &e.job_id {
                ids.push(format!("job {}", short(job)));
            }
            if let Some(action) = &e.action_run_id {
                ids.push(format!("action {}", short(action)));
            }
            let mut line = vec![
                Span::styled(e.kind.clone(), Style::default().fg(Color::Cyan)),
                sep(),
                Span::styled(e.actor.clone(), Style::default().fg(actor_color(&e.actor))),
                sep(),
                dim(format!("trust {}", e.trust)),
            ];
            if !ids.is_empty() {
                line.push(sep());
                line.push(dim(ids.join(" · ")));
            }
            doc.line(line);
            doc.text(&e.summary);
        }
    }
    let mut scroll = 0;
    render_doc(frame, rows[1], &doc, &mut scroll, panel(" Selected event "));
}
