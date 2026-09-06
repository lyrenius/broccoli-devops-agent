//! Typed client for the control plane's HTTP API: every shape the screens render, and every
//! call the web console makes, so the two consoles can do the same things.

use serde::Deserialize;
use serde_json::{Value, json};

/* ---- /api/status ---- */

/// `/api/status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Status {
    /// Scheduler mode, e.g. `running` or `dispatch_frozen`.
    pub mode: String,
    /// Wired Team backend label.
    pub team_backend: String,
    /// Whether the Platform is in dry-run mode.
    pub dry_run: bool,
    /// Seconds since the API started.
    pub uptime_secs: u64,
    /// The deployment the agent watches.
    pub deployment: Deployment,
    /// The agent's configured output language.
    pub language: String,
    /// Seconds between periodic Snapshots; zero when off.
    pub snapshot_interval_secs: u64,
    /// Startup recovery summary, when the server recovered at startup.
    pub recovery: Option<RecoverySummary>,
    /// Object counts.
    pub counts: Counts,
    /// Inbox counts.
    pub inbox: InboxCounts,
    /// Passes running right now, with the Job that can be interrupted.
    pub running: Vec<RunningPass>,
    /// What the model relay has been asked to do, and what it cost.
    pub usage: UsageTotals,
}

/// The deployment inside `/api/status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Deployment {
    /// Deployment name.
    pub name: String,
    /// Topology revision.
    pub topology_revision: String,
    /// Operation mode, e.g. `rehearsal`.
    pub operation_mode: String,
}

/// What recovery found at startup.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RecoverySummary {
    /// The mode a human had set last.
    pub previous_mode: String,
    /// The mode recovery left the Scheduler in.
    pub final_mode: String,
    /// Jobs that were running with no Team left to run them.
    pub interrupted_job_ids: Vec<String>,
    /// Actions whose execution was interrupted or never evaluated.
    pub interrupted_action_ids: Vec<String>,
    /// An earlier recovery's items still wait in the inbox.
    pub pending_recovery_review: bool,
}

impl RecoverySummary {
    /// Items recovery put in the inbox.
    pub fn interrupted(&self) -> usize {
        self.interrupted_job_ids.len() + self.interrupted_action_ids.len()
    }

    /// Whether a human still has something to look at because of the restart.
    pub fn needs_attention(&self) -> bool {
        self.interrupted() > 0 || self.pending_recovery_review
    }
}

/// One pass in flight.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RunningPass {
    /// Job being run; the ID the cancel route takes.
    pub job_id: String,
    /// Issue it serves.
    pub issue_id: String,
    /// When the run started.
    pub started_at: String,
}

/// Token and cost totals.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UsageTotals {
    /// Model-backed passes counted.
    pub passes: u32,
    /// Input tokens, cached ones included.
    pub input_tokens: u64,
    /// Input tokens served from the cache.
    pub cached_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input plus output.
    pub total_tokens: u64,
    /// Requests made.
    pub requests: u32,
    /// Requests whose response reported no usage.
    pub requests_without_usage: u32,
    /// Cost under the configured price list, when there is one.
    pub cost: Option<f64>,
    /// Currency of `cost`.
    pub currency: Option<String>,
    /// Totals per model.
    pub by_model: Vec<ModelTotals>,
    /// The configured ceiling and how close the totals are to it.
    pub budget: Option<BudgetStatus>,
}

/// Totals for one model.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelTotals {
    /// Model name.
    pub model: String,
    /// Passes counted.
    pub passes: u32,
    /// Input tokens.
    pub input_tokens: u64,
    /// Cached input tokens.
    pub cached_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cost, when priced.
    pub cost: Option<f64>,
}

/// How the totals stand against the configured spend ceiling.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BudgetStatus {
    /// Token ceiling; zero when unset.
    pub max_total_tokens: u64,
    /// Cost ceiling; zero when unset.
    pub max_total_cost: f64,
    /// Whether the ceiling has been reached.
    pub exceeded: bool,
    /// Why, when it has.
    pub reason: Option<String>,
    /// Fraction of the ceiling used, clamped to one.
    pub used_fraction: f64,
}

