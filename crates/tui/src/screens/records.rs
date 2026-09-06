//! Issues & jobs: each Issue with every pass a Team made on it, filtered and searched, with
//! closing, export, and import.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::api::{Issue, Job};
use crate::app::{App, Pending};
use crate::format::{date_time, label, pad, short, time};
use crate::screens::step;
use crate::ui::{
    Doc, badge, bold, dim, inner_width, panel, render_doc, segmented, sep, status_span,
};

/// Footer hints.
pub const HINTS: &str = "j/k select · [/] filter · / search · Enter/t trace · e export · i import · R resolve · C cancel";

/// Which Issues the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// Everything.
    #[default]
    All,
    /// Work outstanding.
    Live,
    /// Closed.
    Closed,
    /// Imported archives.
    Archived,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::Live, Filter::Closed, Filter::Archived];

    fn index(self) -> usize {
        Self::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    fn shifted(self, delta: isize) -> Self {
        let count = Self::ALL.len() as isize;
        Self::ALL[((self.index() as isize + delta).rem_euclid(count)) as usize]
    }

    fn shows(self, issue: &Issue) -> bool {
        let archived = issue.provenance.is_some();
        match self {
            Filter::All => true,
            Filter::Live => issue.is_live(),
            Filter::Closed => !archived && !issue.is_live(),
            Filter::Archived => archived,
        }
    }
}

/// Issues & jobs state.
#[derive(Debug, Default)]
pub struct RecordsState {
    /// Which Issues show.
    pub filter: Filter,
    /// Search text; empty means all.
    pub query: String,
    /// Selection.
    pub list: ListState,
    /// Scroll of the detail pane.
    pub detail_scroll: usize,
}

/// The Issues the filter and search show, newest first.
pub fn visible(app: &App) -> Vec<Issue> {
    let needle = app.records.query.trim().to_lowercase();
    app.issues
        .iter()
        .rev()
        .filter(|issue| app.records.filter.shows(issue))
        .filter(|issue| {
            needle.is_empty()
                || issue.title.to_lowercase().contains(&needle)
                || issue.description.to_lowercase().contains(&needle)
                || issue.issue_id.starts_with(&needle)
        })
        .cloned()
        .collect()
}

/// The selected Issue.
pub fn selected(app: &App) -> Option<Issue> {
    let rows = visible(app);
    app.records
        .list
        .selected()
        .and_then(|i| rows.get(i).cloned())
        .or_else(|| rows.first().cloned())
}

/// The passes on an Issue, oldest first.
pub fn jobs_of<'a>(app: &'a App, issue_id: &str) -> Vec<&'a Job> {
    let mut jobs: Vec<&Job> = app.jobs.iter().filter(|j| j.issue_id == issue_id).collect();
    jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    jobs
}

/// Issues & jobs keys.
pub fn key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> bool {
    let len = visible(app).len();
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.records
                .list
                .select(step(app.records.list.selected(), len, 1));
            app.records.detail_scroll = 0;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.records
                .list
                .select(step(app.records.list.selected(), len, -1));
            app.records.detail_scroll = 0;
        }
        KeyCode::Char('g') | KeyCode::Home => app.records.list.select(step(None, len, 0)),
        KeyCode::Char('G') | KeyCode::End => app.records.list.select(step(None, len, len as isize)),
        KeyCode::PageDown => app.records.detail_scroll += 5,
        KeyCode::PageUp => app.records.detail_scroll = app.records.detail_scroll.saturating_sub(5),
        KeyCode::Char(']') | KeyCode::Right => {
            app.records.filter = app.records.filter.shifted(1);
            app.records.list.select(Some(0));
        }
        KeyCode::Char('[') | KeyCode::Left => {
            app.records.filter = app.records.filter.shifted(-1);
            app.records.list.select(Some(0));
        }
        KeyCode::Char('/') => {
            let query = app.records.query.clone();
            app.open_prompt(
                Pending::Search,
                "search title, description, or ID (empty shows all):",
                query,
            );
        }
        KeyCode::Enter | KeyCode::Char('t') => match selected(app) {
            Some(issue) => app.open_trace(issue.issue_id, None),
            None => app.message = Some("no issue selected".to_string()),
        },
        KeyCode::Char('e') => match selected(app) {
            Some(issue) => {
                let path = format!(
                    "session-{}.json",
                    &issue.issue_id[..issue.issue_id.len().min(8)]
                );
                app.open_prompt(
                    Pending::Export {
                        issue_id: issue.issue_id,
                    },
                    "export the session (every pass, transcript, action, Snapshot, and event) to:",
                    path,
                );
            }
            None => app.message = Some("no issue selected".to_string()),
        },
        KeyCode::Char('i') => {
            app.open_prompt(
                Pending::Import,
                "import a session file as a read-only archive from:",
                "",
            );
        }
        KeyCode::Char('R') => close(app, "resolved"),
        KeyCode::Char('C') => close(app, "cancelled"),
        _ => return false,
    }
    true
}

