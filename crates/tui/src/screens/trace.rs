//! Trace: every pass on one Issue as it happened — what the model was given, what it did,
//! and what the Scheduler and the humans decided in between. Live while a pass runs: forwarded
//! transcript entries feed the transcript directly, and the stored transcript replaces them
//! when the pass ends.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Tabs};
use serde_json::Value;

use crate::api::{
    ActionRun, EventRecord, Job, SessionBundle, TraceStep, Transcript, TranscriptEntry, TurnRecord,
};
use crate::app::{App, Pending};
use crate::format::{
    actor_color, clamp, date_time, duration_ms, label, millis, pretty, short, thousands, time, wrap,
};
use crate::ui::{Doc, badge, bold, dim, inner_width, panel, render_doc, sep, status_span};

/// Footer hints.
pub const HINTS: &str = "←/→ pass · j/k entry · Enter expand · v View · I instructions · J/K side pane · e export · Esc back";

/// Lines of an entry shown before it is folded.
const FOLDED_LINES: usize = 12;

/// Trace state.
#[derive(Debug, Default)]
pub struct TraceState {
    /// The traced Issue; absent until one is opened.
    pub issue_id: Option<String>,
    /// The session, once loaded.
    pub bundle: Option<SessionBundle>,
    /// Why it could not be loaded.
    pub error: Option<String>,
    /// Whether a load is in flight.
    pub loading: bool,
    /// The selected pass; the newest when absent.
    pub picked: Option<String>,
    /// Forwarded transcript entries per running Job.
    pub steps: HashMap<String, Vec<TraceStep>>,
    /// Events that arrived after the session was read.
    pub live_events: Vec<EventRecord>,
    /// The newest sequence seen on this Issue.
    pub last_seq: u64,
    /// A record changed since the session was read.
    pub dirty: bool,
    /// Whether an event read is in flight.
    pub polling: bool,
    /// When events were last read.
    pub last_poll: Option<Instant>,
    /// Whether the last read succeeded.
    pub connected: bool,
    /// Selected transcript row.
    pub selected: usize,
    /// Transcript scroll, in lines.
    pub scroll: usize,
    /// Entry indexes shown unfolded.
    pub expanded: HashSet<usize>,
    /// Whether the Snapshot View is shown.
    pub show_view: bool,
    /// Whether the instructions are shown.
    pub show_instructions: bool,
    /// Scroll of the result / actions / events pane.
    pub side_scroll: usize,
    /// Keep the newest entry selected while the pass runs.
    pub follow: bool,
}

impl TraceState {
    /// A trace of an Issue, not loaded yet.
    pub fn new(issue_id: String) -> Self {
        Self {
            issue_id: Some(issue_id),
            dirty: true,
            follow: true,
            ..Self::default()
        }
    }

    /// Forgets the selection and folds, for a different pass.
    pub fn reset_view(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.expanded.clear();
        self.side_scroll = 0;
        self.follow = true;
    }
}

/// Starts the loads and polls the trace needs.
pub fn tick(app: &mut App) {
    let Some(issue_id) = app.trace.issue_id.clone() else {
        return;
    };
    if app.trace.loading {
        return;
    }
    if app.trace.bundle.is_none() || app.trace.dirty {
        app.spawn_session(issue_id);
        return;
    }
    let due = app
        .trace
        .last_poll
        .is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
    if !app.trace.polling && due {
        app.spawn_trace_events(issue_id);
    }
}

/// The session loaded (or not).
pub fn on_session(app: &mut App, issue_id: String, result: Result<SessionBundle, String>) {
    if app.trace.issue_id.as_deref() != Some(issue_id.as_str()) {
        return;
    }
    app.trace.loading = false;
    match result {
        Ok(bundle) => {
            let newest = bundle.events.iter().map(|e| e.sequence).max().unwrap_or(0);
            app.trace.last_seq = app.trace.last_seq.max(newest);
            app.trace.bundle = Some(bundle);
            app.trace.dirty = false;
            app.trace.error = None;
            app.trace.connected = true;
        }
        Err(error) => {
            app.trace.error = Some(error);
            app.trace.dirty = false;
        }
    }
}

