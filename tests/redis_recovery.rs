use std::collections::HashMap;
use std::time::Duration;

use broccoli_devops_agent::domain::{OperationMode, ResourceKind};
use broccoli_devops_agent::policy::{
    Authority, AuthorityPolicy, CAP_RESTART, ClassificationLists, OperationClass, RunbookRegistry,
};

#[test]
fn redis_recovery_is_a_distinct_mutating_operation_with_stable_old_rows() {
    let registry = RunbookRegistry::new(ClassificationLists::default());
    assert_eq!(
        registry.classify("redis.restart", &[]),
        Some(OperationClass::RedisRestart)
    );
    assert_eq!(
        registry.classify("redis.ping", &[]),
        Some(OperationClass::Observe)
    );
    assert!(OperationClass::RedisRestart.is_mutating());
    assert_eq!(OperationClass::RedisRestart.capability(), CAP_RESTART);
    assert_eq!(OperationClass::RedisRestart.row(), 27);
    assert_eq!(OperationClass::RedisDestructive.row(), 21);
    assert_eq!(OperationClass::ModeChange.row(), 26);
    assert_eq!(
        OperationClass::RedisRestart.authority(OperationMode::Rehearsal),
        Authority::Auto
    );
    assert_eq!(
        OperationClass::RedisRestart.authority(OperationMode::ContestLocked),
        Authority::Approve
    );
}

#[test]
fn redis_restart_cannot_borrow_worker_scope_or_observe_capability() {
    let policy = AuthorityPolicy::new(ClassificationLists::default(), Duration::from_secs(600))
        .with_resources(HashMap::from([
            ("redis-mq".to_owned(), ResourceKind::Redis),
            ("worker-1".to_owned(), ResourceKind::Worker),
        ]));
    let targets = vec!["redis-mq".to_owned(), "worker-1".to_owned()];
    let capabilities = vec![CAP_RESTART.to_owned()];
    assert!(
        policy
            .validate_scope(
                "redis.restart",
                &["redis-mq".into()],
                &[],
                &capabilities,
                &targets
            )
            .is_ok()
    );
    assert!(
        policy
            .validate_scope(
                "redis.restart",
                &["worker-1".into()],
                &[],
                &capabilities,
                &targets
            )
            .is_err()
    );
    assert!(
        policy
            .validate_scope("redis.restart", &["redis-mq".into()], &[], &[], &targets)
            .is_err()
    );
}
