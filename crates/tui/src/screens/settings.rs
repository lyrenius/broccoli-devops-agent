//! Settings: the configurator. Operational knobs change at once; policy changes only while the
//! Scheduler is frozen and are recorded in the operator's name; the rest is edited in the file
//! and needs a restart. Every accepted change is written back into the config file itself.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use serde_json::{Value, json};

use crate::api::SettingsPage;
use crate::app::{App, Outcome, Pending};
use crate::format::{label, pad};
use crate::ui::{Doc, badge, bold, dim, inner_width, panel, render_doc, sep};

/// Footer hints.
pub const HINTS: &str = "j/k select · Enter edit · +/- add/remove runbook · w save · U discard · f freeze dispatch to unlock policy";

/// The label of the save action, so its completion reloads the page.
pub const SAVE_LABEL: &str = "saving settings";

/// When a setting may be changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Any time.
    Live,
    /// While frozen.
    Policy,
    /// Never, at runtime.
    Startup,
}

/// What a row is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A section title; not selectable.
    Header,
    /// A number; `float` when decimals are allowed.
    Number {
        /// Whether decimals are allowed.
        float: bool,
    },
    /// Free text.
    Text,
    /// A switch.
    Bool,
    /// A list of strings, edited comma-separated.
    List,
    /// Shown, never edited here.
    ReadOnly,
}

/// One row of the configurator.
#[derive(Debug, Clone)]
pub struct RowSpec {
    /// Dotted key into the config (numeric segments index arrays).
    pub key: String,
    /// What it is called.
    pub label: String,
    /// A hint.
    pub hint: Option<String>,
    /// When it may change.
    pub class: Class,
    /// What it is.
    pub kind: Kind,
}

/// Settings state.
#[derive(Debug, Default)]
pub struct SettingsState {
    /// The page, once loaded.
    pub page: Option<SettingsPage>,
    /// The config as edited.
    pub draft: Option<Value>,
    /// Why it could not be loaded.
    pub error: Option<String>,
    /// Whether a load is in flight.
    pub loading: bool,
    /// Whether a save is in flight.
    pub saving: bool,
    /// Selection among the rows.
    pub list: ListState,
}

/// The page loaded (or not); the draft starts from what is in force.
pub fn on_page(app: &mut App, result: Result<SettingsPage, String>) {
    app.settings.loading = false;
    match result {
        Ok(page) => {
            app.settings.draft = Some(page.config.clone());
            app.settings.page = Some(page);
            app.settings.error = None;
        }
        Err(error) => app.settings.error = Some(error),
    }
}

/* ---- JSON paths ---- */

/// The value at a dotted key.
pub fn get_path<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.')
        .try_fold(value, |current, segment| match current {
            Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get(i)),
            Value::Object(map) => map.get(segment),
            _ => None,
        })
}

/// Sets the value at a dotted key, creating objects on the way.
pub fn set_path(value: &mut Value, key: &str, new: Value) {
    let segments: Vec<&str> = key.split('.').collect();
    let mut current = value;
    for (i, segment) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match current {
            Value::Array(items) => {
                let Ok(index) = segment.parse::<usize>() else {
                    return;
                };
                if index >= items.len() {
                    return;
                }
                if last {
                    items[index] = new;
                    return;
                }
                current = &mut items[index];
            }
            _ => {
                if !current.is_object() {
                    *current = json!({});
                }
                let map = current.as_object_mut().expect("object");
                if last {
                    map.insert((*segment).to_string(), new);
                    return;
                }
                current = map.entry((*segment).to_string()).or_insert(json!({}));
            }
        }
    }
}

/// The nested object of leaves that differ between `base` and `draft`, in the file's shape.
pub fn changes_between(base: &Value, draft: &Value) -> Option<Value> {
    match (base, draft) {
        (Value::Object(b), Value::Object(d)) if !b.is_empty() || !d.is_empty() => {
            let mut out = serde_json::Map::new();
            let keys: Vec<&String> = b.keys().chain(d.keys()).collect();
            for key in keys {
                if out.contains_key(key) {
                    continue;
                }
                let inner = changes_between(
                    b.get(key).unwrap_or(&Value::Null),
                    d.get(key).unwrap_or(&Value::Null),
                );
                if let Some(inner) = inner {
                    out.insert(key.clone(), inner);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Object(out))
            }
        }
        _ => (base != draft).then(|| draft.clone()),
    }
}