/// New events on the traced Issue: steps feed the live transcript; anything else means a
/// record changed, so the session is re-read.
pub fn on_events(app: &mut App, issue_id: String, events: Vec<EventRecord>) {
    if app.trace.issue_id.as_deref() != Some(issue_id.as_str()) {
        return;
    }
    app.trace.polling = false;
    for event in events {
        if event.sequence <= app.trace.last_seq {
            continue;
        }
        app.trace.last_seq = event.sequence;
        match (event.step(), &event.job_id) {
            (Some(step), Some(job_id)) => {
                app.trace
                    .steps
                    .entry(job_id.clone())
                    .or_default()
                    .push(step);
            }
            _ => {
                app.trace.live_events.push(event);
                app.trace.dirty = true;
            }
        }
    }
}

/* ---- reading the bundle ---- */

/// The passes, oldest first.
fn jobs(bundle: &SessionBundle) -> Vec<&Job> {
    let mut jobs: Vec<&Job> = bundle.jobs.iter().collect();
    jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    jobs
}

fn selected_index(state: &TraceState, jobs: &[&Job]) -> Option<usize> {
    if jobs.is_empty() {
        return None;
    }
    state
        .picked
        .as_deref()
        .and_then(|picked| jobs.iter().position(|job| job.job_id == picked))
        .or(Some(jobs.len() - 1))
}

/// The pass's transcript: a model-backed pass stores one as a DiagnosticBundle; a deterministic
/// pass has none, and a bundle whose body is missing or not JSON is gone.
enum Source {
    Stored(Transcript),
    Gone,
    None,
}

fn transcript_of(bundle: &SessionBundle, job: &Job) -> Source {
    let ids = job
        .result
        .as_ref()
        .map_or(&[][..], |r| r.artifact_ids.as_slice());
    let mut any = false;
    for artifact in bundle
        .artifacts
        .iter()
        .filter(|a| ids.contains(&a.artifact.artifact_id) && a.artifact.kind == "diagnostic_bundle")
    {
        any = true;
        if artifact.body.encoding == "json"
            && artifact.body.content.get("entries").is_some()
            && let Ok(transcript) =
                serde_json::from_value::<Transcript>(artifact.body.content.clone())
        {
            return Source::Stored(transcript);
        }
    }
    if any { Source::Gone } else { Source::None }
}

fn view_of<'a>(bundle: &'a SessionBundle, job: &Job) -> Option<&'a Value> {
    bundle
        .artifacts
        .iter()
        .find(|a| a.artifact.artifact_id == job.snapshot_view.artifact_id)
        .filter(|a| a.body.encoding == "json")
        .map(|a| &a.body.content)
}

fn actions_of<'a>(bundle: &'a SessionBundle, job: &Job) -> Vec<&'a ActionRun> {
    bundle
        .action_runs
        .iter()
        .filter(|a| a.originating_job_id == job.job_id)
        .collect()
}

/// Every event on the Issue but the forwarded steps, in order, once each.
fn events(state: &TraceState) -> Vec<EventRecord> {
    let mut seen = HashSet::new();
    let mut all: Vec<EventRecord> = state
        .bundle
        .iter()
        .flat_map(|b| b.events.iter())
        .chain(state.live_events.iter())
        .filter(|e| e.kind != "team.step" && seen.insert(e.sequence))
        .cloned()
        .collect();
    all.sort_by_key(|e| e.sequence);
    all
}

/// When the Team was last heard from on a Job: humans may review it long after it ended.
fn end_of(events: &[EventRecord], job: &Job) -> Option<i64> {
    events
        .iter()
        .rev()
        .find(|e| {
            e.job_id.as_deref() == Some(job.job_id.as_str())
                && (e.kind.starts_with("team.") || e.kind == "model.usage")
        })
        .and_then(|e| millis(&e.occurred_at))
}

/* ---- rows ---- */

/// One transcript row: an entry, its paired output when it is a tool call, and its timing.
#[derive(Debug, Clone)]
pub struct Row {
    /// Position in the transcript.
    pub index: usize,
    /// The entry.
    pub entry: TranscriptEntry,
    /// The output that answered a tool call, with its position.
    pub output: Option<(usize, TranscriptEntry)>,
    /// Set on the first entry of a model turn, when the stored transcript has turn records.
    pub turn: Option<TurnRecord>,
    /// Wall time this row covers: a tool from call to output, anything else until the next entry.
    pub duration: i64,
}

