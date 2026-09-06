//! Inbox: everything that waits for a human, in three categories, and the history of every
//! decided action. Items leave only through a recorded decision made in the operator's name.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::api::{ActionRun, Job};
use crate::app::{App, Pending};
use crate::format::{label, pad, short, time};
use crate::screens::step;
use crate::ui::{
    Doc, badge, bold, dim, inner_width, panel, render_doc, segmented, sep, status_span,
};

/// Footer hints.
pub const HINTS: &str = "j/k select · [/] filter · a approve · r reject · b send upstream · x acknowledge · h history · Enter/t trace";

/// Which inbox category an item belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Waiting for approval.
    Request,
    /// Denied by rule or by a human, awaiting review.
    Denied,
    /// A failed Job, awaiting review.
    FailedJob,
    /// A failed action, awaiting review.
    FailedAction,
}

impl Category {
    /// Short label for the list.
    pub fn label(self) -> &'static str {
        match self {
            Category::Request => "request",
            Category::Denied => "denied",
            Category::FailedJob => "failed job",
            Category::FailedAction => "failed action",
        }
    }

    fn color(self) -> Color {
        match self {
            Category::Request => Color::Yellow,
            Category::Denied => Color::Red,
            Category::FailedJob | Category::FailedAction => Color::Magenta,
        }
    }
}

/// Which categories the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// Everything.
    #[default]
    All,
    /// Permission requests.
    Requests,
    /// Denials.
    Denied,
    /// Failed Jobs and actions.
    Failed,
}

impl Filter {
    const ALL: [Filter; 4] = [
        Filter::All,
        Filter::Requests,
        Filter::Denied,
        Filter::Failed,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    fn shifted(self, delta: isize) -> Self {
        let count = Self::ALL.len() as isize;
        Self::ALL[((self.index() as isize + delta).rem_euclid(count)) as usize]
    }

    fn shows(self, category: Category) -> bool {
        match self {
            Filter::All => true,
            Filter::Requests => category == Category::Request,
            Filter::Denied => category == Category::Denied,
            Filter::Failed => matches!(category, Category::FailedJob | Category::FailedAction),
        }
    }
}

/// One selectable inbox row.
#[derive(Debug, Clone)]
pub struct Item {
    /// Category.
    pub category: Category,
    /// The action, for action items.
    pub action: Option<ActionRun>,
    /// The Job, for failed Jobs.
    pub job: Option<Job>,
}

impl Item {
    /// ActionRun or Job ID.
    pub fn id(&self) -> &str {
        match (&self.action, &self.job) {
            (Some(action), _) => &action.action_run_id,
            (_, Some(job)) => &job.job_id,
            _ => "",
        }
    }

    /// The Issue it belongs to.
    pub fn issue_id(&self) -> &str {
        match (&self.action, &self.job) {
            (Some(action), _) => &action.issue_id,
            (_, Some(job)) => &job.issue_id,
            _ => "",
        }
    }

    /// What it is.
    pub fn title(&self) -> String {
        match (&self.action, &self.job) {
            (Some(action), _) => action.title(),
            (_, Some(job)) => format!("job {}", short(&job.job_id)),
            _ => String::new(),
        }
    }

    fn status(&self) -> &str {
        match (&self.action, &self.job) {
            (Some(action), _) => &action.status,
            (_, Some(job)) => &job.status,
            _ => "",
        }
    }

