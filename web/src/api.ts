import type { ActionRun, EventRecord, ImportSummary, Inbox, Issue, Job, PassOutcome, ReviewOutcome, SessionBundle, Snapshot, Status, UsageTotals } from "./types";

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: { "Content-Type": "application/json", ...(init?.headers ?? {}) },
  });
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = (await response.json()) as { error?: string };
      if (body.error) message = body.error;
    } catch {
      // body was not JSON; keep the status text
    }
    throw new Error(message);
  }
  return (await response.json()) as T;
}

function post<T>(path: string, body?: unknown): Promise<T> {
  return request<T>(path, { method: "POST", body: body === undefined ? undefined : JSON.stringify(body) });
}

export type ReviewChoice = "acknowledge" | "send_upstream";
export type IssueClosure = "resolved" | "cancelled" | "failed";

export const api = {
  status: () => request<Status>("/api/status"),
  latestSnapshot: () => request<Snapshot>("/api/snapshots/latest"),
  capture: () => post<Snapshot>("/api/snapshots"),
  issues: () => request<Issue[]>("/api/issues"),
  jobs: () => request<Job[]>("/api/jobs"),
  actions: () => request<ActionRun[]>("/api/actions"),
  inbox: () => request<Inbox>("/api/inbox"),
  approve: (id: string, by: string) => post<ActionRun>(`/api/actions/${id}/approve`, { by }),
  reject: (id: string, by: string, comment: string) => post<ActionRun>(`/api/actions/${id}/reject`, { by, comment }),
  reviewAction: (id: string, by: string, decision: ReviewChoice, comment: string) =>
    post<ReviewOutcome<ActionRun>>(`/api/actions/${id}/review`, { by, decision, comment }),
  reviewJob: (id: string, by: string, decision: ReviewChoice, comment: string) =>
    post<ReviewOutcome<Job>>(`/api/jobs/${id}/review`, { by, decision, comment }),
  closeIssue: (id: string, outcome: IssueClosure, by: string, comment: string) =>
    post<Issue>(`/api/issues/${id}/close`, { outcome, by, comment }),
  usage: () => request<UsageTotals>("/api/usage"),
  /** Asks a running pass to stop; the Team still delivers a final result and keeps its transcript. */
  cancelJob: (id: string, by: string) => post<{ job_id: string }>(`/api/jobs/${id}/cancel`, { by }),
  events: (limit: number, filter?: { issue_id?: string; job_id?: string; after?: number }) => {
    const query = new URLSearchParams({ limit: String(limit) });
    if (filter?.issue_id) query.set("issue_id", filter.issue_id);
    if (filter?.job_id) query.set("job_id", filter.job_id);
    if (filter?.after !== undefined) query.set("after", String(filter.after));
    return request<EventRecord[]>(`/api/events?${query}`);
  },
  /** The Issue with its whole pass chain — passes, actions, Snapshots, transcripts, events. */
  session: (issueId: string) => request<SessionBundle>(`/api/issues/${issueId}/session`),
  /** The same document as a file download, with the operator recorded as the exporter. */
  sessionDownloadUrl: (issueId: string, by: string) => `/api/issues/${issueId}/session?download=true&by=${encodeURIComponent(by)}`,
  /** Loads a session file as a read-only archive. */
  importSession: (bundle: SessionBundle, by: string) => post<ImportSummary>(`/api/sessions/import?by=${encodeURIComponent(by)}`, bundle),
  transition: (name: "freeze-dispatch" | "freeze-all" | "resume") => post<{ mode: string }>(`/api/scheduler/${name}`),
  report: (body: { title: string; description: string; reporter: string; priority?: string }) =>
    post<{ issue: Issue; job: Job; actions: ActionRun[]; passes: PassOutcome[] }>("/api/reports", body),
};

/** Subscribes to the live event stream after the given sequence. */
export function streamEvents(after: number, onEvent: (event: EventRecord) => void): () => void {
  const source = new EventSource(`/api/events/stream?after=${after}`);
  source.addEventListener("log", (message) => {
    onEvent(JSON.parse((message as MessageEvent<string>).data) as EventRecord);
  });
  return () => source.close();
}
