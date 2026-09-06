//! Terminal operator console for the Broccoli DevOps Agent.
//!
//! A pure client of the control plane's HTTP API with the same reach as the web console: the
//! Overview with the latest Snapshot, what runs now, and what it cost; the Inbox with its
//! three categories and the history of decided actions; Issues & jobs with filters, search,
//! export, and import; the Trace of an Issue's pass chain, transcript by transcript, live while
//! a pass runs; the live event log; filing a report and watching it run; and the Settings
//! configurator. It has no state of its own and no access to machines — everything it can do,
//! the API and therefore the authority matrix allow, and every decision is recorded in the
//! operator's name (`--as`).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod api;
mod app;
mod format;
mod screens;
#[cfg(test)]
mod tests;
mod ui;

use std::io;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use api::ApiClient;
use app::{App, Msg};

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

#[tokio::main]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let client = ApiClient::new(&cli.api, cli.token);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(client, cli.operator, cli.api, tx);

    // A panic must not leave the terminal in raw mode with the alternate screen up.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let outcome = run(&mut terminal, &mut app, &mut rx).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome
}

/// The draw / input / message loop. Reads never block it: every call runs in a task and
/// reports back through the channel.
async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<Msg>,
) -> io::Result<()> {
    loop {
        app.tick();
        terminal.draw(|frame| ui::draw(frame, app))?;
        while let Ok(msg) = rx.try_recv() {
            app.handle_msg(msg);
        }
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && !app.handle_key(key)
        {
            return Ok(());
        }
    }
}