    fn when(&self) -> String {
        match (&self.action, &self.job) {
            (Some(action), _) => time(
                action
                    .denial
                    .as_ref()
                    .map_or(&action.created_at, |d| &d.decided_at),
            ),
            (_, Some(job)) => time(&job.created_at),
            _ => String::new(),
        }
    }
}

/// Inbox state.
#[derive(Debug, Default)]
pub struct InboxState {
    /// Which categories show.
    pub filter: Filter,
    /// Selection in the waiting list.
    pub list: ListState,
    /// Whether the history replaces the waiting list.
    pub history: bool,
    /// Selection in the history.
    pub history_list: ListState,
    /// Scroll of the detail pane.
    pub detail_scroll: usize,
}

/// The rows the filter shows: requests, then denials, then failures.
pub fn items(app: &App) -> Vec<Item> {
    let inbox = &app.inbox;
    let mut rows = Vec::new();
    let action = |category, a: &ActionRun| Item {
        category,
        action: Some(a.clone()),
        job: None,
    };
    rows.extend(
        inbox
            .permission_requests
            .iter()
            .map(|a| action(Category::Request, a)),
    );
    rows.extend(
        inbox
            .permission_denied
            .iter()
            .map(|a| action(Category::Denied, a)),
    );
    rows.extend(inbox.failed_jobs.iter().map(|job| Item {
        category: Category::FailedJob,
        action: None,
        job: Some(job.clone()),
    }));
    rows.extend(
        inbox
            .failed_actions
            .iter()
            .map(|a| action(Category::FailedAction, a)),
    );
    rows.retain(|item| app.inbox_view.filter.shows(item.category));
    rows
}

/// Every decided action, newest first: exactly what the inbox does not show.
pub fn history(app: &App) -> Vec<&ActionRun> {
    app.actions.iter().rev().filter(|a| !a.in_inbox()).collect()
}

/// The selected waiting item.
pub fn selected(app: &App) -> Option<Item> {
    let rows = items(app);
    app.inbox_view
        .list
        .selected()
        .and_then(|index| rows.get(index.min(rows.len().saturating_sub(1))).cloned())
        .or_else(|| rows.first().cloned())
}

/// Inbox keys.
pub fn key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> bool {
    let len = if app.inbox_view.history {
        history(app).len()
    } else {
        items(app).len()
    };
    let list = if app.inbox_view.history {
        &mut app.inbox_view.history_list
    } else {
        &mut app.inbox_view.list
    };
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            list.select(step(list.selected(), len, 1));
            app.inbox_view.detail_scroll = 0;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            list.select(step(list.selected(), len, -1));
            app.inbox_view.detail_scroll = 0;
        }
        KeyCode::Char('g') | KeyCode::Home => list.select(step(None, len, 0)),
        KeyCode::Char('G') | KeyCode::End => list.select(step(None, len, len as isize)),
        KeyCode::PageDown => app.inbox_view.detail_scroll += 5,
        KeyCode::PageUp => {
            app.inbox_view.detail_scroll = app.inbox_view.detail_scroll.saturating_sub(5);
        }
        KeyCode::Char(']') | KeyCode::Right => {
            app.inbox_view.filter = app.inbox_view.filter.shifted(1);
            app.inbox_view.list.select(Some(0));
        }
        KeyCode::Char('[') | KeyCode::Left => {
            app.inbox_view.filter = app.inbox_view.filter.shifted(-1);
            app.inbox_view.list.select(Some(0));
        }
        KeyCode::Char('h') => {
            app.inbox_view.history = !app.inbox_view.history;
            app.inbox_view.detail_scroll = 0;
        }
        KeyCode::Char('a') => approve(app),
        KeyCode::Char('r') => open_decision(app, Pending::Reject { id: String::new() }),
        KeyCode::Char('b') => open_decision(
            app,
            Pending::Review {
                id: String::new(),
                is_job: false,
                decision: "send_upstream",
            },
        ),
        KeyCode::Char('x') => open_decision(
            app,
            Pending::Review {
                id: String::new(),
                is_job: false,
                decision: "acknowledge",
            },
        ),
        KeyCode::Enter | KeyCode::Char('t') => {
            let target = if app.inbox_view.history {
                let rows = history(app);
                app.inbox_view
                    .history_list
                    .selected()
                    .and_then(|i| rows.get(i))
                    .map(|a| (a.issue_id.clone(), Some(a.originating_job_id.clone())))
            } else {
                selected(app).map(|item| {
                    (
                        item.issue_id().to_string(),
                        item.job
                            .as_ref()
                            .map(|j| j.job_id.clone())
                            .or_else(|| item.action.as_ref().map(|a| a.originating_job_id.clone())),
                    )
                })
            };
            match target {
                Some((issue, job)) if !issue.is_empty() => app.open_trace(issue, job),
                _ => app.message = Some("nothing selected".to_string()),
            }
        }
        _ => return false,
    }
    true
}

fn approve(app: &mut App) {
    let Some(item) = selected_waiting(app) else {
        return;
    };
    if item.category != Category::Request {
        app.message = Some(format!("cannot approve a {} item", item.category.label()));
        return;
    }
    app.approve(item.id().to_string(), item.title());
}

/// The selected waiting item, or a message explaining why there is none.
fn selected_waiting(app: &mut App) -> Option<Item> {
    if app.inbox_view.history {
        app.message = Some("decisions are made on the waiting list (h)".to_string());
        return None;
    }
    let item = selected(app);
    if item.is_none() {
        app.message = Some("no inbox item selected".to_string());
    }
    item
}