/// Pairs each tool call with its output and computes what every row took.
pub fn build_rows(
    entries: &[TranscriptEntry],
    turns: &[TurnRecord],
    end_at: Option<i64>,
    now_ms: i64,
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut open: HashMap<String, usize> = HashMap::new();
    let turn_at: HashMap<usize, &TurnRecord> = turns.iter().map(|t| (t.first_entry, t)).collect();
    for (index, entry) in entries.iter().enumerate() {
        let item = &entry.item;
        let kind = item["type"].as_str().unwrap_or("");
        let call_id = item["call_id"].as_str().unwrap_or("").to_string();
        if kind == "tool_output"
            && let Some(at) = open.remove(&call_id)
        {
            rows[at].output = Some((index, entry.clone()));
            continue;
        }
        if kind == "tool_call" {
            open.insert(call_id, rows.len());
        }
        rows.push(Row {
            index,
            entry: entry.clone(),
            output: None,
            turn: turn_at.get(&index).map(|t| (*t).clone()),
            duration: 0,
        });
    }
    for i in 0..rows.len() {
        let start = millis(&rows[i].entry.at).unwrap_or(now_ms);
        let next = rows
            .get(i + 1)
            .and_then(|r| millis(&r.entry.at))
            .unwrap_or(end_at.unwrap_or(now_ms));
        rows[i].duration = match &rows[i].output {
            Some((_, output)) => millis(&output.at).unwrap_or(start) - start,
            None => next - start,
        };
    }
    rows
}

/// What the selected pass's transcript is made of.
struct PassView {
    entries: Vec<TranscriptEntry>,
    stored: Option<Transcript>,
    gone: bool,
    truncated: HashSet<usize>,
}

fn pass_view(state: &TraceState, bundle: &SessionBundle, job: &Job) -> PassView {
    match transcript_of(bundle, job) {
        Source::Stored(transcript) => PassView {
            entries: transcript.entries.clone(),
            stored: Some(transcript),
            gone: false,
            truncated: HashSet::new(),
        },
        source => {
            let mut steps: Vec<&TraceStep> = state
                .steps
                .get(&job.job_id)
                .map(|s| s.iter().collect())
                .unwrap_or_default();
            steps.sort_by_key(|s| s.index);
            steps.dedup_by_key(|s| s.index);
            PassView {
                entries: steps
                    .iter()
                    .map(|s| TranscriptEntry {
                        at: s.at.clone(),
                        item: s.item.clone(),
                    })
                    .collect(),
                stored: None,
                gone: matches!(source, Source::Gone),
                truncated: steps
                    .iter()
                    .filter(|s| s.truncated)
                    .map(|s| s.index)
                    .collect(),
            }
        }
    }
}

/// How many rows the selected pass's transcript has, for the keys.
fn row_count(app: &App) -> usize {
    let Some(bundle) = &app.trace.bundle else {
        return 0;
    };
    let jobs = jobs(bundle);
    let Some(index) = selected_index(&app.trace, &jobs) else {
        return 0;
    };
    let view = pass_view(&app.trace, bundle, jobs[index]);
    let turns = view.stored.as_ref().map_or(&[][..], |t| t.turns.as_slice());
    build_rows(&view.entries, turns, None, 0).len()
}

/* ---- keys ---- */

/// Trace keys.
pub fn key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> bool {
    let rows = row_count(app);
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if app.trace.selected + 1 < rows {
                app.trace.selected += 1;
            }
            app.trace.follow = app.trace.selected + 1 >= rows;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.trace.selected = app.trace.selected.saturating_sub(1);
            app.trace.follow = false;
        }
        KeyCode::PageDown => {
            app.trace.selected = (app.trace.selected + 5).min(rows.saturating_sub(1));
            app.trace.follow = app.trace.selected + 1 >= rows;
        }
        KeyCode::PageUp => {
            app.trace.selected = app.trace.selected.saturating_sub(5);
            app.trace.follow = false;
        }
        KeyCode::Char('g') | KeyCode::Home => {
            app.trace.selected = 0;
            app.trace.follow = false;
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.trace.selected = rows.saturating_sub(1);
            app.trace.follow = true;
        }
        KeyCode::Enter => {
            let index = selected_entry_index(app);
            if let Some(index) = index
                && !app.trace.expanded.remove(&index)
            {
                app.trace.expanded.insert(index);
            }
        }
        KeyCode::Left | KeyCode::Char('h') => pick(app, -1),
        KeyCode::Right | KeyCode::Char('l') => pick(app, 1),
        KeyCode::Char('v') => app.trace.show_view = !app.trace.show_view,
        KeyCode::Char('I') => app.trace.show_instructions = !app.trace.show_instructions,
        KeyCode::Char('J') => app.trace.side_scroll += 3,
        KeyCode::Char('K') => app.trace.side_scroll = app.trace.side_scroll.saturating_sub(3),
        KeyCode::Backspace => app.set_screen(crate::app::Screen::Records),
        KeyCode::Char('e') => match app.trace.issue_id.clone() {
            Some(issue_id) => {
                let path = format!("session-{}.json", &issue_id[..issue_id.len().min(8)]);
                app.open_prompt(
                    Pending::Export { issue_id },
                    "export the session (every pass, transcript, action, Snapshot, and event) to:",
                    path,
                );
            }
            None => app.message = Some("no issue is traced".to_string()),
        },
        _ => return false,
    }
    true
}

