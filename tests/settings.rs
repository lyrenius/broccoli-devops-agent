//! The configurator behind the console's Settings page: live settings apply to the next
//! decision, policy settings change only while the Scheduler is frozen and under a name,
//! startup settings never change at runtime, and every accepted change lands in the config
//! file (comments kept) and in the event log.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use broccoli_devops_agent::AgentError;
use broccoli_devops_agent::config::AppConfig;
use broccoli_devops_agent::domain::{OperationMode, ResourceKind};
use broccoli_devops_agent::platform::PlatformConfig;
use broccoli_devops_agent::ports::StateStore;
use broccoli_devops_agent::runner::{SliceRunner, TeamBackend};
use broccoli_devops_agent::settings::{
    LiveSettings, SettingClass, SettingsRequest, apply_settings,
};
use broccoli_devops_agent::topology::{
    DeploymentInfo, DeploymentTopology, ProbeSpec, TopologyResource,
};
use serde_json::json;
use uuid::Uuid;

const CONFIG: &str = r#"# operator notes stay
[agent]
max_auto_passes = 3   # observe, act, check

[collector]
snapshot_interval_secs = 120

[platform]
dry_run = true

[[platform.runbooks]]
id = "worker.restart"
command = "echo restart {target}"
"#;

fn topology() -> DeploymentTopology {
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    DeploymentTopology {
        deployment: DeploymentInfo {
            id: Uuid::now_v7(),
            name: "settings-test".into(),
            topology_revision: "t1".into(),
            operation_mode: OperationMode::Rehearsal,
        },
        resources: vec![TopologyResource {
            id: "worker-1".into(),
            kind: ResourceKind::Worker,
            node: None,
            probes: vec![ProbeSpec {
                target: Some(format!("127.0.0.1:{closed}")),
                url: None,
                ..ProbeSpec::new("tcp.connect")
            }],
        }],
        dependencies: Vec::new(),
    }
}

