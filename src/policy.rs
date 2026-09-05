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
//! Authority is decided over the whole proposal, not the runbook name alone: the target's kind
//! must be one the operation class applies to (a worker restart cannot be pointed at the API
//! server to borrow the worker row's `auto`), every target must be inside the Job's target
//! scope, the Job must hold the class's capability, and required arguments must be present and
//! shell-safe. Any of those failing is a denial with the reason spelled out.
//!
//! Models never see this table as an instruction. They see refusals as data.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::{ApprovalState, NamedValue, OperationMode, ResourceId, ResourceKind};

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

    /// Resource kinds this operation class may target; `None` means any resource.
    ///
    /// This is the binding between a runbook and what it is for. A class's authority row only
    /// applies to targets of these kinds, so a permissive row cannot be borrowed for a different
    /// kind of machine.
    pub fn target_kinds(self) -> Option<&'static [ResourceKind]> {
        use ResourceKind::{
            BalloonStation, BroccoliServer, Frontend, Gateway, ObjectStorage, PostgreSql, Printer,
            PrinterStation, Redis, Worker,
        };
        match self {
            Self::Observe
            | Self::FirewallAllowKnown
            | Self::FirewallDeny
            | Self::MachineReboot
            | Self::Shell
            | Self::ModeChange => None,
            Self::WorkerRestart | Self::WorkerStart => Some(&[Worker]),
            Self::ServerRestart => Some(&[BroccoliServer]),
            Self::FrontendRestart => Some(&[Frontend, Gateway]),
            Self::StationRestart => Some(&[PrinterStation, BalloonStation, Printer]),
            Self::DlqRequeue | Self::QueuePurge | Self::RedisDestructive => Some(&[Redis]),
            Self::ConfigTunable | Self::ConfigContest | Self::ConfigSecurity => Some(&[
                BroccoliServer,
                Worker,
                Frontend,
                Gateway,
                PrinterStation,
                BalloonStation,
            ]),
            Self::WasmDeploy | Self::BundleDeploy | Self::Rollback => {
                Some(&[BroccoliServer, Frontend, Worker])
            }
            Self::DbReadonly | Self::DbMaintain | Self::DbMigrate | Self::DbWrite => {
                Some(&[PostgreSql])
            }
            Self::StorageDestructive | Self::StorageDaemon => Some(&[ObjectStorage]),
        }
    }

    /// The capability a Job must hold for this class to be proposable at all.
    ///
    /// Capabilities are coarser than rows: they say what kind of work a Job was dispatched for,
    /// while the matrix says whether a human must approve it in the current mode.
    pub fn capability(self) -> &'static str {
        match self {
            Self::Observe => CAP_OBSERVE,
            Self::WorkerRestart
            | Self::WorkerStart
            | Self::ServerRestart
            | Self::FrontendRestart
            | Self::StationRestart => CAP_RESTART,
            Self::DlqRequeue | Self::QueuePurge | Self::RedisDestructive => CAP_QUEUE,
            Self::ConfigTunable | Self::ConfigContest | Self::ConfigSecurity => CAP_CONFIG,
            Self::FirewallAllowKnown | Self::FirewallDeny => CAP_FIREWALL,
            Self::WasmDeploy | Self::BundleDeploy | Self::Rollback => CAP_DEPLOY,
            Self::DbReadonly | Self::DbMaintain | Self::DbMigrate | Self::DbWrite => CAP_DATABASE,
            Self::StorageDestructive | Self::StorageDaemon => CAP_STORAGE,
            Self::MachineReboot => CAP_MACHINE,
            Self::Shell => CAP_SHELL,
            Self::ModeChange => CAP_MODE,
        }
    }

    /// Whether the class changes machine state, so verification must look for an effect.
    pub fn is_mutating(self) -> bool {
        !matches!(self, Self::Observe | Self::DbReadonly)
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

/// Capability: observe-only operations (rows 1 and 17).
pub const CAP_OBSERVE: &str = "observe";
/// Capability: service restarts and starts (rows 2–6).
pub const CAP_RESTART: &str = "operate.restart";
/// Capability: queue and Redis operations (rows 7, 8, 21).
pub const CAP_QUEUE: &str = "operate.queue";
/// Capability: configuration changes (rows 9–11).
pub const CAP_CONFIG: &str = "operate.config";
/// Capability: firewall changes (rows 12–13).
pub const CAP_FIREWALL: &str = "operate.firewall";
/// Capability: plugin, WASM, and Bundle deployment or rollback (rows 14–16).
pub const CAP_DEPLOY: &str = "operate.deploy";
/// Capability: database operations (rows 18–20).
pub const CAP_DATABASE: &str = "operate.database";
/// Capability: storage operations (rows 22–23).
pub const CAP_STORAGE: &str = "operate.storage";
/// Capability: machine reboots (row 24).
pub const CAP_MACHINE: &str = "operate.machine";
/// Capability: free-form shell (row 25); never granted.
pub const CAP_SHELL: &str = "operate.shell";
/// Capability: operation-mode changes (row 26); never granted to a Team.
pub const CAP_MODE: &str = "operate.mode";

/// The capabilities an Operate Job dispatched for a human report holds: everything the matrix
/// can decide, minus the two classes no Team may ever hold. The matrix, not this list, decides
/// what actually runs without a human.
pub const OPERATE_CAPABILITIES: &[&str] = &[
    CAP_OBSERVE,
    CAP_RESTART,
    CAP_QUEUE,
    CAP_CONFIG,
    CAP_FIREWALL,
    CAP_DEPLOY,
    CAP_DATABASE,
    CAP_STORAGE,
    CAP_MACHINE,
];

/// Characters that must never appear in an argument that reaches a shell template.
pub const SHELL_METACHARACTERS: [char; 8] = [';', '|', '&', '`', '$', '\n', '>', '<'];

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

    /// Arguments a runbook requires; the Platform's templates also substitute them.
    pub fn required_arguments(runbook_id: &str) -> &'static [&'static str] {
        match runbook_id {
            "config.set" => &["key", "value"],
            "ufw.allow" | "ufw.deny" | "ufw.delete" => &["address"],
            "redis.del" => &["key"],
            "storage.delete" => &["object"],
            "db.query_readonly" => &["statement"],
            _ => &[],
        }
    }

    /// Checks required arguments are present, non-empty, and free of shell metacharacters.
    pub fn validate_arguments(runbook_id: &str, arguments: &[NamedValue]) -> Result<(), String> {
        for required in Self::required_arguments(runbook_id) {
            let present = arguments
                .iter()
                .any(|argument| argument.name == *required && !argument.value.trim().is_empty());
            if !present {
                return Err(format!(
                    "runbook `{runbook_id}` requires argument `{required}`"
                ));
            }
        }
        for argument in arguments {
            if argument.value.contains(SHELL_METACHARACTERS) {
                return Err(format!(
                    "argument `{}` contains shell metacharacters",
                    argument.name
                ));
            }
        }
        Ok(())
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

/// Everything the matrix needs to know about one proposal, beyond the runbook name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalContext<'a> {
    /// Runbook ID from the Runbook Registry.
    pub runbook_id: &'a str,
    /// Targets the proposal names.
    pub target_ids: &'a [ResourceId],
    /// Structured arguments.
    pub arguments: &'a [NamedValue],
    /// Capabilities the proposing Job holds.
    pub allowed_capabilities: &'a [String],
    /// Targets the proposing Job may operate.
    pub allowed_target_ids: &'a [ResourceId],
    /// Operation mode of the before Snapshot.
    pub mode: OperationMode,
    /// Whether the same automatic action ran on one of these targets inside the window.
    pub recent_auto_repeat: bool,
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
    /// Matrix value after scope checks and rate-limit escalation.
    pub authority: Authority,
    /// Approval state the Scheduler applies.
    pub approval: ApprovalState,
    /// Human-readable reason, recorded in the event log.
    pub rationale: String,
}

