//! Settings that may change while the control plane runs, and the configurator behind the
//! console's Settings page.
//!
//! The config file is read once at startup, and most of it wires components that cannot be
//! rebuilt mid-incident: the store, the topology, the relay client, the listener. The rest is
//! held here, behind one lock every consumer reads at the moment it needs a value — the runner
//! at the start of a pass, the Platform when it executes, the authority matrix when it decides —
//! so a change applies to the next decision without restarting anything that is in flight.
//!
//! Three classes of setting, decided by [`classify`]:
//!
//! - **Live**: operational knobs — the capture cadence, pass and run budgets, prices, the spend
//!   ceiling. They change how much the agent does, never what it may do; any operator may change
//!   them at any time.
//! - **Policy**: what executes on machines and which matrix row a proposal lands in — `dry_run`,
//!   timeouts, the classification lists, the runbook commands. Changed only while the Scheduler
//!   is frozen, always under a name, and turning dry-run off needs an explicit confirmation.
//! - **Startup**: everything else. The Settings page shows it read-only; the file is edited by
//!   hand and the process restarted.
//!
//! Every accepted change is written back into the config file itself, comments kept, so the
//! file stays the one source of truth across restarts, and is recorded as a
//! `human.settings_changed` event with the before-and-after values.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use broccoli_agent_harness::AgentConfig as HarnessBudget;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table};

use crate::config::{AppConfig, ModelConfig};
use crate::domain::NewEvent;
use crate::error::{AgentError, AgentResult};
use crate::platform::PlatformConfig;
use crate::ports::StateStore;
use crate::runner::{PassPolicy, SliceRunner};
use crate::scheduler::SchedulerMode;
use crate::tr;
use crate::usage::{Pricing, SpendBudget};

/// The values a running control plane reads live.
#[derive(Debug, Clone)]
pub struct LiveSettings {
    /// Cadence of periodic Snapshots; zero means off.
    pub snapshot_interval: Duration,
    /// Passes per chain and inspections per pass.
    pub pass_policy: PassPolicy,
    /// Per-run budgets for the model-backed Team.
    pub harness: HarnessBudget,
    /// What the relay charges, when known.
    pub pricing: Option<Pricing>,
    /// The cumulative spend ceiling.
    pub budget: SpendBudget,
    /// Dry-run, timeouts, classification lists, runbook commands.
    pub platform: PlatformConfig,
}

impl LiveSettings {
    /// The live subset of a configuration.
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            snapshot_interval: config
                .collector
                .snapshot_interval()
                .unwrap_or(Duration::ZERO),
            pass_policy: config.pass_policy(),
            harness: config
                .model
                .as_ref()
                .map(ModelConfig::harness_budget)
                .unwrap_or_default(),
            pricing: config
                .model
                .as_ref()
                .and_then(|model| model.pricing.clone()),
            budget: config.budget.clone(),
            platform: config.platform.clone(),
        }
    }
}

impl Default for LiveSettings {
    fn default() -> Self {
        Self::from_config(&AppConfig::default())
    }
}

/// One handle to the live settings, cloned into every component that reads them.
///
/// A plain `std` lock, never held across an await: readers copy what they need and let go.
#[derive(Debug, Clone, Default)]
pub struct SharedSettings(Arc<RwLock<LiveSettings>>);

impl SharedSettings {
    /// Wraps an initial set of values.
    pub fn new(settings: LiveSettings) -> Self {
        Self(Arc::new(RwLock::new(settings)))
    }

    /// A copy of the current values.
    pub fn current(&self) -> LiveSettings {
        self.read(LiveSettings::clone)
    }

    /// Reads under the lock.
    pub fn read<T>(&self, read: impl FnOnce(&LiveSettings) -> T) -> T {
        read(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replaces every value at once.
    pub fn replace(&self, next: LiveSettings) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = next;
    }
}

/// When a setting may be changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingClass {
    /// Any time, by any operator: it changes how much the agent does.
    Live,
    /// Only while the Scheduler is frozen: it changes what may execute.
    Policy,
    /// Only by editing the file and restarting.
    Startup,
}

