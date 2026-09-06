//! Shared rendering: the frame around every screen (header, banner, footer, help overlay) and
//! the text-document helper the detail panes are built from.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs};

use crate::app::{App, Banner, Screen};
use crate::format::{self, duration_secs, label, tone, wrap};
use crate::screens;

/// Draws the whole frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let banner_height = app.banner.as_ref().map_or(0, |banner| {
        banner_lines(banner, area.width).len() as u16 + 2
    });
    let areas = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(banner_height),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .split(area);
    draw_header(frame, areas[0], app);
    if let Some(banner) = &app.banner {
        draw_banner(frame, areas[1], banner);
    }
    screens::draw(frame, areas[2], app);
    draw_footer(frame, areas[3], app);
    if app.help {
        draw_help(frame, area, app);
    }
}

/// Tabs on the first line, the deployment's state on the second.
fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![
            Span::styled(
                " broccoli ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ops console "),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    let titles: Vec<Line> = Screen::ALL
        .iter()
        .map(|screen| {
            let mut spans = vec![Span::raw(screen.title())];
            if *screen == Screen::Inbox {
                let total = app.status.as_ref().map_or(0, |s| s.inbox.total);
                if total > 0 {
                    spans.push(Span::styled(
                        format!(" {total} "),
                        Style::default().fg(Color::Black).bg(Color::Green),
                    ));
                }
            }
            Line::from(spans)
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.screen.index())
        .highlight_style(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, rows[0]);

    let line = match (&app.status, &app.api_error) {
        (_, Some(error)) => Line::from(Span::styled(
            format!("✗ API unreachable at {}: {error}", app.api_url),
            Style::default().fg(Color::Red),
        )),
        (None, None) => Line::from(Span::styled(
            format!("connecting to {}…", app.api_url),
            Style::default().fg(Color::DarkGray),
        )),
        (Some(status), None) => {
            let mut spans = vec![
                Span::styled(
                    format!("● {}", label(&status.mode)),
                    Style::default()
                        .fg(tone(&status.mode))
                        .add_modifier(Modifier::BOLD),
                ),
                sep(),
                if status.dry_run {
                    Span::styled("Platform dry-run", Style::default().fg(Color::Yellow))
                } else {
                    Span::styled(
                        "Platform LIVE",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )
                },
                sep(),
                Span::raw(format!(
                    "{} ({})",
                    status.deployment.name,
                    label(&status.deployment.operation_mode)
                )),
                sep(),
                Span::raw(format!("topology {}", status.deployment.topology_revision)),
                sep(),
                Span::styled(
                    status.team_backend.clone(),
                    Style::default().fg(Color::DarkGray),
                ),
                sep(),
                Span::styled(
                    format!("up {}", duration_secs(status.uptime_secs)),
                    Style::default().fg(Color::DarkGray),
                ),
                sep(),
                Span::styled(
                    format!("operator {}", app.operator),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if let Some(recovery) = &status.recovery
                && status.mode != "running"
                && recovery.needs_attention()
            {
                spans.push(sep());
                spans.push(Span::styled(
                    format!(
                        "recovered from a restart ({} interrupted)",
                        recovery.interrupted()
                    ),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Line::from(spans)
        }
    };
    frame.render_widget(Paragraph::new(line), rows[1]);
}

fn banner_lines(banner: &Banner, width: u16) -> Vec<String> {
    let width = usize::from(width).saturating_sub(4).max(10);
    banner
        .lines
        .iter()
        .flat_map(|line| wrap(line, width))
        .collect()
}

fn draw_banner(frame: &mut Frame, area: Rect, banner: &Banner) {
    let lines: Vec<Line> = banner_lines(banner, area.width)
        .into_iter()
        .map(|line| Line::from(Span::raw(line)))
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(banner.tone))
        .title(Span::styled(
            " notice (Esc dismisses) ",
            Style::default().fg(banner.tone),
        ));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Key hints (or the active prompt) on the first line, the last message on the second.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let first = match &app.prompt {
        Some(prompt) => Line::from(vec![
            Span::styled(
                format!("{} ", prompt.label),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(prompt.buffer.clone()),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
            Span::styled(
                "   (Enter submits · Esc cancels · Ctrl-U clears)",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        None => Line::from(Span::styled(
            format!("{} · 1-7 screens · ? help · q quit", screens::hints(app)),
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(first), rows[0]);

    let second = if let Some(message) = &app.message {
        Line::from(Span::styled(
            message.clone(),
            Style::default().fg(Color::Yellow),
        ))
    } else if let Some(busy) = app.busy.last() {
        Line::from(Span::styled(
            format!("⏳ {busy}…"),
            Style::default().fg(Color::Cyan),
        ))
    } else {
        Line::from("")
    };
    frame.render_widget(Paragraph::new(second), rows[1]);
}

/// The key reference, over everything: two columns, wrapped, so it fits a normal terminal.
fn draw_help(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered(area, 90, 96);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green))
        .title(format!(" Keys · {} · Esc closes ", app.screen.title()));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
    let width = usize::from(columns[0].width).saturating_sub(2);
    let section = |doc: &mut Doc, title: &str, keys: &[(&str, &str)]| {
        doc.heading(title);
        for (key, what) in keys {
            let mut first = true;
            for line in wrap(what, doc.width().saturating_sub(26)) {
                let head = if first {
                    Span::styled(format!("  {key:<24}"), Style::default().fg(Color::Green))
                } else {
                    Span::raw(" ".repeat(26))
                };
                first = false;
                doc.line(vec![head, Span::raw(line)]);
            }
        }
        doc.blank();
    };
    let mut left = Doc::new(width);
    section(
        &mut left,
        "Everywhere",
        &[
            ("1-7, Tab", "switch screens (Shift-Tab goes back)"),
            ("s", "capture a Snapshot now"),
            (
                "f / F / u",
                "freeze dispatch / freeze all / resume the Scheduler",
            ),
            (
                "c",
                "interrupt the running pass (the selected one on the Overview)",
            ),
            (
                "Esc",
                "dismiss the notice, leave the trace, or leave the report form",
            ),
            ("?", "this reference"),
            ("q, Ctrl-C", "quit"),
        ],
    );
    section(
        &mut left,
        "Overview",
        &[
            ("j / k", "select a running pass"),
            ("Enter, t", "open the selected pass's trace"),
        ],
    );
    section(
        &mut left,
        "Inbox",
        &[
            ("j / k, g / G", "select an item"),
            ("[ / ], ← / →", "filter: all, requests, denied, failed"),
            ("a", "approve the selected request"),
            ("r", "reject it, with a comment"),
            (
                "b",
                "send the selected denial or failure back upstream, with feedback",
            ),
            ("x", "acknowledge it, with an optional comment"),
            (
                "h",
                "switch between what waits and the history of decided actions",
            ),
            ("Enter, t", "open the item's Issue in the trace"),
            ("PgUp / PgDn", "scroll the detail pane"),
        ],
    );
    section(
        &mut left,
        "Issues & jobs",
        &[
            ("j / k, g / G", "select an Issue"),
            ("[ / ], ← / →", "filter: all, open, closed, archives"),
            ("/", "search title, description, or ID (empty clears)"),
            ("Enter, t", "open the Issue's trace"),
            ("e", "export the session to a JSON file"),
            ("i", "import a session file as a read-only archive"),
            ("R / C", "resolve / cancel the Issue, with a comment"),
            ("PgUp / PgDn", "scroll the detail pane"),
        ],
    );
    let mut right = Doc::new(width);
    section(
        &mut right,
        "Trace",
        &[
            ("← / →, h / l", "select a pass in the chain"),
            ("j / k, g / G", "select a transcript entry"),
            ("Enter", "expand or fold the selected entry"),
            (
                "v / I",
                "show the Snapshot View / the instructions the run started with",
            ),
            ("J / K", "scroll the result, actions, and events pane"),
            ("e", "export the session"),
            ("Esc, Backspace", "back to Issues & jobs"),
        ],
    );
    section(
        &mut right,
        "Events",
        &[
            ("j / k, PgUp / PgDn", "move (leaves follow mode)"),
            ("G", "jump to the newest event and follow"),
            ("Enter, t", "open the trace of the event's Issue"),
        ],
    );
    section(
        &mut right,
        "File a report",
        &[
            ("Enter, i", "edit the form; Esc leaves it"),
            ("Tab / ↓, Shift-Tab / ↑", "next / previous field"),
            (
                "Enter",
                "next field (a newline in the description; files on the button)",
            ),
            ("Ctrl-S", "file the report from anywhere in the form"),
            ("t", "open the outcome's trace"),
            ("n", "clear the form"),
        ],
    );
    section(
        &mut right,
        "Settings",
        &[
            ("j / k", "select a setting"),
            (
                "Enter",
                "edit it (toggles a switch; lists are comma-separated)",
            ),
            ("+ / -", "add / remove a runbook"),
            ("w, Ctrl-S", "save the pending changes"),
            ("U", "discard them"),
        ],
    );
    frame.render_widget(Paragraph::new(Text::from(left.lines)), columns[0]);
    frame.render_widget(Paragraph::new(Text::from(right.lines)), columns[1]);
}

/// A rectangle centered in `area`, as percentages of it.
pub fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area)[1];
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical)[1]
}

/* ---- spans ---- */

/// ` · ` in gray.
pub fn sep() -> Span<'static> {
    Span::styled(" · ", Style::default().fg(Color::DarkGray))
}

/// A status, health, mode, or approval value in its color.
pub fn status_span(value: &str) -> Span<'static> {
    Span::styled(
        label(value),
        Style::default()
            .fg(tone(value))
            .add_modifier(Modifier::BOLD),
    )
}

/// A pill: black text on a colored ground.
pub fn badge(text: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default().fg(Color::Black).bg(color),
    )
}

/// Gray text.
pub fn dim(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(Color::DarkGray))
}

/// Bold text.
pub fn bold(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::default().add_modifier(Modifier::BOLD))
}

/// A bordered panel.
pub fn panel(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default().borders(Borders::ALL).title(title)
}

/// A segmented filter, like the web console's: the selected option is highlighted.
pub fn segmented(options: &[(String, usize)], selected: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (name, count)) in options.iter().enumerate() {
        let text = format!(" {name} {count} ");
        spans.push(if index == selected {
            Span::styled(
                text,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(text, Style::default().fg(Color::DarkGray))
        });
    }
    spans
}

/* ---- documents ---- */

/// A column of styled lines, wrapped to a width as it is built, so scrolling it is exact.
pub struct Doc {
    width: usize,
    /// The lines.
    pub lines: Vec<Line<'static>>,
}

impl Doc {
    /// An empty document wrapped to `width` cells.
    pub fn new(width: usize) -> Self {
        Self {
            width: width.max(8),
            lines: Vec::new(),
        }
    }

    /// Number of lines.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Wrap width.
    pub fn width(&self) -> usize {
        self.width
    }

    /// An empty line.
    pub fn blank(&mut self) {
        self.lines.push(Line::from(""));
    }

    /// One line of spans, not wrapped.
    pub fn line(&mut self, spans: Vec<Span<'static>>) {
        self.lines.push(Line::from(spans));
    }

    /// A bold line.
    pub fn heading(&mut self, text: &str) {
        for line in wrap(text, self.width) {
            self.lines.push(Line::from(bold(line)));
        }
    }

    /// Plain wrapped text.
    pub fn text(&mut self, text: &str) {
        self.styled(text, Style::default());
    }

    /// Wrapped text in a style.
    pub fn styled(&mut self, text: &str, style: Style) {
        for line in wrap(text, self.width) {
            self.lines.push(Line::from(Span::styled(line, style)));
        }
    }

    /// Gray wrapped text.
    pub fn note(&mut self, text: &str) {
        self.styled(text, Style::default().fg(Color::DarkGray));
    }

    /// A wrapped alert line with a marker in its tone.
    pub fn alert(&mut self, tone: Color, text: &str) {
        let mut first = true;
        for line in wrap(text, self.width.saturating_sub(2)) {
            let marker = if first { "⚠ " } else { "  " };
            first = false;
            self.lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(tone)),
                Span::styled(line, Style::default().fg(tone)),
            ]));
        }
    }

    /// `key: value`, the value wrapped with a hanging indent.
    pub fn kv(&mut self, key: &str, value: &str) {
        let key_text = format!("{key}: ");
        let indent = format::width(&key_text).min(self.width / 2);
        let value = if value.trim().is_empty() {
            "—"
        } else {
            value
        };
        let mut first = true;
        for line in wrap(value, self.width.saturating_sub(indent)) {
            if first {
                self.lines
                    .push(Line::from(vec![dim(key_text.clone()), Span::raw(line)]));
                first = false;
            } else {
                self.lines.push(Line::from(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::raw(line),
                ]));
            }
        }
    }

    /// `• text` in a style, wrapped with a hanging indent.
    pub fn bullet_styled(&mut self, text: &str, style: Style) {
        let mut first = true;
        for line in wrap(text, self.width.saturating_sub(2)) {
            let marker = if first { "• " } else { "  " };
            first = false;
            self.lines
                .push(Line::from(vec![dim(marker), Span::styled(line, style)]));
        }
    }

    /// Indented wrapped text.
    pub fn indented(&mut self, indent: usize, text: &str, style: Style) {
        for line in wrap(text, self.width.saturating_sub(indent)) {
            self.lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(line, style),
            ]));
        }
    }

    /// A rule with a label, like a section divider.
    pub fn rule(&mut self, text: &str) {
        let text = format!("─ {text} ");
        let rest = self.width.saturating_sub(format::width(&text));
        self.lines.push(Line::from(vec![
            Span::styled(text, Style::default().fg(Color::DarkGray)),
            Span::styled("─".repeat(rest), Style::default().fg(Color::DarkGray)),
        ]));
    }
}

/// Renders a document in a panel, clamping the scroll offset and showing the position.
pub fn render_doc(
    frame: &mut Frame,
    area: Rect,
    doc: &Doc,
    scroll: &mut usize,
    block: Block<'static>,
) {
    let inner = block.inner(area);
    let height = usize::from(inner.height);
    let max = doc.len().saturating_sub(height);
    if *scroll > max {
        *scroll = max;
    }
    let block = if max > 0 {
        block.title_bottom(Line::from(dim(format!(
            " {}-{} of {} (PgUp/PgDn) ",
            *scroll + 1,
            (*scroll + height).min(doc.len()),
            doc.len()
        ))))
    } else {
        block
    };
    let paragraph = Paragraph::new(Text::from(doc.lines.clone()))
        .block(block)
        .scroll((*scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

/// The inner width of a bordered panel of this size.
pub fn inner_width(area: Rect) -> usize {
    usize::from(area.inner(Margin::new(1, 1)).width)
}