/// How many leaves a change object has.
pub fn count_leaves(value: &Value) -> usize {
    match value {
        Value::Object(map) if !map.is_empty() => map.values().map(count_leaves).sum(),
        _ => 1,
    }
}

/// A value as the row shows it.
fn value_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "—".to_string(),
        Some(Value::Bool(true)) => "on".to_string(),
        Some(Value::Bool(false)) => "off".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => {
            let parts: Vec<String> = items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect();
            if parts.is_empty() {
                "(none)".to_string()
            } else {
                parts.join(", ")
            }
        }
        Some(other) => other.to_string(),
    }
}

/// The rows, from the draft.
pub fn rows(draft: &Value, page: &SettingsPage, mode: &str) -> Vec<RowSpec> {
    let class_of = |key: &str| -> Class {
        let classes = &page.classes;
        if classes.live.iter().any(|k| k == key) {
            Class::Live
        } else if classes.policy.iter().any(|k| k == key) {
            Class::Policy
        } else if classes.startup.iter().any(|k| k == key) {
            Class::Startup
        } else if key.starts_with("platform.") {
            Class::Policy
        } else if key.starts_with("collector.")
            || key.starts_with("budget.")
            || key.starts_with("model.pricing")
        {
            Class::Live
        } else {
            Class::Startup
        }
    };
    let row = |key: &str, label: &str, hint: Option<&str>, kind: Kind| RowSpec {
        key: key.to_string(),
        label: label.to_string(),
        hint: hint.map(str::to_string),
        class: class_of(key),
        kind,
    };
    let header = |text: &str, class: Class| RowSpec {
        key: String::new(),
        label: text.to_string(),
        hint: None,
        class,
        kind: Kind::Header,
    };
    let int = Kind::Number { float: false };
    let float = Kind::Number { float: true };
    let mut out = vec![
        header(
            "Operational — how much the agent does and what it may spend. Any operator may change these; the next pass picks them up.",
            Class::Live,
        ),
        row(
            "collector.snapshot_interval_secs",
            "Snapshot every (seconds)",
            Some("0 = off"),
            int.clone(),
        ),
        row(
            "agent.max_auto_passes",
            "Automatic passes per report or send-back",
            None,
            int.clone(),
        ),
    ];
    let has_model = draft.get("model").is_some_and(Value::is_object);
    if has_model {
        out.push(row(
            "model.max_inspections",
            "Inspections per pass",
            Some("0 = off"),
            int.clone(),
        ));
        out.push(row(
            "model.max_model_turns",
            "Model turns per pass",
            None,
            int.clone(),
        ));
        out.push(row(
            "model.max_tool_calls",
            "Tool calls per pass",
            None,
            int.clone(),
        ));
        out.push(row(
            "model.max_tokens_per_run",
            "Tokens per pass",
            Some("0 = no limit"),
            int.clone(),
        ));
        out.push(row(
            "model.pricing.input_per_mtok",
            "Price list: input, per million tokens",
            None,
            float.clone(),
        ));
        out.push(row(
            "model.pricing.cached_input_per_mtok",
            "Price list: cached input, per million tokens",
            None,
            float.clone(),
        ));
        out.push(row(
            "model.pricing.output_per_mtok",
            "Price list: output, per million tokens",
            None,
            float.clone(),
        ));
        out.push(row(
            "model.pricing.currency",
            "Price list: currency",
            None,
            Kind::Text,
        ));
    } else {
        out.push(RowSpec {
            key: "model".to_string(),
            label: "No [model] section: the deterministic Team runs. Add one in the file and restart to use the model relay.".to_string(),
            hint: None,
            class: Class::Startup,
            kind: Kind::ReadOnly,
        });
    }
    out.push(row(
        "budget.max_total_tokens",
        "Spend ceiling, tokens",
        Some("0 = off · reaching either ceiling freezes the Scheduler until raised and resumed"),
        int.clone(),
    ));
    out.push(row(
        "budget.max_total_cost",
        "Spend ceiling, cost",
        Some("0 = off"),
        float,
    ));

    let frozen = matches!(mode, "dispatch_frozen" | "fully_frozen");
    out.push(header(
        &format!(
            "Policy — what executes on real machines and which matrix row a proposal lands in. {}",
            if frozen {
                format!("Editable: the Scheduler is {}.", label(mode))
            } else {
                format!("Locked: changes only while the Scheduler is frozen, so nothing is mid-flight when the rules change; it is {} now (f freezes dispatch).", label(mode))
            }
        ),
        Class::Policy,
    ));
    out.push(row(
        "platform.dry_run",
        "Dry-run: render and record commands, never execute them",
        Some("Enter toggles; turning it off asks for a confirmation when saving"),
        Kind::Bool,
    ));
    out.push(row(
        "platform.command_timeout_secs",
        "Command timeout (seconds)",
        None,
        int.clone(),
    ));
    out.push(row(
        "platform.auto_repeat_window_secs",
        "Auto-repeat window (seconds)",
        Some("an automatic action repeated inside this window escalates to approval"),
        int,
    ));
    out.push(row(
        "platform.classification.tunable_config_keys",
        "Tunable config keys",
        Some("comma-separated"),
        Kind::List,
    ));
    out.push(row(
        "platform.classification.contest_config_keys",
        "Contest config keys",
        Some("comma-separated"),
        Kind::List,
    ));
    out.push(row(
        "platform.classification.security_config_keys",
        "Security config keys",
        Some("comma-separated"),
        Kind::List,
    ));
    out.push(row(
        "platform.classification.known_internal_address_prefixes",
        "Known internal address prefixes",
        Some("comma-separated"),
        Kind::List,
    ));
    let runbooks = get_path(draft, "platform.runbooks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    out.push(header(
        &format!(
            "Runbook commands ({}): {{target}} is the resource ID; {{arg:name}} a proposal argument. A runbook without a command is refused. + adds one, - removes the selected one.",
            runbooks.len()
        ),
        Class::Policy,
    ));
    for i in 0..runbooks.len() {
        out.push(row(
            &format!("platform.runbooks.{i}.id"),
            &format!("Runbook {} ID", i + 1),
            None,
            Kind::Text,
        ));
        out.push(row(
            &format!("platform.runbooks.{i}.command"),
            &format!("Runbook {} command", i + 1),
            None,
            Kind::Text,
        ));
    }

    out.push(header("Startup only — read once when the process starts. Edit the file and restart to change them.", Class::Startup));
    let read_only = |key: &str, label: &str| RowSpec {
        key: key.to_string(),
        label: label.to_string(),
        hint: None,
        class: Class::Startup,
        kind: Kind::ReadOnly,
    };
    out.push(read_only("agent.language", "Agent output language"));
    out.push(read_only("data.dir", "Data directory"));
    out.push(read_only("topology.path", "Topology file"));
    if has_model {
        out.push(read_only("model.base_url", "Model relay"));
        out.push(read_only("model.model", "Model"));
        out.push(read_only("model.wire_api", "Wire format"));
        out.push(read_only("model.api_key_env", "API key variable"));
        out.push(read_only("model.timeout_secs", "Relay timeout (seconds)"));
    }
    out.push(read_only("api.bind", "API bind address"));
    out.push(read_only("api.token", "API token"));
    out
}