/// Object counts inside `/api/status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Counts {
    /// Total Issues.
    pub issues: usize,
    /// Total Jobs.
    pub jobs: usize,
    /// Total ActionRuns.
    pub actions: usize,
    /// Total events.
    pub events: usize,
}

/// Inbox counts inside `/api/status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct InboxCounts {
    /// Actions waiting for approval.
    pub permission_requests: usize,
    /// Denied actions awaiting review.
    pub permission_denied: usize,
    /// Failed Jobs awaiting review.
    pub failed_jobs: usize,
    /// Failed actions awaiting review.
    pub failed_actions: usize,
    /// Everything waiting for a human.
    pub total: usize,
}

/* ---- Snapshots ---- */

/// One Snapshot.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// Snapshot ID.
    pub snapshot_id: String,
    /// When it was captured.
    pub created_at: String,
    /// Why it was captured.
    pub cause: String,
    /// Operation mode at the time.
    pub operation_mode: String,
    /// Topology revision.
    pub topology_revision: String,
    /// Every resource as observed.
    pub resources: Vec<ResourceState>,
    /// What could not be observed.
    pub coverage_gaps: Vec<CoverageGap>,
}

/// One resource inside a Snapshot.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ResourceState {
    /// Resource ID.
    pub resource_id: String,
    /// Resource kind.
    pub kind: String,
    /// Health state.
    pub health: String,
    /// When it was observed.
    pub observed_at: String,
    /// Metrics, probe latencies included.
    pub metrics: Vec<Metric>,
    /// Facts.
    pub facts: Vec<Fact>,
}

/// One metric.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Metric {
    /// Name.
    pub name: String,
    /// Value.
    pub value: f64,
    /// Unit.
    pub unit: String,
}

/// One fact.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Fact {
    /// Name.
    pub name: String,
    /// Value.
    pub value: String,
}

/// A probe that could not observe its resource.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CoverageGap {
    /// Resource ID.
    pub resource_id: String,
    /// Probe ID.
    pub probe_id: String,
    /// Why.
    pub reason: String,
}

/* ---- Issues, Jobs, actions ---- */

/// Where an imported Issue came from; present only on read-only archives.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionProvenance {
    /// Deployment the session was exported from.
    pub source_deployment: String,
    /// Agent version that exported it.
    pub source_agent_version: String,
    /// When it was exported.
    pub exported_at: String,
    /// Who exported it.
    pub exported_by: String,
    /// When it was imported here.
    pub imported_at: String,
    /// Who imported it.
    pub imported_by: String,
}

/// One Issue.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Issue {
    /// Issue ID.
    pub issue_id: String,
    /// `judge` or `human`.
    pub source: String,
    /// Title.
    pub title: String,
    /// Description.
    pub description: String,
    /// Priority.
    pub priority: String,
    /// Status.
    pub status: String,
    /// When it was created.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// Resources it affects.
    pub affected_resource_ids: Vec<String>,
    /// Set on imported archives.
    pub provenance: Option<SessionProvenance>,
}

impl Issue {
    /// Whether work is still outstanding on it (and it is not an archive).
    pub fn is_live(&self) -> bool {
        self.provenance.is_none()
            && matches!(
                self.status.as_str(),
                "open" | "investigating" | "waiting_for_human" | "mitigating" | "verifying"
            )
    }
}

/// An action a Team proposed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ActionProposal {
    /// Runbook ID.
    pub runbook_id: String,
    /// Targets.
    pub target_ids: Vec<String>,
    /// Why.
    pub reason: String,
    /// Expected effect.
    pub expected_effect: String,
}

/// A probe a Team asked for.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProbeRequest {
    /// Probe ID.
    pub probe_id: String,
    /// Targets.
    pub target_ids: Vec<String>,
    /// Why.
    pub reason: String,
}

/// The Team's result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct JobResult {
    /// Outcome, e.g. `diagnosis_only`.
    pub outcome: String,
    /// Summary.
    pub summary: String,
    /// Questions left open.
    pub unresolved_questions: Vec<String>,
    /// Proposed actions.
    pub proposed_actions: Vec<ActionProposal>,
    /// Requested probes.
    pub requested_probes: Vec<ProbeRequest>,
    /// Whether the Team asked to go on after its actions ran.
    pub follow_up_requested: bool,
    /// Artifacts it produced (the transcript among them).
    pub artifact_ids: Vec<String>,
}

