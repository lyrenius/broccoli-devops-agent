//! The seven screens: each owns its state, its keys, and its drawing.

pub mod events;
pub mod inbox;
pub mod overview;
pub mod records;
pub mod report;
pub mod settings;
pub mod trace;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::{App, Screen};

/// Offers a key to the active screen; true when it took it.
pub fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    match app.screen {
        Screen::Overview => overview::key(app, code, mods),
        Screen::Inbox => inbox::key(app, code, mods),
        Screen::Records => records::key(app, code, mods),
        Screen::Trace => trace::key(app, code, mods),
        Screen::Events => events::key(app, code, mods),
        Screen::Report => report::key(app, code, mods),
        Screen::Settings => settings::key(app, code, mods),
    }
}

/// Draws the active screen.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    match app.screen {
        Screen::Overview => overview::draw(frame, area, app),
        Screen::Inbox => inbox::draw(frame, area, app),
        Screen::Records => records::draw(frame, area, app),
        Screen::Trace => trace::draw(frame, area, app),
        Screen::Events => events::draw(frame, area, app),
        Screen::Report => report::draw(frame, area, app),
        Screen::Settings => settings::draw(frame, area, app),
    }
}

/// The active screen's key hints for the footer.
pub fn hints(app: &App) -> &'static str {
    match app.screen {
        Screen::Overview => overview::HINTS,
        Screen::Inbox => inbox::HINTS,
        Screen::Records => records::HINTS,
        Screen::Trace => trace::HINTS,
        Screen::Events => events::HINTS,
        Screen::Report => report::hints(app),
        Screen::Settings => settings::HINTS,
    }
}

/// Moves a selection by a signed amount inside a list of `len`.
pub fn step(selected: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = selected.unwrap_or(0).min(len - 1) as isize;
    Some((current + delta).clamp(0, len as isize - 1) as usize)
}
