// Shapes served by the control plane API (serde snake_case of the Rust domain types).

export interface Counts {
  issues: number;
  jobs: number;
  actions: number;
  events: number;
}

export interface InboxCounts {
  permission_requests: number;
  permission_denied: number;
  failed_jobs: number;
  failed_actions: number;
  total: number;
}

export interface RecoverySummary {
  previous_mode: string;
  final_mode: string;
  interrupted_job_ids: string[];
  interrupted_action_ids: string[];
  verified_action_ids: string[];
  reconstructed_review_job_ids: string[];
}

/** A pass running right now; `job_id` is what the cancel route takes. */
export interface RunningPass {
  job_id: string;
  issue_id: string;
  started_at: string;
}

/** How the totals stand against the configured spend ceiling. */
export interface BudgetStatus {
  max_total_tokens: number;
  max_total_cost: number;
  exceeded: boolean;
  reason: string | null;
  used_fraction: number;
}

export interface ModelTotals {
  model: string;
  passes: number;
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  cost: number | null;
}

/**
 * What the model relay has been asked to do, and what it cost.
 *
 * `cost` is null when no price list is configured — tokens are still counted. A non-zero
 * `requests_without_usage` means the relay did not report some of its usage, so the real
 * figures are higher than these.
 */
export interface UsageTotals {
  passes: number;
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  total_tokens: number;
  requests: number;
  requests_without_usage: number;
  cost: number | null;
  currency: string | null;
  by_model: ModelTotals[];
  budget: BudgetStatus | null;
}

export interface Status {
  mode: string;
  team_backend: string;
  dry_run: boolean;
  uptime_secs: number;
  deployment: { name: string; topology_revision: string; operation_mode: string };
  /** The agent's configured output language, e.g. `en` or `zh-CN`. */
  language: string;
  recovery: RecoverySummary | null;
  counts: Counts;
  inbox: InboxCounts;
  running: RunningPass[];
  usage: UsageTotals;
}

export interface Metric {
  name: string;
  value: number;
  unit: string;
}

export interface ResourceState {
  resource_id: string;
  kind: string;
  health: string;
  observed_at: string;
  metrics: Metric[];
  facts: { name: string; value: string }[];
}

export interface CoverageGap {
  resource_id: string;
  probe_id: string;
  reason: string;
}

export interface Snapshot {
  snapshot_id: string;
  created_at: string;
  cause: string;
  operation_mode: string;
  topology_revision: string;
  resources: ResourceState[];
  coverage_gaps: CoverageGap[];
}

/** Where an imported Issue came from; present only on read-only archives. */
export interface SessionProvenance {
  source_deployment: string;
  source_agent_version: string;
  exported_at: string;
  exported_by: string;
  imported_at: string;
  imported_by: string;
}

export interface Issue {
  issue_id: string;
  source: string;
  title: string;
  description: string;
  priority: string;
  status: string;
  created_at: string;
  updated_at: string;
  affected_resource_ids: string[];
  provenance?: SessionProvenance | null;
}

export interface ActionProposal {
  runbook_id: string;
  target_ids: string[];
  reason: string;
  expected_effect: string;
}

export interface ProbeRequest {
  probe_id: string;
  target_ids: string[];
  reason: string;
}

export interface JobResult {
  outcome: string;
  summary: string;
  unresolved_questions: string[];
  proposed_actions: ActionProposal[];
  requested_probes?: ProbeRequest[];
  follow_up_requested?: boolean;
  artifact_ids: string[];
}

export interface Denial {
  source: "policy" | "human";
  reason: string;
  comment: string | null;
  decided_by: string | null;
  decided_at: string;
}

export type ReviewDecision = { decision: "acknowledged" } | { decision: "sent_upstream"; job_id: string };

export interface HumanReview {
  reviewer: string;
  decision: ReviewDecision;
  comment: string | null;
  reviewed_at: string;
}

export type FeedbackOrigin =
  | { kind: "denied_action"; action_run_id: string; runbook_id: string; target_ids: string[]; denial: Denial }
  | { kind: "failed_action"; action_run_id: string; runbook_id: string; target_ids: string[]; summary: string; evidence: string | null }
  | { kind: "failed_job"; job_id: string; summary: string }
  | { kind: "stalled_job"; job_id: string; summary: string; requested_probe_ids: string[] };

export interface HumanFeedback {
  feedback_id: string;
  origin: FeedbackOrigin;
  reviewer: string;
  comment: string | null;
  recorded_at: string;
}