/// Why an action was denied.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Denial {
    /// `policy` or `human`.
    pub source: String,
    /// The rule's rationale or the fixed human-rejection text.
    pub reason: String,
    /// The human's comment, if any.
    pub comment: Option<String>,
    /// Who decided, for human denials.
    pub decided_by: Option<String>,
    /// When.
    pub decided_at: String,
}

impl Denial {
    /// Who denied it, for a badge: the human's name, or `rule`.
    pub fn who(&self) -> String {
        if self.source == "human" {
            self.decided_by
                .clone()
                .unwrap_or_else(|| "a human".to_string())
        } else {
            "rule".to_string()
        }
    }
}

/// What a reviewer decided: `acknowledged`, or `sent_upstream` with the revising Job.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReviewDecision {
    /// `acknowledged` or `sent_upstream`.
    pub decision: String,
    /// The revising Job, when sent upstream.
    pub job_id: Option<String>,
}

/// A human's review of an inbox item.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HumanReview {
    /// Who.
    pub reviewer: String,
    /// What.
    pub decision: ReviewDecision,
    /// Comment.
    pub comment: Option<String>,
    /// When.
    pub reviewed_at: String,
}

impl HumanReview {
    /// One phrase: `sent upstream (job …)` or `acknowledged`.
    pub fn phrase(&self) -> String {
        match self.decision.job_id.as_deref() {
            Some(job) if self.decision.decision == "sent_upstream" => {
                format!("sent upstream (job {})", crate::format::short(job))
            }
            _ => crate::format::label(&self.decision.decision),
        }
    }
}

/// What feedback is about (a tagged union, flattened: `kind` says which fields are set).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FeedbackOrigin {
    /// `denied_action`, `failed_action`, `failed_job`, or `stalled_job`.
    pub kind: String,
    /// The runbook, for actions.
    pub runbook_id: Option<String>,
    /// The denial, for denied actions.
    pub denial: Option<Denial>,
    /// The summary, for failures.
    pub summary: Option<String>,
    /// Evidence, for failed actions.
    pub evidence: Option<String>,
    /// Probes the stalled Job asked for.
    pub requested_probe_ids: Vec<String>,
}

/// Feedback a human sent upstream.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HumanFeedback {
    /// Feedback ID.
    pub feedback_id: String,
    /// What it is about.
    pub origin: FeedbackOrigin,
    /// Who.
    pub reviewer: String,
    /// Comment.
    pub comment: Option<String>,
    /// When.
    pub recorded_at: String,
}

impl HumanFeedback {
    /// One sentence, as the web console phrases it.
    pub fn phrase(&self) -> String {
        let origin = &self.origin;
        let runbook = origin.runbook_id.as_deref().unwrap_or("?");
        let summary = origin.summary.as_deref().unwrap_or("");
        let mut text = match origin.kind.as_str() {
            "denied_action" => {
                let denial = origin.denial.as_ref();
                let mut text = format!(
                    "on the denied {runbook}: {}",
                    denial.map_or("", |d| d.reason.as_str())
                );
                if let Some(comment) = denial.and_then(|d| d.comment.as_deref()) {
                    text.push_str(&format!(" — {comment}"));
                }
                text
            }
            "failed_action" => format!("on the failed {runbook}: {summary}"),
            "failed_job" => format!("on the failed job: {summary}"),
            "stalled_job" => format!(
                "on the job that needed more observations ({}): {summary}",
                origin.requested_probe_ids.join(", ")
            ),
            other => crate::format::label(other),
        };
        if let Some(comment) = &self.comment {
            text.push_str(&format!(" · “{comment}”"));
        }
        format!("{}: {text}", self.reviewer)
    }
}

/// The Snapshot View a Job read.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SnapshotViewRef {
    /// Snapshot ID.
    pub snapshot_id: String,
    /// Artifact holding the View.
    pub artifact_id: String,
}

/// Tokens one request or pass used.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Cached input tokens.
    pub cached_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Requests.
    pub requests: u32,
    /// Requests that reported no usage.
    pub requests_without_usage: u32,
}