/// The transcript index of the selected row.
fn selected_entry_index(app: &App) -> Option<usize> {
    let bundle = app.trace.bundle.as_ref()?;
    let jobs = jobs(bundle);
    let index = selected_index(&app.trace, &jobs)?;
    let view = pass_view(&app.trace, bundle, jobs[index]);
    let turns = view.stored.as_ref().map_or(&[][..], |t| t.turns.as_slice());
    build_rows(&view.entries, turns, None, 0)
        .get(app.trace.selected)
        .map(|row| row.index)
}

/// Selects the previous or next pass in the chain.
fn pick(app: &mut App, delta: isize) {
    let Some(bundle) = &app.trace.bundle else {
        return;
    };
    let jobs = jobs(bundle);
    let Some(current) = selected_index(&app.trace, &jobs) else {
        return;
    };
    let next = (current as isize + delta).clamp(0, jobs.len() as isize - 1) as usize;
    if next != current {
        app.trace.picked = Some(jobs[next].job_id.clone());
        app.trace.reset_view();
    }
}

/* ---- drawing ---- */

/// Draws the Trace.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(issue_id) = app.trace.issue_id.clone() else {
        let mut doc = Doc::new(inner_width(area));
        doc.heading("No Issue is traced yet");
        doc.note("Open one from Issues & jobs (Enter), the Inbox (Enter), a running pass on the Overview (Enter), or an event (Enter).");
        let mut scroll = 0;
        render_doc(frame, area, &doc, &mut scroll, panel(" Trace "));
        return;
    };
    let Some(bundle) = app.trace.bundle.clone() else {
        let mut doc = Doc::new(inner_width(area));
        match &app.trace.error {
            Some(error) => doc.alert(
                Color::Red,
                &format!("This Issue could not be loaded: {error}"),
            ),
            None => doc.note(&format!("Loading the session of {}…", short(&issue_id))),
        }
        let mut scroll = 0;
        render_doc(frame, area, &doc, &mut scroll, panel(" Trace "));
        return;
    };

    let issue = &bundle.issue;
    let jobs = jobs(&bundle);
    let has_live = jobs.iter().any(|j| j.is_live());
    let all_events = events(&app.trace);
    let now_ms = app.now.timestamp_millis();

    // Header: the Issue.
    let mut head = Doc::new(inner_width(area));
    let mut line = vec![
        dim(issue.issue_id.clone()),
        Span::raw(" "),
        badge(&label(&issue.priority), Color::DarkGray),
        Span::raw(" "),
        status_span(&issue.status),
        sep(),
        dim(date_time(&issue.created_at)),
    ];
    if issue.provenance.is_some() {
        line.push(Span::raw(" "));
        line.push(badge("archive", Color::Yellow));
    }
    if has_live {
        line.push(Span::raw(" "));
        line.push(badge(
            if app.trace.connected {
                "live"
            } else {
                "live · not connected"
            },
            Color::Green,
        ));
    }
    if let Some(error) = &app.trace.error {
        line.push(Span::raw(" "));
        line.push(Span::styled(error.clone(), Style::default().fg(Color::Red)));
    }
    head.line(line);
    head.note(&issue.description);
    if let Some(archive) = &issue.provenance {
        head.styled(
            &format!(
                "Imported from {} (exported by {} {}; imported by {} {}). Read-only.",
                archive.source_deployment,
                archive.exported_by,
                date_time(&archive.exported_at),
                archive.imported_by,
                date_time(&archive.imported_at)
            ),
            Style::default().fg(Color::Yellow),
        );
    }
    let head_height = (head.len() as u16 + 2).min(7);
    let rows = Layout::vertical([
        Constraint::Length(head_height),
        Constraint::Length(4),
        Constraint::Min(6),
    ])
    .split(area);
    let mut scroll = 0;
    render_doc(
        frame,
        rows[0],
        &head,
        &mut scroll,
        panel(Line::from(vec![
            Span::raw(" Trace: "),
            bold(issue.title.clone()),
            Span::raw(" "),
        ])),
    );

    // The pass chain.
    let selected = selected_index(&app.trace, &jobs);
    let titles: Vec<Line> = jobs
        .iter()
        .enumerate()
        .map(|(i, job)| {
            let end = end_of(&all_events, job);
            let elapsed = if job.is_live() {
                now_ms - millis(&job.created_at).unwrap_or(now_ms)
            } else {
                end.unwrap_or_else(|| millis(&job.created_at).unwrap_or(0))
                    - millis(&job.created_at).unwrap_or(0)
            };
            let mut spans = vec![
                bold(format!("Pass {}", i + 1)),
                Span::raw(" "),
                status_span(&job.status),
            ];
            if job.is_live() {
                spans.push(Span::styled(" ●", Style::default().fg(Color::Green)));
            }
            spans.push(dim(format!(
                "{}{} · {}",
                job.result
                    .as_ref()
                    .map_or(String::new(), |r| format!(" {}", label(&r.outcome))),
                job.tokens()
                    .map_or(String::new(), |t| format!(" · {} tokens", thousands(t))),
                duration_ms(elapsed)
            )));
            Line::from(spans)
        })
        .collect();
    let chain_block =
        panel(" Pass chain · ←/→ select · one pass is one Team run over one immutable Snapshot ");
    let chain_inner = chain_block.inner(rows[1]);
    frame.render_widget(chain_block, rows[1]);
    let chain_rows =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(chain_inner);
    if jobs.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(dim("No Job yet."))),
            chain_rows[0],
        );
    } else {
        let tabs = Tabs::new(titles)
            .select(selected.unwrap_or(0))
            .divider(Span::styled(" → ", Style::default().fg(Color::DarkGray)))
            .highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_widget(tabs, chain_rows[0]);
    }
    let Some(index) = selected else {
        return;
    };
    let job = jobs[index];
    let (relation, explanation) = job.relation();
    let mut about = vec![dim(format!("Pass {} · {relation}", index + 1))];
    if let Some(explanation) = explanation {
        about.push(dim(format!(": {explanation}")));
    }
    if let Some(result) = &job.result {
        about.push(sep());
        about.push(Span::raw(crate::format::truncate(
            &result.summary,
            usize::from(chain_inner.width).saturating_sub(40),
        )));
    }
    frame.render_widget(Paragraph::new(Line::from(about)), chain_rows[1]);

    // Transcript and the side pane.
    let columns =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).split(rows[2]);
    let end_at = end_of(&all_events, job);
    let live = job.is_live();
    let view = pass_view(&app.trace, &bundle, job);
    draw_transcript(
        frame,
        columns[0],
        app,
        job,
        &view,
        live,
        end_at,
        now_ms,
        view_of(&bundle, job),
    );
    draw_side(frame, columns[1], app, job, &bundle, &all_events);
}

