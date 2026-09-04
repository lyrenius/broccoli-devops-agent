//! Terminal operator console for the Broccoli DevOps Agent.
//!
//! A pure client of the control plane's HTTP API: it refreshes status, the latest Snapshot,
//! actions, issues, and the event tail once a second, and lets an operator approve or reject held
//! actions, capture a Snapshot, and freeze or resume the Scheduler. It has no state of its own and
//! no access to machines — everything it can do, the API and therefore the authority matrix allow.

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

use api::{Action, ApiClient, EventRow, IssueRow, ResourceRow, Status};
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
}

/// Everything the screens render.
#[derive(Debug, Default)]
pub struct App {
    /// Active screen.
    pub screen: Screen,
    /// Latest `/api/status`.
    pub status: Status,
    /// Latest Snapshot resources.
    pub resources: Vec<ResourceRow>,
    /// All ActionRuns, waiting ones first.
    pub actions: Vec<Action>,
    /// All Issues.
    pub issues: Vec<IssueRow>,
    /// Event tail.
    pub events: Vec<EventRow>,
    /// Selected action index.
    pub selected: usize,
    /// Transient footer message (last error or confirmation).
    pub message: Option<String>,
}

impl App {
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
        if let Ok(mut actions) = client.actions().await {
            actions.sort_by_key(|a| a.status != "waiting_for_approval");
            self.actions = actions;
            if self.selected >= self.actions.len() {
                self.selected = self.actions.len().saturating_sub(1);
            }
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
        self.message = None;
        match key {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('1') => self.screen = Screen::Overview,
            KeyCode::Char('2') => self.screen = Screen::Actions,
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
                if self.selected + 1 < self.actions.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('a') => self.decide(client, true).await,
            KeyCode::Char('r') => self.decide(client, false).await,
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

    async fn decide(&mut self, client: &ApiClient, approve: bool) {
        let Some(action) = self.actions.get(self.selected) else {
            self.message = Some("no action selected".to_string());
            return;
        };
        if action.status != "waiting_for_approval" {
            self.message = Some(format!(
                "action is {}, not waiting for approval",
                action.status
            ));
            return;
        }
        let id = action.action_run_id.clone();
        let result = if approve {
            client.approve(&id).await
        } else {
            client.reject(&id).await
        };
        self.message = Some(match result {
            Ok(value) => format!(
                "{} → {}",
                if approve { "approved" } else { "rejected" },
                value["status"].as_str().unwrap_or("?")
            ),
            Err(error) => format!("decision failed: {error}"),
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
    let mut app = App::default();
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
            app.refresh(client).await;
            last_refresh = Instant::now();
        }
        if last_refresh.elapsed() >= Duration::from_secs(1) {
            app.refresh(client).await;
            last_refresh = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// The actions screen renders a waiting action with its inbox affordances.
    #[test]
    fn actions_screen_renders_inbox() {
        let mut app = App {
            screen: Screen::Actions,
            ..App::default()
        };
        app.status.counts.actions_waiting = 1;
        app.actions.push(Action {
            action_run_id: "01a0-test".into(),
            runbook_id: "mq.purge".into(),
            target_ids: vec!["redis-mq".into()],
            status: "waiting_for_approval".into(),
            approval: "pending".into(),
            reason: "queue is stuck".into(),
            verification_summary: None,
        });
        let backend = TestBackend::new(120, 24);
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
        assert!(rendered.contains("inbox 1"));
    }
}