/// Opens the comment prompt for a decision, when it applies to the selected item.
fn open_decision(app: &mut App, pending: Pending) {
    let Some(item) = selected_waiting(app) else {
        return;
    };
    let id = item.id().to_string();
    let is_job = item.category == Category::FailedJob;
    let (pending, label) = match pending {
        Pending::Reject { .. } => {
            if item.category != Category::Request {
                app.message = Some(format!(
                    "reject does not apply to a {} item",
                    item.category.label()
                ));
                return;
            }
            (
                Pending::Reject { id },
                format!(
                    "reject {} — comment (the agent sees it if the denial is sent back):",
                    item.title()
                ),
            )
        }
        Pending::Review { decision, .. } => {
            if item.category == Category::Request {
                app.message = Some(format!(
                    "{} does not apply to a request: approve (a) or reject (r) it",
                    label(decision)
                ));
                return;
            }
            let what = if decision == "send_upstream" {
                if is_job {
                    "send back upstream — anything the next pass should know?"
                } else {
                    "send back upstream — what should the next pass do differently?"
                }
            } else {
                "acknowledge — comment (optional):"
            };
            (
                Pending::Review {
                    id,
                    is_job,
                    decision,
                },
                format!("{what} [{}]", item.title()),
            )
        }
        other => (other, String::new()),
    };
    app.open_prompt(pending, label, "");
}

/// Draws the Inbox.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Percentage(55),
        Constraint::Min(6),
    ])
    .split(area);
    draw_tiles(frame, rows[0], app);
    if app.inbox_view.history {
        draw_history(frame, rows[1], rows[2], app);
    } else {
        draw_waiting(frame, rows[1], rows[2], app);
    }
}

fn draw_tiles(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::horizontal([Constraint::Percentage(34); 3]).split(area);
    let inbox = &app.inbox;
    let failed = inbox.failed_jobs.len() + inbox.failed_actions.len();
    let tiles = [
        (
            "Permission requests",
            inbox.permission_requests.len(),
            "approve, or reject with a comment".to_string(),
            Color::Yellow,
        ),
        (
            "Permission denied",
            inbox.permission_denied.len(),
            "review the reason; send upstream or acknowledge".to_string(),
            Color::Red,
        ),
        (
            "Failed",
            failed,
            format!(
                "{} jobs · {} actions",
                inbox.failed_jobs.len(),
                inbox.failed_actions.len()
            ),
            Color::Magenta,
        ),
    ];
    for (column, (title, count, hint, color)) in columns.iter().zip(tiles) {
        let title_line = vec![Span::raw(format!(" {title} "))];
        let value_style = Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(if count > 0 { color } else { Color::Reset });
        let text = Text::from(vec![
            Line::from(Span::styled(count.to_string(), value_style)),
            Line::from(dim(hint)),
        ]);
        frame.render_widget(
            Paragraph::new(text).block(panel(Line::from(title_line))),
            *column,
        );
    }
}

fn draw_waiting(frame: &mut Frame, list_area: Rect, detail_area: Rect, app: &mut App) {
    let rows = items(app);
    if rows.is_empty() {
        app.inbox_view.list.select(None);
    } else if app
        .inbox_view
        .list
        .selected()
        .is_none_or(|i| i >= rows.len())
    {
        app.inbox_view.list.select(Some(
            rows.len()
                .saturating_sub(1)
                .min(app.inbox_view.list.selected().unwrap_or(0)),
        ));
    }
    let width = inner_width(list_area);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|item| {
            let (badge_text, badge_color) = match item.category {
                Category::Request => ("needs approval".to_string(), Color::Yellow),
                Category::Denied => (
                    format!(
                        "denied by {}",
                        item.action
                            .as_ref()
                            .and_then(|a| a.denial.as_ref())
                            .map_or("?".to_string(), |d| d.who())
                    ),
                    Color::Red,
                ),
                Category::FailedJob => ("job failed".to_string(), Color::Magenta),
                Category::FailedAction => (
                    item.action
                        .as_ref()
                        .and_then(ActionRun::evidence)
                        .unwrap_or_else(|| "no evidence".to_string()),
                    Color::Magenta,
                ),
            };
            let title_width = width.saturating_sub(14 + 22 + 26 + 10).max(10);
            ListItem::new(Line::from(vec![
                Span::styled(
                    pad(item.category.label(), 14),
                    Style::default()
                        .fg(item.category.color())
                        .add_modifier(Modifier::BOLD),
                ),
                bold(pad(&item.title(), title_width)),
                Span::raw(" "),
                Span::styled(
                    pad(&label(item.status()), 22),
                    Style::default().fg(crate::format::tone(item.status())),
                ),
                badge(&crate::format::truncate(&badge_text, 24), badge_color),
                Span::raw(" "),
                dim(item.when()),
            ]))
        })
        .collect();
    let inbox = &app.inbox;
    let failed = inbox.failed_jobs.len() + inbox.failed_actions.len();
    let total = inbox.total();
    let mut title = vec![Span::raw(format!(
        " Waiting for you · {} ",
        if total == 0 {
            "nothing waits for a human".to_string()
        } else {
            format!("{total} item(s)")
        }
    ))];
    title.extend(segmented(
        &[
            ("All".to_string(), total),
            ("Requests".to_string(), inbox.permission_requests.len()),
            ("Denied".to_string(), inbox.permission_denied.len()),
            ("Failed".to_string(), failed),
        ],
        app.inbox_view.filter.index(),
    ));
    title.push(Span::raw(" h history "));
    title.push(match app.status.as_ref().map(|s| s.dry_run) {
        Some(true) => badge(
            "dry-run: approved actions are rendered, not executed",
            Color::Yellow,
        ),
        Some(false) => badge("Platform LIVE", Color::Red),
        None => Span::raw(""),
    });
    let list = List::new(items)
        .block(panel(Line::from(title)))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, list_area, &mut app.inbox_view.list);

    let mut doc = Doc::new(inner_width(detail_area));
    match selected(app) {
        None if total == 0 => {
            doc.heading("Inbox zero");
            doc.note("Denials and failures land here with their reasons; permission requests with the Team's reasoning.");
        }
        None => doc.note("Nothing matches this filter."),
        Some(item) => item_doc(&item, &mut doc),
    }
    render_doc(
        frame,
        detail_area,
        &doc,
        &mut app.inbox_view.detail_scroll,
        panel(" Detail "),
    );
}