/// The rows of the current draft, or none before it loads.
fn current_rows(app: &App) -> Vec<RowSpec> {
    match (&app.settings.draft, &app.settings.page) {
        (Some(draft), Some(page)) => {
            let mode = app
                .status
                .as_ref()
                .map_or(page.mode.as_str(), |s| s.mode.as_str());
            rows(draft, page, mode)
        }
        _ => Vec::new(),
    }
}

fn frozen(app: &App) -> bool {
    let mode = app
        .status
        .as_ref()
        .map(|s| s.mode.as_str())
        .or(app.settings.page.as_ref().map(|p| p.mode.as_str()))
        .unwrap_or("unknown");
    matches!(mode, "dispatch_frozen" | "fully_frozen")
}

/// Whether the row may be edited now, or why not.
fn editable(app: &App, row: &RowSpec) -> Result<(), String> {
    match row.kind {
        Kind::Header => Err("not a setting".to_string()),
        Kind::ReadOnly => Err("startup-only: edit the file and restart".to_string()),
        _ => match row.class {
            Class::Startup => Err("startup-only: edit the file and restart".to_string()),
            Class::Policy if !frozen(app) => Err(
                "policy changes only while the Scheduler is frozen: press f to freeze dispatch first".to_string(),
            ),
            _ => Ok(()),
        },
    }
}