/// Opens the closing-comment prompt for the selected Issue, when it is still live.
fn close(app: &mut App, outcome: &'static str) {
    let Some(issue) = selected(app) else {
        app.message = Some("no issue selected".to_string());
        return;
    };
    if issue.provenance.is_some() {
        app.message = Some("an archive is read-only: no decision here can touch it".to_string());
        return;
    }
    if !issue.is_live() {
        app.message = Some(format!("issue is already {}", label(&issue.status)));
        return;
    }
    let what = if outcome == "resolved" {
        "resolve (the problem is fixed or was not a problem) — closing comment:"
    } else {
        "cancel (stop working on it without claiming it is fixed) — closing comment:"
    };
    let title = issue.title.clone();
    app.open_prompt(
        Pending::CloseIssue {
            id: issue.issue_id,
            title: title.clone(),
            outcome,
        },
        format!("{what} [{title}]"),
        "",
    );
}

/// Draws Issues & jobs.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::vertical([Constraint::Percentage(45), Constraint::Min(6)]).split(area);
    let issues = visible(app);
    if issues.is_empty() {
        app.records.list.select(None);
    } else if app
        .records
        .list
        .selected()
        .is_none_or(|i| i >= issues.len())
    {
        app.records.list.select(Some(0));
    }
    let width = inner_width(rows[0]);
    let items: Vec<ListItem> = issues
        .iter()
        .map(|issue| {
            let title_width = width.saturating_sub(12 + 20 + 10 + 22 + 6).max(12);
            let mut spans = vec![
                bold(pad(&issue.title, title_width)),
                Span::raw(" "),
                dim(pad(&label(&issue.priority), 11)),
                Span::styled(
                    pad(&label(&issue.status), 19),
                    Style::default().fg(crate::format::tone(&issue.status)),
                ),
            ];
            spans.push(if issue.provenance.is_some() {
                badge("archive", Color::Yellow)
            } else {
                Span::raw(pad("", 9))
            });
            spans.push(Span::raw(" "));
            spans.push(dim(date_time(&issue.created_at)));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let counts = (
        app.issues.len(),
        app.issues.iter().filter(|i| i.is_live()).count(),
        app.issues
            .iter()
            .filter(|i| i.provenance.is_none() && !i.is_live())
            .count(),
        app.issues.iter().filter(|i| i.provenance.is_some()).count(),
    );
    let mut title = vec![Span::raw(" Issues & jobs ")];
    title.extend(segmented(
        &[
            ("All".to_string(), counts.0),
            ("Open".to_string(), counts.1),
            ("Closed".to_string(), counts.2),
            ("Archives".to_string(), counts.3),
        ],
        app.records.filter.index(),
    ));
    if !app.records.query.is_empty() {
        title.push(Span::raw(format!(" search “{}” ", app.records.query)));
    }
    let list = List::new(items)
        .block(panel(Line::from(title)))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, rows[0], &mut app.records.list);

    let mut doc = Doc::new(inner_width(rows[1]));
    match selected(app) {
        None if app.issues.is_empty() => {
            doc.heading("No issues filed yet");
            doc.note("File a report (6) to open one, or import a session file (i).");
        }
        None => doc.note("Nothing matches."),
        Some(issue) => issue_doc(app, &issue, &mut doc),
    }
    render_doc(
        frame,
        rows[1],
        &doc,
        &mut app.records.detail_scroll,
        panel(" Issue "),
    );
}