#[allow(clippy::too_many_arguments)]
fn draw_transcript(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    job: &Job,
    view: &PassView,
    live: bool,
    end_at: Option<i64>,
    now_ms: i64,
    snapshot_view: Option<&Value>,
) {
    let width = inner_width(area);
    let turns = view.stored.as_ref().map_or(&[][..], |t| t.turns.as_slice());
    let rows = build_rows(
        &view.entries,
        turns,
        if live { None } else { end_at },
        now_ms,
    );
    let mut doc = Doc::new(width);

    // The stats line.
    let tool_calls = view
        .entries
        .iter()
        .filter(|e| e.item["type"].as_str() == Some("tool_call"))
        .count();
    let retries: u32 = turns.iter().map(|t| t.retries).sum();
    let wrapped_up = turns.iter().any(|t| t.wrap_up);
    let first = view
        .entries
        .first()
        .and_then(|e| millis(&e.at))
        .or_else(|| millis(&job.created_at))
        .unwrap_or(now_ms);
    let last = if live {
        now_ms
    } else {
        end_at.unwrap_or_else(|| {
            view.entries
                .last()
                .and_then(|e| millis(&e.at))
                .unwrap_or(first)
        })
    };
    let mut stats = vec![
        bold(if turns.is_empty() {
            if live {
                "…".to_string()
            } else {
                "—".to_string()
            }
        } else {
            turns.len().to_string()
        }),
        dim(" model turns"),
        sep(),
        bold(tool_calls.to_string()),
        dim(" tool calls"),
        sep(),
        bold(view.entries.len().to_string()),
        dim(" entries"),
        sep(),
        bold(duration_ms(last - first)),
        dim(" duration"),
    ];
    if retries > 0 {
        stats.push(sep());
        stats.push(bold(retries.to_string()));
        stats.push(dim(" retries"));
    }
    if let Some(tokens) = job.tokens() {
        stats.push(sep());
        stats.push(dim(format!("{} tokens", thousands(tokens))));
    }
    if wrapped_up {
        stats.push(Span::raw(" "));
        stats.push(badge(
            "reached its budget and was asked to conclude",
            Color::Yellow,
        ));
    }
    doc.line(stats);
    doc.blank();

    if app.trace.show_instructions {
        doc.rule("Instructions the run started with (I hides)");
        match &view.stored {
            Some(stored) => doc.note(&stored.instructions),
            None => doc.note("(only the stored transcript carries them)"),
        }
        doc.blank();
    }
    if app.trace.show_view {
        doc.rule("Snapshot View the model read (v hides)");
        match snapshot_view {
            Some(value) => doc.note(&pretty(value)),
            None => doc.note("(the View artifact is not in the session)"),
        }
        doc.blank();
    }

    // The rows.
    if rows.is_empty() {
        if view.gone {
            doc.alert(
                Color::Red,
                "The transcript artifact is missing from the store.",
            );
        } else if live {
            doc.note("Waiting for the first entry…");
        } else {
            doc.note("No transcript: this pass was not run by the model-backed Team.");
        }
    }
    if app.trace.follow && live && !rows.is_empty() {
        app.trace.selected = rows.len() - 1;
    }
    if app.trace.selected >= rows.len() {
        app.trace.selected = rows.len().saturating_sub(1);
    }
    let max_duration = rows.iter().map(|r| r.duration).max().unwrap_or(0);
    let mut spans_of_rows: Vec<(usize, usize)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if let Some(turn) = &row.turn {
            let latency =
                millis(&turn.finished_at).unwrap_or(0) - millis(&turn.started_at).unwrap_or(0);
            let mut text = format!("Turn {} · {}", turn.turn, duration_ms(latency));
            if turn.usage.requests_without_usage == 0 {
                text.push_str(&format!(
                    " · {} tokens",
                    thousands(turn.usage.input_tokens + turn.usage.output_tokens)
                ));
            }
            if turn.retries > 0 {
                text.push_str(&format!(
                    " · {} retr{} after a backend failure",
                    turn.retries,
                    if turn.retries == 1 { "y" } else { "ies" }
                ));
            }
            if turn.wrap_up {
                text.push_str(" · wrap-up turn: only terminal tools offered");
            }
            text.push_str(&format!(" · {} tool(s) offered", turn.offered_tools.len()));
            doc.rule(&text);
        }
        let start = doc.len();
        row_lines(
            &mut doc,
            row,
            i == app.trace.selected,
            &app.trace.expanded,
            &view.truncated,
            max_duration,
        );
        doc.blank();
        spans_of_rows.push((start, doc.len().saturating_sub(1)));
    }

    // Keep the selected row in view.
    let height = usize::from(area.height).saturating_sub(2).max(1);
    if let Some((start, end)) = spans_of_rows.get(app.trace.selected) {
        if *start < app.trace.scroll {
            app.trace.scroll = *start;
        } else if *end >= app.trace.scroll + height {
            app.trace.scroll = if end - start + 1 > height {
                *start
            } else {
                end + 1 - height
            };
        }
    }
    let title = Line::from(vec![
        Span::raw(" Transcript "),
        dim(format!("job {} ", short(&job.job_id))),
        if live {
            badge("live", Color::Green)
        } else {
            badge("ended", Color::DarkGray)
        },
        dim(if live {
            " entries as the harness appends them "
        } else {
            " exactly what the model saw and did, entry by entry "
        }),
    ]);
    render_doc(frame, area, &doc, &mut app.trace.scroll, panel(title));
}

