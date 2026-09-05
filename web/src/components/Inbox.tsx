import { AlertTriangle, Ban, CheckCircle2, ClipboardList, Inbox as InboxIcon, ShieldQuestion, XCircle } from "lucide-react";
import { useEffect, useState } from "react";
import { api } from "../api";
import { loadOperator } from "../lib/prefs";
import type { ActionRun, Inbox as InboxData, Job, Revision, Status } from "../types";
import { Page } from "./Shell";
import { EvidenceBadge, StatusBadge } from "./status";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, EmptyState, Kv, Segmented, StatTile, Textarea } from "./ui";

/** Mirrors the runner's inbox membership rule, so History shows exactly what the inbox does not. */
function inInbox(a: ActionRun): boolean {
  if (a.status === "waiting_for_approval") return true;
  if (a.review !== null) return false;
  return a.denial !== null || a.status === "failed" || a.status === "verification_failed";
}

const EMPTY: InboxData = { permission_requests: [], permission_denied: [], failed_jobs: [], failed_actions: [] };
type Filter = "all" | "requests" | "denied" | "failed";

function ItemCard({ children, accent }: { children: React.ReactNode; accent: "amber" | "red" | "muted" }) {
  const border = accent === "amber" ? "border-l-amber-500" : accent === "red" ? "border-l-red-500" : "border-l-muted-foreground/40";
  return <div className={`rounded-lg border border-l-4 bg-card p-4 transition-colors hover:bg-accent/30 ${border}`}>{children}</div>;
}

