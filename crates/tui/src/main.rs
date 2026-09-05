//! Terminal operator console for the Broccoli DevOps Agent.
//!
//! A pure client of the control plane's HTTP API: it refreshes status, the latest Snapshot, the
//! inbox, issues, and the event tail once a second, and lets an operator work the inbox —
//! approve, reject with a comment, acknowledge, or send an item back upstream — capture a
//! Snapshot, and freeze or resume the Scheduler. It has no state of its own and no access to
//! machines — everything it can do, the API and therefore the authority matrix allow.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod api;
mod ui;

use std::io;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use api::{ApiClient, EventRow, Inbox, IssueRow, ResourceRow, Status};
use ui::Screen;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(name = "broccoli-tui", version, about)]
struct Cli {
    /// Base URL of the control plane API.
    #[arg(long, default_value = "http://127.0.0.1:4720")]
    api: String,
    /// Bearer token, if the API requires one.
    #[arg(long)]
    token: Option<String>,
    /// Your name, recorded with every decision you make.
    #[arg(long = "as", default_value = "tui")]
    operator: String,
}

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
}

/// One selectable inbox row.
#[derive(Debug, Clone)]
pub struct InboxItem {
    /// Category.
    pub category: Category,
    /// ActionRun or Job ID.
    pub id: String,
    /// What it is: runbook and targets, or the Job.
    pub title: String,
    /// Status string.
    pub status: String,
    /// Detail lines for the lower pane.
    pub detail: String,
}

impl InboxItem {
    fn from_action(category: Category, action: &api::Action) -> Self {
        let mut detail = format!(
            "id: {}\napproval: {}\nreason: {}",
            action.action_run_id, action.approval, action.reason
        );
        if let Some(denial) = &action.denial {
            detail.push_str(&format!(
                "\ndenied by {}: {}",
                denial.decided_by.as_deref().unwrap_or(&denial.source),
                denial.reason
            ));
            if let Some(comment) = &denial.comment {
                detail.push_str(&format!("\ncomment: {comment}"));
            }
        }
        if let Some(summary) = &action.verification_summary {
            detail.push_str(&format!("\nverification: {summary}"));
        }
        if let Some(review) = &action.review {
            detail.push_str(&format!(
                "\nreviewed by {}: {}",
                review["reviewer"].as_str().unwrap_or("?"),
                review["decision"]["decision"].as_str().unwrap_or("?")
            ));
        }
        Self {
            category,
            id: action.action_run_id.clone(),
            title: format!("{} on {}", action.runbook_id, action.target_ids.join(",")),
            status: action.status.clone(),
            detail,
        }
    }

    fn from_job(job: &api::JobRow) -> Self {
        Self {
            category: Category::FailedJob,
            id: job.job_id.clone(),
            title: format!("job {}", &job.job_id[..job.job_id.len().min(8)]),
            status: job.status.clone(),
            detail: format!(
                "id: {}\nissue: {}\n{}",
                job.job_id,
                job.issue_id,
                job.result
                    .as_ref()
                    .map_or("no result was recorded", |r| r.summary.as_str())
            ),
        }
    }
}

/// What a footer prompt will do with its text once entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// Reject the selected request with the text as comment.
    Reject,
    /// Send the selected item back upstream with the text as feedback.
    SendUpstream,
    /// Acknowledge the selected item with the text as comment.
    Acknowledge,
}

/// A one-line text prompt in the footer.
#[derive(Debug, Clone)]
pub struct Prompt {
    /// What the text is for.
    pub pending: Pending,
    /// Text entered so far.
    pub buffer: String,
}

impl Prompt {
    /// Footer label.
    pub fn label(&self) -> &'static str {
        match self.pending {
            Pending::Reject => "reject — comment (Enter to submit, Esc to cancel): ",
            Pending::SendUpstream => "send upstream — feedback for the next pass: ",
            Pending::Acknowledge => "acknowledge — comment (optional): ",
        }
    }
}

/// Everything the screens render.
#[derive(Debug, Default)]
pub struct App {
    /// Active screen.
    pub screen: Screen,
    /// Operator name recorded with decisions.
    pub operator: String,
    /// Latest `/api/status`.
    pub status: Status,
    /// Latest Snapshot resources.
    pub resources: Vec<ResourceRow>,
    /// Inbox rows: requests, then denials, then failures.
    pub inbox: Vec<InboxItem>,
    /// All Issues.
    pub issues: Vec<IssueRow>,
    /// Event tail.
    pub events: Vec<EventRow>,
    /// Selected inbox index.
    pub selected: usize,
    /// Active footer prompt, if any.
    pub prompt: Option<Prompt>,
    /// Transient footer message (last error or confirmation).
    pub message: Option<String>,
}

