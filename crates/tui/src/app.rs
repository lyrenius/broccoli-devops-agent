//! The console's state: what the screens render, what the keys do, and the background work
//! that keeps it current without ever blocking the screen.
//!
//! Every HTTP call runs in a spawned task and reports back through one channel, so a report
//! that runs a model-backed pass for minutes, or a send-upstream that runs a revision, shows
//! its progress while the operator keeps working the other screens.

use std::future::Future;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::api::{
    ActionRun, ApiClient, EventRecord, EventsQuery, Inbox, Issue, Job, ReportOutcome,
    SessionBundle, SettingsPage, Snapshot, Status,
};
use crate::format::short;
use crate::screens::events::EventsState;
use crate::screens::inbox::InboxState;
use crate::screens::overview::OverviewState;
use crate::screens::records::RecordsState;
use crate::screens::report::ReportState;
use crate::screens::settings::SettingsState;
use crate::screens::trace::TraceState;
use crate::screens::{self};

/// How often the lists and the event tail are re-read.
const REFRESH_EVERY: Duration = Duration::from_secs(1);
/// Events kept in memory for the Events screen and the report's progress window.
const EVENT_TAIL: usize = 2000;

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// Status, the latest Snapshot, what runs now, what it cost.
    #[default]
    Overview,
    /// Everything that waits for a human, and the history of decided actions.
    Inbox,
    /// Every Issue with its passes.
    Records,
    /// One Issue's pass chain, transcript by transcript.
    Trace,
    /// The append-only log, live.
    Events,
    /// File a human report and watch it run.
    Report,
    /// The configurator.
    Settings,
}

impl Screen {
    /// All screens in tab order; the digit keys follow it.
    pub const ALL: [Screen; 7] = [
        Screen::Overview,
        Screen::Inbox,
        Screen::Records,
        Screen::Trace,
        Screen::Events,
        Screen::Report,
        Screen::Settings,
    ];

    /// Tab label.
    pub fn title(self) -> &'static str {
        match self {
            Screen::Overview => "1 Overview",
            Screen::Inbox => "2 Inbox",
            Screen::Records => "3 Issues & jobs",
            Screen::Trace => "4 Trace",
            Screen::Events => "5 Events",
            Screen::Report => "6 File a report",
            Screen::Settings => "7 Settings",
        }
    }

    /// Position in tab order.
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// What a background task reports back.
pub enum Msg {
    /// A periodic read of every list.
    Data(Box<Fetched>),
    /// New events after the last seen sequence.
    Events(Result<Vec<EventRecord>, String>),
    /// An Issue's session loaded for the trace.
    Session {
        /// Which Issue.
        issue_id: String,
        /// The bundle, or why not.
        result: Box<Result<SessionBundle, String>>,
    },
    /// New events on the traced Issue.
    SessionEvents {
        /// Which Issue.
        issue_id: String,
        /// The events.
        events: Vec<EventRecord>,
    },
    /// The settings page loaded.
    Settings(Result<SettingsPage, String>),
    /// An action finished.
    Done {
        /// What it was.
        label: String,
        /// What it produced.
        outcome: Outcome,
    },
    /// A report finished running.
    Report(Box<Result<ReportOutcome, String>>),
}

/// One periodic read.
pub struct Fetched {
    /// `/api/status`, or why the API could not be reached.
    pub status: Result<Status, String>,
    /// The latest Snapshot: absent when the read failed, `Some(None)` when none exists yet.
    pub snapshot: Option<Option<Snapshot>>,
    /// The inbox, when read.
    pub inbox: Option<Inbox>,
    /// Every Issue, when read.
    pub issues: Option<Vec<Issue>>,
    /// Every Job, when read.
    pub jobs: Option<Vec<Job>>,
    /// Every action, when read.
    pub actions: Option<Vec<ActionRun>>,
}