/// Moves the selection to the next selectable row in a direction.
fn move_selection(app: &mut App, delta: isize) {
    let rows = current_rows(app);
    if rows.is_empty() {
        return;
    }
    let mut index = app
        .settings
        .list
        .selected()
        .unwrap_or(0)
        .min(rows.len() - 1) as isize;
    loop {
        index += delta;
        if index < 0 || index >= rows.len() as isize {
            return;
        }
        if rows[index as usize].kind != Kind::Header {
            app.settings.list.select(Some(index as usize));
            return;
        }
    }
}

fn selected_row(app: &App) -> Option<RowSpec> {
    let rows = current_rows(app);
    app.settings
        .list
        .selected()
        .and_then(|i| rows.get(i).cloned())
}

/// Settings keys.
pub fn key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            if app.settings.list.selected().is_none() {
                app.settings.list.select(Some(0));
            }
            move_selection(app, 1);
        }
        KeyCode::Char('k') | KeyCode::Up => move_selection(app, -1),
        KeyCode::PageDown => {
            for _ in 0..8 {
                move_selection(app, 1);
            }
        }
        KeyCode::PageUp => {
            for _ in 0..8 {
                move_selection(app, -1);
            }
        }
        KeyCode::Enter => edit(app),
        KeyCode::Char('+') | KeyCode::Char('=') => add_runbook(app),
        KeyCode::Char('-') => remove_runbook(app),
        KeyCode::Char('w') => save(app, false),
        KeyCode::Char('s') if mods.contains(KeyModifiers::CONTROL) => save(app, false),
        KeyCode::Char('U') => {
            if let Some(page) = &app.settings.page {
                app.settings.draft = Some(page.config.clone());
                app.message = Some("changes discarded".to_string());
            }
        }
        KeyCode::Char('L') => app.spawn_settings(),
        _ => return false,
    }
    true
}

/// Opens the editor for the selected row.
fn edit(app: &mut App) {
    let Some(row) = selected_row(app) else {
        app.message = Some("select a setting first (j/k)".to_string());
        return;
    };
    if let Err(why) = editable(app, &row) {
        app.message = Some(why);
        return;
    }
    let current = app
        .settings
        .draft
        .as_ref()
        .and_then(|d| get_path(d, &row.key).cloned());
    match row.kind {
        Kind::Bool => {
            let next = !current.as_ref().and_then(Value::as_bool).unwrap_or(false);
            if let Some(draft) = &mut app.settings.draft {
                set_path(draft, &row.key, Value::Bool(next));
            }
            app.message = Some(format!(
                "{} → {}{}",
                row.label,
                if next { "on" } else { "off" },
                if row.key == "platform.dry_run" && !next {
                    " (saving asks for a confirmation)"
                } else {
                    ""
                }
            ));
        }
        _ => {
            let initial = match current.as_ref() {
                Some(Value::Null) | None => String::new(),
                Some(value) => value_text(Some(value)),
            };
            let what = match row.kind {
                Kind::List => " (comma-separated)",
                Kind::Number { float: true } => " (a number)",
                Kind::Number { float: false } => " (a whole number)",
                _ => "",
            };
            app.open_prompt(
                Pending::Setting {
                    key: row.key.clone(),
                },
                format!("{}{what}:", row.label),
                initial,
            );
        }
    }
}