/// What a pass spent, with the model.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelUsage {
    /// Model name.
    pub model: String,
    /// Input tokens.
    pub input_tokens: u64,
    /// Cached input tokens.
    pub cached_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Requests.
    pub requests: u32,
    /// Requests that reported no usage.
    pub requests_without_usage: u32,
}

/// One Job (one pass).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Job {
    /// Job ID.
    pub job_id: String,
    /// Owning Issue.
    pub issue_id: String,
    /// `operate` or `develop`.
    pub team_kind: String,
    /// Status.
    pub status: String,
    /// When it was created.
    pub created_at: String,
    /// When it started.
    pub started_at: Option<String>,
    /// When it ended.
    pub completed_at: Option<String>,
    /// The View it read.
    pub snapshot_view: SnapshotViewRef,
    /// What it spent, once finished.
    pub usage: Option<ModelUsage>,
    /// Feedback it was given.
    pub feedback: Vec<HumanFeedback>,
    /// The Job it revises on feedback.
    pub revises_job_id: Option<String>,
    /// The Job it superseded after a probe request.
    pub supersedes_job_id: Option<String>,
    /// The Job whose actions it re-observes.
    pub continues_job_id: Option<String>,
    /// Earlier passes on the Issue.
    pub earlier_passes: Vec<Value>,
    /// Automatic passes left after this one.
    pub follow_up_budget: u32,
    /// A human's review, once given.
    pub review: Option<HumanReview>,
    /// The Team's result.
    pub result: Option<JobResult>,
}

impl Job {
    /// Whether the pass is still running or waiting to.
    pub fn is_live(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "running")
    }

    /// Input plus output tokens, when the pass reported them fully.
    pub fn tokens(&self) -> Option<u64> {
        self.usage
            .as_ref()
            .filter(|usage| usage.requests_without_usage == 0)
            .map(|usage| usage.input_tokens + usage.output_tokens)
    }

    /// How this pass relates to the one before it in the chain: a short label and the long
    /// explanation the web console shows on hover.
    pub fn relation(&self) -> (&'static str, Option<String>) {
        use crate::format::short;
        if let Some(from) = &self.continues_job_id {
            return (
                "follow-up",
                Some(format!(
                    "continues {} over the Snapshot captured after its actions ran",
                    short(from)
                )),
            );
        }
        if let Some(from) = &self.revises_job_id {
            return (
                "human feedback",
                Some(format!(
                    "revises {} on feedback sent back upstream",
                    short(from)
                )),
            );
        }
        if let Some(from) = &self.supersedes_job_id {
            return (
                "more data",
                Some(format!(
                    "superseded {}: it asked for probes, so a fresh Snapshot was captured",
                    short(from)
                )),
            );
        }
        ("initial pass", None)
    }
}

/// One ActionRun.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ActionRun {
    /// ActionRun ID.
    pub action_run_id: String,
    /// Owning Issue.
    pub issue_id: String,
    /// The Job that proposed it.
    pub originating_job_id: String,
    /// Runbook ID.
    pub runbook_id: String,
    /// Targets.
    pub target_ids: Vec<String>,
    /// Why the Team proposed it.
    pub reason: String,
    /// What the Team expected.
    pub expected_effect: String,
    /// Lifecycle status.
    pub status: String,
    /// Approval state.
    pub approval: String,
    /// Who approved it.
    pub approved_by: Option<String>,
    /// Denial, when denied.
    pub denial: Option<Denial>,
    /// Review, once given.
    pub review: Option<HumanReview>,
    /// The Platform's own summary of the execution.
    pub execution_summary: Option<String>,
    /// Whether it ran dry.
    pub dry_run: bool,
    /// Verification conclusion, when reached.
    pub verification_summary: Option<String>,
    /// How much the verification proves: `dry_run`, `weak`, or `strong`.
    pub verification_evidence: Option<String>,
    /// When it was created.
    pub created_at: String,
}

impl ActionRun {
    /// `runbook on target, target`.
    pub fn title(&self) -> String {
        format!("{} on {}", self.runbook_id, self.target_ids.join(", "))
    }

