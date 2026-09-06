//! File a report: a human report bypasses anomaly detection and runs the Operate Team from a
//! fresh Snapshot; the steps stream in while it runs, and the outcome shows what happened to
//! its proposals.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::api::ReportOutcome;
use crate::app::{App, Msg};
use crate::format::{duration_ms, label, time, wrap};
use crate::ui::{Doc, badge, bold, dim, inner_width, panel, render_doc, sep, status_span};

/// The priority choices: the label, and what the API is sent (nothing means human-top).
const PRIORITIES: [(&str, Option<&str>); 5] = [
    ("top (default)", None),
    ("critical", Some("critical")),
    ("high", Some("high")),
    ("normal", Some("normal")),
    ("low", Some("low")),
];

/// A form field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// Title.
    Title,
    /// What you observed.
    Observed,
    /// Reporter.
    Reporter,
    /// Priority.
    Priority,
    /// The file button.
    Submit,
}

impl Field {
    const ALL: [Field; 5] = [
        Field::Title,
        Field::Observed,
        Field::Reporter,
        Field::Priority,
        Field::Submit,
    ];

    fn shifted(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|f| *f == self).unwrap_or(0) as isize;
        let count = Self::ALL.len() as isize;
        Self::ALL[((index + delta).rem_euclid(count)) as usize]
    }
}

/// Report state.
#[derive(Debug)]
pub struct ReportState {
    /// Title.
    pub title: String,
    /// What you observed.
    pub description: String,
    /// Reporter.
    pub reporter: String,
    /// Index into the priority choices.
    pub priority: usize,
    /// The focused field, when the form is being edited.
    pub focus: Option<Field>,
    /// Whether a report is running now.
    pub running: bool,
    /// Events after this sequence are this report's progress.
    pub progress_after: u64,
    /// When the report was filed.
    pub started: Option<Instant>,
    /// The last outcome.
    pub outcome: Option<ReportOutcome>,
    /// Why the last report failed.
    pub error: Option<String>,
}

impl ReportState {
    /// An empty form with the operator as reporter.
    pub fn new(operator: &str) -> Self {
        Self {
            title: String::new(),
            description: String::new(),
            reporter: operator.to_string(),
            priority: 0,
            focus: None,
            running: false,
            progress_after: 0,
            started: None,
            outcome: None,
            error: None,
        }
    }
}

/// Footer hints, depending on whether the form has the keys.
pub fn hints(app: &App) -> &'static str {
    if app.report.focus.is_some() {
        "Tab/↓ next · Shift-Tab/↑ previous · Enter next (newline in the description) · Ctrl-S file · Esc leave the form"
    } else {
        "Enter/i edit the form · Ctrl-S file the report · t trace of the outcome · n clear"
    }
}

/// Keys while the form does not have focus.
pub fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Enter | KeyCode::Char('i') => app.report.focus = Some(Field::Title),
        KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => file(app),
        KeyCode::Char('t') => match &app.report.outcome {
            Some(outcome) => {
                let (issue, job) = (outcome.issue.issue_id.clone(), outcome.job.job_id.clone());
                app.open_trace(issue, Some(job));
            }
            None => app.message = Some("no report has run yet".to_string()),
        },
        KeyCode::Char('n') => {
            let reporter = app.report.reporter.clone();
            let running = app.report.running;
            app.report = ReportState::new(&reporter);
            app.report.running = running;
        }
        _ => return false,
    }
    true
}

/// Keys while the form has focus: typing edits the field; Esc gives the keys back.
pub fn form_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let Some(field) = app.report.focus else {
        return;
    };
    match code {
        KeyCode::Esc => app.report.focus = None,
        KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => file(app),
        KeyCode::Tab | KeyCode::Down => app.report.focus = Some(field.shifted(1)),
        KeyCode::BackTab | KeyCode::Up => app.report.focus = Some(field.shifted(-1)),
        KeyCode::Enter => match field {
            Field::Observed => app.report.description.push('\n'),
            Field::Submit => file(app),
            _ => app.report.focus = Some(field.shifted(1)),
        },
        KeyCode::Backspace => {
            match field {
                Field::Title => app.report.title.pop(),
                Field::Observed => app.report.description.pop(),
                Field::Reporter => app.report.reporter.pop(),
                _ => None,
            };
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if field == Field::Priority => {
            let count = PRIORITIES.len();
            app.report.priority = if code == KeyCode::Left {
                (app.report.priority + count - 1) % count
            } else {
                (app.report.priority + 1) % count
            };
        }
        KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => match field {
            Field::Title => app.report.title.push(c),
            Field::Observed => app.report.description.push(c),
            Field::Reporter => app.report.reporter.push(c),
            Field::Submit if c == ' ' => file(app),
            _ => {}
        },
        _ => {}
    }
}

