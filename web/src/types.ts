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

export interface Issue {
  issue_id: string;
  title: string;
  description: string;
  priority: string;
  status: string;
  created_at: string;
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
  occurred_at: string;
  actor: string;
  kind: string;
  summary: string;
  issue_id: string | null;
  job_id: string | null;
  action_run_id: string | null;
  trust: string;
}
