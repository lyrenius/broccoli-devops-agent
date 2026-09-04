//! Action authority: the encoded OD-2 matrix and the Runbook Registry classification.
//!
//! This module is the approved `docs/action-authority.md` in executable form. Every Team proposal
//! is classified into one operation class through the Runbook Registry, then the matrix row for
//! that class and the current operation mode yields `auto`, `approve`, or `deny`. Two rules from
//! the document are enforced here as well: an unclassifiable proposal is denied (never guessed),
//! and an unclassified config key is treated as contest-affecting until an operator classifies it.
//! The rate-limit rule (one automatic repeat per resource per window) escalates `auto` to
//! `approve`; the Scheduler supplies the "recent repeat" fact from the ActionRun history.
//!
//! Models never see this table as an instruction. They see refusals as data.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::{ApprovalState, NamedValue, OperationMode};

/// What the matrix says about one operation class in one mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// No human gate; still evented, idempotency-keyed, and before/after-verified.
    Auto,
    /// A named human must approve before execution.
    Approve,
    /// Refused; cannot be overridden by an approval click.
    Deny,
    /// Not an ActionRun at all — only a human may perform it (operation-mode changes).
    HumanOnly,
}

impl Authority {
    /// Maps the matrix value onto the ActionRun approval state the Scheduler applies.
    pub fn approval(self) -> ApprovalState {
        match self {
            Self::Auto => ApprovalState::NotRequired,
            Self::Approve => ApprovalState::Pending,
            Self::Deny | Self::HumanOnly => ApprovalState::Rejected,
        }
    }
}

/// The 26 operation classes of the authority matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationClass {
    /// Row 1: probes, log reads, status queries.
    Observe,
    /// Row 2: graceful restart of a judge worker.
    WorkerRestart,
    /// Row 3: start an idle configured worker.
    WorkerStart,
    /// Row 4: restart the API server.
    ServerRestart,
    /// Row 5: restart the web frontend or reload the gateway.
    FrontendRestart,
    /// Row 6: restart a printer or balloon station.
    StationRestart,
    /// Row 7: requeue dead-letter jobs.
    DlqRequeue,
    /// Row 8: purge a queue (drops pending work).
    QueuePurge,
    /// Row 9: change a tunable config key.
    ConfigTunable,
    /// Row 10: change a contest-affecting config key.
    ConfigContest,
    /// Row 11: change a security config key.
    ConfigSecurity,
    /// Row 12: firewall allow for a known internal address.
    FirewallAllowKnown,
    /// Row 13: firewall deny or rule removal (and allows for unknown addresses).
    FirewallDeny,
    /// Row 14: deploy a plugin / WASM module.
    WasmDeploy,
    /// Row 15: deploy a release Bundle.
    BundleDeploy,
    /// Row 16: roll back to the previous Bundle / WASM.
    Rollback,
    /// Row 17: read-only diagnostic database query.
    DbReadonly,
    /// Row 18: database maintenance (VACUUM, REINDEX, ANALYZE).
    DbMaintain,
    /// Row 19: schema migration.
    DbMigrate,
    /// Row 20: any write to contest data.
    DbWrite,
    /// Row 21: Redis flush or key deletion.
    RedisDestructive,
    /// Row 22: delete or overwrite storage objects.
    StorageDestructive,
    /// Row 23: remount storage or restart a storage daemon.
    StorageDaemon,
    /// Row 24: reboot a machine.
    MachineReboot,
    /// Row 25: free-form shell command.
    Shell,
    /// Row 26: change the operation mode itself.
    ModeChange,
}

impl OperationClass {
    /// The matrix row number, for events and operator output.
    pub fn row(self) -> u8 {
        match self {
            Self::Observe => 1,
            Self::WorkerRestart => 2,
            Self::WorkerStart => 3,
            Self::ServerRestart => 4,
            Self::FrontendRestart => 5,
            Self::StationRestart => 6,
            Self::DlqRequeue => 7,
            Self::QueuePurge => 8,
            Self::ConfigTunable => 9,
            Self::ConfigContest => 10,
            Self::ConfigSecurity => 11,
            Self::FirewallAllowKnown => 12,
            Self::FirewallDeny => 13,
            Self::WasmDeploy => 14,
            Self::BundleDeploy => 15,
            Self::Rollback => 16,
            Self::DbReadonly => 17,
            Self::DbMaintain => 18,
            Self::DbMigrate => 19,
            Self::DbWrite => 20,
            Self::RedisDestructive => 21,
            Self::StorageDestructive => 22,
            Self::StorageDaemon => 23,
            Self::MachineReboot => 24,
            Self::Shell => 25,
            Self::ModeChange => 26,
        }
    }