/// A box of text at the top of the screen, dismissed with Esc, like the web console's alerts.
#[derive(Debug, Clone)]
pub struct Banner {
    /// Its color.
    pub tone: Color,
    /// Its lines (wrapped at draw time).
    pub lines: Vec<String>,
}

/// What an action produced, for the operator.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// A footer line.
    pub message: Option<String>,
    /// A banner.
    pub banner: Option<Banner>,
    /// Whether the lists should be re-read now.
    pub refresh: bool,
}

impl Outcome {
    /// A confirmation that changed something.
    pub fn message(text: impl Into<String>) -> Self {
        Self {
            message: Some(text.into()),
            banner: None,
            refresh: true,
        }
    }

    /// A failure; nothing changed.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            message: Some(text.into()),
            banner: None,
            refresh: false,
        }
    }

    /// A multi-line result worth keeping on screen.
    pub fn banner(tone: Color, lines: Vec<String>) -> Self {
        Self {
            message: None,
            banner: Some(Banner { tone, lines }),
            refresh: true,
        }
    }
}

/// What a footer prompt will do with its text once entered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Reject an ActionRun with the text as comment.
    Reject {
        /// ActionRun ID.
        id: String,
    },
    /// Review a denied or failed item with the text as comment.
    Review {
        /// ActionRun or Job ID.
        id: String,
        /// Whether `id` is a Job.
        is_job: bool,
        /// `send_upstream` or `acknowledge`.
        decision: &'static str,
    },
    /// Close an Issue with the text as comment.
    CloseIssue {
        /// Issue ID.
        id: String,
        /// Its title, for the confirmation.
        title: String,
        /// `resolved` or `cancelled`.
        outcome: &'static str,
    },
    /// Filter the Issues list by the text.
    Search,
    /// Save a session file at the path.
    Export {
        /// Issue ID.
        issue_id: String,
    },
    /// Load a session file from the path.
    Import,
    /// Set a setting from the text.
    Setting {
        /// Dotted key.
        key: String,
    },
    /// Confirm turning dry-run off by typing `yes`.
    ConfirmLive,
}

/// A one-line text prompt in the footer.
#[derive(Debug, Clone)]
pub struct Prompt {
    /// What the text is for.
    pub pending: Pending,
    /// Footer label.
    pub label: String,
    /// Text entered so far.
    pub buffer: String,
}

/// Everything the screens render.
pub struct App {
    /// Active screen.
    pub screen: Screen,
    /// Operator name recorded with decisions.
    pub operator: String,
    /// Where the API is.
    pub api_url: String,
    /// The current time, taken once per tick so ages and durations agree across a frame.
    pub now: DateTime<Utc>,
    /// Latest `/api/status`; absent until the first read succeeds.
    pub status: Option<Status>,
    /// Why the last status read failed, when it did.
    pub api_error: Option<String>,
    /// The latest Snapshot.
    pub snapshot: Option<Snapshot>,
    /// The inbox.
    pub inbox: Inbox,
    /// Every Issue, oldest first as served.
    pub issues: Vec<Issue>,
    /// Every Job.
    pub jobs: Vec<Job>,
    /// Every ActionRun.
    pub actions: Vec<ActionRun>,
    /// The event tail, oldest first.
    pub events: Vec<EventRecord>,
    /// The newest sequence seen.
    pub events_seq: u64,
    /// Whether the last tail read succeeded.
    pub events_connected: bool,
    /// Overview screen state.
    pub overview: OverviewState,
    /// Inbox screen state.
    pub inbox_view: InboxState,
    /// Issues & jobs screen state.
    pub records: RecordsState,
    /// Trace screen state.
    pub trace: TraceState,
    /// Events screen state.
    pub events_view: EventsState,
    /// Report screen state.
    pub report: ReportState,
    /// Settings screen state.
    pub settings: SettingsState,
    /// Active footer prompt, if any.
    pub prompt: Option<Prompt>,
    /// Banner at the top of the screen, if any.
    pub banner: Option<Banner>,
    /// Transient footer message (last error or confirmation).
    pub message: Option<String>,
    /// Whether the help overlay is open.
    pub help: bool,
    /// Labels of the actions in flight.
    pub busy: Vec<String>,
    /// The API.
    pub client: ApiClient,
    tx: mpsc::UnboundedSender<Msg>,
    refreshing: bool,
    refresh_wanted: bool,
    last_refresh: Option<Instant>,
    events_loading: bool,
    last_events: Option<Instant>,
}