export interface Job {
  job_id: string;
  issue_id: string;
  team_kind: string;
  status: string;
  created_at: string;
  snapshot_view: { snapshot_id: string; artifact_id: string; content_sha256: string };
  usage?: ModelUsage | null;
  feedback: HumanFeedback[];
  revises_job_id: string | null;
  supersedes_job_id?: string | null;
  continues_job_id?: string | null;
  earlier_passes?: unknown[];
  follow_up_budget?: number;
  review: HumanReview | null;
  result: JobResult | null;
}

/** One pass of an investigation chain, as the report endpoint returns it. */
export interface PassOutcome {
  job: Job;
  actions: ActionRun[];
  stop: string;
}

export interface ActionRun {
  action_run_id: string;
  issue_id: string;
  originating_job_id: string;
  runbook_id: string;
  target_ids: string[];
  reason: string;
  expected_effect: string;
  status: string;
  approval: string;
  approved_by: string | null;
  denial: Denial | null;
  review: HumanReview | null;
  execution_summary: string | null;
  dry_run: boolean;
  verification_summary: string | null;
  verification_evidence: "dry_run" | "weak" | "strong" | null;
  created_at: string;
}

export interface Inbox {
  permission_requests: ActionRun[];
  permission_denied: ActionRun[];
  failed_jobs: Job[];
  failed_actions: ActionRun[];
}

export interface Revision {
  job: Job;
  actions: ActionRun[];
}

export interface ReviewOutcome<T> {
  reviewed: T;
  revision: Revision | null;
}

export interface EventRecord {
  sequence: number;
  event_id: string;
  occurred_at: string;
  actor: string;
  kind: string;
  summary: string;
  issue_id: string | null;
  job_id: string | null;
  action_run_id: string | null;
  artifact_ids: string[];
  trust: string;
  /** Event-specific body; `team.step` carries `{ step: TraceStep }`, `model.usage` a ModelUsage. */
  payload?: unknown;
}

/* ---- Transcripts and traces (the harness's replay record, and the live window onto it) ---- */

export type Trust = "trusted" | "untrusted" | "mixed";

/** One item of a pass's conversation, as the harness transcript serializes it. */
export type TranscriptItem =
  | { type: "user_input"; text: string; trust: Trust }
  | { type: "assistant_text"; text: string }
  | { type: "notice"; text: string }
  | { type: "tool_call"; call_id: string; tool: string; arguments: unknown }
  | { type: "tool_output"; call_id: string; tool: string; output: unknown; is_error: boolean; trust: Trust };

export interface TranscriptEntry {
  at: string;
  item: TranscriptItem;
}

export interface Usage {
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  requests: number;
  requests_without_usage: number;
}

/** One model request of a pass: timing, cost, retries, and what was on offer. */
export interface TurnRecord {
  turn: number;
  started_at: string;
  finished_at: string;
  first_entry: number;
  usage: Usage;
  retries: number;
  wrap_up: boolean;
  offered_tools: string[];
}

/** The stored transcript of one pass (a DiagnosticBundle artifact). */
export interface Transcript {
  instructions: string;
  entries: TranscriptEntry[];
  turns?: TurnRecord[];
}

/** One transcript entry forwarded while the pass runs (`team.step` event payload). */
export interface TraceStep {
  index: number;
  at: string;
  item: TranscriptItem;
  truncated: boolean;
}

export interface ModelUsage extends Usage {
  model: string;
}

/* ---- Session files ---- */

export interface Artifact {
  artifact_id: string;
  kind: string;
  produced_by_job_id: string | null;
  produced_by_action_run_id: string | null;
  uri: string;
  content_sha256: string;
  size_bytes: number;
  created_at: string;
}

export type ArtifactBody = { encoding: "json"; content: unknown } | { encoding: "base64"; content: string };

export interface SessionArtifact {
  artifact: Artifact;
  body: ArtifactBody;
}

/** An Issue with its whole pass chain, as `/api/issues/{id}/session` serves it and the Export button saves it. */
export interface SessionBundle {
  format: string;
  version: number;
  exported_at: string;
  exported_by: string;
  deployment: string;
  agent_version: string;
  language: string;
  issue: Issue;
  jobs: Job[];
  action_runs: ActionRun[];
  snapshots: Snapshot[];
  artifacts: SessionArtifact[];
  events: EventRecord[];
}

export interface ImportSummary {
  issue_id: string;
  title: string;
  source_deployment: string;
  exported_at: string;
  exported_by: string;
  jobs: number;
  action_runs: number;
  snapshots: number;
  artifacts: number;
  events: number;
}