    /// Mirrors the runner's inbox membership rule, so History shows exactly what the inbox
    /// does not.
    pub fn in_inbox(&self) -> bool {
        if self.status == "waiting_for_approval" {
            return true;
        }
        if self.review.is_some() {
            return false;
        }
        self.denial.is_some() || matches!(self.status.as_str(), "failed" | "verification_failed")
    }

    /// The evidence badge text, when there is evidence.
    pub fn evidence(&self) -> Option<String> {
        self.verification_evidence
            .as_deref()
            .map(|evidence| format!("{} evidence", crate::format::label(evidence)))
    }
}

/// `/api/inbox`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Inbox {
    /// Actions waiting for approval.
    pub permission_requests: Vec<ActionRun>,
    /// Denied actions awaiting review.
    pub permission_denied: Vec<ActionRun>,
    /// Failed Jobs awaiting review.
    pub failed_jobs: Vec<Job>,
    /// Failed actions awaiting review.
    pub failed_actions: Vec<ActionRun>,
}

impl Inbox {
    /// Everything waiting.
    pub fn total(&self) -> usize {
        self.permission_requests.len()
            + self.permission_denied.len()
            + self.failed_jobs.len()
            + self.failed_actions.len()
    }
}

/// The revising Job a send-upstream produced.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Revision {
    /// The Job.
    pub job: Job,
    /// Its actions, already decided.
    pub actions: Vec<ActionRun>,
}

/// What a review produced.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReviewOutcome {
    /// The revision, when the item was sent upstream.
    pub revision: Option<Revision>,
}

/// One event record.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EventRecord {
    /// Sequence number.
    pub sequence: u64,
    /// Event ID.
    pub event_id: String,
    /// When.
    pub occurred_at: String,
    /// Producing component.
    pub actor: String,
    /// Kind.
    pub kind: String,
    /// Summary.
    pub summary: String,
    /// Issue it belongs to.
    pub issue_id: Option<String>,
    /// Job it belongs to.
    pub job_id: Option<String>,
    /// ActionRun it belongs to.
    pub action_run_id: Option<String>,
    /// Artifacts it references.
    pub artifact_ids: Vec<String>,
    /// Trust.
    pub trust: String,
    /// Event-specific body; `team.step` carries `{ step: TraceStep }`.
    pub payload: Option<Value>,
}

impl EventRecord {
    /// The forwarded transcript entry, for a `team.step` event.
    pub fn step(&self) -> Option<TraceStep> {
        if self.kind != "team.step" {
            return None;
        }
        self.payload
            .as_ref()
            .and_then(|payload| serde_json::from_value(payload["step"].clone()).ok())
    }
}

/* ---- Transcripts and session files ---- */

/// One transcript entry forwarded while the pass runs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TraceStep {
    /// Position in the transcript.
    pub index: usize,
    /// When it was appended.
    pub at: String,
    /// The item (`type`, `text`, `tool`, `arguments`, `output`, `trust`, …).
    pub item: Value,
    /// Whether any field was shortened to a preview.
    pub truncated: bool,
}

/// One item of a stored transcript.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TranscriptEntry {
    /// When.
    pub at: String,
    /// The item.
    pub item: Value,
}

/// One model request of a pass.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TurnRecord {
    /// Turn number.
    pub turn: u32,
    /// When the request started.
    pub started_at: String,
    /// When it finished.
    pub finished_at: String,
    /// The transcript entry the turn starts at.
    pub first_entry: usize,
    /// What it used.
    pub usage: Usage,
    /// Retries after backend failures.
    pub retries: u32,
    /// Whether only terminal tools were offered.
    pub wrap_up: bool,
    /// Tools offered.
    pub offered_tools: Vec<String>,
}

/// The stored transcript of one pass.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Transcript {
    /// Instructions the run started with.
    pub instructions: String,
    /// Entries.
    pub entries: Vec<TranscriptEntry>,
    /// Turn records.
    pub turns: Vec<TurnRecord>,
}

/// An Artifact record.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Artifact {
    /// Artifact ID.
    pub artifact_id: String,
    /// Kind, e.g. `diagnostic_bundle`.
    pub kind: String,
    /// Size.
    pub size_bytes: u64,
}