/// Classifies one setting by its dotted key, e.g. `platform.dry_run`.
pub fn classify(key: &str) -> SettingClass {
    if key.starts_with("collector.")
        || key.starts_with("budget.")
        || key.starts_with("model.pricing")
    {
        return SettingClass::Live;
    }
    match key {
        "agent.max_auto_passes"
        | "model.max_inspections"
        | "model.max_model_turns"
        | "model.max_tool_calls"
        | "model.max_tokens_per_run" => SettingClass::Live,
        _ if key.starts_with("platform.") => SettingClass::Policy,
        _ => SettingClass::Startup,
    }
}

/// One setting that changed: its key, class, and both values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingChange {
    /// Dotted key.
    pub key: String,
    /// Its class.
    pub class: SettingClass,
    /// The value before; `null` when the key was absent.
    pub from: Value,
    /// The value after; `null` when the key is removed.
    pub to: Value,
}

/// What an operator asks to change.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingsRequest {
    /// Who is asking; recorded with the change.
    pub by: String,
    /// A partial configuration object: only the keys to change, in the file's shape.
    pub changes: Value,
    /// Required when the request turns `platform.dry_run` off.
    #[serde(default)]
    pub confirm_live_execution: bool,
}

/// What an accepted request produced.
#[derive(Debug, Clone)]
pub struct SettingsOutcome {
    /// The configuration now in force.
    pub config: AppConfig,
    /// What changed, in key order.
    pub changes: Vec<SettingChange>,
}

/// Leaves of a JSON document by dotted key; arrays and empty objects are leaves.
fn flatten(value: &Value, prefix: &str, out: &mut BTreeMap<String, Value>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, inner) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(inner, &path, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.clone());
        }
    }
}

/// Every setting of a configuration by dotted key, grouped by class — what the Settings page
/// uses to decide which controls to enable.
pub fn classes(config: &AppConfig) -> AgentResult<Value> {
    let mut leaves = BTreeMap::new();
    flatten(&serde_json::to_value(config)?, "", &mut leaves);
    let mut live = Vec::new();
    let mut policy = Vec::new();
    let mut startup = Vec::new();
    for key in leaves.keys() {
        match classify(key) {
            SettingClass::Live => live.push(key.clone()),
            SettingClass::Policy => policy.push(key.clone()),
            SettingClass::Startup => startup.push(key.clone()),
        }
    }
    Ok(json!({ "live": live, "policy": policy, "startup": startup }))
}

/// The settings that differ between two configurations.
pub fn diff(current: &AppConfig, next: &AppConfig) -> AgentResult<Vec<SettingChange>> {
    let mut before = BTreeMap::new();
    flatten(&serde_json::to_value(current)?, "", &mut before);
    let mut after = BTreeMap::new();
    flatten(&serde_json::to_value(next)?, "", &mut after);
    let keys: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    Ok(keys
        .into_iter()
        .filter_map(|key| {
            let from = before.get(key).cloned().unwrap_or(Value::Null);
            let to = after.get(key).cloned().unwrap_or(Value::Null);
            (from != to).then(|| SettingChange {
                key: key.clone(),
                class: classify(key),
                from,
                to,
            })
        })
        .collect())
}

