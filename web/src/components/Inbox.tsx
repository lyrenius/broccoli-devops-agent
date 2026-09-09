import { AlertTriangle, Ban, CheckCircle2, ClipboardList, Inbox as InboxIcon, MessageSquareQuote, ShieldQuestion, XCircle } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import { useT } from "../i18n";
import type { Key } from "../i18n/en";
import { loadOperator } from "../lib/prefs";
import { traceHash } from "../lib/routes";
import type { ActionRun, EventRecord, Inbox as InboxData, Issue, Job, Revision, Status } from "../types";
import { Page } from "./Shell";
import { IssueFeedback } from "./IssueFeedback";
import { EvidenceBadge, StatusBadge } from "./status";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, EmptyState, Input, Kv, Segmented, StatTile, Textarea } from "./ui";

/** Completed or reviewed actions retained alongside the human decision log. */
function inInbox(a: ActionRun): boolean {
  if (a.status === "waiting_for_approval" || a.status === "ready") return true;
  if (a.review !== null) return false;
  if (a.status === "waiting_for_human") return true;
  return a.denial !== null || a.status === "failed" || a.status === "verification_failed";
}

const HISTORY_LABELS: Record<string, Key> = {
  "human.issue_reported": "history.human.issue_reported",
  "human.issue_closed": "history.human.issue_closed",
  "human.issue_feedback": "history.human.issue_feedback",
  "human.action_approved": "history.human.action_approved",
  "human.action_rejected": "history.human.action_rejected",
  "human.review_recorded": "history.human.review_recorded",
  "human.pass_cancelled": "history.human.pass_cancelled",
};

const EMPTY: InboxData = { waiting_issues: [], blocked_actions: [], queued_actions: [], permission_requests: [], permission_denied: [], failed_jobs: [], failed_actions: [] };
type Filter = "all" | "waiting" | "requests" | "denied" | "failed";

function ItemCard({ children, accent }: { children: React.ReactNode; accent: "amber" | "red" | "muted" }) {
  const border = accent === "amber" ? "border-l-amber-500" : accent === "red" ? "border-l-red-500" : "border-l-muted-foreground/40";
  return <div className={`rounded-lg border border-l-4 bg-card p-4 transition-colors hover:bg-accent/30 ${border}`}>{children}</div>;
}

