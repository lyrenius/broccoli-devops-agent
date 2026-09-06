//! Overview: the latest Snapshot with its coverage gaps, what runs now, and what it has cost.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Cell, Paragraph, Row, Table};

use crate::app::App;
use crate::format::{
    age, duration_ms, kind_name, label, millis, money, short, thousands, time, tokens,
};
use crate::ui::{Doc, badge, bold, dim, inner_width, panel, render_doc, sep, status_span};

/// Footer hints.
pub const HINTS: &str =
    "j/k pass · Enter/t trace · c interrupt · s capture · f/F/u freeze dispatch/all/resume";

/// Overview state.
#[derive(Debug, Default)]
pub struct OverviewState {
    /// Selected running pass.
    pub selected: usize,
    gaps_scroll: usize,
}

/// Overview keys.
pub fn key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> bool {
    let running = app.running().len();
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if app.overview.selected + 1 < running {
                app.overview.selected += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.overview.selected = app.overview.selected.saturating_sub(1);
        }
        KeyCode::Enter | KeyCode::Char('t') => {
            let index = app.overview.selected.min(running.saturating_sub(1));
            match app.running().get(index).cloned() {
                Some(pass) => app.open_trace(pass.issue_id, Some(pass.job_id)),
                None => app.message = Some("no pass is running".to_string()),
            }
        }
        KeyCode::PageDown => app.overview.gaps_scroll += 5,
        KeyCode::PageUp => app.overview.gaps_scroll = app.overview.gaps_scroll.saturating_sub(5),
        _ => return false,
    }
    true
}

/// Draws the Overview.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let intro = intro_lines(app, inner_width(area));
    let rows = Layout::vertical([
        Constraint::Length(intro.len() as u16 + 2),
        Constraint::Length(4),
        Constraint::Length(9),
        Constraint::Min(4),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Text::from(intro)).block(panel(" Overview ")),
        rows[0],
    );
    draw_tiles(frame, rows[1], app);
    let middle =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[2]);
    draw_running(frame, middle[0], app);
    draw_usage(frame, middle[1], app);
    let bottom =
        Layout::horizontal([Constraint::Percentage(68), Constraint::Percentage(32)]).split(rows[3]);
    draw_resources(frame, bottom[0], app);
    draw_gaps(frame, bottom[1], app);
}

/// The subtitle and alerts the web console shows above its tiles.
fn intro_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut doc = Doc::new(width);
    let mut subtitle = match &app.snapshot {
        Some(snapshot) => format!(
            "Snapshot captured {} · {} · topology {}",
            age(&snapshot.created_at, app.now),
            label(&snapshot.cause),
            snapshot.topology_revision
        ),
        None => "The latest Snapshot of the deployment, with its coverage gaps.".to_string(),
    };
    if let Some(status) = &app.status {
        let secs = status.snapshot_interval_secs;
        subtitle.push_str(&if secs == 0 {
            " · periodic capture off".to_string()
        } else if secs % 60 == 0 {
            format!(" · captured every {} min", secs / 60)
        } else {
            format!(" · captured every {secs} s")
        });
    }
    doc.note(&subtitle);
    if let Some(status) = &app.status
        && let Some(recovery) = &status.recovery
        && status.mode != "running"
        && recovery.needs_attention()
    {
        let previous = if recovery.previous_mode != "running" {
            format!(
                " · the previous process was {}",
                label(&recovery.previous_mode)
            )
        } else {
            String::new()
        };
        doc.alert(
            Color::Yellow,
            &format!(
                "Recovered from a restart; the Scheduler is {}. {} interrupted item(s) were put in the Inbox{previous}. Check the Inbox, then press u to resume.",
                label(&status.mode),
                recovery.interrupted()
            ),
        );
    }
    if let Some(snapshot) = &app.snapshot {
        // Old means well past the configured cadence (or ten minutes when there is none).
        let cadence_ms = app
            .status
            .as_ref()
            .map_or(0, |s| s.snapshot_interval_secs as i64 * 1000);
        let stale_after = if cadence_ms > 0 {
            (3 * cadence_ms).max(60_000)
        } else {
            10 * 60_000
        };
        let age_ms = millis(&snapshot.created_at).map_or(0, |t| app.now.timestamp_millis() - t);
        if age_ms > stale_after {
            doc.alert(
                Color::Cyan,
                "This Snapshot is old; the probes may have changed since. Press s for the current picture.",
            );
        }
    }
    doc.lines
}

