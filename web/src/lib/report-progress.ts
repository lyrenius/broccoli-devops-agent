import type { EventRecord } from "../types";

export interface ReportProgressRequest { reportId: string; after: number }

/** Bind the client report to its admitted Issue through the persisted source event. */
export class ReportProgressScope {
  private sourceEventId: string | null = null;
  private lastSequence = 0;
  issueId: string | null = null;
  jobId: string | null = null;
  constructor(private readonly reportId: string) {}

  accept(event: EventRecord): boolean {
    if (event.sequence <= this.lastSequence) return false;
    this.lastSequence = event.sequence;
    const payload = event.payload as { report_id?: string; source_event_id?: string; operation_id?: string } | undefined;
    if (event.kind.startsWith("operation.")) return payload?.operation_id === this.reportId;
    if (event.kind === "human.issue_reported" && payload?.report_id === this.reportId && !this.sourceEventId) this.sourceEventId = event.event_id;
    if (event.kind === "scheduler.issue_created" && this.sourceEventId && payload?.source_event_id === this.sourceEventId && !this.issueId) this.issueId = event.issue_id;
    if (!this.issueId || event.issue_id !== this.issueId) return false;
    if (event.job_id) this.jobId = event.job_id;
    if (event.kind === "scheduler.issue_created") return true;
    return Boolean(event.job_id) && ["team.callback", "model.usage", "model.request_started", "model.request_finished", "scheduler.job_cancelled", "scheduler.job_failed"].includes(event.kind);
  }
}
