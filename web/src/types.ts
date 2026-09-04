// Shapes served by the control plane API (serde snake_case of the Rust domain types).

export interface Counts {
  issues: number;
  jobs: number;
  actions: number;
  actions_waiting: number;
  events: number;
}

export interface Status {
  mode: string;
  team_backend: string;
  dry_run: boolean;
  uptime_secs: number;
  deployment: { name: string; topology_revision: string; operation_mode: string };
  counts: Counts;
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

export interface JobResult {
  outcome: string;
  summary: string;
  unresolved_questions: string[];
  proposed_actions: ActionProposal[];
  artifact_ids: string[];
}

export interface Job {
  job_id: string;
  issue_id: string;
  team_kind: string;
  status: string;
  created_at: string;
  result: JobResult | null;
}

export interface ActionRun {
  action_run_id: string;
  issue_id: string;
  runbook_id: string;
  target_ids: string[];
  reason: string;
  expected_effect: string;
  status: string;
  approval: string;
  verification_summary: string | null;
  created_at: string;
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