/// Files the report; the outcome arrives as a message while the steps stream in.
fn file(app: &mut App) {
    if app.report.running {
        app.message = Some("a report is already running".to_string());
        return;
    }
    if app.report.title.trim().is_empty() || app.report.description.trim().is_empty() {
        app.message = Some("title and description are required".to_string());
        return;
    }
    app.report.running = true;
    app.report.error = None;
    app.report.outcome = None;
    app.report.progress_after = app.events_seq;
    app.report.started = Some(Instant::now());
    app.report.focus = None;
    app.message = Some(
        "running the Operate Team… (a model-backed run takes minutes; keep working)".to_string(),
    );
    let client = app.client.clone();
    let title = app.report.title.trim().to_string();
    let description = app.report.description.trim().to_string();
    let reporter = if app.report.reporter.trim().is_empty() {
        app.operator.clone()
    } else {
        app.report.reporter.trim().to_string()
    };
    let priority = PRIORITIES[app.report.priority].1.map(str::to_string);
    let tx = app.sender();
    tokio::spawn(async move {
        let result = client
            .report(&title, &description, &reporter, priority.as_deref())
            .await;
        let _ = tx.send(Msg::Report(Box::new(result)));
    });
}

/// The report finished.
pub fn on_done(app: &mut App, result: Result<ReportOutcome, String>) {
    app.report.running = false;
    match result {
        Ok(outcome) => {
            app.message = Some(format!(
                "report filed: issue “{}” is {}; the first pass is {}",
                outcome.issue.title,
                label(&outcome.issue.status),
                label(&outcome.job.status)
            ));
            app.report.outcome = Some(outcome);
        }
        Err(error) => {
            app.message = Some(format!("report failed: {error}"));
            app.report.error = Some(error);
        }
    }
    app.request_refresh();
}

/// Draws the Report screen.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    draw_form(frame, columns[0], app);
    draw_outcome(frame, columns[1], app);
}

fn field_block(title: &str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(
            format!(" {title} "),
            if focused {
                style.add_modifier(Modifier::BOLD)
            } else {
                style
            },
        ))
}

fn field_text(text: &str, focused: bool, placeholder: &str, width: usize) -> Text<'static> {
    if text.is_empty() && !focused {
        return Text::from(Line::from(dim(placeholder.to_string())));
    }
    let mut lines: Vec<Line> = wrap(text, width)
        .into_iter()
        .map(|line| Line::from(Span::raw(line)))
        .collect();
    if focused {
        if let Some(last) = lines.last_mut() {
            last.spans
                .push(Span::styled("▏", Style::default().fg(Color::Green)));
        } else {
            lines.push(Line::from(Span::styled(
                "▏",
                Style::default().fg(Color::Green),
            )));
        }
    }
    Text::from(lines)
}

fn draw_form(frame: &mut Frame, area: Rect, app: &App) {
    let report = &app.report;
    let focus = report.focus;
    let outer = panel(if focus.is_some() {
        " Report · editing (Esc leaves the form) "
    } else {
        " Report · Enter to edit "
    });
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    let progress_height = if report.running || report.started.is_some() {
        8
    } else {
        0
    };
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(progress_height),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(dim(
            "What you saw, in your own words. The Team reads it as data, never as instructions.",
        ))),
        rows[0],
    );
    let width = usize::from(inner.width).saturating_sub(2);
    frame.render_widget(
        Paragraph::new(field_text(
            &report.title,
            focus == Some(Field::Title),
            "Contestants cannot submit",
            width,
        ))
        .block(field_block("Title", focus == Some(Field::Title))),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(field_text(
            &report.description,
            focus == Some(Field::Observed),
            "Web submissions time out since 10:12; the queue keeps growing",
            width,
        ))
        .block(field_block(
            "What you observed",
            focus == Some(Field::Observed),
        )),
        rows[2],
    );
    let pair =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[3]);
    frame.render_widget(
        Paragraph::new(field_text(
            &report.reporter,
            focus == Some(Field::Reporter),
            "your name",
            width / 2,
        ))
        .block(field_block("Reporter", focus == Some(Field::Reporter))),
        pair[0],
    );
    let priority_focused = focus == Some(Field::Priority);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(if priority_focused { "◀ " } else { "  " }),
            bold(PRIORITIES[report.priority].0),
            Span::raw(if priority_focused { " ▶" } else { "" }),
            dim("  humans only"),
        ]))
        .block(field_block("Priority (←/→)", priority_focused)),
        pair[1],
    );
    let submit_focused = focus == Some(Field::Submit);
    let button = if report.running {
        Span::styled(
            " Running the Operate Team… ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        )
    } else if submit_focused {
        Span::styled(
            " File report (Enter) ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(" File report ", Style::default().fg(Color::Green))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            button,
            dim("   Ctrl-S files from anywhere in the form"),
        ]))
        .block(field_block("", submit_focused)),
        rows[4],
    );
    if progress_height > 0 {
        draw_progress(frame, rows[5], app);
    }
}