export function Inbox({ tick, status, onChanged }: { tick: number; status: Status | null; onChanged: () => void }) {
  const [inbox, setInbox] = useState<InboxData>(EMPTY);
  const [history, setHistory] = useState<ActionRun[]>([]);
  const [filter, setFilter] = useState<Filter>("all");
  const [comments, setComments] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revision, setRevision] = useState<Revision | null>(null);

  useEffect(() => {
    api.inbox().then(setInbox).catch((e) => setError((e as Error).message));
    api
      .actions()
      .then((all) => setHistory(all.filter((a) => !inInbox(a)).reverse()))
      .catch(() => undefined);
  }, [tick]);

  const comment = (id: string) => comments[id] ?? "";
  const setComment = (id: string, value: string) => setComments((c) => ({ ...c, [id]: value }));

  const run = async (id: string, work: () => Promise<Revision | null | undefined>) => {
    setBusy(id);
    setError(null);
    try {
      const result = await work();
      if (result) setRevision(result);
      setComment(id, "");
      onChanged();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(null);
    }
  };
  const by = () => loadOperator();
  const approve = (a: ActionRun) => run(a.action_run_id, async () => void (await api.approve(a.action_run_id, by())));
  const reject = (a: ActionRun) => run(a.action_run_id, async () => void (await api.reject(a.action_run_id, by(), comment(a.action_run_id))));
  const reviewAction = (a: ActionRun, decision: "acknowledge" | "send_upstream") =>
    run(a.action_run_id, async () => (await api.reviewAction(a.action_run_id, by(), decision, comment(a.action_run_id))).revision);
  const reviewJob = (j: Job, decision: "acknowledge" | "send_upstream") =>
    run(j.job_id, async () => (await api.reviewJob(j.job_id, by(), decision, comment(j.job_id))).revision);

  const reviewControls = (id: string, placeholder: string, send: () => void, ack: () => void) => (
    <div className="mt-4 grid gap-2">
      <Textarea rows={2} value={comment(id)} placeholder={placeholder} onChange={(e) => setComment(id, e.target.value)} />
      <div className="flex gap-2">
        <Button size="sm" disabled={busy === id} onClick={send} title="A revising Job runs now with the reason and your comment as input">
          Send back upstream
        </Button>
        <Button size="sm" variant="outline" disabled={busy === id} onClick={ack}>
          Acknowledge
        </Button>
      </div>
    </div>
  );

  const failedCount = inbox.failed_jobs.length + inbox.failed_actions.length;
  const show = (section: Exclude<Filter, "all">) => filter === "all" || filter === section;
  const total = inbox.permission_requests.length + inbox.permission_denied.length + failedCount;

  return (
    <Page
      icon={InboxIcon}
      title="Inbox"
      subtitle="Everything that waits for a human, in three categories. Items leave only through a recorded decision made in your name."
      actions={status?.dry_run ? <Badge variant="warning">Platform dry-run: approved actions are rendered, not executed</Badge> : status ? <Badge variant="danger">Platform LIVE</Badge> : null}
    >
      {error && (
        <Alert icon={AlertTriangle}>
          <p>{error}</p>
        </Alert>
      )}
      {revision && (
        <Alert tone="info" icon={CheckCircle2}>
          <div className="flex items-start gap-3">
            <div className="min-w-0 flex-1">
              <p className="font-medium">
                Revision ran: job <span className="font-mono">{revision.job.job_id.slice(0, 8)}…</span> is {revision.job.status.replace(/_/g, " ")}
              </p>
              {revision.job.result && <p className="mt-0.5 text-muted-foreground">{revision.job.result.summary}</p>}
              {revision.actions.length > 0 && (
                <ul className="mt-1 font-mono text-xs">
                  {revision.actions.map((a) => (
                    <li key={a.action_run_id}>
                      {a.runbook_id} on {a.target_ids.join(",")} → {a.status} ({a.approval})
                    </li>
                  ))}
                </ul>
              )}
            </div>
            <Button size="sm" variant="ghost" onClick={() => setRevision(null)}>
              Dismiss
            </Button>
          </div>
        </Alert>
      )}

      <div className="grid gap-4 sm:grid-cols-3">
        <StatTile label="Permission requests" value={inbox.permission_requests.length} icon={ShieldQuestion} tone={inbox.permission_requests.length > 0 ? "warn" : "default"} hint="approve, or reject with a comment" />
        <StatTile label="Permission denied" value={inbox.permission_denied.length} icon={Ban} tone={inbox.permission_denied.length > 0 ? "alert" : "default"} hint="review the reason; send upstream or acknowledge" />
        <StatTile label="Failed" value={failedCount} icon={XCircle} tone={failedCount > 0 ? "alert" : "default"} hint={`${inbox.failed_jobs.length} jobs · ${inbox.failed_actions.length} actions`} />
      </div>

      <Card>
        <CardHeader className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <CardTitle className="text-base">Waiting for you</CardTitle>
            <CardDescription className="mt-1">{total === 0 ? "Nothing waits for a human." : `${total} item(s)`}</CardDescription>
          </div>
          <Segmented
            value={filter}
            onChange={setFilter}
            options={[
              { id: "all", label: "All", count: total },
              { id: "requests", label: "Requests", count: inbox.permission_requests.length },
              { id: "denied", label: "Denied", count: inbox.permission_denied.length },
              { id: "failed", label: "Failed", count: failedCount },
            ]}
          />
        </CardHeader>
        <CardContent className="grid gap-3">
          {total === 0 && <EmptyState icon={InboxIcon} title="Inbox zero" hint="Denials and failures land here with their reasons; permission requests with the Team's reasoning." />}

          {show("requests") &&
            inbox.permission_requests.map((a) => (
              <ItemCard key={a.action_run_id} accent="amber">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-sm font-medium">{a.runbook_id}</span>
                  <span className="font-mono text-xs text-muted-foreground">on {a.target_ids.join(", ")}</span>
                  <Badge variant="warning">needs approval</Badge>
                  <span className="ml-auto text-xs text-muted-foreground tabular-nums">{new Date(a.created_at).toLocaleTimeString()}</span>
                </div>
                <Kv
                  rows={[
                    { k: "Why", v: a.reason || "—" },
                    { k: "Expected effect", v: a.expected_effect || "—" },
                    { k: "Verification", v: <span className="text-muted-foreground">every target must be Healthy in the after-Snapshot; a dry run passes as dry-run evidence only</span> },
                  ]}
                />
                <div className="mt-4 grid gap-2">
                  <Textarea rows={2} value={comment(a.action_run_id)} placeholder="Comment (recorded with a rejection; the agent sees it if the denial is sent back)" onChange={(e) => setComment(a.action_run_id, e.target.value)} />
                  <div className="flex gap-2">
                    <Button size="sm" disabled={busy === a.action_run_id} onClick={() => approve(a)}>
                      <CheckCircle2 />
                      Approve and run
                    </Button>
                    <Button size="sm" variant="outline" className="text-destructive" disabled={busy === a.action_run_id} onClick={() => reject(a)}>
                      <XCircle />
                      Reject
                    </Button>
                  </div>
                </div>
              </ItemCard>
            ))}

          {show("denied") &&
            inbox.permission_denied.map((a) => (
              <ItemCard key={a.action_run_id} accent="red">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-sm font-medium">{a.runbook_id}</span>
                  <span className="font-mono text-xs text-muted-foreground">on {a.target_ids.join(", ")}</span>
                  <Badge variant="danger">denied by {a.denial?.source === "human" ? a.denial.decided_by ?? "a human" : "rule"}</Badge>
                  <span className="ml-auto text-xs text-muted-foreground tabular-nums">{a.denial && new Date(a.denial.decided_at).toLocaleTimeString()}</span>
                </div>
                <Kv
                  rows={[
                    { k: "Proposed because", v: a.reason || "—" },
                    { k: "Denial reason", v: a.denial?.reason ?? "—" },
                    ...(a.denial?.comment ? [{ k: "Comment", v: a.denial.comment }] : []),
                  ]}
                />
                {reviewControls(a.action_run_id, "What should the next pass do differently?", () => reviewAction(a, "send_upstream"), () => reviewAction(a, "acknowledge"))}
              </ItemCard>
            ))}

          {show("failed") &&
            inbox.failed_jobs.map((j) => (
              <ItemCard key={j.job_id} accent="muted">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-sm font-medium">job {j.job_id.slice(0, 8)}…</span>
                  <Badge variant="danger">job failed</Badge>
                  <span className="ml-auto text-xs text-muted-foreground tabular-nums">{new Date(j.created_at).toLocaleTimeString()}</span>
                </div>
                <p className="mt-2 text-sm">{j.result?.summary ?? "no result was recorded"}</p>
                {j.result?.artifact_ids.map((id) => (
                  <a key={id} className="mr-3 font-mono text-xs text-primary underline-offset-4 hover:underline" href={`/api/artifacts/${id}/body`} target="_blank" rel="noreferrer">
                    transcript {id.slice(0, 8)}…
                  </a>
                ))}
                {reviewControls(j.job_id, "Anything the next pass should know?", () => reviewJob(j, "send_upstream"), () => reviewJob(j, "acknowledge"))}
              </ItemCard>
            ))}
          {show("failed") &&
            inbox.failed_actions.map((a) => (
              <ItemCard key={a.action_run_id} accent="muted">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-sm font-medium">{a.runbook_id}</span>
                  <span className="font-mono text-xs text-muted-foreground">on {a.target_ids.join(", ")}</span>
                  <StatusBadge value={a.status} />
                  <EvidenceBadge evidence={a.verification_evidence} />
                </div>
                <Kv
                  rows={[
                    { k: "Proposed because", v: a.reason || "—" },
                    { k: "Execution", v: a.execution_summary ?? "—" },
                    { k: "Verification", v: a.verification_summary ?? "not reached" },
                  ]}
                />
                {reviewControls(a.action_run_id, "What should the next pass do differently?", () => reviewAction(a, "send_upstream"), () => reviewAction(a, "acknowledge"))}
              </ItemCard>
            ))}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <ClipboardList className="h-4 w-4" />
            History
          </CardTitle>
          <CardDescription>Every decided action, newest first.</CardDescription>
        </CardHeader>
        <CardContent>
          {history.length === 0 && <p className="text-sm text-muted-foreground">No decided actions yet.</p>}
          {history.length > 0 && (
            <div className="overflow-x-auto rounded-lg border">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
                    <th className="px-3 py-2 font-medium">Runbook</th>
                    <th className="px-3 py-2 font-medium">Targets</th>
                    <th className="px-3 py-2 font-medium">Status</th>
                    <th className="px-3 py-2 font-medium">Approval</th>
                    <th className="px-3 py-2 font-medium">Outcome</th>
                    <th className="px-3 py-2 font-medium">Review</th>
                  </tr>
                </thead>
                <tbody className="divide-y">
                  {history.map((a) => (
                    <tr key={a.action_run_id} className="align-top transition-colors hover:bg-accent/30">
                      <td className="px-3 py-2 font-mono text-xs font-medium">{a.runbook_id}</td>
                      <td className="px-3 py-2 font-mono text-xs text-muted-foreground">{a.target_ids.join(", ")}</td>
                      <td className="px-3 py-2">
                        <StatusBadge value={a.status} />
                      </td>
                      <td className="px-3 py-2 text-xs text-muted-foreground">
                        {a.approval.replace(/_/g, " ")}
                        {a.approved_by && <> by {a.approved_by}</>}
                      </td>
                      <td className="px-3 py-2 text-xs text-muted-foreground">
                        {a.denial ? `${a.denial.reason}${a.denial.comment ? ` — ${a.denial.comment}` : ""}` : a.verification_summary ?? a.execution_summary ?? "—"}
                        {a.verification_evidence && (
                          <div className="mt-1">
                            <EvidenceBadge evidence={a.verification_evidence} />
                          </div>
                        )}
                      </td>
                      <td className="px-3 py-2 text-xs text-muted-foreground">
                        {a.review ? `${a.review.reviewer}: ${a.review.decision.decision === "sent_upstream" ? `sent upstream (job ${a.review.decision.job_id.slice(0, 8)}…)` : "acknowledged"}` : "—"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>
    </Page>
  );
}