/// The five stat tiles.
fn draw_tiles(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::horizontal([Constraint::Percentage(20); 5]).split(area);
    let status = app.status.as_ref();
    let snapshot = app.snapshot.as_ref();
    let healthy = snapshot.map_or(0, |s| {
        s.resources.iter().filter(|r| r.health == "healthy").count()
    });
    let total = snapshot.map_or(0, |s| s.resources.len());
    let live_issues = app.issues.iter().filter(|i| i.is_live()).count();
    let inbox_total = status.map_or(0, |s| s.inbox.total);
    let usage = status.map(|s| &s.usage);
    let tiles: [(&str, String, String, Color); 5] = [
        (
            "Healthy resources",
            if snapshot.is_some() {
                format!("{healthy} / {total}")
            } else {
                "—".to_string()
            },
            String::new(),
            if snapshot.is_some() && healthy < total {
                Color::Yellow
            } else {
                Color::Reset
            },
        ),
        (
            "Inbox",
            status.map_or("—".to_string(), |s| s.inbox.total.to_string()),
            status.map_or(String::new(), |s| {
                format!(
                    "{} requests · {} denied · {} failed",
                    s.inbox.permission_requests,
                    s.inbox.permission_denied,
                    s.inbox.failed_jobs + s.inbox.failed_actions
                )
            }),
            if inbox_total > 0 {
                Color::Red
            } else {
                Color::Reset
            },
        ),
        (
            "Open issues",
            live_issues.to_string(),
            format!("{} total", app.issues.len()),
            Color::Reset,
        ),
        (
            "Events",
            status.map_or("—".to_string(), |s| s.counts.events.to_string()),
            status.map_or(String::new(), |s| {
                format!("{} jobs · {} actions", s.counts.jobs, s.counts.actions)
            }),
            Color::Reset,
        ),
        (
            "Model spend",
            usage.map_or("—".to_string(), |u| {
                money(u).unwrap_or_else(|| tokens(u.total_tokens))
            }),
            usage.map_or(String::new(), |u| {
                if u.cost.is_none() {
                    "no [model.pricing] configured".to_string()
                } else {
                    format!("{} pass(es) · {} tokens", u.passes, tokens(u.total_tokens))
                }
            }),
            match usage.and_then(|u| u.budget.as_ref()) {
                Some(budget) if budget.exceeded => Color::Red,
                Some(budget) if budget.used_fraction >= 0.8 => Color::Yellow,
                _ => Color::Reset,
            },
        ),
    ];
    for (column, (title, value, hint, color)) in columns.iter().zip(tiles) {
        let value_style = Style::default().add_modifier(Modifier::BOLD).fg(color);
        let text = Text::from(vec![
            Line::from(Span::styled(value, value_style)),
            Line::from(dim(hint)),
        ]);
        frame.render_widget(
            Paragraph::new(text).block(panel(format!(" {title} "))),
            *column,
        );
    }
}

/// The passes in flight, each selectable so it can be interrupted or traced.
fn draw_running(frame: &mut Frame, area: Rect, app: &mut App) {
    let running = app.running().to_vec();
    if app.overview.selected >= running.len() {
        app.overview.selected = running.len().saturating_sub(1);
    }
    let mut doc = Doc::new(inner_width(area));
    doc.note("Passes in flight. Interrupting one stops it at its next step; its transcript is kept and the Job lands in the Failed inbox.");
    if running.is_empty() {
        doc.note("Nothing is running.");
    }
    for (index, pass) in running.iter().enumerate() {
        let marker = if index == app.overview.selected {
            "▶ "
        } else {
            "  "
        };
        let elapsed = millis(&pass.started_at).map_or(0, |t| app.now.timestamp_millis() - t);
        doc.line(vec![
            Span::styled(marker, Style::default().fg(Color::Green)),
            Span::styled("● ", Style::default().fg(Color::Green)),
            bold(format!("Job {}", short(&pass.job_id))),
            sep(),
            Span::raw(format!("Issue {}", short(&pass.issue_id))),
            sep(),
            dim(format!(
                "since {} ({})",
                time(&pass.started_at),
                duration_ms(elapsed)
            )),
        ]);
    }
    if let Some(latest) = app.latest_progress() {
        doc.styled(
            &format!("↳ {} {}", time(&latest.occurred_at), latest.summary),
            Style::default().fg(Color::Cyan),
        );
    }
    let title = if running.is_empty() {
        " Running now ".to_string()
    } else {
        format!(
            " Running now · {} pass(es) · c interrupt · Enter trace ",
            running.len()
        )
    };
    let mut scroll = 0;
    render_doc(frame, area, &doc, &mut scroll, panel(title));
}