impl App {
    /// Flattens the inbox into selectable rows.
    fn set_inbox(&mut self, inbox: Inbox) {
        let mut rows = Vec::new();
        rows.extend(
            inbox
                .permission_requests
                .iter()
                .map(|a| InboxItem::from_action(Category::Request, a)),
        );
        rows.extend(
            inbox
                .permission_denied
                .iter()
                .map(|a| InboxItem::from_action(Category::Denied, a)),
        );
        rows.extend(inbox.failed_jobs.iter().map(InboxItem::from_job));
        rows.extend(
            inbox
                .failed_actions
                .iter()
                .map(|a| InboxItem::from_action(Category::FailedAction, a)),
        );
        self.inbox = rows;
        if self.selected >= self.inbox.len() {
            self.selected = self.inbox.len().saturating_sub(1);
        }
    }

    /// Refreshes every list from the API; failures become a footer message, never a crash.
    async fn refresh(&mut self, client: &ApiClient) {
        match client.status().await {
            Ok(status) => self.status = status,
            Err(error) => {
                self.message = Some(format!("api: {error}"));
                return;
            }
        }
        if let Ok(resources) = client.latest_resources().await {
            self.resources = resources;
        }
        if let Ok(inbox) = client.inbox().await {
            self.set_inbox(inbox);
        }
        if let Ok(issues) = client.issues().await {
            self.issues = issues;
        }
        if let Ok(events) = client.events(200).await {
            self.events = events;
        }
    }

    /// Applies one key press; returns false when the app should quit.
    async fn handle_key(
        &mut self,
        key: KeyCode,
        modifiers: KeyModifiers,
        client: &ApiClient,
    ) -> bool {
        if let Some(prompt) = &mut self.prompt {
            match key {
                KeyCode::Esc => {
                    self.prompt = None;
                    self.message = Some("cancelled".to_string());
                }
                KeyCode::Enter => {
                    let prompt = self.prompt.take().expect("prompt is active");
                    self.submit(client, prompt).await;
                }
                KeyCode::Backspace => {
                    prompt.buffer.pop();
                }
                KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    prompt.buffer.push(c);
                }
                KeyCode::Char('c') => return false,
                _ => {}
            }
            return true;
        }

        self.message = None;
        match key {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('1') => self.screen = Screen::Overview,
            KeyCode::Char('2') => self.screen = Screen::Inbox,
            KeyCode::Char('3') => self.screen = Screen::Issues,
            KeyCode::Char('4') => self.screen = Screen::Events,
            KeyCode::Tab => {
                let index = Screen::ALL
                    .iter()
                    .position(|s| *s == self.screen)
                    .unwrap_or(0);
                self.screen = Screen::ALL[(index + 1) % Screen::ALL.len()];
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.inbox.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('a') => self.approve(client).await,
            KeyCode::Char('r') => self.open_prompt(Pending::Reject),
            KeyCode::Char('b') => self.open_prompt(Pending::SendUpstream),
            KeyCode::Char('x') => self.open_prompt(Pending::Acknowledge),
            KeyCode::Char('s') => {
                self.message = Some(match client.capture().await {
                    Ok(_) => "Snapshot captured".to_string(),
                    Err(error) => format!("capture failed: {error}"),
                });
            }
            KeyCode::Char('f') => self.transition(client, "freeze-dispatch").await,
            KeyCode::Char('F') => self.transition(client, "freeze-all").await,
            KeyCode::Char('u') => self.transition(client, "resume").await,
            _ => {}
        }
        true
    }

    /// The selected inbox item, or a footer message explaining why there is none.
    fn selected_item(&mut self) -> Option<InboxItem> {
        let item = self.inbox.get(self.selected).cloned();
        if item.is_none() {
            self.message = Some("no inbox item selected".to_string());
        }
        item
    }

    /// Opens a footer prompt for the selected item, if the decision applies to it.
    fn open_prompt(&mut self, pending: Pending) {
        let Some(item) = self.selected_item() else {
            return;
        };
        let applies = match pending {
            Pending::Reject => item.category == Category::Request,
            Pending::SendUpstream | Pending::Acknowledge => item.category != Category::Request,
        };
        if !applies {
            self.message = Some(format!(
                "{:?} does not apply to a {} item",
                pending,
                item.category.label()
            ));
            return;
        }
        self.screen = Screen::Inbox;
        self.prompt = Some(Prompt {
            pending,
            buffer: String::new(),
        });
    }

    async fn approve(&mut self, client: &ApiClient) {
        let Some(item) = self.selected_item() else {
            return;
        };
        if item.category != Category::Request {
            self.message = Some(format!("cannot approve a {} item", item.category.label()));
            return;
        }
        self.message = Some(match client.approve(&item.id, &self.operator).await {
            Ok(value) => format!("approved → {}", value["status"].as_str().unwrap_or("?")),
            Err(error) => format!("approve failed: {error}"),
        });
    }