export function Inbox({ tick, status, onChanged }: { tick: number; status: Status | null; onChanged: () => void }) {
  const [inbox, setInbox] = useState<InboxData>(EMPTY);
  const [history, setHistory] = useState<ActionRun[]>([]);
  const [decisions, setDecisions] = useState<EventRecord[]>([]);
  const [issues, setIssues] = useState<Issue[]>([]);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [historyLimit, setHistoryLimit] = useState(50);
  const historyCursor = useRef(0);
  const [filter, setFilter] = useState<Filter>("all");
  const [comments, setComments] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revision, setRevision] = useState<Revision | null>(null);
  const { t, status: label, dateTime } = useT();

  useEffect(() => {
    api.inbox().then((next) => setInbox({ ...EMPTY, ...next, waiting_issues: next.waiting_issues ?? [], blocked_actions: next.blocked_actions ?? [] })).catch((e) => setError((e as Error).message));
  }, [tick]);

  useEffect(() => {
    let cancelled = false;
    // No tail limit: old human decisions must survive arbitrarily many model/probe events.
    Promise.all([api.events(undefined, { after: historyCursor.current }), api.issues(), api.actions()])
      .then(([events, allIssues, allActions]) => {
        if (cancelled) return;
        historyCursor.current = events.reduce((last, event) => Math.max(last, event.sequence), historyCursor.current);
        setDecisions((previous) => {
          const all = new Map(previous.map((event) => [event.sequence, event]));
          for (const event of events) if (event.kind.startsWith("human.") && event.issue_id) all.set(event.sequence, event);
          return [...all.values()].sort((a, b) => b.sequence - a.sequence);
        });
        setIssues(allIssues);
        setHistory(allActions.filter((action) => !inInbox(action)).reverse());
        setHistoryError(null);
      }).catch((e) => { if (!cancelled) setHistoryError((e as Error).message); });
    return () => { cancelled = true; };
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
        <Button size="sm" disabled={busy === id} onClick={send} title={t("btn.sendUpstream.title")}>
          {t("btn.sendUpstream")}
        </Button>
        <Button size="sm" variant="outline" disabled={busy === id} onClick={ack}>
          {t("btn.acknowledge")}
        </Button>
      </div>
    </div>
  );

  const waitingCount = inbox.waiting_issues.length + inbox.blocked_actions.length;
  const failedCount = inbox.failed_jobs.length + inbox.failed_actions.length;
  const show = (section: Exclude<Filter, "all">) => filter === "all" || filter === section;
  const total = waitingCount + inbox.permission_requests.length + inbox.permission_denied.length + failedCount;

  return (
    <Page
      icon={InboxIcon}
      title={t("inbox.title")}
      subtitle={t("inbox.subtitle")}
      actions={status?.dry_run ? <Badge variant="warning">{t("inbox.dryRunBadge")}</Badge> : status ? <Badge variant="danger">{t("inbox.liveBadge")}</Badge> : null}
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
              <a className="font-medium text-primary hover:underline" href={traceHash(revision.job.issue_id, revision.job.job_id)}>{t("inbox.revision.title", { id: `…${revision.job.job_id.slice(-8)}`, status: label(revision.job.status) })}</a>
              <div className="mt-1 text-xs text-muted-foreground"><time dateTime={revision.job.created_at}>{t("feedback.passStarted", { time: dateTime(revision.job.created_at) })}</time></div>
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
              {t("inbox.dismiss")}
            </Button>
          </div>
        </Alert>
      )}

      <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
        <StatTile label={t("feedback.waiting")} value={waitingCount} icon={MessageSquareQuote} tone={waitingCount > 0 ? "warn" : "default"} hint={t("feedback.waitingHint")} />
        <StatTile label={t("tile.requests")} value={inbox.permission_requests.length} icon={ShieldQuestion} tone={inbox.permission_requests.length > 0 ? "warn" : "default"} hint={t("tile.requests.hint")} />
        <StatTile label={t("tile.denied")} value={inbox.permission_denied.length} icon={Ban} tone={inbox.permission_denied.length > 0 ? "alert" : "default"} hint={t("tile.denied.hint")} />
        <StatTile label={t("tile.failed")} value={failedCount} icon={XCircle} tone={failedCount > 0 ? "alert" : "default"} hint={t("tile.failed.hint", { jobs: inbox.failed_jobs.length, actions: inbox.failed_actions.length })} />
      </div>

      {inbox.queued_actions.length > 0 && (
        <Card>
          <CardHeader>
            <CardTitle>{t("queue.title", { count: inbox.queued_actions.length })}</CardTitle>
            <CardDescription>{t("queue.desc")}</CardDescription>
          </CardHeader>
          <CardContent className="grid gap-3">
            {inbox.queued_actions.map((a) => (
              <ItemCard key={a.action_run_id} accent="amber">
                <div className="flex flex-wrap items-center gap-2">
                  <a className="font-mono text-sm font-medium text-primary hover:underline" href={traceHash(a.issue_id, a.originating_job_id)} title={t("records.trace.title")}>{a.runbook_id}</a>
                  <span className="font-mono text-xs text-muted-foreground">on {a.target_ids.join(", ")}</span>
                  <Badge variant="warning">{t("queue.badge")}</Badge>
                  {a.approved_by && <span className="text-xs text-muted-foreground">{t("approval.by", { who: a.approved_by })}</span>}
                </div>
                <p className="mt-2 text-sm text-muted-foreground">{a.reason}</p>
              </ItemCard>
            ))}
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <CardTitle className="text-base">{t("inbox.waiting.title")}</CardTitle>
            <CardDescription className="mt-1">{total === 0 ? t("inbox.waiting.none") : t("inbox.waiting.count", { count: total })}</CardDescription>
          </div>
          <Segmented
            value={filter}
            onChange={setFilter}
            options={[
              { id: "all", label: t("filter.all"), count: total },
              { id: "waiting", label: t("feedback.waiting"), count: waitingCount },
              { id: "requests", label: t("filter.requests"), count: inbox.permission_requests.length },
              { id: "denied", label: t("filter.denied"), count: inbox.permission_denied.length },
              { id: "failed", label: t("filter.failed"), count: failedCount },
            ]}
          />
        </CardHeader>
        <CardContent className="grid gap-3">
          {total === 0 && <EmptyState icon={InboxIcon} title={t("inbox.zero.title")} hint={t("inbox.zero.hint")} />}

          {show("waiting") && inbox.waiting_issues.map((item) => (
            <ItemCard key={item.issue.issue_id} accent="amber">
              <div className="flex flex-wrap items-center gap-2">
                <a className="font-medium text-primary hover:underline" href={traceHash(item.issue.issue_id, item.job.job_id)}>{item.issue.title}</a>
                <StatusBadge value={item.issue.status} />
              </div>
              <div className="mt-1 flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
                <time dateTime={item.issue.created_at}>{t("feedback.issueCreated", { time: dateTime(item.issue.created_at) })}</time>
                <time dateTime={item.job.created_at}>{t("feedback.passStarted", { time: dateTime(item.job.created_at) })}</time>
              </div>
              <p className="mt-2 text-sm">{item.job.result?.summary ?? item.issue.description}</p>
              {(item.job.result?.unresolved_questions.length ?? 0) > 0 && <ul className="mt-2 list-inside list-disc text-sm text-muted-foreground">{item.job.result!.unresolved_questions.map((question, index) => <li key={index}>{question}</li>)}</ul>}
              <IssueFeedback key={item.job.job_id} item={item} onChanged={onChanged} onRevision={setRevision} />
              <div className="mt-3 flex flex-wrap items-center gap-2 border-t pt-3">
                <Input className="h-8 min-w-48 flex-1 text-xs" aria-label={t("records.closingComment")} placeholder={t("records.closingComment")} value={comment(item.issue.issue_id)} onChange={(e) => setComment(item.issue.issue_id, e.target.value)} />
                <Button size="sm" variant="outline" disabled={busy === item.issue.issue_id} onClick={() => run(item.issue.issue_id, async () => {
                  await api.closeIssue(item.issue.issue_id, "resolved", by(), comment(item.issue.issue_id));
                  setInbox((current) => ({ ...current, waiting_issues: current.waiting_issues.filter((entry) => entry.issue.issue_id !== item.issue.issue_id) }));
                  return undefined;
                })} title={t("btn.resolve.title")}><CheckCircle2 />{t("btn.resolve")}</Button>
              </div>
            </ItemCard>
          ))}

          {show("waiting") && inbox.blocked_actions.map((action) => (
            <ItemCard key={action.action_run_id} accent="amber">
              <div className="flex flex-wrap items-center gap-2">
                <a className="font-mono text-sm text-primary hover:underline" href={traceHash(action.issue_id, action.originating_job_id)}>{action.runbook_id} · {action.target_ids.join(", ")}</a>
                <StatusBadge value={action.status} />
                <time dateTime={action.created_at} className="ml-auto text-xs text-muted-foreground">{dateTime(action.created_at)}</time>
              </div>
              <p className="mt-2 text-sm">{action.reason}</p>
              <Alert tone="warning" icon={AlertTriangle}><p>{action.human_intervention}</p></Alert>
              <p className="mt-2 text-xs text-muted-foreground">{t("blocked.help")}</p>
              {reviewControls(action.action_run_id, t("blocked.comment"), () => reviewAction(action, "send_upstream"), () => reviewAction(action, "acknowledge"))}
            </ItemCard>
          ))}

          {show("requests") &&
            inbox.permission_requests.map((a) => (
              <ItemCard key={a.action_run_id} accent="amber">
                <div className="flex flex-wrap items-center gap-2">
                  <a className="font-mono text-sm font-medium text-primary hover:underline" href={traceHash(a.issue_id, a.originating_job_id)} title={t("records.trace.title")}>{a.runbook_id}</a>
                  <span className="font-mono text-xs text-muted-foreground">on {a.target_ids.join(", ")}</span>
                  <Badge variant="warning">{t("badge.needsApproval")}</Badge>
                  <span className="ml-auto text-xs text-muted-foreground tabular-nums">{dateTime(a.created_at)}</span>
                </div>
                <Kv
                  rows={[
                    { k: t("kv.why"), v: a.reason || "—" },
                    { k: t("kv.expected"), v: a.expected_effect || "—" },
                    { k: t("kv.verification"), v: <span className="text-muted-foreground">{t("kv.verification.hint")}</span> },
                  ]}
                />
                <div className="mt-4 grid gap-2">
                  <Textarea rows={2} value={comment(a.action_run_id)} placeholder={t("comment.request.placeholder")} onChange={(e) => setComment(a.action_run_id, e.target.value)} />
                  <div className="flex gap-2">
                    <Button size="sm" disabled={busy === a.action_run_id} onClick={() => approve(a)}>
                      <CheckCircle2 />
                      {t("btn.approve")}
                    </Button>
                    <Button size="sm" variant="outline" className="text-destructive" disabled={busy === a.action_run_id} onClick={() => reject(a)}>
                      <XCircle />
                      {t("btn.reject")}
                    </Button>
                  </div>
                </div>
              </ItemCard>
            ))}

          {show("denied") &&
            inbox.permission_denied.map((a) => (
              <ItemCard key={a.action_run_id} accent="red">
                <div className="flex flex-wrap items-center gap-2">
                  <a className="font-mono text-sm font-medium text-primary hover:underline" href={traceHash(a.issue_id, a.originating_job_id)} title={t("records.trace.title")}>{a.runbook_id}</a>
                  <span className="font-mono text-xs text-muted-foreground">on {a.target_ids.join(", ")}</span>
                  <Badge variant="danger">{t("badge.deniedBy", { who: a.denial?.source === "human" ? a.denial.decided_by ?? t("who.human") : t("who.rule") })}</Badge>
                  <span className="ml-auto text-xs text-muted-foreground tabular-nums">{a.denial && dateTime(a.denial.decided_at)}</span>
                </div>
                <Kv
                  rows={[
                    { k: t("kv.proposedBecause"), v: a.reason || "—" },
                    { k: t("kv.denialReason"), v: a.denial?.reason ?? "—" },
                    ...(a.denial?.comment ? [{ k: t("kv.comment"), v: a.denial.comment }] : []),
                  ]}
                />
                {reviewControls(a.action_run_id, t("comment.next.placeholder"), () => reviewAction(a, "send_upstream"), () => reviewAction(a, "acknowledge"))}
              </ItemCard>
            ))}

          {show("failed") &&
            inbox.failed_jobs.map((j) => (
              <ItemCard key={j.job_id} accent="muted">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-sm font-medium">{t("records.job", { id: `${j.job_id.slice(0, 8)}…` })}</span>
                  <Badge variant="danger">{t("badge.jobFailed")}</Badge>
                  <span className="ml-auto text-xs text-muted-foreground tabular-nums">{dateTime(j.created_at)}</span>
                </div>
                <p className="mt-2 text-sm">{j.result?.summary ?? t("noResult")}</p>
                {j.result?.artifact_ids.map((id) => (
                  <a key={id} className="mr-3 font-mono text-xs text-primary underline-offset-4 hover:underline" href={`/api/artifacts/${id}/body`} target="_blank" rel="noreferrer">
                    {t("transcript", { id: `${id.slice(0, 8)}…` })}
                  </a>
                ))}
                {reviewControls(j.job_id, t("comment.job.placeholder"), () => reviewJob(j, "send_upstream"), () => reviewJob(j, "acknowledge"))}
              </ItemCard>
            ))}
          {show("failed") &&
            inbox.failed_actions.map((a) => (
              <ItemCard key={a.action_run_id} accent="muted">
                <div className="flex flex-wrap items-center gap-2">
                  <a className="font-mono text-sm font-medium text-primary hover:underline" href={traceHash(a.issue_id, a.originating_job_id)} title={t("records.trace.title")}>{a.runbook_id}</a>
                  <span className="font-mono text-xs text-muted-foreground">on {a.target_ids.join(", ")}</span>
                  <StatusBadge value={a.status} />
                  <EvidenceBadge evidence={a.verification_evidence} />
                </div>
                <Kv
                  rows={[
                    { k: t("kv.proposedBecause"), v: a.reason || "—" },
                    { k: t("kv.execution"), v: a.execution_summary ?? "—" },
                    { k: t("kv.verification"), v: a.verification_summary ?? t("kv.verificationNotReached") },
                  ]}
                />
                {reviewControls(a.action_run_id, t("comment.next.placeholder"), () => reviewAction(a, "send_upstream"), () => reviewAction(a, "acknowledge"))}
              </ItemCard>
            ))}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <ClipboardList className="h-4 w-4" />
            {t("history.title")}
          </CardTitle>
          <CardDescription>{t("history.desc")}</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-4">
          {historyError && <Alert icon={AlertTriangle}>{historyError}</Alert>}
          {decisions.length === 0 && history.length === 0 && !historyError && <p className="text-sm text-muted-foreground">{t("history.none")}</p>}
          {decisions.length > 0 && <section className="grid gap-2">
            <h3 className="text-sm font-medium">{t("history.decisions")}</h3>
            <ol className="divide-y rounded-lg border">
              {decisions.slice(0, historyLimit).map((event) => {
                const issue = issues.find((item) => item.issue_id === event.issue_id);
                const payload = event.payload as { comment?: unknown } | undefined;
                const reviewComment = event.kind === "human.review_recorded" && typeof payload?.comment === "string" ? payload.comment : null;
                return <li key={event.sequence} className="grid gap-1 p-3 text-sm">
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge variant="outline">{HISTORY_LABELS[event.kind] ? t(HISTORY_LABELS[event.kind]) : event.kind}</Badge>
                    <a className="text-primary hover:underline" href={traceHash(event.issue_id!, event.job_id ?? undefined)}>{issue?.title ?? event.issue_id}</a>
                    <time dateTime={event.occurred_at} className="ml-auto text-xs text-muted-foreground">{dateTime(event.occurred_at)}</time>
                  </div>
                  <p className="whitespace-pre-wrap break-words">{event.summary}</p>
                  {reviewComment && <p className="whitespace-pre-wrap break-words text-muted-foreground">{reviewComment}</p>}
                </li>;
              })}
            </ol>
            {decisions.length > historyLimit && <Button variant="outline" size="sm" onClick={() => setHistoryLimit((limit) => limit + 50)}>{t("history.more")}</Button>}
          </section>}
          {history.length > 0 && <h3 className="text-sm font-medium">{t("history.actions")}</h3>}
          {history.length > 0 && (
            <div className="overflow-x-auto rounded-lg border">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
                    <th className="px-3 py-2 font-medium">{t("history.actionTime")}</th>
                    <th className="px-3 py-2 font-medium">{t("history.actionContext")}</th>
                    <th className="px-3 py-2 font-medium">{t("col.runbook")}</th>
                    <th className="px-3 py-2 font-medium">{t("col.targets")}</th>
                    <th className="px-3 py-2 font-medium">{t("col.status")}</th>
                    <th className="px-3 py-2 font-medium">{t("col.approval")}</th>
                    <th className="px-3 py-2 font-medium">{t("col.outcome")}</th>
                    <th className="px-3 py-2 font-medium">{t("col.review")}</th>
                  </tr>
                </thead>
                <tbody className="divide-y">
                  {history.map((a) => (
                    <tr key={a.action_run_id} className="align-top transition-colors hover:bg-accent/30">
                      <td className="whitespace-nowrap px-3 py-2 text-xs text-muted-foreground">
                        <time dateTime={a.created_at}>{dateTime(a.created_at)}</time>
                        {a.completed_at && <div>{t("history.endedAt", { time: dateTime(a.completed_at) })}</div>}
                      </td>
                      <td className="min-w-48 px-3 py-2 text-xs">
                        <a className="text-primary hover:underline" href={traceHash(a.issue_id, a.originating_job_id)}>{issues.find((issue) => issue.issue_id === a.issue_id)?.title ?? a.issue_id}</a>
                        <div className="mt-1 font-mono text-muted-foreground" title={a.action_run_id}>{t("events.action", { id: a.action_run_id.slice(-8) })}</div>
                      </td>
                      <td className="whitespace-nowrap px-3 py-2 font-mono text-xs font-medium"><a className="text-primary hover:underline" href={traceHash(a.issue_id, a.originating_job_id)}>{a.runbook_id}</a></td>
                      <td className="px-3 py-2 font-mono text-xs text-muted-foreground">{a.target_ids.join(", ")}</td>
                      <td className="whitespace-nowrap px-3 py-2">
                        <StatusBadge value={a.status} />
                      </td>
                      <td className="whitespace-nowrap px-3 py-2 text-xs text-muted-foreground">
                        {label(a.approval)}
                        {a.approved_by && t("approval.by", { who: a.approved_by })}
                      </td>
                      <td className="px-3 py-2 text-xs text-muted-foreground">
                        {a.denial ? `${a.denial.reason}${a.denial.comment ? ` — ${a.denial.comment}` : ""}` : a.human_intervention ?? a.verification_summary ?? a.execution_summary ?? "—"}
                        {a.verification_evidence && (
                          <div className="mt-1">
                            <EvidenceBadge evidence={a.verification_evidence} />
                          </div>
                        )}
                      </td>
                      <td className="px-3 py-2 text-xs text-muted-foreground">
                        {a.review ? `${a.review.reviewer}: ${a.review.decision.decision === "sent_upstream" ? t("review.sentUpstream", { id: `${a.review.decision.job_id.slice(0, 8)}…` }) : t("review.acknowledged")}` : "—"}
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