    /// The encoded matrix: one row per class, one column per operation mode.
    pub fn authority(self, mode: OperationMode) -> Authority {
        use Authority::{Approve, Auto, Deny, HumanOnly};
        use OperationMode::{ContestLocked, PostContest, Rehearsal};
        match (self, mode) {
            (Self::Observe, _) => Auto,
            (Self::WorkerRestart, _) => Auto,
            (Self::WorkerStart, ContestLocked) => Approve,
            (Self::WorkerStart, _) => Auto,
            (Self::ServerRestart, ContestLocked) => Approve,
            (Self::ServerRestart, _) => Auto,
            (Self::FrontendRestart, ContestLocked) => Approve,
            (Self::FrontendRestart, _) => Auto,
            (Self::StationRestart, _) => Auto,
            (Self::DlqRequeue, ContestLocked) => Approve,
            (Self::DlqRequeue, _) => Auto,
            (Self::QueuePurge, ContestLocked) => Deny,
            (Self::QueuePurge, _) => Approve,
            (Self::ConfigTunable, ContestLocked) => Approve,
            (Self::ConfigTunable, _) => Auto,
            (Self::ConfigContest, ContestLocked) => Deny,
            (Self::ConfigContest, _) => Approve,
            (Self::ConfigSecurity, ContestLocked) => Deny,
            (Self::ConfigSecurity, _) => Approve,
            (Self::FirewallAllowKnown, ContestLocked) => Approve,
            (Self::FirewallAllowKnown, _) => Auto,
            (Self::FirewallDeny, _) => Approve,
            (Self::WasmDeploy, ContestLocked) => Deny,
            (Self::WasmDeploy, _) => Approve,
            (Self::BundleDeploy, ContestLocked) => Deny,
            (Self::BundleDeploy, _) => Approve,
            (Self::Rollback, ContestLocked) => Approve,
            (Self::Rollback, _) => Auto,
            (Self::DbReadonly, _) => Auto,
            (Self::DbMaintain, ContestLocked) => Approve,
            (Self::DbMaintain, _) => Auto,
            (Self::DbMigrate, ContestLocked) => Deny,
            (Self::DbMigrate, _) => Approve,
            (Self::DbWrite, PostContest) => Approve,
            (Self::DbWrite, Rehearsal | ContestLocked) => Deny,
            (Self::RedisDestructive, ContestLocked) => Deny,
            (Self::RedisDestructive, _) => Approve,
            (Self::StorageDestructive, ContestLocked) => Deny,
            (Self::StorageDestructive, _) => Approve,
            (Self::StorageDaemon, _) => Approve,
            (Self::MachineReboot, _) => Approve,
            (Self::Shell, _) => Deny,
            (Self::ModeChange, _) => HumanOnly,
        }
    }
}

/// Operator-maintained classification lists referenced by rows 9–13.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassificationLists {
    /// Config keys that are safe tunables (row 9).
    pub tunable_config_keys: Vec<String>,
    /// Config keys that affect scoring, limits, visibility, or freezes (row 10).
    pub contest_config_keys: Vec<String>,
    /// Config keys that affect auth, CORS, or secrets (row 11).
    pub security_config_keys: Vec<String>,
    /// Address prefixes on the contest-LAN allowlist (row 12), e.g. `10.0.` or `192.168.1.`.
    pub known_internal_address_prefixes: Vec<String>,
}

/// Fixed runbook IDs and their classes. Runbooks with argument-dependent classes are handled in
/// [`RunbookRegistry::classify`].
const FIXED_RUNBOOKS: &[(&str, OperationClass)] = &[
    ("service.status", OperationClass::Observe),
    ("log.tail", OperationClass::Observe),
    ("worker.restart", OperationClass::WorkerRestart),
    ("worker.start", OperationClass::WorkerStart),
    ("server.restart", OperationClass::ServerRestart),
    ("frontend.restart", OperationClass::FrontendRestart),
    ("gateway.reload", OperationClass::FrontendRestart),
    ("station.restart", OperationClass::StationRestart),
    ("mq.dlq_requeue", OperationClass::DlqRequeue),
    ("mq.purge", OperationClass::QueuePurge),
    ("ufw.deny", OperationClass::FirewallDeny),
    ("ufw.delete", OperationClass::FirewallDeny),
    ("wasm.install", OperationClass::WasmDeploy),
    ("bundle.install", OperationClass::BundleDeploy),
    ("bundle.rollback", OperationClass::Rollback),
    ("wasm.rollback", OperationClass::Rollback),
    ("db.query_readonly", OperationClass::DbReadonly),
    ("db.maintain", OperationClass::DbMaintain),
    ("db.migrate", OperationClass::DbMigrate),
    ("redis.flush", OperationClass::RedisDestructive),
    ("redis.del", OperationClass::RedisDestructive),
    ("storage.delete", OperationClass::StorageDestructive),
    ("storage.remount", OperationClass::StorageDaemon),
    ("ceph.restart_daemon", OperationClass::StorageDaemon),
    ("machine.reboot", OperationClass::MachineReboot),
    ("mode.set", OperationClass::ModeChange),
];