impl App {
    /// A console bound to one API, with nothing loaded yet.
    pub fn new(
        client: ApiClient,
        operator: String,
        api_url: String,
        tx: mpsc::UnboundedSender<Msg>,
    ) -> Self {
        Self {
            screen: Screen::Overview,
            report: ReportState::new(&operator),
            operator,
            api_url,
            now: Utc::now(),
            status: None,
            api_error: None,
            snapshot: None,
            inbox: Inbox::default(),
            issues: Vec::new(),
            jobs: Vec::new(),
            actions: Vec::new(),
            events: Vec::new(),
            events_seq: 0,
            events_connected: false,
            overview: OverviewState::default(),
            inbox_view: InboxState::default(),
            records: RecordsState::default(),
            trace: TraceState::default(),
            events_view: EventsState::default(),
            settings: SettingsState::default(),
            prompt: None,
            banner: None,
            message: None,
            help: false,
            busy: Vec::new(),
            client,
            tx,
            refreshing: false,
            refresh_wanted: false,
            last_refresh: None,
            events_loading: false,
            last_events: None,
        }
    }

    /* ---- background work ---- */

    /// Starts whatever periodic work is due. Called once per loop iteration.
    pub fn tick(&mut self) {
        self.now = Utc::now();
        let due = |last: Option<Instant>| last.is_none_or(|at| at.elapsed() >= REFRESH_EVERY);
        if !self.refreshing && due(self.last_refresh) {
            self.spawn_refresh();
        }
        if !self.events_loading && due(self.last_events) {
            self.spawn_events();
        }
        if self.screen == Screen::Trace {
            screens::trace::tick(self);
        }
    }

    /// Asks for the lists to be re-read as soon as possible.
    pub fn request_refresh(&mut self) {
        if self.refreshing {
            self.refresh_wanted = true;
        } else {
            self.last_refresh = None;
        }
        self.trace.dirty = true;
    }