/// Applies a typed value to the draft.
pub fn apply_edit(app: &mut App, key: &str, text: &str) {
    let rows = current_rows(app);
    let Some(row) = rows.iter().find(|r| r.key == key) else {
        return;
    };
    let base = app
        .settings
        .page
        .as_ref()
        .and_then(|p| get_path(&p.config, key).cloned())
        .unwrap_or(Value::Null);
    let value = match &row.kind {
        Kind::Number { float } => {
            let allow_float = *float || base.is_f64();
            if allow_float {
                match text.parse::<f64>() {
                    Ok(n) if n.is_finite() && n >= 0.0 => json!(n),
                    _ => {
                        app.message = Some(format!("{}: `{text}` is not a number", row.label));
                        return;
                    }
                }
            } else {
                match text.parse::<u64>() {
                    Ok(n) => json!(n),
                    Err(_) => {
                        app.message =
                            Some(format!("{}: `{text}` is not a whole number", row.label));
                        return;
                    }
                }
            }
        }
        Kind::List => Value::Array(
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect(),
        ),
        _ => Value::String(text.to_string()),
    };
    if let Some(draft) = &mut app.settings.draft {
        // A price list that did not exist yet needs its other fields before the value lands.
        if key.starts_with("model.pricing.")
            && !get_path(draft, "model.pricing").is_some_and(Value::is_object)
        {
            set_path(
                draft,
                "model.pricing",
                json!({ "input_per_mtok": 0.0, "cached_input_per_mtok": null, "output_per_mtok": 0.0, "currency": "USD" }),
            );
        }
        set_path(draft, key, value.clone());
    }
    app.message = Some(format!(
        "{} → {} (w saves)",
        row.label,
        value_text(Some(&value))
    ));
}

fn add_runbook(app: &mut App) {
    if !frozen(app) {
        app.message =
            Some("policy changes only while the Scheduler is frozen: press f first".to_string());
        return;
    }
    let Some(draft) = &mut app.settings.draft else {
        return;
    };
    if !get_path(draft, "platform.runbooks").is_some_and(Value::is_array) {
        set_path(draft, "platform.runbooks", json!([]));
    }
    if let Some(items) = draft
        .get_mut("platform")
        .and_then(|p| p.get_mut("runbooks"))
        .and_then(Value::as_array_mut)
    {
        items.push(json!({ "id": "", "command": "" }));
        let count = items.len();
        app.message = Some(format!(
            "runbook {count} added: set its ID and command, then w saves"
        ));
    }
}

fn remove_runbook(app: &mut App) {
    if !frozen(app) {
        app.message =
            Some("policy changes only while the Scheduler is frozen: press f first".to_string());
        return;
    }
    let Some(row) = selected_row(app) else {
        return;
    };
    let Some(index) = row
        .key
        .strip_prefix("platform.runbooks.")
        .and_then(|rest| rest.split('.').next())
        .and_then(|i| i.parse::<usize>().ok())
    else {
        app.message = Some("select a runbook row first".to_string());
        return;
    };
    if let Some(items) = app
        .settings
        .draft
        .as_mut()
        .and_then(|d| d.get_mut("platform"))
        .and_then(|p| p.get_mut("runbooks"))
        .and_then(Value::as_array_mut)
        && index < items.len()
    {
        let removed = items.remove(index);
        app.message = Some(format!(
            "runbook {} removed (w saves)",
            removed["id"].as_str().unwrap_or("?")
        ));
        move_selection(app, -1);
    }
}

/// The pending changes, in the file's shape.
fn pending(app: &App) -> Option<Value> {
    let page = app.settings.page.as_ref()?;
    let draft = app.settings.draft.as_ref()?;
    changes_between(&page.config, draft)
}