fn request(by: &str, changes: serde_json::Value, confirm: bool) -> SettingsRequest {
    SettingsRequest {
        by: by.into(),
        changes,
        confirm_live_execution: confirm,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_settings_apply_at_once_and_land_in_the_file_and_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let mut config = AppConfig::load(&path).unwrap();
    let runner = Arc::new(
        SliceRunner::wire(
            topology(),
            &dir.path().join("data"),
            TeamBackend::ReadOnly,
            config.platform.clone(),
        )
        .unwrap()
        .with_live_settings(LiveSettings::from_config(&config)),
    );
    assert_eq!(
        runner.settings().current().snapshot_interval,
        Duration::from_secs(120)
    );
    let events_before = runner.store().list_events().await.unwrap().len();

    let outcome = apply_settings(
        &runner,
        &config,
        request(
            "alice",
            json!({
                "collector": { "snapshot_interval_secs": 45 },
                "budget": { "max_total_tokens": 500 },
                "agent": { "max_auto_passes": 5 },
            }),
            false,
        ),
        Some(&path),
    )
    .await
    .unwrap();
    config = outcome.config;
    let keys: Vec<&str> = outcome.changes.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "agent.max_auto_passes",
            "budget.max_total_tokens",
            "collector.snapshot_interval_secs"
        ]
    );
    assert!(
        outcome
            .changes
            .iter()
            .all(|c| c.class == SettingClass::Live)
    );

    // In force now.
    let live = runner.settings().current();
    assert_eq!(live.snapshot_interval, Duration::from_secs(45));
    assert_eq!(live.budget.max_total_tokens, 500);
    assert_eq!(runner.pass_policy().max_auto_passes, 5);
    assert!(
        runner.usage_totals().await.unwrap().budget.is_some(),
        "the totals are measured against the new ceiling"
    );

    // In the file, with the operator's comments kept.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# operator notes stay\n"), "{text}");
    assert!(
        text.contains("max_auto_passes = 5   # observe, act, check"),
        "{text}"
    );
    assert!(text.contains("snapshot_interval_secs = 45"), "{text}");
    assert!(text.contains("max_total_tokens = 500"), "{text}");
    assert_eq!(AppConfig::load(&path).unwrap(), config);

    // On the record.
    let events = runner.store().list_events().await.unwrap();
    assert_eq!(events.len(), events_before + 1);
    let event = events.last().unwrap();
    assert_eq!(event.kind, "human.settings_changed");
    assert!(event.summary.contains("alice"), "{}", event.summary);
    assert_eq!(event.payload["changes"][2]["from"], 120);
    assert_eq!(event.payload["changes"][2]["to"], 45);

    // Nothing changed: nothing written, nothing logged.
    let again = apply_settings(
        &runner,
        &config,
        request(
            "alice",
            json!({ "budget": { "max_total_tokens": 500 } }),
            false,
        ),
        Some(&path),
    )
    .await
    .unwrap();
    assert!(again.changes.is_empty());
    assert_eq!(
        runner.store().list_events().await.unwrap().len(),
        events_before + 1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_keys_are_refused_and_policy_keys_need_a_freeze_and_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let mut config = AppConfig::load(&path).unwrap();
    let runner = Arc::new(
        SliceRunner::wire(
            topology(),
            &dir.path().join("data"),
            TeamBackend::ReadOnly,
            config.platform.clone(),
        )
        .unwrap()
        .with_live_settings(LiveSettings::from_config(&config)),
    );
    let refused = |error: AgentError, needle: &str| {
        assert!(
            matches!(&error, AgentError::InvalidInput(text) if text.contains(needle)),
            "expected `{needle}` in: {error}"
        );
    };

    // Startup-only keys: edit the file and restart.
    let error = apply_settings(
        &runner,
        &config,
        request("alice", json!({ "api": { "bind": "0.0.0.0:1" } }), false),
        Some(&path),
    )
    .await
    .unwrap_err();
    refused(error, "startup");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        CONFIG,
        "nothing written"
    );

    // A value that does not parse.
    let error = apply_settings(
        &runner,
        &config,
        request(
            "alice",
            json!({ "agent": { "max_auto_passes": "many" } }),
            false,
        ),
        Some(&path),
    )
    .await
    .unwrap_err();
    refused(error, "parse");

    // No name, no change.
    let error = apply_settings(
        &runner,
        &config,
        request("  ", json!({ "budget": { "max_total_tokens": 1 } }), false),
        Some(&path),
    )
    .await
    .unwrap_err();
    refused(error, "name");

    // Policy keys change only while frozen…
    let error = apply_settings(
        &runner,
        &config,
        request("alice", json!({ "platform": { "dry_run": false } }), true),
        Some(&path),
    )
    .await
    .unwrap_err();
    refused(error, "frozen");
    assert!(runner.dry_run(), "still dry-run");

    runner.scheduler().freeze_dispatch().await.unwrap();

    // …and turning dry-run off needs an explicit confirmation.
    let error = apply_settings(
        &runner,
        &config,
        request("alice", json!({ "platform": { "dry_run": false } }), false),
        Some(&path),
    )
    .await
    .unwrap_err();
    refused(error, "confirm");
    assert!(runner.dry_run());

    let outcome = apply_settings(
        &runner,
        &config,
        request(
            "alice",
            json!({
                "platform": {
                    "dry_run": false,
                    "runbooks": [
                        { "id": "worker.restart", "command": "ssh {target} restart" },
                        { "id": "service.status", "command": "ssh {target} status" }
                    ],
                    "classification": { "security_config_keys": ["auth.secret"] }
                }
            }),
            true,
        ),
        Some(&path),
    )
    .await
    .unwrap();
    config = outcome.config;
    assert!(
        outcome
            .changes
            .iter()
            .all(|c| c.class == SettingClass::Policy)
    );
    assert!(!runner.dry_run(), "the Platform executes for real now");
    let live = runner.settings().current();
    assert_eq!(live.platform.runbooks.len(), 2);
    assert_eq!(
        live.platform.classification.security_config_keys,
        vec!["auth.secret".to_string()]
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("dry_run = false"), "{text}");
    assert_eq!(text.matches("[[platform.runbooks]]").count(), 2, "{text}");
    assert_eq!(AppConfig::load(&path).unwrap(), config);
    let event = runner.store().list_events().await.unwrap().pop().unwrap();
    assert_eq!(event.kind, "human.settings_changed");
    assert!(
        event.payload["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["key"] == "platform.dry_run" && c["to"] == false)
    );

    // Without a file, a change still applies in memory.
    let outcome = apply_settings(
        &runner,
        &config,
        request("bob", json!({ "budget": { "max_total_cost": 9.5 } }), false),
        None,
    )
    .await
    .unwrap();
    assert_eq!(outcome.changes.len(), 1);
    assert_eq!(runner.settings().current().budget.max_total_cost, 9.5);
}

#[test]
fn a_default_platform_config_is_what_the_runner_starts_from() {
    // Guards the assumption the tests above make: the file's [platform] is what wire() got.
    let config = AppConfig::from_toml(CONFIG).unwrap();
    assert_eq!(config.platform.runbooks.len(), 1);
    assert_ne!(config.platform, PlatformConfig::default());
}