/// An Artifact body inside a session file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ArtifactBody {
    /// `json` or `base64`.
    pub encoding: String,
    /// The body.
    pub content: Value,
}

/// An Artifact with its body.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionArtifact {
    /// The record.
    pub artifact: Artifact,
    /// The body.
    pub body: ArtifactBody,
}

/// An Issue with its whole pass chain, as `/api/issues/{id}/session` serves it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionBundle {
    /// Deployment name.
    pub deployment: String,
    /// The Issue.
    pub issue: Issue,
    /// Every pass.
    pub jobs: Vec<Job>,
    /// Every action.
    pub action_runs: Vec<ActionRun>,
    /// Every Artifact with its body.
    pub artifacts: Vec<SessionArtifact>,
    /// Every event bound to the Issue.
    pub events: Vec<EventRecord>,
}

/// What an import produced.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ImportSummary {
    /// The archived Issue.
    pub issue_id: String,
    /// Its title.
    pub title: String,
    /// Where it came from.
    pub source_deployment: String,
    /// Passes.
    pub jobs: usize,
    /// Actions.
    pub action_runs: usize,
    /// Events.
    pub events: usize,
}

/* ---- Reports and settings ---- */

/// One pass of an investigation chain, as the report endpoint returns it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PassOutcome {
    /// The Job.
    pub job: Job,
    /// Its actions.
    pub actions: Vec<ActionRun>,
    /// What happened after it, e.g. `waiting_for_approval`.
    pub stop: String,
}

/// What filing a report produced.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReportOutcome {
    /// The Issue.
    pub issue: Issue,
    /// The first pass.
    pub job: Job,
    /// The first pass's actions.
    pub actions: Vec<ActionRun>,
    /// The whole chain.
    pub passes: Vec<PassOutcome>,
}

/// Which keys may change when.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingClasses {
    /// Any time.
    pub live: Vec<String>,
    /// While frozen.
    pub policy: Vec<String>,
    /// Never, at runtime.
    pub startup: Vec<String>,
}

/// `/api/settings`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingsPage {
    /// The effective config (token masked, key shown only as present).
    pub config: Value,
    /// Where it lives.
    pub path: Option<String>,
    /// Scheduler mode.
    pub mode: String,
    /// Keys by class.
    pub classes: SettingClasses,
}

/// One setting that changed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingChange {
    /// Dotted key.
    pub key: String,
    /// Class.
    pub class: String,
}

/// What a settings change produced.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingsOutcome {
    /// What changed.
    pub changes: Vec<SettingChange>,
}

/* ---- The client ---- */

/// A filter for `/api/events`.
#[derive(Debug, Clone, Default)]
pub struct EventsQuery {
    /// At most this many, newest.
    pub limit: usize,
    /// Only after this sequence.
    pub after: Option<u64>,
    /// Only this Issue.
    pub issue_id: Option<String>,
    /// Only this Job.
    pub job_id: Option<String>,
}

/// HTTP client bound to one API base URL and optional token.
#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