/// The web console's card for one item, as a document.
fn item_doc(item: &Item, doc: &mut Doc) {
    match (item.category, &item.action, &item.job) {
        (Category::Request, Some(a), _) => {
            doc.line(vec![
                bold(a.title()),
                Span::raw(" "),
                badge("needs approval", Color::Yellow),
                sep(),
                dim(time(&a.created_at)),
            ]);
            doc.kv("Why", &a.reason);
            doc.kv("Expected effect", &a.expected_effect);
            doc.kv(
                "Verification",
                "every target must be Healthy in the after-Snapshot; a dry run passes as dry-run evidence only",
            );
            doc.blank();
            doc.note("a approve and run · r reject with a comment");
        }
        (Category::Denied, Some(a), _) => {
            let denial = a.denial.as_ref();
            doc.line(vec![
                bold(a.title()),
                Span::raw(" "),
                badge(
                    &format!("denied by {}", denial.map_or("?".to_string(), |d| d.who())),
                    Color::Red,
                ),
                sep(),
                dim(denial.map_or(String::new(), |d| time(&d.decided_at))),
            ]);
            doc.kv("Proposed because", &a.reason);
            doc.kv("Denial reason", denial.map_or("—", |d| d.reason.as_str()));
            if let Some(comment) = denial.and_then(|d| d.comment.as_deref()) {
                doc.kv("Comment", comment);
            }
            doc.blank();
            doc.note("b send back upstream (a revising pass runs now with the reason and your comment) · x acknowledge");
        }
        (Category::FailedJob, _, Some(job)) => {
            doc.line(vec![
                bold(format!("job {}", short(&job.job_id))),
                Span::raw(" "),
                badge("job failed", Color::Magenta),
                sep(),
                dim(time(&job.created_at)),
                sep(),
                dim(format!("issue {}", short(&job.issue_id))),
            ]);
            doc.text(
                job.result
                    .as_ref()
                    .map_or("no result was recorded", |r| r.summary.as_str()),
            );
            if let Some(result) = &job.result {
                for id in &result.artifact_ids {
                    doc.note(&format!("transcript {} (open the trace: Enter)", short(id)));
                }
            }
            doc.blank();
            doc.note("b send back upstream · x acknowledge · Enter open the trace");
        }
        (Category::FailedAction, Some(a), _) => {
            let mut line = vec![bold(a.title()), Span::raw(" "), status_span(&a.status)];
            if let Some(evidence) = a.evidence() {
                line.push(Span::raw(" "));
                line.push(badge(
                    &evidence,
                    if a.verification_evidence.as_deref() == Some("strong") {
                        Color::Green
                    } else {
                        Color::Yellow
                    },
                ));
            }
            doc.line(line);
            doc.kv("Proposed because", &a.reason);
            doc.kv("Execution", a.execution_summary.as_deref().unwrap_or("—"));
            doc.kv(
                "Verification",
                a.verification_summary.as_deref().unwrap_or("not reached"),
            );
            doc.blank();
            doc.note("b send back upstream · x acknowledge");
        }
        _ => {}
    }
}