    /// Submits a finished prompt.
    async fn submit(&mut self, client: &ApiClient, prompt: Prompt) {
        let Some(item) = self.selected_item() else {
            return;
        };
        let comment = prompt.buffer.trim();
        let result = match prompt.pending {
            Pending::Reject => client.reject(&item.id, &self.operator, comment).await,
            Pending::SendUpstream | Pending::Acknowledge => {
                let decision = if prompt.pending == Pending::SendUpstream {
                    "send_upstream"
                } else {
                    "acknowledge"
                };
                if item.category == Category::FailedJob {
                    client
                        .review_job(&item.id, &self.operator, decision, comment)
                        .await
                } else {
                    client
                        .review_action(&item.id, &self.operator, decision, comment)
                        .await
                }
            }
        };
        self.message = Some(match (prompt.pending, result) {
            (Pending::Reject, Ok(value)) => {
                format!("rejected → {}", value["status"].as_str().unwrap_or("?"))
            }
            (Pending::SendUpstream, Ok(value)) => {
                let job = &value["revision"]["job"];
                format!(
                    "sent upstream → revision job {} is {}: {}",
                    job["job_id"].as_str().unwrap_or("?"),
                    job["status"].as_str().unwrap_or("?"),
                    job["result"]["summary"].as_str().unwrap_or("no result")
                )
            }
            (Pending::Acknowledge, Ok(_)) => "acknowledged".to_string(),
            (_, Err(error)) => format!("{:?} failed: {error}", prompt.pending),
        });
    }

    async fn transition(&mut self, client: &ApiClient, transition: &str) {
        self.message = Some(match client.transition(transition).await {
            Ok(value) => format!("scheduler mode → {}", value["mode"].as_str().unwrap_or("?")),
            Err(error) => format!("{transition} failed: {error}"),
        });
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let client = ApiClient::new(&cli.api, cli.token);
    let mut app = App {
        operator: cli.operator,
        ..App::default()
    };
    app.refresh(&client).await;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let outcome = run(&mut terminal, &mut app, &client).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome
}

/// The draw / input / refresh loop.
async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    client: &ApiClient,
) -> io::Result<()> {
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind == KeyEventKind::Press
                && !app.handle_key(key.code, key.modifiers, client).await
            {
                return Ok(());
            }
            if app.prompt.is_none() {
                app.refresh(client).await;
                last_refresh = Instant::now();
            }
        }
        // Do not reshuffle the list under a prompt the operator is typing into.
        if app.prompt.is_none() && last_refresh.elapsed() >= Duration::from_secs(1) {
            app.refresh(client).await;
            last_refresh = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn action(id: &str, runbook: &str, status: &str, approval: &str) -> api::Action {
        api::Action {
            action_run_id: id.into(),
            runbook_id: runbook.into(),
            target_ids: vec!["redis-mq".into()],
            status: status.into(),
            approval: approval.into(),
            reason: "queue is stuck".into(),
            denial: None,
            review: None,
            verification_summary: None,
        }
    }

    /// The inbox screen renders every category with its affordances.
    #[test]
    fn inbox_screen_renders_categories() {
        let mut app = App {
            screen: Screen::Inbox,
            ..App::default()
        };
        app.status.inbox.total = 3;
        let mut denied = action("01a0-denied", "mode.set", "cancelled", "rejected");
        denied.denial = Some(api::Denial {
            source: "policy".into(),
            reason: "row 26 is human-only".into(),
            comment: None,
            decided_by: None,
        });
        app.set_inbox(Inbox {
            permission_requests: vec![action(
                "01a0-test",
                "mq.purge",
                "waiting_for_approval",
                "pending",
            )],
            permission_denied: vec![denied],
            failed_jobs: vec![api::JobRow {
                job_id: "01a0-job-failed".into(),
                issue_id: "issue".into(),
                status: "failed".into(),
                result: Some(api::JobResultRow {
                    summary: "the model ended without calling submit_diagnosis".into(),
                }),
            }],
            failed_actions: Vec::new(),
        });
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("mq.purge"));
        assert!(rendered.contains("waiting_for_approval"));
        assert!(rendered.contains("queue is stuck"));
        assert!(rendered.contains("denied"));
        assert!(rendered.contains("failed job"));
        assert!(rendered.contains("inbox 3"));
    }

    /// A prompt captures typed text and Esc cancels it.
    #[test]
    fn prompt_collects_text() {
        let mut app = App::default();
        app.set_inbox(Inbox {
            permission_requests: vec![action(
                "01a0-test",
                "mq.purge",
                "waiting_for_approval",
                "pending",
            )],
            ..Inbox::default()
        });
        app.open_prompt(Pending::Reject);
        let prompt = app.prompt.as_mut().unwrap();
        prompt.buffer.push_str("too risky");
        assert!(app.prompt.as_ref().unwrap().label().starts_with("reject"));
        app.open_prompt(Pending::SendUpstream);
        // Send-upstream does not apply to a request; the reject prompt stays.
        assert_eq!(app.prompt.as_ref().unwrap().pending, Pending::Reject);
    }
}
