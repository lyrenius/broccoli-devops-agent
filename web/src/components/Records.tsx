import { AlertTriangle, CheckCircle2, ListChecks, MessageSquareQuote, XCircle } from "lucide-react";
import { useEffect, useState } from "react";
import { api } from "../api";
import { useT } from "../i18n";
import { loadOperator } from "../lib/prefs";
import type { Issue, Job } from "../types";
import { Page } from "./Shell";
import { StatusBadge } from "./status";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, EmptyState, Input } from "./ui";

const LIVE = new Set(["open", "investigating", "waiting_for_human", "mitigating", "verifying"]);

export function Records({ tick, onChanged }: { tick: number; onChanged: () => void }) {
  const [issues, setIssues] = useState<Issue[]>([]);
  const [jobs, setJobs] = useState<Job[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [comments, setComments] = useState<Record<string, string>>({});
  const { t, status: label, dateTime } = useT();

  useEffect(() => {
    api.issues().then((list) => setIssues([...list].reverse())).catch(() => undefined);
    api.jobs().then((list) => setJobs(list)).catch(() => undefined);
  }, [tick]);

  const close = async (issue: Issue, outcome: "resolved" | "cancelled") => {
    setBusy(issue.issue_id);
    setError(null);
    try {
      const updated = await api.closeIssue(issue.issue_id, outcome, loadOperator(), comments[issue.issue_id] ?? "");
      setIssues((list) => list.map((i) => (i.issue_id === issue.issue_id ? updated : i)));
      onChanged();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(null);
    }
  };

  return (
    <Page icon={ListChecks} title={t("records.title")} subtitle={t("records.subtitle")}>
      {error && (
        <Alert icon={AlertTriangle}>
          <p>{error}</p>
        </Alert>
      )}
      {issues.length === 0 && <EmptyState icon={ListChecks} title={t("records.empty.title")} hint={t("records.empty.hint")} />}
      {issues.map((issue) => {
        const related = jobs.filter((j) => j.issue_id === issue.issue_id);
        const live = LIVE.has(issue.status);
        return (
          <Card key={issue.issue_id}>
            <CardHeader>
              <div className="flex flex-wrap items-center gap-2">
                <CardTitle className="text-base">{issue.title}</CardTitle>
                <Badge variant="secondary">{label(issue.priority)}</Badge>
                <StatusBadge value={issue.status} />
                <span className="ml-auto text-xs text-muted-foreground">{dateTime(issue.created_at)}</span>
              </div>
              <CardDescription>{issue.description}</CardDescription>
              {live && (
                <div className="mt-2 flex flex-wrap items-center gap-2">
                  <Input className="max-w-xs" placeholder={t("records.closingComment")} value={comments[issue.issue_id] ?? ""} onChange={(e) => setComments((c) => ({ ...c, [issue.issue_id]: e.target.value }))} />
                  <Button size="sm" disabled={busy === issue.issue_id} onClick={() => close(issue, "resolved")} title={t("btn.resolve.title")}>
                    <CheckCircle2 />
                    {t("btn.resolve")}
                  </Button>
                  <Button size="sm" variant="outline" disabled={busy === issue.issue_id} onClick={() => close(issue, "cancelled")} title={t("btn.cancel.title")}>
                    <XCircle />
                    {t("btn.cancel")}
                  </Button>
                </div>
              )}
            </CardHeader>
            <CardContent>
              {related.length === 0 && <p className="text-sm text-muted-foreground">{t("records.noJob")}</p>}
              {related.length > 0 && (
                <ul className="divide-y rounded-lg border bg-muted/20">
                  {related.map((job) => (
                    <li key={job.job_id} className="p-3">
                      <div className="flex flex-wrap items-center gap-2 text-sm">
                        <span className="font-mono text-xs text-muted-foreground">{t("records.job", { id: `${job.job_id.slice(0, 8)}…` })}</span>
                        <span className="text-muted-foreground">·</span>
                        <span className="text-xs">{label(job.team_kind)}</span>
                        <StatusBadge value={job.status} />
                        {job.result && <Badge variant="outline">{label(job.result.outcome)}</Badge>}
                        {job.revises_job_id && <Badge variant="outline">{t("records.revises", { id: `${job.revises_job_id.slice(0, 8)}…` })}</Badge>}
                        {job.review && (
                          <Badge variant="outline">
                            {t("records.reviewedBy", { who: job.review.reviewer, decision: job.review.decision.decision === "sent_upstream" ? t("review.sentUpstream", { id: `${job.review.decision.job_id.slice(0, 8)}…` }) : t("review.acknowledged") })}
                          </Badge>
                        )}
                      </div>
                      {job.feedback.length > 0 && (
                        <ul className="mt-2 grid gap-1.5">
                          {job.feedback.map((f) => (
                            <li key={f.feedback_id} className="flex items-start gap-2 rounded-md border border-primary/30 bg-primary/5 p-2 text-xs">
                              <MessageSquareQuote className="mt-0.5 h-3.5 w-3.5 shrink-0 text-primary" />
                              <div className="min-w-0">
                                <span className="font-medium">{f.reviewer}</span>{" "}
                                {f.origin.kind === "denied_action" && (
                                  <>
                                    {t("records.feedback.denied", { runbook: f.origin.runbook_id, reason: f.origin.denial.reason })}
                                    {f.origin.denial.comment && <> — {f.origin.denial.comment}</>}
                                  </>
                                )}
                                {f.origin.kind === "failed_action" && (
                                  <>
                                    {t("records.feedback.failed", { runbook: f.origin.runbook_id, summary: f.origin.summary })}
                                    {f.origin.evidence && <pre className="mt-1 whitespace-pre-wrap rounded bg-muted/60 p-2 font-mono text-[11px] text-muted-foreground">{f.origin.evidence}</pre>}
                                  </>
                                )}
                                {f.origin.kind === "failed_job" && <>{t("records.feedback.job", { summary: f.origin.summary })}</>}
                                {f.comment && <span className="text-muted-foreground"> · “{f.comment}”</span>}
                              </div>
                            </li>
                          ))}
                        </ul>
                      )}
                      {job.result && (
                        <div className="mt-2 text-sm">
                          <p>{job.result.summary}</p>
                          {job.result.unresolved_questions.length > 0 && (
                            <ul className="mt-1 list-disc pl-5 text-xs text-muted-foreground">
                              {job.result.unresolved_questions.map((q, i) => (
                                <li key={i}>{q}</li>
                              ))}
                            </ul>
                          )}
                          {job.result.proposed_actions.length > 0 && (
                            <div className="mt-1 text-xs text-muted-foreground">
                              {t("records.proposed")}{" "}
                              {job.result.proposed_actions.map((p, i) => (
                                <span key={i} className="font-mono">
                                  {p.runbook_id} on {p.target_ids.join(",")}
                                  {i < job.result!.proposed_actions.length - 1 ? "; " : ""}
                                </span>
                              ))}
                            </div>
                          )}
                          <div className="mt-1">
                            {job.result.artifact_ids.map((id) => (
                              <a key={id} className="mr-3 font-mono text-xs text-primary underline-offset-4 hover:underline" href={`/api/artifacts/${id}/body`} target="_blank" rel="noreferrer">
                                {t("transcript", { id: `${id.slice(0, 8)}…` })}
                              </a>
                            ))}
                          </div>
                        </div>
                      )}
                    </li>
                  ))}
                </ul>
              )}
            </CardContent>
          </Card>
        );
      })}
    </Page>
  );
}