/// Seconds a read may take before the console gives up on it; writes that run a pass are
/// unbounded.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl ApiClient {
    /// Creates a client for the given base URL (no trailing slash needed).
    pub fn new(base: &str, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// Sends and reads the body; a non-2xx status becomes the API's `error` text.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(reqwest::StatusCode, Vec<u8>), String> {
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| e.to_string())?.to_vec();
        if status.is_success() {
            Ok((status, bytes))
        } else {
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            Err(body["error"]
                .as_str()
                .map_or_else(|| status.to_string(), ToString::to_string))
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let (_, bytes) = self
            .send(
                self.request(reqwest::Method::GET, path)
                    .timeout(READ_TIMEOUT),
            )
            .await?;
        serde_json::from_slice(&bytes).map_err(|e| format!("bad response from {path}: {e}"))
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, String> {
        let mut request = self.request(reqwest::Method::POST, path);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let (_, bytes) = self.send(request).await?;
        serde_json::from_slice(&bytes).map_err(|e| format!("bad response from {path}: {e}"))
    }

    /// `/api/status`.
    pub async fn status(&self) -> Result<Status, String> {
        self.get("/api/status").await
    }

    /// The latest Snapshot; `None` when none was captured yet.
    pub async fn latest_snapshot(&self) -> Result<Option<Snapshot>, String> {
        let request = self
            .request(reqwest::Method::GET, "/api/snapshots/latest")
            .timeout(READ_TIMEOUT);
        let response = request.send().await.map_err(|e| e.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            return Err(body["error"]
                .as_str()
                .map_or_else(|| status.to_string(), ToString::to_string));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| e.to_string())
    }

    /// Captures a Snapshot now.
    pub async fn capture(&self) -> Result<Snapshot, String> {
        self.post("/api/snapshots", None).await
    }

    /// Every Issue.
    pub async fn issues(&self) -> Result<Vec<Issue>, String> {
        self.get("/api/issues").await
    }

    /// Every Job.
    pub async fn jobs(&self) -> Result<Vec<Job>, String> {
        self.get("/api/jobs").await
    }

    /// Every ActionRun.
    pub async fn actions(&self) -> Result<Vec<ActionRun>, String> {
        self.get("/api/actions").await
    }

    /// The inbox.
    pub async fn inbox(&self) -> Result<Inbox, String> {
        self.get("/api/inbox").await
    }

    /// Events matching a query.
    pub async fn events(&self, query: &EventsQuery) -> Result<Vec<EventRecord>, String> {
        let mut path = format!("/api/events?limit={}", query.limit);
        if let Some(after) = query.after {
            path.push_str(&format!("&after={after}"));
        }
        if let Some(issue) = &query.issue_id {
            path.push_str(&format!("&issue_id={issue}"));
        }
        if let Some(job) = &query.job_id {
            path.push_str(&format!("&job_id={job}"));
        }
        self.get(&path).await
    }

    /// Approves an ActionRun in the operator's name.
    pub async fn approve(&self, id: &str, by: &str) -> Result<ActionRun, String> {
        self.post(
            &format!("/api/actions/{id}/approve"),
            Some(json!({ "by": by })),
        )
        .await
    }

    /// Rejects an ActionRun with a comment.
    pub async fn reject(&self, id: &str, by: &str, comment: &str) -> Result<ActionRun, String> {
        self.post(
            &format!("/api/actions/{id}/reject"),
            Some(json!({ "by": by, "comment": comment })),
        )
        .await
    }

    /// Reviews a denied or failed ActionRun: `acknowledge` or `send_upstream`.
    pub async fn review_action(
        &self,
        id: &str,
        by: &str,
        decision: &str,
        comment: &str,
    ) -> Result<ReviewOutcome, String> {
        self.post(
            &format!("/api/actions/{id}/review"),
            Some(json!({ "by": by, "decision": decision, "comment": comment })),
        )
        .await
    }

    /// Reviews a failed Job: `acknowledge` or `send_upstream`.
    pub async fn review_job(
        &self,
        id: &str,
        by: &str,
        decision: &str,
        comment: &str,
    ) -> Result<ReviewOutcome, String> {
        self.post(
            &format!("/api/jobs/{id}/review"),
            Some(json!({ "by": by, "decision": decision, "comment": comment })),
        )
        .await
    }

    /// Closes an Issue: `resolved`, `cancelled`, or `failed`.
    pub async fn close_issue(
        &self,
        id: &str,
        by: &str,
        outcome: &str,
        comment: &str,
    ) -> Result<Issue, String> {
        self.post(
            &format!("/api/issues/{id}/close"),
            Some(json!({ "by": by, "outcome": outcome, "comment": comment })),
        )
        .await
    }

    /// Asks a running pass to stop; the Team still delivers a final result.
    pub async fn cancel_job(&self, id: &str, by: &str) -> Result<Value, String> {
        self.post(&format!("/api/jobs/{id}/cancel"), Some(json!({ "by": by })))
            .await
    }

    /// Requests a Scheduler transition: `freeze-dispatch`, `freeze-all`, or `resume`.
    pub async fn transition(&self, transition: &str) -> Result<Value, String> {
        self.post(&format!("/api/scheduler/{transition}"), None)
            .await
    }

    /// The Issue with its whole pass chain.
    pub async fn session(&self, issue_id: &str) -> Result<SessionBundle, String> {
        self.get(&format!("/api/issues/{issue_id}/session")).await
    }

    /// The same document as bytes, with the operator recorded as the exporter.
    pub async fn session_bytes(&self, issue_id: &str, by: &str) -> Result<Vec<u8>, String> {
        let path = format!(
            "/api/issues/{issue_id}/session?download=true&by={}",
            urlencode(by)
        );
        let (_, bytes) = self
            .send(
                self.request(reqwest::Method::GET, &path)
                    .timeout(READ_TIMEOUT),
            )
            .await?;
        Ok(bytes)
    }

    /// Loads a session file as a read-only archive. The file's bytes are sent as they are:
    /// the server checks every artifact body against its recorded hash, so nothing may
    /// re-serialize the document on the way.
    pub async fn import_session(&self, file: Vec<u8>, by: &str) -> Result<ImportSummary, String> {
        let path = format!("/api/sessions/import?by={}", urlencode(by));
        let request = self
            .request(reqwest::Method::POST, &path)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(file);
        let (_, bytes) = self.send(request).await?;
        serde_json::from_slice(&bytes).map_err(|e| format!("bad response from {path}: {e}"))
    }

    /// The effective config, where it lives, and which keys may change now.
    pub async fn settings(&self) -> Result<SettingsPage, String> {
        self.get("/api/settings").await
    }

    /// Applies a partial config (the file's shape, changed keys only) under the operator's name.
    pub async fn update_settings(
        &self,
        by: &str,
        changes: Value,
        confirm_live_execution: bool,
    ) -> Result<SettingsOutcome, String> {
        let request = self
            .request(reqwest::Method::PATCH, "/api/settings")
            .json(&json!({
                "by": by,
                "changes": changes,
                "confirm_live_execution": confirm_live_execution,
            }));
        let (_, bytes) = self.send(request).await?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }

    /// Files a human report and runs the investigation; blocks until the chain stops.
    pub async fn report(
        &self,
        title: &str,
        description: &str,
        reporter: &str,
        priority: Option<&str>,
    ) -> Result<ReportOutcome, String> {
        let mut body = json!({
            "title": title,
            "description": description,
            "reporter": reporter,
        });
        if let Some(priority) = priority {
            body["priority"] = json!(priority);
        }
        self.post("/api/reports", Some(body)).await
    }
}