    fn spawn_refresh(&mut self) {
        self.refreshing = true;
        self.last_refresh = Some(Instant::now());
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let fetched = match client.status().await {
                Err(error) => Fetched {
                    status: Err(error),
                    snapshot: None,
                    inbox: None,
                    issues: None,
                    jobs: None,
                    actions: None,
                },
                Ok(status) => {
                    let (snapshot, inbox, issues, jobs, actions) = tokio::join!(
                        client.latest_snapshot(),
                        client.inbox(),
                        client.issues(),
                        client.jobs(),
                        client.actions()
                    );
                    Fetched {
                        status: Ok(status),
                        snapshot: snapshot.ok(),
                        inbox: inbox.ok(),
                        issues: issues.ok(),
                        jobs: jobs.ok(),
                        actions: actions.ok(),
                    }
                }
            };
            let _ = tx.send(Msg::Data(Box::new(fetched)));
        });
    }

    fn spawn_events(&mut self) {
        self.events_loading = true;
        self.last_events = Some(Instant::now());
        let client = self.client.clone();
        let tx = self.tx.clone();
        let query = EventsQuery {
            limit: if self.events_seq == 0 {
                500
            } else {
                EVENT_TAIL
            },
            after: Some(self.events_seq),
            ..EventsQuery::default()
        };
        tokio::spawn(async move {
            let _ = tx.send(Msg::Events(client.events(&query).await));
        });
    }

    /// Loads an Issue's session for the trace.
    pub fn spawn_session(&mut self, issue_id: String) {
        self.trace.loading = true;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = Box::new(client.session(&issue_id).await);
            let _ = tx.send(Msg::Session { issue_id, result });
        });
    }

    /// Reads the traced Issue's events after the last seen sequence.
    pub fn spawn_trace_events(&mut self, issue_id: String) {
        self.trace.polling = true;
        self.trace.last_poll = Some(Instant::now());
        let client = self.client.clone();
        let tx = self.tx.clone();
        let query = EventsQuery {
            limit: 1000,
            after: Some(self.trace.last_seq),
            issue_id: Some(issue_id.clone()),
            job_id: None,
        };
        tokio::spawn(async move {
            let events = client.events(&query).await.unwrap_or_default();
            let _ = tx.send(Msg::SessionEvents { issue_id, events });
        });
    }

    /// Loads the settings page.
    pub fn spawn_settings(&mut self) {
        self.settings.loading = true;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Msg::Settings(client.settings().await));
        });
    }

    /// A handle for a task that reports through the channel itself.
    pub fn sender(&self) -> mpsc::UnboundedSender<Msg> {
        self.tx.clone()
    }

    /// Runs an action in the background; its outcome lands in the footer or a banner.
    pub fn run<F>(&mut self, label: &str, work: F)
    where
        F: Future<Output = Outcome> + Send + 'static,
    {
        self.busy.push(label.to_string());
        self.message = Some(format!("{label}…"));
        let tx = self.tx.clone();
        let label = label.to_string();
        tokio::spawn(async move {
            let outcome = work.await;
            let _ = tx.send(Msg::Done { label, outcome });
        });
    }

    /// Applies what a background task reported.
    pub fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Data(fetched) => {
                self.refreshing = false;
                match fetched.status {
                    Ok(status) => {
                        self.status = Some(status);
                        self.api_error = None;
                    }
                    Err(error) => self.api_error = Some(error),
                }
                if let Some(snapshot) = fetched.snapshot {
                    self.snapshot = snapshot;
                }
                if let Some(inbox) = fetched.inbox {
                    self.inbox = inbox;
                }
                if let Some(issues) = fetched.issues {
                    self.issues = issues;
                }
                if let Some(jobs) = fetched.jobs {
                    self.jobs = jobs;
                }
                if let Some(actions) = fetched.actions {
                    self.actions = actions;
                }
                if self.refresh_wanted {
                    self.refresh_wanted = false;
                    self.spawn_refresh();
                }
            }
            Msg::Events(result) => {
                self.events_loading = false;
                match result {
                    Ok(fresh) => {
                        self.events_connected = true;
                        for event in fresh {
                            if event.sequence > self.events_seq {
                                self.events_seq = event.sequence;
                                self.events.push(event);
                            }
                        }
                        if self.events.len() > EVENT_TAIL {
                            let drop = self.events.len() - EVENT_TAIL;
                            self.events.drain(..drop);
                        }
                        screens::events::on_events(self);
                    }
                    Err(_) => self.events_connected = false,
                }
            }
            Msg::Session { issue_id, result } => {
                screens::trace::on_session(self, issue_id, *result);
            }
            Msg::SessionEvents { issue_id, events } => {
                screens::trace::on_events(self, issue_id, events);
            }
            Msg::Settings(result) => screens::settings::on_page(self, result),
            Msg::Done { label, outcome } => {
                self.busy.retain(|b| *b != label);
                if label == screens::settings::SAVE_LABEL {
                    screens::settings::on_saved(self, outcome.refresh);
                }
                if self.message.as_deref() == Some(&format!("{label}…")) {
                    self.message = None;
                }
                if let Some(message) = outcome.message {
                    self.message = Some(message);
                }
                if let Some(banner) = outcome.banner {
                    self.banner = Some(banner);
                }
                if outcome.refresh {
                    self.request_refresh();
                }
            }
            Msg::Report(result) => screens::report::on_done(self, *result),
        }
    }

    /* ---- keys ---- */

    /// Applies one key press; returns false when the app should quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let (code, mods) = (key.code, key.modifiers);
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if self.prompt.is_some() {
            self.prompt_key(code, mods);
            return true;
        }
        if self.help {
            if matches!(
                code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter
            ) {
                self.help = false;
            }
            return true;
        }
        if self.screen == Screen::Report && self.report.focus.is_some() {
            screens::report::form_key(self, code, mods);
            return true;
        }
        self.message = None;
        if code == KeyCode::Esc {
            self.escape();
            return true;
        }
        if screens::key(self, code, mods) {
            return true;
        }
        match code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char(digit @ '1'..='7') => {
                let index = digit as usize - '1' as usize;
                self.set_screen(Screen::ALL[index]);
            }
            KeyCode::Tab => {
                self.set_screen(Screen::ALL[(self.screen.index() + 1) % Screen::ALL.len()])
            }
            KeyCode::BackTab => {
                let count = Screen::ALL.len();
                self.set_screen(Screen::ALL[(self.screen.index() + count - 1) % count]);
            }
            KeyCode::Char('s') => self.capture(),
            KeyCode::Char('f') => self.transition("freeze-dispatch"),
            KeyCode::Char('F') => self.transition("freeze-all"),
            KeyCode::Char('u') => self.transition("resume"),
            KeyCode::Char('c') => self.interrupt_selected(),
            _ => {}
        }
        true
    }

    /// Esc: closes the banner, leaves the trace, or does nothing.
    fn escape(&mut self) {
        if self.banner.is_some() {
            self.banner = None;
        } else if self.screen == Screen::Trace {
            self.set_screen(Screen::Records);
        }
    }

    /// Switches screens, loading what the new one needs.
    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
        match screen {
            Screen::Trace => {
                if self.trace.issue_id.is_some() {
                    self.trace.dirty = true;
                }
            }
            Screen::Settings if self.settings.page.is_none() && !self.settings.loading => {
                self.spawn_settings();
            }
            _ => {}
        }
    }

    /// Opens the trace of an Issue, on one pass when a Job is given.
    pub fn open_trace(&mut self, issue_id: String, job_id: Option<String>) {
        if self.trace.issue_id.as_deref() != Some(issue_id.as_str()) {
            self.trace = TraceState::new(issue_id);
        }
        if job_id.is_some() {
            self.trace.picked = job_id;
            self.trace.reset_view();
        }
        self.set_screen(Screen::Trace);
    }

    /// Opens a footer prompt.
    pub fn open_prompt(
        &mut self,
        pending: Pending,
        label: impl Into<String>,
        initial: impl Into<String>,
    ) {
        self.prompt = Some(Prompt {
            pending,
            label: label.into(),
            buffer: initial.into(),
        });
    }

    fn prompt_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.prompt = None;
                self.message = Some("cancelled".to_string());
            }
            KeyCode::Enter => {
                let prompt = self.prompt.take().expect("prompt is active");
                self.submit_prompt(prompt);
            }
            KeyCode::Backspace => {
                prompt.buffer.pop();
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => prompt.buffer.clear(),
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => prompt.buffer.push(c),
            _ => {}
        }
    }

    /// Submits a finished prompt.
    fn submit_prompt(&mut self, prompt: Prompt) {
        let text = prompt.buffer.trim().to_string();
        match prompt.pending {
            Pending::Reject { id } => self.reject(id, text),
            Pending::Review {
                id,
                is_job,
                decision,
            } => self.review(id, is_job, decision, text),
            Pending::CloseIssue { id, title, outcome } => {
                self.close_issue(id, title, outcome, text);
            }
            Pending::Search => {
                self.records.query = text;
                self.records.list.select(Some(0));
            }
            Pending::Export { issue_id } => {
                if text.is_empty() {
                    self.message = Some("export cancelled: no path".to_string());
                } else {
                    self.export_session(issue_id, text);
                }
            }
            Pending::Import => {
                if text.is_empty() {
                    self.message = Some("import cancelled: no path".to_string());
                } else {
                    self.import_session(text);
                }
            }
            Pending::Setting { key } => screens::settings::apply_edit(self, &key, &text),
            Pending::ConfirmLive => {
                if text.eq_ignore_ascii_case("yes") {
                    screens::settings::save(self, true);
                } else {
                    self.message = Some("not saved: dry-run stays on".to_string());
                }
            }
        }
    }

    /* ---- actions, all through the API ---- */

    /// Captures a Snapshot now.
    pub fn capture(&mut self) {
        let client = self.client.clone();
        self.run("capturing a Snapshot", async move {
            match client.capture().await {
                Ok(snapshot) => Outcome::message(format!(
                    "Snapshot captured: {} resource(s), {} coverage gap(s)",
                    snapshot.resources.len(),
                    snapshot.coverage_gaps.len()
                )),
                Err(error) => Outcome::error(format!("capture failed: {error}")),
            }
        });
    }

    /// Requests a Scheduler transition.
    pub fn transition(&mut self, name: &'static str) {
        let client = self.client.clone();
        self.run(name, async move {
            match client.transition(name).await {
                Ok(value) => Outcome::message(format!(
                    "scheduler mode → {}",
                    value["mode"].as_str().unwrap_or("?")
                )),
                Err(error) => Outcome::error(format!("{name} failed: {error}")),
            }
        });
    }

    /// Interrupts the pass selected on the Overview, or the one running longest.
    fn interrupt_selected(&mut self) {
        let running = self
            .status
            .as_ref()
            .map(|s| s.running.clone())
            .unwrap_or_default();
        let index = if self.screen == Screen::Overview {
            self.overview.selected.min(running.len().saturating_sub(1))
        } else {
            0
        };
        match running.get(index) {
            Some(pass) => self.interrupt(pass.job_id.clone()),
            None => self.message = Some("no pass is running".to_string()),
        }
    }

    /// Interrupts a running pass. Cooperative: the Team stops at its next step boundary and
    /// still delivers a final callback, so the Job lands in the Failed inbox with its transcript.
    pub fn interrupt(&mut self, job_id: String) {
        let client = self.client.clone();
        let by = self.operator.clone();
        let label = format!("interrupting Job {}", short(&job_id));
        self.run(&label, async move {
            match client.cancel_job(&job_id, &by).await {
                Ok(_) => Outcome::message(format!(
                    "Job {} is being interrupted; it will land in the Failed inbox with its transcript",
                    short(&job_id)
                )),
                Err(error) => Outcome::error(format!("interrupt failed: {error}")),
            }
        });
    }

    /// Approves an ActionRun.
    pub fn approve(&mut self, id: String, title: String) {
        let client = self.client.clone();
        let by = self.operator.clone();
        self.run(&format!("approving {title}"), async move {
            match client.approve(&id, &by).await {
                Ok(action) => {
                    let mut text = format!("approved {title} → {}", action.status);
                    if let Some(summary) = action
                        .verification_summary
                        .as_deref()
                        .or(action.execution_summary.as_deref())
                    {
                        text.push_str(&format!(": {summary}"));
                    }
                    Outcome::message(text)
                }
                Err(error) => Outcome::error(format!("approve failed: {error}")),
            }
        });
    }

    /// Rejects an ActionRun with a comment.
    pub fn reject(&mut self, id: String, comment: String) {
        let client = self.client.clone();
        let by = self.operator.clone();
        self.run("rejecting", async move {
            match client.reject(&id, &by, &comment).await {
                Ok(action) => {
                    Outcome::message(format!("rejected {} → {}", action.title(), action.status))
                }
                Err(error) => Outcome::error(format!("reject failed: {error}")),
            }
        });
    }

    /// Reviews a denied or failed item: acknowledges it, or sends it back upstream (which runs
    /// a revising pass now, so the outcome may take a while).
    pub fn review(&mut self, id: String, is_job: bool, decision: &'static str, comment: String) {
        let client = self.client.clone();
        let by = self.operator.clone();
        let label = if decision == "send_upstream" {
            "sending upstream (a revising pass runs now)"
        } else {
            "acknowledging"
        };
        self.run(label, async move {
            let result = if is_job {
                client.review_job(&id, &by, decision, &comment).await
            } else {
                client.review_action(&id, &by, decision, &comment).await
            };
            match result {
                Ok(outcome) => match outcome.revision {
                    Some(revision) => {
                        let job = revision.job;
                        let mut lines = vec![format!(
                            "Revision ran: job {} is {}",
                            short(&job.job_id),
                            crate::format::label(&job.status)
                        )];
                        if let Some(result) = &job.result {
                            lines.push(result.summary.clone());
                        }
                        for action in &revision.actions {
                            lines.push(format!(
                                "  {} → {} ({})",
                                action.title(),
                                action.status,
                                action.approval
                            ));
                        }
                        Outcome::banner(Color::Cyan, lines)
                    }
                    None => Outcome::message("acknowledged"),
                },
                Err(error) => Outcome::error(format!("{decision} failed: {error}")),
            }
        });
    }

    /// Closes an Issue.
    pub fn close_issue(
        &mut self,
        id: String,
        title: String,
        outcome: &'static str,
        comment: String,
    ) {
        let client = self.client.clone();
        let by = self.operator.clone();
        self.run(&format!("closing “{title}”"), async move {
            match client.close_issue(&id, &by, outcome, &comment).await {
                Ok(issue) => {
                    Outcome::message(format!("issue “{}” → {}", issue.title, issue.status))
                }
                Err(error) => Outcome::error(format!("close failed: {error}")),
            }
        });
    }

    /// Saves an Issue's session to a file, with the operator recorded as the exporter.
    pub fn export_session(&mut self, issue_id: String, path: String) {
        let client = self.client.clone();
        let by = self.operator.clone();
        self.run("exporting the session", async move {
            let bytes = match client.session_bytes(&issue_id, &by).await {
                Ok(bytes) => bytes,
                Err(error) => return Outcome::error(format!("export failed: {error}")),
            };
            match tokio::fs::write(&path, &bytes).await {
                Ok(()) => Outcome {
                    message: Some(format!("exported {} bytes to {path}", bytes.len())),
                    banner: None,
                    refresh: false,
                },
                Err(error) => Outcome::error(format!("could not write {path}: {error}")),
            }
        });
    }

    /// Loads a session file as a read-only archive.
    pub fn import_session(&mut self, path: String) {
        let client = self.client.clone();
        let by = self.operator.clone();
        self.run("importing the session", async move {
            let file = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) => return Outcome::error(format!("could not read {path}: {error}")),
            };
            // Checked here so a wrong path gets a local message; the bytes themselves are sent.
            if let Err(error) = serde_json::from_slice::<Value>(&file) {
                return Outcome::error(format!("{path} is not a session file: {error}"));
            }
            match client.import_session(file, &by).await {
                Ok(summary) => Outcome::banner(
                    Color::Cyan,
                    vec![format!(
                        "Imported “{}” from {} as an archive: {} pass(es), {} action(s), {} event(s).",
                        summary.title,
                        summary.source_deployment,
                        summary.jobs,
                        summary.action_runs,
                        summary.events
                    )],
                ),
                Err(error) => Outcome::error(format!("import failed: {error}")),
            }
        });
    }

    /// The running passes, from the last status.
    pub fn running(&self) -> &[crate::api::RunningPass] {
        self.status.as_ref().map_or(&[], |s| s.running.as_slice())
    }

    /// The newest `team.callback` line, so a slow pass is visibly working rather than stalled.
    pub fn latest_progress(&self) -> Option<&EventRecord> {
        self.events
            .iter()
            .rev()
            .find(|event| event.kind == "team.callback")
    }
}