/// The Runbook Registry: which runbook IDs exist and what class each proposal falls into.
#[derive(Debug, Clone)]
pub struct RunbookRegistry {
    fixed: HashMap<&'static str, OperationClass>,
    lists: ClassificationLists,
}

impl RunbookRegistry {
    /// Builds the registry over the operator's classification lists.
    pub fn new(lists: ClassificationLists) -> Self {
        Self {
            fixed: FIXED_RUNBOOKS.iter().copied().collect(),
            lists,
        }
    }

    /// Every runbook ID a Team may propose, for prompts and operator help.
    pub fn runbook_ids() -> Vec<&'static str> {
        let mut ids: Vec<_> = FIXED_RUNBOOKS.iter().map(|(id, _)| *id).collect();
        ids.push("config.set");
        ids.push("ufw.allow");
        ids.sort_unstable();
        ids
    }

    /// Classifies one proposal; `None` means the runbook is unknown and must be denied.
    pub fn classify(&self, runbook_id: &str, arguments: &[NamedValue]) -> Option<OperationClass> {
        let argument = |name: &str| {
            arguments
                .iter()
                .find(|value| value.name == name)
                .map(|value| value.value.as_str())
        };
        match runbook_id {
            "config.set" => {
                let key = argument("key").unwrap_or_default();
                let lists = &self.lists;
                Some(if lists.security_config_keys.iter().any(|k| k == key) {
                    OperationClass::ConfigSecurity
                } else if lists.tunable_config_keys.iter().any(|k| k == key) {
                    OperationClass::ConfigTunable
                } else {
                    // Rule 4: an unclassified key is contest-affecting until classified.
                    OperationClass::ConfigContest
                })
            }
            "ufw.allow" => {
                let address = argument("address").unwrap_or_default();
                let known = self
                    .lists
                    .known_internal_address_prefixes
                    .iter()
                    .any(|prefix| !prefix.is_empty() && address.starts_with(prefix.as_str()));
                Some(if known {
                    OperationClass::FirewallAllowKnown
                } else {
                    OperationClass::FirewallDeny
                })
            }
            other => self.fixed.get(other).copied(),
        }
    }
}

/// The outcome of evaluating one proposal against the matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityDecision {
    /// Classified operation, or `None` for an unknown runbook.
    pub class: Option<OperationClass>,
    /// Matrix row, when classified.
    pub row: Option<u8>,
    /// Operation mode the decision was made in.
    pub mode: OperationMode,
    /// Matrix value after rate-limit escalation.
    pub authority: Authority,
    /// Approval state the Scheduler applies.
    pub approval: ApprovalState,
    /// Human-readable reason, recorded in the event log.
    pub rationale: String,
}

/// The authority policy the Scheduler consults for every proposal.
#[derive(Debug, Clone)]
pub struct AuthorityPolicy {
    registry: RunbookRegistry,
    auto_repeat_window: Duration,
}

impl Default for AuthorityPolicy {
    /// Empty classification lists and the document's suggested ten-minute repeat window.
    fn default() -> Self {
        Self::new(ClassificationLists::default(), Duration::from_secs(600))
    }
}

impl AuthorityPolicy {
    /// Builds a policy over the operator's lists and repeat window.
    pub fn new(lists: ClassificationLists, auto_repeat_window: Duration) -> Self {
        Self {
            registry: RunbookRegistry::new(lists),
            auto_repeat_window,
        }
    }

    /// The window inside which a repeated automatic action escalates to approval.
    pub fn auto_repeat_window(&self) -> Duration {
        self.auto_repeat_window
    }

    /// The registry, for prompts and validation.
    pub fn registry(&self) -> &RunbookRegistry {
        &self.registry
    }