/// Saves the pending changes; turning dry-run off asks for a confirmation first.
pub fn save(app: &mut App, confirmed: bool) {
    if app.settings.saving {
        app.message = Some("a save is already in flight".to_string());
        return;
    }
    let Some(changes) = pending(app) else {
        app.message = Some("nothing to save".to_string());
        return;
    };
    let turning_live =
        app.settings.page.as_ref().is_some_and(|p| {
            get_path(&p.config, "platform.dry_run").and_then(Value::as_bool) == Some(true)
        }) && app.settings.draft.as_ref().is_some_and(|d| {
            get_path(d, "platform.dry_run").and_then(Value::as_bool) == Some(false)
        });
    if turning_live && !confirmed {
        app.open_prompt(
            Pending::ConfirmLive,
            "Turning dry-run OFF makes every approved action execute on the real machines named in the topology. Type yes to continue:",
            "",
        );
        return;
    }
    app.settings.saving = true;
    let client = app.client.clone();
    let by = app.operator.clone();
    let count = count_leaves(&changes);
    app.run(SAVE_LABEL, async move {
        match client.update_settings(&by, changes, turning_live).await {
            Ok(outcome) => {
                let keys: Vec<String> = outcome.changes.iter().map(|c| c.key.clone()).collect();
                Outcome::message(format!(
                    "saved {} change(s): {}",
                    outcome.changes.len().max(count.min(1)),
                    keys.join(", ")
                ))
            }
            Err(error) => Outcome::error(format!("not saved: {error}")),
        }
    });
}

/// The save finished: the page is re-read so the draft starts from what is now in force.
pub fn on_saved(app: &mut App, succeeded: bool) {
    app.settings.saving = false;
    if succeeded {
        app.spawn_settings();
    }
}

