import type { ActionRun, AgentConfig, EventRecord, ImportSummary, Inbox, Issue, Job, PassOutcome, ReviewOutcome, Revision, SessionBundle, SettingChange, SettingsPage, Snapshot, Status, UsageTotals } from "./types";

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
  feedbackIssue: (id: string, expectedJobId: string, by: string, comment: string) =>
    post<Revision>(`/api/issues/${id}/feedback`, { expected_job_id: expectedJobId, by, comment }),
  usage: () => request<UsageTotals>("/api/usage"),
  /** Asks a running pass to stop; the Team still delivers a final result and keeps its transcript. */
  cancelJob: (id: string, by: string) => post<{ job_id: string }>(`/api/jobs/${id}/cancel`, { by }),
  events: (limit: number | undefined, filter?: { issue_id?: string; job_id?: string; after?: number; before?: number; q?: string }) => {
    const query = new URLSearchParams();
    if (limit !== undefined) query.set("limit", String(limit));
    if (filter?.issue_id) query.set("issue_id", filter.issue_id);
    if (filter?.job_id) query.set("job_id", filter.job_id);
    if (filter?.after !== undefined) query.set("after", String(filter.after));
    if (filter?.before !== undefined) query.set("before", String(filter.before));
    if (filter?.q) query.set("q", filter.q);
    return request<EventRecord[]>(`/api/events?${query}`);
  },
  /** The Issue with its whole pass chain — passes, actions, Snapshots, transcripts, events. */
  session: (issueId: string, signal?: AbortSignal) => request<SessionBundle>(`/api/issues/${issueId}/session`, { signal }),
  /** The same document as a file download, with the operator recorded as the exporter. */
  sessionDownloadUrl: (issueId: string, by: string) => `/api/issues/${issueId}/session?download=true&by=${encodeURIComponent(by)}`,
  /** The effective config, where it lives, and which keys may change now. */
  settings: () => request<SettingsPage>("/api/settings"),
  /** Applies a partial config (the file's shape, changed keys only) under the operator's name. */
  updateSettings: (by: string, changes: unknown, confirmLiveExecution: boolean) =>
    request<{ config: AgentConfig; changes: SettingChange[] }>("/api/settings", {
      method: "PATCH",
      body: JSON.stringify({ by, changes, confirm_live_execution: confirmLiveExecution }),
    }),
  /** Preserve the file's numeric representation: artifact hashes cover exact JSON bytes. */
  importSession: (fileText: string, by: string) => request<ImportSummary>(`/api/sessions/import?by=${encodeURIComponent(by)}`, {
    method: "POST", body: fileText,
  }),
  transition: (name: "freeze-dispatch" | "freeze-all" | "resume") => post<{ mode: string }>(`/api/scheduler/${name}`),
  report: (body: { title: string; description: string; reporter: string; priority?: string; report_id?: string }) =>
    post<{ issue: Issue; job: Job; actions: ActionRun[]; passes: PassOutcome[] }>("/api/reports", body),
};

/** Subscribes to the live event stream after the given sequence. */
export function streamEvents(after: number, onEvent: (event: EventRecord) => void, onConnection?: (connected: boolean) => void): () => void {
  let closed = false;
  let cursor = after;
  let failures = 0;
  let source: EventSource;
  let retry: ReturnType<typeof setTimeout> | undefined;
  const connect = () => {
    if (closed) return;
    const current = new EventSource(`/api/events/stream?after=${cursor}`);
    source = current;
    current.addEventListener("open", () => {
      if (closed || source !== current) return;
      failures = 0;
      onConnection?.(true);
    });
    current.addEventListener("error", () => {
      if (closed || source !== current) return;
      onConnection?.(false);
      // Some HTTP failures close EventSource permanently rather than scheduling native retry.
      if (current.readyState === EventSource.CLOSED && retry === undefined) {
        retry = setTimeout(() => { retry = undefined; connect(); }, Math.min(1000 * 2 ** failures++, 10_000));
      }
    });
    current.addEventListener("log", (message) => {
      if (closed || source !== current) return;
      const event = JSON.parse((message as MessageEvent<string>).data) as EventRecord;
      cursor = Math.max(cursor, event.sequence);
      onEvent(event);
    });
  };
  connect();
  return () => {
    closed = true;
    if (retry !== undefined) clearTimeout(retry);
    source.close();
  };
}