impl AuthorityDecision {
    fn denied(class: Option<OperationClass>, mode: OperationMode, rationale: String) -> Self {
        Self {
            class,
            row: class.map(OperationClass::row),
            mode,
            authority: Authority::Deny,
            approval: ApprovalState::Rejected,
            rationale,
        }
    }
}

/// The authority policy the Scheduler consults for every proposal: the encoded matrix, the
/// operator's lists, the repeat window, and the resource catalog the target-kind check reads.
#[derive(Debug, Clone)]
pub struct AuthorityPolicy {
    registry: RunbookRegistry,
    auto_repeat_window: Duration,
    resources: HashMap<ResourceId, ResourceKind>,
}

impl Default for AuthorityPolicy {
    /// Empty classification lists, no resource catalog, and the document's suggested ten-minute
    /// repeat window.
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
            resources: HashMap::new(),
        }
    }

    /// Supplies the resource catalog (ID to kind) the target-kind check is decided against.
    ///
    /// Without a catalog every targeted proposal is denied: the policy cannot tell a worker from
    /// the API server, so it does not guess.
    pub fn with_resources(mut self, resources: HashMap<ResourceId, ResourceKind>) -> Self {
        self.resources = resources;
        self
    }

    /// The window inside which a repeated automatic action escalates to approval.
    pub fn auto_repeat_window(&self) -> Duration {
        self.auto_repeat_window
    }

    /// The registry, for prompts and validation.
    pub fn registry(&self) -> &RunbookRegistry {
        &self.registry
    }

    /// The resource catalog in force.
    pub fn resources(&self) -> &HashMap<ResourceId, ResourceKind> {
        &self.resources
    }

    /// Validates a proposal's scope: known targets of an allowed kind, inside the Job's target
    /// scope, a capability the Job holds, and well-formed arguments. Returns the class on success.
    pub fn validate_scope(
        &self,
        runbook_id: &str,
        target_ids: &[ResourceId],
        arguments: &[NamedValue],
        allowed_capabilities: &[String],
        allowed_target_ids: &[ResourceId],
    ) -> Result<OperationClass, (Option<OperationClass>, String)> {
        let class = self
            .registry
            .classify(runbook_id, arguments)
            .ok_or_else(|| {
                (
                    None,
                    format!("runbook `{runbook_id}` is not in the Runbook Registry"),
                )
            })?;
        let fail = |reason: String| (Some(class), reason);

        if target_ids.is_empty() {
            return Err(fail("the proposal names no target".to_string()));
        }
        if !allowed_capabilities.iter().any(|c| c == class.capability()) {
            return Err(fail(format!(
                "the Job does not hold capability `{}` required by row {} ({class:?})",
                class.capability(),
                class.row()
            )));
        }
        for target in target_ids {
            if !allowed_target_ids.iter().any(|allowed| allowed == target) {
                return Err(fail(format!(
                    "target `{target}` is outside the Job's target scope"
                )));
            }
            let Some(kind) = self.resources.get(target) else {
                return Err(fail(format!(
                    "target `{target}` is not in the resource catalog"
                )));
            };
            if let Some(kinds) = class.target_kinds()
                && !kinds.contains(kind)
            {
                return Err(fail(format!(
                    "row {} ({class:?}) does not apply to `{target}`, a {kind:?} resource",
                    class.row()
                )));
            }
        }
        RunbookRegistry::validate_arguments(runbook_id, arguments).map_err(fail)?;
        Ok(class)
    }

    /// Decides authority for one proposal.
    ///
    /// Scope is validated first; a scope failure is a denial that names the failed check. Then
    /// the matrix row for the class and mode applies. `recent_auto_repeat` is true when an
    /// automatic ActionRun with the same runbook already touched one of the same targets inside
    /// the repeat window (rule 5); it escalates `auto` to `approve` so a flapping service is not
    /// restarted in a loop.
    pub fn decide(&self, proposal: &ProposalContext<'_>) -> AuthorityDecision {
        let mode = proposal.mode;
        let class = match self.validate_scope(
            proposal.runbook_id,
            proposal.target_ids,
            proposal.arguments,
            proposal.allowed_capabilities,
            proposal.allowed_target_ids,
        ) {
            Ok(class) => class,
            Err((class, reason)) => {
                return AuthorityDecision::denied(class, mode, format!("scope: {reason}"));
            }
        };
        let base = class.authority(mode);
        let (authority, rationale) = match base {
            Authority::Auto if proposal.recent_auto_repeat => (
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

    /// A catalog with one of each commonly targeted kind.
    fn catalog() -> HashMap<ResourceId, ResourceKind> {
        [
            ("worker-1", ResourceKind::Worker),
            ("broccoli-server", ResourceKind::BroccoliServer),
            ("redis-mq", ResourceKind::Redis),
            ("postgres-main", ResourceKind::PostgreSql),
        ]
        .into_iter()
        .map(|(id, kind)| (id.to_string(), kind))
        .collect()
    }

    fn policy(lists: ClassificationLists) -> AuthorityPolicy {
        AuthorityPolicy::new(lists, Duration::from_secs(600)).with_resources(catalog())
    }

    fn full_scope() -> (Vec<String>, Vec<String>) {
        (
            OPERATE_CAPABILITIES
                .iter()
                .map(ToString::to_string)
                .collect(),
            catalog().into_keys().collect(),
        )
    }

    fn decide(
        policy: &AuthorityPolicy,
        runbook: &str,
        targets: &[&str],
        arguments: &[NamedValue],
        mode: OperationMode,
        repeat: bool,
    ) -> AuthorityDecision {
        let (capabilities, scope) = full_scope();
        let targets: Vec<String> = targets.iter().map(ToString::to_string).collect();
        policy.decide(&ProposalContext {
            runbook_id: runbook,
            target_ids: &targets,
            arguments,
            allowed_capabilities: &capabilities,
            allowed_target_ids: &scope,
            mode,
            recent_auto_repeat: repeat,
        })
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
        let policy = policy(ClassificationLists::default());
        let unknown = decide(
            &policy,
            "rm.rf",
            &["worker-1"],
            &[],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(unknown.authority, Authority::Deny);
        assert!(unknown.class.is_none());

        let config = decide(
            &policy,
            "config.set",
            &["broccoli-server"],
            &[arg("key", "contest.freeze_at"), arg("value", "18:00")],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(config.class, Some(OperationClass::ConfigContest));
        assert_eq!(config.authority, Authority::Approve);
    }

    #[test]
    fn classification_lists_route_config_and_firewall_rows() {
        let policy = policy(ClassificationLists {
            tunable_config_keys: vec!["worker.concurrency".into()],
            security_config_keys: vec!["auth.secret".into()],
            known_internal_address_prefixes: vec!["10.0.".into()],
            ..ClassificationLists::default()
        });
        let tunable = decide(
            &policy,
            "config.set",
            &["worker-1"],
            &[arg("key", "worker.concurrency"), arg("value", "4")],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(tunable.authority, Authority::Auto);
        let security = decide(
            &policy,
            "config.set",
            &["broccoli-server"],
            &[arg("key", "auth.secret"), arg("value", "x")],
            OperationMode::ContestLocked,
            false,
        );
        assert_eq!(security.authority, Authority::Deny);
        let known = decide(
            &policy,
            "ufw.allow",
            &["worker-1"],
            &[arg("address", "10.0.3.7")],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(known.class, Some(OperationClass::FirewallAllowKnown));
        let unknown = decide(
            &policy,
            "ufw.allow",
            &["worker-1"],
            &[arg("address", "8.8.8.8")],
            OperationMode::Rehearsal,
            false,
        );
        assert_eq!(unknown.authority, Authority::Approve);
    }

    #[test]
    fn repeated_automatic_actions_escalate_to_approval() {
        let policy = policy(ClassificationLists::default());
        let first = decide(
            &policy,
            "worker.restart",
            &["worker-1"],
            &[],
            OperationMode::ContestLocked,
            false,
        );
        assert_eq!(first.approval, ApprovalState::NotRequired);
        let repeat = decide(
            &policy,
            "worker.restart",
            &["worker-1"],
            &[],
            OperationMode::ContestLocked,
            true,
        );
        assert_eq!(repeat.approval, ApprovalState::Pending);
        assert!(repeat.rationale.contains("escalated"));
    }

    /// The worker row cannot be borrowed for the API server, and scope is enforced jointly.
    #[test]
    fn scope_is_validated_with_the_runbook() {
        let policy = policy(ClassificationLists::default());
        // Wrong kind: worker.restart on the server would inherit the worker row's `auto`.
        let wrong_kind = decide(
            &policy,
            "worker.restart",
            &["broccoli-server"],
            &[],
            OperationMode::ContestLocked,
            false,
        );
        assert_eq!(wrong_kind.authority, Authority::Deny);
        assert!(wrong_kind.rationale.contains("does not apply"));
        assert!(wrong_kind.rationale.contains("BroccoliServer"));

        // Unknown target and empty target list.
        let unknown = decide(
            &policy,
            "worker.restart",
            &["worker-9"],
            &[],
            OperationMode::Rehearsal,
            false,
        );
        assert!(unknown.rationale.contains("target scope"));
        let none = decide(
            &policy,
            "service.status",
            &[],
            &[],
            OperationMode::Rehearsal,
            false,
        );
        assert!(none.rationale.contains("no target"));

        // Outside the Job's scope or capability.
        let (capabilities, _) = full_scope();
        let narrow = policy.decide(&ProposalContext {
            runbook_id: "worker.restart",
            target_ids: &["worker-1".to_string()],
            arguments: &[],
            allowed_capabilities: &capabilities,
            allowed_target_ids: &["redis-mq".to_string()],
            mode: OperationMode::Rehearsal,
            recent_auto_repeat: false,
        });
        assert!(narrow.rationale.contains("outside the Job's target scope"));
        let observe_only = policy.decide(&ProposalContext {
            runbook_id: "worker.restart",
            target_ids: &["worker-1".to_string()],
            arguments: &[],
            allowed_capabilities: &[CAP_OBSERVE.to_string()],
            allowed_target_ids: &["worker-1".to_string()],
            mode: OperationMode::Rehearsal,
            recent_auto_repeat: false,
        });
        assert!(observe_only.rationale.contains("capability"));

        // Missing or unsafe arguments.
        let missing = decide(
            &policy,
            "config.set",
            &["worker-1"],
            &[arg("key", "worker.concurrency")],
            OperationMode::Rehearsal,
            false,
        );
        assert!(missing.rationale.contains("requires argument `value`"));
        let unsafe_arg = decide(
            &policy,
            "redis.del",
            &["redis-mq"],
            &[arg("key", "queue; rm -rf /")],
            OperationMode::PostContest,
            false,
        );
        assert!(unsafe_arg.rationale.contains("metacharacters"));

        // No catalog at all: every targeted proposal is denied, never guessed.
        let blind = AuthorityPolicy::default();
        let (capabilities, scope) = full_scope();
        let denied = blind.decide(&ProposalContext {
            runbook_id: "worker.restart",
            target_ids: &["worker-1".to_string()],
            arguments: &[],
            allowed_capabilities: &capabilities,
            allowed_target_ids: &scope,
            mode: OperationMode::Rehearsal,
            recent_auto_repeat: false,
        });
        assert!(denied.rationale.contains("resource catalog"));
    }
}