/// Draws the Settings screen.
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows = Layout::vertical([Constraint::Min(6), Constraint::Length(6)]).split(area);
    let specs = current_rows(app);
    let (page, draft) = match (&app.settings.page, &app.settings.draft) {
        (Some(page), Some(draft)) => (page, draft),
        _ => {
            let mut doc = Doc::new(inner_width(area));
            match &app.settings.error {
                Some(error) => doc.alert(Color::Red, error),
                None => doc.note("Loading the configuration…"),
            }
            let mut scroll = 0;
            render_doc(frame, area, &doc, &mut scroll, panel(" Settings "));
            return;
        }
    };
    if app
        .settings
        .list
        .selected()
        .is_none_or(|i| i >= specs.len())
    {
        app.settings
            .list
            .select(specs.iter().position(|r| r.kind != Kind::Header));
    }
    let frozen_now = frozen(app);
    let width = inner_width(rows[0]);
    let items: Vec<ListItem> = specs
        .iter()
        .map(|row| {
            if row.kind == Kind::Header {
                let color = match row.class {
                    Class::Live => Color::Green,
                    Class::Policy => {
                        if frozen_now {
                            Color::Green
                        } else {
                            Color::Yellow
                        }
                    }
                    Class::Startup => Color::DarkGray,
                };
                let lines: Vec<Line> = crate::format::wrap(&row.label, width.saturating_sub(2))
                    .into_iter()
                    .map(|l| {
                        Line::from(Span::styled(
                            l,
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ))
                    })
                    .collect();
                return ListItem::new(lines);
            }
            let current = get_path(draft, &row.key);
            let base = get_path(&page.config, &row.key);
            let changed = current != base;
            let mut value = value_text(current);
            if row.key == "model.api_key_env" {
                let present =
                    get_path(&page.config, "model.api_key_present").and_then(Value::as_bool);
                value.push_str(match present {
                    Some(true) => "  (set in the environment)",
                    Some(false) => "  (NOT set in the environment)",
                    None => "",
                });
            }
            if row.key == "api.token" {
                value = if value == "—" || value.is_empty() {
                    "none (localhost only)".to_string()
                } else {
                    "set".to_string()
                };
            }
            let locked = editable(app, row).is_err();
            let marker = if changed { "* " } else { "  " };
            let label_width = 46;
            let mut spans = vec![
                Span::styled(marker, Style::default().fg(Color::Yellow)),
                if locked {
                    dim(pad(&row.label, label_width))
                } else {
                    Span::raw(pad(&row.label, label_width))
                },
                Span::raw(" "),
                Span::styled(
                    pad(&value, width.saturating_sub(label_width + 16)),
                    if changed {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else if locked {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default()
                    },
                ),
            ];
            spans.push(match (row.class, row.kind == Kind::ReadOnly) {
                (_, true) | (Class::Startup, _) => badge("restart", Color::DarkGray),
                (Class::Policy, _) if !frozen_now => badge("locked", Color::Yellow),
                (Class::Policy, _) => badge("policy", Color::Green),
                (Class::Live, _) => badge("live", Color::Green),
            });
            ListItem::new(Line::from(spans))
        })
        .collect();
    let pending_count = pending(app).map_or(0, |c| count_leaves(&c));
    let mut title = vec![Span::raw(" Settings ")];
    if pending_count > 0 {
        title.push(badge(
            &format!("{pending_count} pending change(s) · w saves · U discards"),
            Color::Yellow,
        ));
    } else {
        title.push(dim("nothing to save"));
    }
    title.push(sep());
    title.push(dim(match &page.path {
        Some(path) => format!("changes are written back to {path}, comments kept "),
        None => "no config file is attached to this process: changes apply until it restarts "
            .to_string(),
    }));
    let list = List::new(items)
        .block(panel(Line::from(title)))
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("▶");
    frame.render_stateful_widget(list, rows[0], &mut app.settings.list);

    let mut doc = Doc::new(inner_width(rows[1]));
    if let Some(error) = &app.settings.error {
        doc.alert(Color::Red, error);
    }
    match selected_row(app) {
        Some(row) if row.kind != Kind::Header => {
            let mut line = vec![bold(row.label.clone()), sep(), dim(row.key.clone())];
            if let Some(base) = get_path(&page.config, &row.key)
                && get_path(draft, &row.key) != Some(base)
            {
                line.push(sep());
                line.push(Span::styled(
                    format!("was {}", value_text(Some(base))),
                    Style::default().fg(Color::Yellow),
                ));
            }
            doc.line(line);
            if let Some(hint) = &row.hint {
                doc.note(hint);
            }
            doc.note(match editable(app, &row) {
                Ok(()) => match row.kind {
                    Kind::Bool => "Enter toggles it.",
                    _ => "Enter edits it.",
                },
                Err(_) => match row.class {
                    Class::Policy => "Locked: policy changes only while the Scheduler is frozen (f freezes dispatch).",
                    _ => "Read-only here: edit the file and restart.",
                },
            });
        }
        _ => doc.note("Select a setting with j/k."),
    }
    let mut scroll = 0;
    render_doc(
        frame,
        rows[1],
        &doc,
        &mut scroll,
        panel(" Selected setting "),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_and_write_nested_values_and_arrays() {
        let mut value = json!({ "platform": { "runbooks": [{ "id": "a", "command": "x" }] } });
        assert_eq!(
            get_path(&value, "platform.runbooks.0.id"),
            Some(&json!("a"))
        );
        set_path(&mut value, "platform.runbooks.0.command", json!("y"));
        set_path(&mut value, "budget.max_total_cost", json!(2.5));
        assert_eq!(value["platform"]["runbooks"][0]["command"], "y");
        assert_eq!(value["budget"]["max_total_cost"], 2.5);
    }

    #[test]
    fn changes_are_the_differing_leaves_in_the_files_shape() {
        let base = json!({ "agent": { "max_auto_passes": 3, "language": "en" }, "budget": { "max_total_cost": 0.0 }, "platform": { "runbooks": [] } });
        let mut draft = base.clone();
        set_path(&mut draft, "agent.max_auto_passes", json!(5));
        set_path(
            &mut draft,
            "platform.runbooks",
            json!([{ "id": "r", "command": "c" }]),
        );
        let changes = changes_between(&base, &draft).unwrap();
        assert_eq!(
            changes,
            json!({ "agent": { "max_auto_passes": 5 }, "platform": { "runbooks": [{ "id": "r", "command": "c" }] } })
        );
        assert_eq!(count_leaves(&changes), 2);
        assert!(changes_between(&base, &base).is_none());
    }
}