/// Live progress for the report the viewer is blocked on: each Team callback is one line.
fn draw_progress(frame: &mut Frame, area: Rect, app: &App) {
    let report = &app.report;
    let lines: Vec<&crate::api::EventRecord> = app
        .events
        .iter()
        .filter(|e| e.sequence > report.progress_after)
        .filter(|e| e.kind == "team.callback" || e.kind == "model.usage")
        .collect();
    let mut doc = Doc::new(inner_width(area));
    if lines.is_empty() {
        doc.note(if report.running {
            "Waiting for the first step…"
        } else {
            "No steps were reported."
        });
    }
    let start = lines
        .len()
        .saturating_sub(usize::from(area.height).saturating_sub(2));
    for event in &lines[start..] {
        doc.line(vec![
            dim(format!("{} ", time(&event.occurred_at))),
            Span::raw(crate::format::truncate(
                &event.summary.replace('\n', " "),
                doc.width().saturating_sub(9),
            )),
        ]);
    }
    let elapsed = report
        .started
        .map_or(0, |at| at.elapsed().as_millis() as i64);
    let title = Line::from(vec![
        Span::raw(" Progress "),
        if report.running {
            badge(&format!("live · {}", duration_ms(elapsed)), Color::Green)
        } else {
            badge(
                &format!("finished in {}", duration_ms(elapsed)),
                Color::DarkGray,
            )
        },
        Span::raw(" "),
    ]);
    let mut scroll = 0;
    render_doc(frame, area, &doc, &mut scroll, panel(title));
}

fn draw_outcome(frame: &mut Frame, area: Rect, app: &App) {
    let mut doc = Doc::new(inner_width(area));
    let report = &app.report;
    match (&report.outcome, &report.error) {
        (None, Some(error)) => doc.alert(Color::Red, error),
        (None, None) => {
            doc.note("The diagnosis appears here. A model-backed run takes up to a minute, often longer; the steps stream in on the left.");
            doc.blank();
            doc.note("Human reports bypass anomaly detection and default to the top priority. The Operate Team diagnoses from a fresh Snapshot; any proposed actions go through the authority matrix.");
        }
        (Some(outcome), _) => {
            doc.line(vec![
                bold(outcome.issue.title.clone()),
                sep(),
                badge(&label(&outcome.issue.priority), Color::DarkGray),
                Span::raw(" "),
                status_span(&outcome.issue.status),
                sep(),
                dim("first pass "),
                status_span(&outcome.job.status),
            ]);
            if let Some(result) = &outcome.job.result {
                doc.line(vec![badge(&label(&result.outcome), Color::Cyan)]);
                doc.text(&result.summary);
                for question in &result.unresolved_questions {
                    doc.bullet_styled(
                        &format!("? {question}"),
                        Style::default().fg(Color::DarkGray),
                    );
                }
            } else {
                doc.note("no result");
            }
            if outcome.passes.len() > 1 {
                let stops: Vec<String> = outcome.passes.iter().map(|p| label(&p.stop)).collect();
                doc.note(&format!(
                    "{} passes ran: {}",
                    outcome.passes.len(),
                    stops.join(" → ")
                ));
            } else if let Some(pass) = outcome.passes.first() {
                doc.note(&format!("after the pass: {}", label(&pass.stop)));
            }
            if !outcome.actions.is_empty() {
                doc.blank();
                doc.rule("Proposed actions");
                for action in &outcome.actions {
                    doc.line(vec![
                        bold(action.title()),
                        sep(),
                        status_span(&action.status),
                        sep(),
                        dim(label(&action.approval)),
                    ]);
                }
            }
            doc.blank();
            doc.note("t open the trace · 2 the Inbox, if a proposal waits for approval");
        }
    }
    let title = if report.outcome.is_some() {
        " Outcome · the Team's diagnosis and what happened to its proposals "
    } else {
        " Outcome "
    };
    let mut scroll = 0;
    render_doc(frame, area, &doc, &mut scroll, panel(title));
}