/// Tokens in and out, the cache hit, the cost, the per-model split, and the budget.
fn draw_usage(frame: &mut Frame, area: Rect, app: &App) {
    let mut doc = Doc::new(inner_width(area));
    match app.status.as_ref().map(|s| &s.usage) {
        None => doc.note("—"),
        Some(usage) if usage.passes == 0 => doc.note("No model-backed pass has run yet."),
        Some(usage) => {
            doc.line(vec![
                dim("input "),
                Span::raw(thousands(usage.input_tokens)),
                sep(),
                dim("of which cached "),
                Span::raw(thousands(usage.cached_input_tokens)),
                sep(),
                dim("output "),
                Span::raw(thousands(usage.output_tokens)),
            ]);
            let mut totals = vec![dim("total "), bold(thousands(usage.total_tokens))];
            if let Some(cost) = money(usage) {
                totals.push(sep());
                totals.push(dim("cost "));
                totals.push(bold(cost));
            }
            totals.push(sep());
            totals.push(dim(format!(
                "{} pass(es) · {} request(s)",
                usage.passes, usage.requests
            )));
            doc.line(totals);
            if usage.by_model.len() > 1 {
                for model in &usage.by_model {
                    let mut line = format!(
                        "  {} · {} tokens",
                        model.model,
                        tokens(model.input_tokens + model.output_tokens)
                    );
                    if let Some(cost) = model.cost {
                        line.push_str(&format!(" · {cost:.4}"));
                    }
                    doc.note(&line);
                }
            }
            if usage.cost.is_none() {
                doc.note("Add [model.pricing] to the agent config to price these tokens.");
            }
            if usage.requests_without_usage > 0 {
                doc.styled(
                    &format!(
                        "{} request(s) reported no usage, so the real figures are higher than these.",
                        usage.requests_without_usage
                    ),
                    Style::default().fg(Color::Yellow),
                );
            }
            if let Some(budget) = &usage.budget {
                let percent = (budget.used_fraction * 100.0).round() as u64;
                let cells = 20;
                let filled = ((budget.used_fraction * cells as f64).round() as usize).min(cells);
                let color = if budget.exceeded {
                    Color::Red
                } else if budget.used_fraction >= 0.8 {
                    Color::Yellow
                } else {
                    Color::Green
                };
                doc.line(vec![
                    dim(format!("budget {percent}% used ")),
                    Span::styled("▮".repeat(filled), Style::default().fg(color)),
                    Span::styled(
                        "▯".repeat(cells - filled),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);
                if budget.exceeded {
                    doc.alert(
                        Color::Red,
                        &format!(
                            "The spend ceiling is reached, so dispatch is frozen. Raise [budget] in Settings, then resume.{}",
                            budget
                                .reason
                                .as_deref()
                                .map(|r| format!(" {r}"))
                                .unwrap_or_default()
                        ),
                    );
                }
            }
        }
    }
    let mut scroll = 0;
    render_doc(frame, area, &doc, &mut scroll, panel(" Model usage "));
}

/// Every resource as the Collector last observed it.
fn draw_resources(frame: &mut Frame, area: Rect, app: &App) {
    let title = match &app.status {
        Some(status) => format!(
            " Resources · {} issues · {} jobs · {} events · s = capture ",
            status.counts.issues, status.counts.jobs, status.counts.events
        ),
        None => " Resources ".to_string(),
    };
    let Some(snapshot) = &app.snapshot else {
        let doc = {
            let mut doc = Doc::new(inner_width(area));
            doc.heading("No Snapshot yet");
            doc.note("Press s to capture one and see the deployment.");
            doc
        };
        let mut scroll = 0;
        render_doc(frame, area, &doc, &mut scroll, panel(title));
        return;
    };
    let rows: Vec<Row> = snapshot
        .resources
        .iter()
        .map(|r| {
            let latency = r.metrics.iter().find(|m| m.name.ends_with(".latency"));
            let signals: Vec<String> = r
                .metrics
                .iter()
                .filter(|m| !m.name.starts_with("probe."))
                .map(|m| {
                    let value = if m.value.fract() == 0.0 {
                        format!("{}", m.value as i64)
                    } else {
                        format!("{:.1}", m.value)
                    };
                    format!("{} {value}{}", m.name, if m.unit == "s" { "s" } else { "" })
                })
                .collect();
            Row::new(vec![
                Cell::from(bold(r.resource_id.clone())),
                Cell::from(dim(kind_name(&r.kind))),
                Cell::from(status_span(&r.health)),
                Cell::from(dim(if signals.is_empty() {
                    "—".to_string()
                } else {
                    signals.join(" · ")
                })),
                Cell::from(dim(
                    latency.map_or("—".to_string(), |m| format!("{:.0} ms", m.value))
                )),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(24),
            Constraint::Percentage(16),
            Constraint::Percentage(12),
            Constraint::Percentage(38),
            Constraint::Percentage(10),
        ],
    )
    .header(
        Row::new(vec!["resource", "kind", "health", "signals", "latency"]).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::DarkGray),
        ),
    )
    .block(panel(title));
    frame.render_widget(table, area);
}

/// What the Collector could not observe. A gap is a fact, never assumed healthy.
fn draw_gaps(frame: &mut Frame, area: Rect, app: &mut App) {
    let mut doc = Doc::new(inner_width(area));
    match &app.snapshot {
        None => doc.note("—"),
        Some(snapshot) if snapshot.coverage_gaps.is_empty() => {
            doc.note("None — every resource was observed.");
        }
        Some(snapshot) => {
            for gap in &snapshot.coverage_gaps {
                doc.line(vec![
                    bold(gap.resource_id.clone()),
                    Span::raw("  "),
                    dim(gap.probe_id.clone()),
                ]);
                doc.indented(2, &gap.reason, Style::default().fg(Color::DarkGray));
            }
        }
    }
    let title = Line::from(vec![
        Span::raw(" Coverage gaps "),
        match &app.snapshot {
            Some(s) if !s.coverage_gaps.is_empty() => {
                badge(&s.coverage_gaps.len().to_string(), Color::Yellow)
            }
            _ => Span::raw(""),
        },
    ]);
    render_doc(
        frame,
        area,
        &doc,
        &mut app.overview.gaps_scroll,
        panel(title),
    );
}