/// One entry, with its paired output when it is a tool call.
fn row_lines(
    doc: &mut Doc,
    row: &Row,
    selected: bool,
    expanded: &HashSet<usize>,
    truncated: &HashSet<usize>,
    max_duration: i64,
) {
    let item = &row.entry.item;
    let kind = item["type"].as_str().unwrap_or("?");
    let (kind_label, kind_color) = match kind {
        "user_input" => ("input", Color::Green),
        "assistant_text" => ("model", Color::White),
        "notice" => ("harness", Color::Yellow),
        "tool_call" => ("tool call", Color::Cyan),
        "tool_output" => ("tool output", Color::Magenta),
        _ => (kind, Color::DarkGray),
    };
    let output_item = row.output.as_ref().map(|(_, e)| &e.item);
    let is_error = item["is_error"].as_bool().unwrap_or(false)
        || output_item.is_some_and(|o| o["is_error"].as_bool().unwrap_or(false));
    let refused = is_error
        && output_item.is_some_and(|o| {
            pretty(&o["output"])
                .trim_start_matches("{\"error\":\"")
                .trim_start()
                .starts_with("refused")
        });
    let trust = |value: &Value| value["trust"].as_str().map(str::to_string);
    let untrusted = trust(item).as_deref() == Some("untrusted")
        || output_item.and_then(trust).as_deref() == Some("untrusted");
    let cut = truncated.contains(&row.index)
        || row
            .output
            .as_ref()
            .is_some_and(|(i, _)| truncated.contains(i));
    let is_open = expanded.contains(&row.index);

    let bar = if max_duration > 0 {
        ((5.0 * (row.duration as f64 / max_duration as f64).sqrt()).round() as usize).clamp(1, 5)
    } else {
        1
    };
    let mut header = vec![
        Span::styled(
            if selected { "▶ " } else { "  " },
            Style::default().fg(Color::Green),
        ),
        dim(format!("{}  ", time(&row.entry.at))),
        Span::styled(
            format!("{kind_label:<11}"),
            Style::default().fg(kind_color).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(tool) = item["tool"].as_str() {
        header.push(bold(format!("{tool} ")));
    }
    if is_error {
        header.push(badge(if refused { "refused" } else { "error" }, Color::Red));
        header.push(Span::raw(" "));
    }
    if untrusted {
        header.push(badge("untrusted data", Color::Yellow));
        header.push(Span::raw(" "));
    } else if trust(item).as_deref() == Some("mixed") {
        header.push(badge("mixed trust", Color::DarkGray));
        header.push(Span::raw(" "));
    }
    if cut {
        header.push(badge("preview", Color::DarkGray));
        header.push(Span::raw(" "));
    }
    header.push(dim(format!("#{}  ", row.index)));
    header.push(Span::styled(
        "▮".repeat(bar),
        Style::default().fg(Color::Green),
    ));
    header.push(Span::styled(
        "▯".repeat(5 - bar),
        Style::default().fg(Color::DarkGray),
    ));
    header.push(dim(format!(" {}", duration_ms(row.duration))));
    let header_line = if selected {
        Line::from(header).style(Style::default().bg(Color::DarkGray))
    } else {
        Line::from(header)
    };
    doc.line(header_line.spans);

    let body_style = match kind {
        "user_input" => Style::default().fg(Color::Green),
        "notice" => Style::default().fg(Color::Yellow),
        "tool_call" | "tool_output" => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    };
    let body = match kind {
        "user_input" | "assistant_text" | "notice" => pretty(&item["text"]),
        "tool_call" => pretty(&item["arguments"]),
        "tool_output" => pretty(&item["output"]),
        _ => pretty(item),
    };
    push_clamped(doc, &body, body_style, is_open, selected);
    if let Some((index, output)) = &row.output {
        let mut out_header = vec![
            dim("    ↳ "),
            Span::styled("tool output", Style::default().fg(Color::Magenta)),
            dim(format!(" #{index}")),
        ];
        if output.item["is_error"].as_bool().unwrap_or(false) {
            out_header.push(Span::raw(" "));
            out_header.push(badge("error", Color::Red));
        }
        doc.line(out_header);
        push_clamped(
            doc,
            &pretty(&output.item["output"]),
            Style::default().fg(Color::DarkGray),
            is_open,
            selected,
        );
    }
}

/// Indented text, folded to a few lines unless expanded.
fn push_clamped(doc: &mut Doc, text: &str, style: Style, open: bool, selected: bool) {
    let lines = wrap(text, doc.width().saturating_sub(4));
    let (shown, hidden) = if open {
        (lines, 0)
    } else {
        clamp(lines, FOLDED_LINES)
    };
    for line in shown {
        doc.line(vec![Span::raw("    "), Span::styled(line, style)]);
    }
    if hidden > 0 {
        doc.line(vec![
            Span::raw("    "),
            Span::styled(
                format!(
                    "… {hidden} more line(s){}",
                    if selected { " — Enter shows all" } else { "" }
                ),
                Style::default().fg(Color::Cyan),
            ),
        ]);
    }
}

/// Result, actions proposed, and the Issue's events, in one scrollable column.
fn draw_side(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    job: &Job,
    bundle: &SessionBundle,
    all_events: &[EventRecord],
) {
    let mut doc = Doc::new(inner_width(area));
    doc.rule("Result");
    match &job.result {
        None => doc.note("No result yet."),
        Some(result) => {
            let mut line = vec![badge(&label(&result.outcome), Color::Cyan)];
            if result.follow_up_requested {
                line.push(Span::raw(" "));
                line.push(badge("follow-up", Color::DarkGray));
            }
            doc.line(line);
            doc.text(&result.summary);
            for question in &result.unresolved_questions {
                doc.bullet_styled(
                    &format!("? {question}"),
                    Style::default().fg(Color::DarkGray),
                );
            }
            for probe in &result.requested_probes {
                doc.bullet_styled(
                    &format!(
                        "{} on {} — {}",
                        probe.probe_id,
                        probe.target_ids.join(", "),
                        probe.reason
                    ),
                    Style::default().fg(Color::DarkGray),
                );
            }
        }
    }
    if !job.feedback.is_empty() {
        doc.blank();
        doc.note("Feedback this pass was given");
        for feedback in &job.feedback {
            doc.bullet_styled(&feedback.phrase(), Style::default().fg(Color::Cyan));
        }
    }
    doc.blank();
    doc.rule("Actions proposed by this pass");
    let actions = actions_of(bundle, job);
    if actions.is_empty() {
        doc.note("No action was proposed.");
    }
    for action in actions {
        let mut line = vec![
            bold(action.title()),
            Span::raw(" "),
            status_span(&action.status),
            sep(),
            dim(format!(
                "{}{}",
                label(&action.approval),
                action
                    .approved_by
                    .as_deref()
                    .map(|who| format!(" by {who}"))
                    .unwrap_or_default()
            )),
        ];
        if action.dry_run {
            line.push(Span::raw(" "));
            line.push(badge("dry-run", Color::DarkGray));
        }
        if let Some(evidence) = action.evidence() {
            line.push(Span::raw(" "));
            line.push(badge(
                &evidence,
                if action.verification_evidence.as_deref() == Some("strong") {
                    Color::Green
                } else {
                    Color::Yellow
                },
            ));
        }
        doc.line(line);
        doc.indented(2, &action.reason, Style::default().fg(Color::DarkGray));
        if let Some(denial) = &action.denial {
            doc.indented(
                2,
                &format!(
                    "Denial reason: {}{}",
                    denial.reason,
                    denial
                        .comment
                        .as_deref()
                        .map(|c| format!(" — “{c}”"))
                        .unwrap_or_default()
                ),
                Style::default(),
            );
        }
        if let Some(summary) = &action.execution_summary {
            doc.indented(2, &format!("Execution: {summary}"), Style::default());
        }
        if let Some(summary) = &action.verification_summary {
            doc.indented(2, &format!("Verification: {summary}"), Style::default());
        }
    }
    doc.blank();
    doc.rule("Events on this Issue (J/K scroll)");
    doc.note("The Scheduler's, the Platform's, and the humans' entries, in order. Forwarded transcript entries are shown in the transcript instead.");
    for event in all_events {
        let own = event.job_id.as_deref() == Some(job.job_id.as_str());
        let style = if own {
            Style::default()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let head = format!("{} {}", time(&event.occurred_at), event.actor);
        let indent = crate::format::width(&head) + 1;
        let mut first = true;
        for line in wrap(&event.summary, doc.width().saturating_sub(indent)) {
            if first {
                doc.line(vec![
                    dim(time(&event.occurred_at)),
                    Span::raw(" "),
                    Span::styled(
                        event.actor.clone(),
                        Style::default().fg(actor_color(&event.actor)),
                    ),
                    Span::raw(" "),
                    Span::styled(line, style),
                ]);
                first = false;
            } else {
                doc.line(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled(line, style),
                ]);
            }
        }
    }
    let title = Line::from(vec![Span::raw(" Result · actions · events ")]);
    render_doc(frame, area, &doc, &mut app.trace.side_scroll, panel(title));
}