    /// Decides authority for one proposal.
    ///
    /// `recent_auto_repeat` is true when an automatic ActionRun with the same runbook already
    /// touched one of the same targets inside the repeat window (rule 5); it escalates `auto` to
    /// `approve` so a flapping service is not restarted in a loop.
    pub fn decide(
        &self,
        runbook_id: &str,
        arguments: &[NamedValue],
        mode: OperationMode,
        recent_auto_repeat: bool,
    ) -> AuthorityDecision {
        let Some(class) = self.registry.classify(runbook_id, arguments) else {
            return AuthorityDecision {
                class: None,
                row: None,
                mode,
                authority: Authority::Deny,
                approval: ApprovalState::Rejected,
                rationale: format!("runbook `{runbook_id}` is not in the Runbook Registry"),
            };
        };
        let base = class.authority(mode);
        let (authority, rationale) = match base {
            Authority::Auto if recent_auto_repeat => (
                Authority::Approve,
                format!(
                    "row {} is auto in {mode:?}, escalated to approve: the same action ran \
                     automatically on this target within the last {}s",
                    class.row(),
                    self.auto_repeat_window.as_secs()
                ),
            ),
            other => (
                other,
                format!("row {} ({class:?}) is {other:?} in {mode:?}", class.row()),
            ),
        };
        AuthorityDecision {
            class: Some(class),
            row: Some(class.row()),
            mode,
            authority,
            approval: authority.approval(),
            rationale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(name: &str, value: &str) -> NamedValue {
        NamedValue::new(name, value)
    }

    #[test]
    fn matrix_rows_match_the_document() {
        use OperationMode::{ContestLocked, PostContest, Rehearsal};
        assert_eq!(
            OperationClass::WorkerRestart.authority(ContestLocked),
            Authority::Auto
        );
        assert_eq!(
            OperationClass::ServerRestart.authority(ContestLocked),
            Authority::Approve
        );
        assert_eq!(
            OperationClass::QueuePurge.authority(ContestLocked),
            Authority::Deny
        );
        assert_eq!(
            OperationClass::QueuePurge.authority(Rehearsal),
            Authority::Approve
        );
        assert_eq!(
            OperationClass::DbWrite.authority(Rehearsal),
            Authority::Deny
        );
        assert_eq!(
            OperationClass::DbWrite.authority(PostContest),
            Authority::Approve
        );
        assert_eq!(
            OperationClass::Shell.authority(PostContest),
            Authority::Deny
        );
        assert_eq!(
            OperationClass::ModeChange.authority(Rehearsal),
            Authority::HumanOnly
        );
        assert_eq!(
            OperationClass::Rollback.authority(Rehearsal),
            Authority::Auto
        );
    }

    #[test]
    fn unknown_runbooks_and_unclassified_keys_fail_closed() {
        let policy = AuthorityPolicy::default();
        let unknown = policy.decide("rm.rf", &[], OperationMode::Rehearsal, false);
        assert_eq!(unknown.authority, Authority::Deny);
        assert!(unknown.class.is_none());

        let config = policy.decide(
            "config.set",
            &[arg("key", "contest.freeze_at")],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(config.class, Some(OperationClass::ConfigContest));
        assert_eq!(config.authority, Authority::Approve);
    }

    #[test]
    fn classification_lists_route_config_and_firewall_rows() {
        let policy = AuthorityPolicy::new(
            ClassificationLists {
                tunable_config_keys: vec!["worker.concurrency".into()],
                security_config_keys: vec!["auth.secret".into()],
                known_internal_address_prefixes: vec!["10.0.".into()],
                ..ClassificationLists::default()
            },
            Duration::from_secs(600),
        );
        let tunable = policy.decide(
            "config.set",
            &[arg("key", "worker.concurrency")],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(tunable.authority, Authority::Auto);
        let security = policy.decide(
            "config.set",
            &[arg("key", "auth.secret")],
            OperationMode::ContestLocked,
            false,
        );
        assert_eq!(security.authority, Authority::Deny);
        let known = policy.decide(
            "ufw.allow",
            &[arg("address", "10.0.3.7")],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(known.class, Some(OperationClass::FirewallAllowKnown));
        let unknown = policy.decide(
            "ufw.allow",
            &[arg("address", "8.8.8.8")],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(unknown.authority, Authority::Approve);
    }

    #[test]
    fn repeated_automatic_actions_escalate_to_approval() {
        let policy = AuthorityPolicy::default();
        let first = policy.decide("worker.restart", &[], OperationMode::ContestLocked, false);
        assert_eq!(first.approval, ApprovalState::NotRequired);
        let repeat = policy.decide("worker.restart", &[], OperationMode::ContestLocked, true);
        assert_eq!(repeat.approval, ApprovalState::Pending);
        assert!(repeat.rationale.contains("escalated"));
    }
}