fn draw_history(frame: &mut Frame, list_area: Rect, detail_area: Rect, app: &mut App) {
    let rows: Vec<ActionRun> = history(app).into_iter().cloned().collect();
    if rows.is_empty() {
        app.inbox_view.history_list.select(None);
    } else if app
        .inbox_view
        .history_list
        .selected()
        .is_none_or(|i| i >= rows.len())
    {
        app.inbox_view.history_list.select(Some(0));
    }
    let width = inner_width(list_area);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|a| {
            let outcome = a.denial.as_ref().map_or_else(
                || {
                    a.verification_summary
                        .clone()
                        .or_else(|| a.execution_summary.clone())
                        .unwrap_or_else(|| "—".to_string())
                },
                |d| {
                    format!(
                        "{}{}",
                        d.reason,
                        d.comment
                            .as_deref()
                            .map(|c| format!(" — {c}"))
                            .unwrap_or_default()
                    )
                },
            );
            let approval = format!(
                "{}{}",
                label(&a.approval),
                a.approved_by
                    .as_deref()
                    .map(|who| format!(" by {who}"))
                    .unwrap_or_default()
            );
            let review = a.review.as_ref().map_or("—".to_string(), |r| {
                format!("{}: {}", r.reviewer, r.phrase())
            });
            let rest = width.saturating_sub(34 + 20 + 22 + 4);
            ListItem::new(Line::from(vec![
                bold(pad(&a.title(), 34)),
                Span::raw(" "),
                Span::styled(
                    pad(&label(&a.status), 20),
                    Style::default().fg(crate::format::tone(&a.status)),
                ),
                dim(pad(&approval, 22)),
                Span::raw(" "),
                dim(pad(&outcome, rest / 2)),
                Span::raw(" "),
                dim(pad(&review, rest.saturating_sub(rest / 2))),
            ]))
        })
        .collect();
    let title = format!(
        " History · every decided action, newest first · {} · h back to the inbox ",
        rows.len()
    );
    let list = List::new(items)
        .block(panel(title))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, list_area, &mut app.inbox_view.history_list);

    let mut doc = Doc::new(inner_width(detail_area));
    match app
        .inbox_view
        .history_list
        .selected()
        .and_then(|i| rows.get(i))
    {
        None => doc.note("No decided actions yet."),
        Some(a) => {
            let mut line = vec![bold(a.title()), Span::raw(" "), status_span(&a.status)];
            if a.dry_run {
                line.push(Span::raw(" "));
                line.push(badge("dry-run", Color::DarkGray));
            }
            if let Some(evidence) = a.evidence() {
                line.push(Span::raw(" "));
                line.push(badge(
                    &evidence,
                    if a.verification_evidence.as_deref() == Some("strong") {
                        Color::Green
                    } else {
                        Color::Yellow
                    },
                ));
            }
            line.push(sep());
            line.push(dim(time(&a.created_at)));
            doc.line(line);
            doc.kv(
                "Approval",
                &format!(
                    "{}{}",
                    label(&a.approval),
                    a.approved_by
                        .as_deref()
                        .map(|who| format!(" by {who}"))
                        .unwrap_or_default()
                ),
            );
            doc.kv("Proposed because", &a.reason);
            doc.kv("Expected effect", &a.expected_effect);
            if let Some(denial) = &a.denial {
                doc.kv(
                    &format!("Denied by {}", denial.who()),
                    &format!(
                        "{}{}",
                        denial.reason,
                        denial
                            .comment
                            .as_deref()
                            .map(|c| format!(" — {c}"))
                            .unwrap_or_default()
                    ),
                );
            }
            if let Some(summary) = &a.execution_summary {
                doc.kv("Execution", summary);
            }
            if let Some(summary) = &a.verification_summary {
                doc.kv("Verification", summary);
            }
            if let Some(review) = &a.review {
                doc.kv(
                    "Review",
                    &format!(
                        "{}: {}{}",
                        review.reviewer,
                        review.phrase(),
                        review
                            .comment
                            .as_deref()
                            .map(|c| format!(" · “{c}”"))
                            .unwrap_or_default()
                    ),
                );
            }
            doc.blank();
            doc.note("Enter open the trace of its Issue");
        }
    }
    render_doc(
        frame,
        detail_area,
        &doc,
        &mut app.inbox_view.detail_scroll,
        panel(" Detail "),
    );
}