/// Lays a partial object over a full one: objects merge, everything else is replaced.
fn merge(base: Value, patch: Value) -> Value {
    match (base, patch) {
        (Value::Object(mut base), Value::Object(patch)) => {
            for (key, value) in patch {
                let merged = match base.remove(&key) {
                    Some(existing) => merge(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (_, patch) => patch,
    }
}

/// Validates, applies, persists, and records a change request.
///
/// Order matters: the file is written before the process changes, so a file that cannot be
/// written leaves the running values as they were and the operator sees the error; the event
/// comes last, so it never describes a change that did not happen.
pub async fn apply_settings(
    runner: &SliceRunner,
    current: &AppConfig,
    request: SettingsRequest,
    config_path: Option<&Path>,
) -> AgentResult<SettingsOutcome> {
    let by = request.by.trim();
    if by.is_empty() {
        return Err(AgentError::InvalidInput(
            tr!(
                "a name is required: every settings change is recorded under one",
                "需要填写姓名：每次设置更改都会记录在名下"
            )
            .to_string(),
        ));
    }
    if !request.changes.is_object() {
        return Err(AgentError::InvalidInput(
            tr!(
                "`changes` must be an object in the config file's shape",
                "`changes` 必须是与配置文件结构一致的对象"
            )
            .to_string(),
        ));
    }
    let merged = merge(serde_json::to_value(current)?, request.changes);
    let next: AppConfig = serde_json::from_value(merged).map_err(|error| {
        AgentError::InvalidInput(tr!(
            format!("the settings do not parse: {error}"),
            format!("设置无法解析：{error}")
        ))
    })?;
    next.validate()?;
    let changes = diff(current, &next)?;
    if changes.is_empty() {
        return Ok(SettingsOutcome {
            config: next,
            changes,
        });
    }

    let keys_of = |class: SettingClass| -> Vec<&str> {
        changes
            .iter()
            .filter(|change| change.class == class)
            .map(|change| change.key.as_str())
            .collect()
    };
    let startup = keys_of(SettingClass::Startup);
    if !startup.is_empty() {
        return Err(AgentError::InvalidInput(tr!(
            format!(
                "{} take effect only at startup: edit the config file and restart",
                startup.join(", ")
            ),
            format!(
                "{} 仅在启动时生效：请编辑配置文件并重启",
                startup.join(", ")
            )
        )));
    }
    let policy = keys_of(SettingClass::Policy);
    if !policy.is_empty() {
        let mode = runner.scheduler().mode().await;
        if !matches!(
            mode,
            SchedulerMode::DispatchFrozen | SchedulerMode::FullyFrozen
        ) {
            return Err(AgentError::InvalidInput(tr!(
                format!(
                    "{} decide what may execute and change only while the Scheduler is frozen \
                     (it is {mode:?}); freeze dispatch first",
                    policy.join(", ")
                ),
                format!(
                    "{} 决定了什么可以执行，只能在调度器冻结时更改（当前为 {mode:?}）；请先冻结派发",
                    policy.join(", ")
                )
            )));
        }
        let goes_live = changes
            .iter()
            .any(|change| change.key == "platform.dry_run" && change.to == Value::Bool(false));
        if goes_live && !request.confirm_live_execution {
            return Err(AgentError::InvalidInput(
                tr!(
                    "turning dry-run off makes approved actions execute on real machines; \
                     confirm that explicitly",
                    "关闭演练模式后，已批准的操作会在真实机器上执行；请明确确认"
                )
                .to_string(),
            ));
        }
    }

    if let Some(path) = config_path {
        write_config(path, &changes)?;
    }
    runner.apply_live(LiveSettings::from_config(&next));

    let keys: Vec<&str> = changes.iter().map(|change| change.key.as_str()).collect();
    runner
        .store()
        .append_event(
            NewEvent::new(
                "human",
                "human.settings_changed",
                tr!(
                    format!(
                        "{by} changed {} setting(s): {}",
                        keys.len(),
                        keys.join(", ")
                    ),
                    format!("{by} 修改了 {} 项设置：{}", keys.len(), keys.join(", "))
                ),
            )
            .with_payload(json!({
                "by": by,
                "changes": changes,
                "config_path": config_path.map(|path| path.display().to_string()),
            })),
        )
        .await?;

    Ok(SettingsOutcome {
        config: next,
        changes,
    })
}

/// What a file the console creates from nothing starts with.
const NEW_FILE_HEADER: &str = "# Broccoli DevOps Agent configuration — written by the console's Settings page.\n\
# Keys not listed here take their defaults; config/agent.example.toml documents every one.\n\n";

/// Writes the changed keys into the config file, keeping everything else — comments included —
/// as the operator wrote it. The file is created when it does not exist.
pub fn write_config(path: &Path, changes: &[SettingChange]) -> AgentResult<()> {
    let existing = path.exists();
    let text = if existing {
        std::fs::read_to_string(path).map_err(|source| AgentError::Io {
            context: format!("reading agent config `{}`", path.display()),
            source,
        })?
    } else {
        String::new()
    };
    let mut document: DocumentMut = text.parse().map_err(|error| {
        AgentError::InvalidInput(format!(
            "agent config `{}` is not valid TOML: {error}",
            path.display()
        ))
    })?;
    for change in changes {
        set_value(document.as_table_mut(), &change.key, &change.to);
    }
    // A comment-only document parses into trailing decor, which would render after the tables;
    // a brand-new file gets its header prepended as text instead.
    let output = if existing {
        document.to_string()
    } else {
        format!("{NEW_FILE_HEADER}{document}")
    };
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, output).map_err(|source| AgentError::Io {
        context: format!("writing `{}`", temp.display()),
        source,
    })?;
    std::fs::rename(&temp, path).map_err(|source| AgentError::Io {
        context: format!("replacing `{}`", path.display()),
        source,
    })
}

/// Sets (or, for `null`, removes) one dotted key in a TOML document, creating the tables on the
/// way as implicit ones so no `[section]` header appears that the operator did not write.
fn set_value(root: &mut Table, key: &str, value: &Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut table = root;
    for part in parents {
        let entry = table.entry(part).or_insert_with(|| {
            let mut inner = Table::new();
            inner.set_implicit(true);
            Item::Table(inner)
        });
        if !entry.is_table() {
            let mut inner = Table::new();
            inner.set_implicit(true);
            *entry = Item::Table(inner);
        }
        table = entry.as_table_mut().expect("just ensured a table");
    }
    if value.is_null() {
        table.remove(last);
        return;
    }
    // A scalar that already exists keeps its decor — the spacing and the comment the operator
    // wrote after it — and only its value changes.
    let fresh = to_item(value);
    let leftover = match (table.get_mut(last), fresh) {
        (Some(Item::Value(existing)), Item::Value(mut next)) => {
            *next.decor_mut() = existing.decor().clone();
            *existing = next;
            None
        }
        (_, fresh) => Some(fresh),
    };
    if let Some(fresh) = leftover {
        table.insert(last, fresh);
    }
}

fn to_item(value: &Value) -> Item {
    match value {
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object) => {
            let mut tables = ArrayOfTables::new();
            for item in items {
                tables.push(to_table(item));
            }
            Item::ArrayOfTables(tables)
        }
        Value::Object(_) => Item::Table(to_table(value)),
        other => Item::Value(to_toml_value(other)),
    }
}

fn to_table(value: &Value) -> Table {
    let mut table = Table::new();
    if let Some(map) = value.as_object() {
        for (key, inner) in map {
            table.insert(key, to_item(inner));
        }
    }
    table
}

fn to_toml_value(value: &Value) -> toml_edit::Value {
    match value {
        Value::Bool(flag) => (*flag).into(),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.into(),
            None => number.as_f64().unwrap_or(0.0).into(),
        },
        Value::String(text) => text.as_str().into(),
        Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(to_toml_value(item));
            }
            array.into()
        }
        Value::Object(map) => {
            let mut table = InlineTable::new();
            for (key, inner) in map {
                table.insert(key, to_toml_value(inner));
            }
            table.into()
        }
        Value::Null => "".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_fall_into_their_classes() {
        assert_eq!(
            classify("collector.snapshot_interval_secs"),
            SettingClass::Live
        );
        assert_eq!(classify("budget.max_total_cost"), SettingClass::Live);
        assert_eq!(classify("model.pricing.input_per_mtok"), SettingClass::Live);
        assert_eq!(classify("model.max_tool_calls"), SettingClass::Live);
        assert_eq!(classify("agent.max_auto_passes"), SettingClass::Live);
        assert_eq!(classify("platform.dry_run"), SettingClass::Policy);
        assert_eq!(classify("platform.runbooks"), SettingClass::Policy);
        assert_eq!(
            classify("platform.classification.security_config_keys"),
            SettingClass::Policy
        );
        assert_eq!(classify("agent.language"), SettingClass::Startup);
        assert_eq!(classify("model.base_url"), SettingClass::Startup);
        assert_eq!(classify("api.token"), SettingClass::Startup);
        assert_eq!(classify("data.dir"), SettingClass::Startup);
    }

    #[test]
    fn diff_names_only_what_changed() {
        let current = AppConfig::default();
        let mut next = current.clone();
        next.collector.snapshot_interval_secs = 30;
        next.platform.dry_run = false;
        let changes = diff(&current, &next).unwrap();
        let keys: Vec<&str> = changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["collector.snapshot_interval_secs", "platform.dry_run"]
        );
        assert_eq!(changes[0].from, json!(120));
        assert_eq!(changes[0].to, json!(30));
        assert_eq!(changes[1].class, SettingClass::Policy);
    }

    #[test]
    fn the_file_keeps_its_comments_and_gains_only_the_changed_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.toml");
        std::fs::write(
            &path,
            "# my notes\n[agent]\nmax_auto_passes = 3   # observe, act, check\n\n[platform]\ndry_run = true\n\n[[platform.runbooks]]\nid = \"a\"\ncommand = \"echo a\"\n",
        )
        .unwrap();
        let changes = vec![
            SettingChange {
                key: "agent.max_auto_passes".into(),
                class: SettingClass::Live,
                from: json!(3),
                to: json!(5),
            },
            SettingChange {
                key: "budget.max_total_cost".into(),
                class: SettingClass::Live,
                from: json!(0.0),
                to: json!(12.5),
            },
            SettingChange {
                key: "platform.classification.security_config_keys".into(),
                class: SettingClass::Policy,
                from: json!([]),
                to: json!(["auth.secret"]),
            },
            SettingChange {
                key: "platform.runbooks".into(),
                class: SettingClass::Policy,
                from: json!([{ "id": "a", "command": "echo a" }]),
                to: json!([{ "id": "a", "command": "echo a" }, { "id": "b", "command": "echo b" }]),
            },
        ];
        write_config(&path, &changes).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# my notes\n"), "{text}");
        assert!(
            text.contains("max_auto_passes = 5   # observe, act, check"),
            "{text}"
        );
        assert!(text.contains("max_total_cost = 12.5"), "{text}");
        assert!(
            text.contains("security_config_keys = [\"auth.secret\"]"),
            "{text}"
        );
        assert_eq!(text.matches("[[platform.runbooks]]").count(), 2, "{text}");
        let parsed = AppConfig::from_toml(&text).unwrap();
        assert_eq!(parsed.agent.max_auto_passes, 5);
        assert_eq!(parsed.budget.max_total_cost, 12.5);
        assert_eq!(parsed.platform.runbooks.len(), 2);
        assert!(parsed.platform.dry_run);

        // A missing file is created; a removed key disappears.
        let fresh = dir.path().join("new.toml");
        write_config(&fresh, &changes[..1]).unwrap();
        let text = std::fs::read_to_string(&fresh).unwrap();
        assert!(
            text.starts_with("# Broccoli DevOps Agent configuration"),
            "{text}"
        );
        assert!(text.contains("max_auto_passes = 5"), "{text}");
        write_config(&fresh, &changes[1..2]).unwrap();
        let text = std::fs::read_to_string(&fresh).unwrap();
        assert!(
            text.starts_with("# Broccoli DevOps Agent configuration"),
            "{text}"
        );
        assert_eq!(
            text.matches("# Broccoli DevOps Agent configuration")
                .count(),
            1
        );
        assert!(text.contains("max_total_cost = 12.5"), "{text}");
        write_config(
            &fresh,
            &[SettingChange {
                key: "agent.max_auto_passes".into(),
                class: SettingClass::Live,
                from: json!(5),
                to: Value::Null,
            }],
        )
        .unwrap();
        assert!(
            !std::fs::read_to_string(&fresh)
                .unwrap()
                .contains("max_auto_passes")
        );
    }
}
