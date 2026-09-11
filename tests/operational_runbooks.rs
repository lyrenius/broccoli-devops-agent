use std::collections::HashMap;
use std::time::Duration;

use broccoli_devops_agent::domain::{NamedValue, OperationMode, ResourceKind};
use broccoli_devops_agent::policy::{
    Authority, AuthorityPolicy, ClassificationLists, OPERATE_CAPABILITIES, OperationClass,
    RunbookRegistry,
};

#[test]
fn operational_runbooks_bind_to_their_actual_resource_types() {
    let policy = AuthorityPolicy::new(ClassificationLists::default(), Duration::from_secs(600))
        .with_resources(HashMap::from([
            ("pg".into(), ResourceKind::PostgreSql),
            ("redis".into(), ResourceKind::Redis),
            ("storage".into(), ResourceKind::ObjectStorage),
            ("app".into(), ResourceKind::BroccoliServer),
            ("worker".into(), ResourceKind::Worker),
        ]));
    let targets: Vec<String> = ["pg", "redis", "storage", "app", "worker"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let caps: Vec<String> = OPERATE_CAPABILITIES.iter().map(|s| s.to_string()).collect();
    for (runbook, target) in [
        ("redis.info", "redis"),
        ("redis.ping", "redis"),
        ("redis.restart", "redis"),
        ("redis.start", "redis"),
        ("postgres.check", "pg"),
        ("postgres.locks", "pg"),
        ("postgres.start", "pg"),
        ("postgres.restart", "pg"),
        ("storage.check", "storage"),
        ("storage.start", "storage"),
        ("storage.restart", "storage"),
        ("infra.resources", "pg"),
        ("app.health", "app"),
    ] {
        assert!(
            policy
                .validate_scope(runbook, &[target.into()], &[], &caps, &targets)
                .is_ok(),
            "{runbook}"
        );
        assert!(
            policy
                .validate_scope(runbook, &["worker".into()], &[], &caps, &targets)
                .is_err(),
            "{runbook}"
        );
        assert!(
            RunbookRegistry::validate_arguments(runbook, &[NamedValue::new("query", "SELECT 1")])
                .is_err()
        );
    }
}

#[test]
fn restores_are_mutating_and_storage_keeps_its_approval_requirement() {
    let registry = RunbookRegistry::new(ClassificationLists::default());
    for id in [
        "redis.start",
        "postgres.start",
        "postgres.restart",
        "storage.start",
        "storage.restart",
    ] {
        assert!(registry.classify(id, &[]).unwrap().is_mutating());
    }
    for id in [
        "redis.info",
        "postgres.check",
        "postgres.locks",
        "storage.check",
        "infra.resources",
        "app.health",
    ] {
        assert!(!registry.classify(id, &[]).unwrap().is_mutating());
    }
    assert_eq!(
        OperationClass::PostgresRestart.authority(OperationMode::ContestLocked),
        Authority::Approve
    );
    assert_eq!(
        OperationClass::PostgresRestart.authority(OperationMode::Rehearsal),
        Authority::Auto
    );
    assert_eq!(
        registry
            .classify("storage.start", &[])
            .unwrap()
            .authority(OperationMode::Rehearsal),
        Authority::Approve
    );
}