/// Percent-encodes a query value.
fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_unions_flatten_onto_plain_structs() {
        let review: HumanReview = serde_json::from_value(json!({
            "reviewer": "ana",
            "decision": { "decision": "sent_upstream", "job_id": "0198c936-5f2a-7000-8000-4a6f8c2d9dd1" },
            "comment": null,
            "reviewed_at": "2026-09-06T10:00:00Z"
        }))
        .unwrap();
        assert_eq!(review.phrase(), "sent upstream (job 0198c936…)");
        let feedback: HumanFeedback = serde_json::from_value(json!({
            "feedback_id": "f",
            "origin": { "kind": "denied_action", "action_run_id": "a", "runbook_id": "mq.purge", "target_ids": ["redis-mq"], "denial": { "source": "policy", "reason": "row 26", "comment": null, "decided_by": null, "decided_at": "2026-09-06T10:00:00Z" } },
            "reviewer": "ana",
            "comment": "try later",
            "recorded_at": "2026-09-06T10:00:00Z"
        }))
        .unwrap();
        assert_eq!(
            feedback.phrase(),
            "ana: on the denied mq.purge: row 26 · “try later”"
        );
        let event: EventRecord = serde_json::from_value(json!({
            "sequence": 3, "event_id": "e", "occurred_at": "2026-09-06T10:00:00Z", "actor": "agent-team",
            "kind": "team.step", "summary": "s", "issue_id": null, "job_id": "j", "action_run_id": null,
            "artifact_ids": [], "trust": "trusted",
            "payload": { "step": { "index": 2, "at": "2026-09-06T10:00:00Z", "item": { "type": "assistant_text", "text": "hi" }, "truncated": false } }
        }))
        .unwrap();
        assert_eq!(event.step().unwrap().index, 2);
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
