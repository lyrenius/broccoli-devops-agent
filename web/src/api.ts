import type { ActionRun, EventRecord, Issue, Job, Snapshot, Status } from "./types";

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

export const api = {
  status: () => request<Status>("/api/status"),
  latestSnapshot: () => request<Snapshot>("/api/snapshots/latest"),
  capture: () => request<Snapshot>("/api/snapshots", { method: "POST" }),
  issues: () => request<Issue[]>("/api/issues"),
  jobs: () => request<Job[]>("/api/jobs"),
  actions: () => request<ActionRun[]>("/api/actions"),
  approve: (id: string) => request<ActionRun>(`/api/actions/${id}/approve`, { method: "POST" }),
  reject: (id: string) => request<ActionRun>(`/api/actions/${id}/reject`, { method: "POST" }),
  events: (limit: number) => request<EventRecord[]>(`/api/events?limit=${limit}`),
  transition: (name: "freeze-dispatch" | "freeze-all" | "resume") =>
    request<{ mode: string }>(`/api/scheduler/${name}`, { method: "POST" }),
  report: (body: { title: string; description: string; reporter: string; priority?: string }) =>
    request<{ issue: Issue; job: Job; actions: ActionRun[] }>("/api/reports", {
      method: "POST",
      body: JSON.stringify(body),
    }),
};

/** Subscribes to the live event stream after the given sequence. */
export function streamEvents(after: number, onEvent: (event: EventRecord) => void): () => void {
  const source = new EventSource(`/api/events/stream?after=${after}`);
  source.addEventListener("log", (message) => {
    onEvent(JSON.parse((message as MessageEvent<string>).data) as EventRecord);
  });
  return () => source.close();
}