/// The web console's Issue card, as a document.
fn issue_doc(app: &App, issue: &Issue, doc: &mut Doc) {
    let mut head = vec![
        bold(issue.title.clone()),
        Span::raw(" "),
        badge(&label(&issue.priority), Color::DarkGray),
        Span::raw(" "),
        status_span(&issue.status),
    ];
    if issue.provenance.is_some() {
        head.push(Span::raw(" "));
        head.push(badge("archive", Color::Yellow));
    }
    head.push(sep());
    head.push(dim(date_time(&issue.created_at)));
    head.push(sep());
    head.push(dim(issue.issue_id.clone()));
    doc.line(head);
    doc.text(&issue.description);
    if !issue.affected_resource_ids.is_empty() {
        doc.kv("Affected", &issue.affected_resource_ids.join(", "));
    }
    if let Some(archive) = &issue.provenance {
        doc.styled(
            &format!(
                "Imported from {} · exported by {} {} · imported by {} {}. Read-only: no decision here can touch it.",
                archive.source_deployment,
                archive.exported_by,
                date_time(&archive.exported_at),
                archive.imported_by,
                date_time(&archive.imported_at)
            ),
            Style::default().fg(Color::Yellow),
        );
    }
    doc.blank();
    let jobs = jobs_of(app, &issue.issue_id);
    if jobs.is_empty() {
        doc.note("No Job yet.");
    }
    for (index, job) in jobs.iter().enumerate() {
        let mut line = vec![
            bold(format!("job {}", short(&job.job_id))),
            sep(),
            Span::raw(label(&job.team_kind)),
            sep(),
            status_span(&job.status),
        ];
        if let Some(result) = &job.result {
            line.push(sep());
            line.push(Span::raw(label(&result.outcome)));
        }
        if index > 0 || !job.earlier_passes.is_empty() {
            line.push(sep());
            line.push(dim(format!("pass {}", index + 1)));
        }
        if let Some(id) = &job.revises_job_id {
            line.push(sep());
            line.push(dim(format!("revises {}", short(id))));
        }
        if let Some(id) = &job.supersedes_job_id {
            line.push(sep());
            line.push(dim(format!("supersedes {}", short(id))));
        }
        if let Some(id) = &job.continues_job_id {
            line.push(sep());
            line.push(dim(format!("follows {}", short(id))));
        }
        if let Some(review) = &job.review {
            line.push(sep());
            line.push(dim(format!(
                "reviewed by {}: {}",
                review.reviewer,
                review.phrase()
            )));
        }
        line.push(sep());
        line.push(dim(time(&job.created_at)));
        doc.line(line);
        for feedback in &job.feedback {
            doc.bullet_styled(&feedback.phrase(), Style::default().fg(Color::Cyan));
        }
        match &job.result {
            Some(result) => {
                doc.indented(2, &result.summary, Style::default());
                for question in &result.unresolved_questions {
                    doc.indented(
                        4,
                        &format!("? {question}"),
                        Style::default().fg(Color::DarkGray),
                    );
                }
                if !result.proposed_actions.is_empty() {
                    let proposed: Vec<String> = result
                        .proposed_actions
                        .iter()
                        .map(|p| format!("{} on {}", p.runbook_id, p.target_ids.join(",")))
                        .collect();
                    doc.indented(
                        2,
                        &format!("proposed: {}", proposed.join("; ")),
                        Style::default().fg(Color::DarkGray),
                    );
                }
                if !result.artifact_ids.is_empty() {
                    let ids: Vec<String> = result.artifact_ids.iter().map(|id| short(id)).collect();
                    doc.indented(
                        2,
                        &format!("transcript {}", ids.join(", ")),
                        Style::default().fg(Color::DarkGray),
                    );
                }
            }
            None if job.is_live() => doc.indented(2, "running…", Style::default().fg(Color::Green)),
            None => doc.indented(
                2,
                "no result was recorded",
                Style::default().fg(Color::DarkGray),
            ),
        }
    }
    doc.blank();
    let mut keys = "Enter open the trace · e export the session".to_string();
    if issue.is_live() {
        keys.push_str(" · R resolve · C cancel");
    }
    doc.note(&keys);
}
