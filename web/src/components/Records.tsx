import { AlertTriangle, Archive, CheckCircle2, Download, ListChecks, MessageSquareQuote, Upload, Waypoints, XCircle } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import { useT } from "../i18n";
import { loadOperator } from "../lib/prefs";
import { traceHash } from "../lib/routes";
import type { Issue, Job, SessionBundle } from "../types";
import { Page } from "./Shell";
import { StatusBadge } from "./status";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, EmptyState, Input, Segmented } from "./ui";

const LIVE = new Set(["open", "investigating", "waiting_for_human", "mitigating", "verifying"]);

type Filter = "all" | "live" | "closed" | "archived";

/** A link styled as a small outline button (a real anchor, so downloads and hashes just work). */
function LinkButton({ href, title, download, children }: { href: string; title?: string; download?: boolean; children: React.ReactNode }) {
  return (
    <a
      href={href}
      title={title}
      download={download}
      className="inline-flex h-8 items-center gap-1 whitespace-nowrap rounded-md border border-input bg-background px-3 text-xs font-medium shadow-xs transition-colors hover:bg-accent hover:text-accent-foreground [&_svg]:size-4"
    >
      {children}
    </a>
  );
}

export function Records({ tick, onChanged }: { tick: number; onChanged: () => void }) {
  const [issues, setIssues] = useState<Issue[]>([]);
  const [jobs, setJobs] = useState<Job[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [comments, setComments] = useState<Record<string, string>>({});
  const [filter, setFilter] = useState<Filter>("all");
  const [query, setQuery] = useState("");
  const [importing, setImporting] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);
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

  const importFile = async (file: File) => {
    setImporting(true);
    setError(null);
    setNotice(null);
    try {
      const bundle = JSON.parse(await file.text()) as SessionBundle;
      const summary = await api.importSession(bundle, loadOperator());
      setNotice(t("records.imported", { title: summary.title, deployment: summary.source_deployment, jobs: summary.jobs, actions: summary.action_runs, events: summary.events }));
      setFilter("archived");
      onChanged();
    } catch (e) {
      setError(t("records.importFailed", { error: (e as Error).message }));
    } finally {
      setImporting(false);
      if (fileInput.current) fileInput.current.value = "";
    }
  };

  const needle = query.trim().toLowerCase();
  const visible = issues.filter((issue) => {
    const archived = Boolean(issue.provenance);
    if (filter === "archived" && !archived) return false;
    if (filter === "live" && (archived || !LIVE.has(issue.status))) return false;
    if (filter === "closed" && (archived || LIVE.has(issue.status))) return false;
    if (!needle) return true;
    return issue.title.toLowerCase().includes(needle) || issue.description.toLowerCase().includes(needle) || issue.issue_id.startsWith(needle);
  });
  const counts = {
    all: issues.length,
    live: issues.filter((i) => !i.provenance && LIVE.has(i.status)).length,
    closed: issues.filter((i) => !i.provenance && !LIVE.has(i.status)).length,
    archived: issues.filter((i) => Boolean(i.provenance)).length,
  };

  return (
    <Page
      icon={ListChecks}
      title={t("records.title")}
      subtitle={t("records.subtitle")}
      actions={
        <>
          <Segmented
            value={filter}
            onChange={setFilter}
            options={[
              { id: "all", label: t("records.filter.all"), count: counts.all },
              { id: "live", label: t("records.filter.live"), count: counts.live },
              { id: "closed", label: t("records.filter.closed"), count: counts.closed },
              { id: "archived", label: t("records.filter.archived"), count: counts.archived },
            ]}
          />
          <Input className="h-8 w-56 text-xs" placeholder={t("records.search")} value={query} onChange={(e) => setQuery(e.target.value)} />
          <input ref={fileInput} type="file" accept="application/json,.json" className="hidden" onChange={(e) => e.target.files?.[0] && void importFile(e.target.files[0])} />
          <Button size="sm" variant="outline" disabled={importing} onClick={() => fileInput.current?.click()} title={t("records.import.title")}>
            <Upload />
            {importing ? t("records.importing") : t("records.import")}
          </Button>
        </>
      }
    >
      {error && (
        <Alert icon={AlertTriangle}>
          <p>{error}</p>
        </Alert>
      )}
      {notice && (
        <Alert tone="info" icon={Archive}>
          <p>{notice}</p>
        </Alert>
      )}
      {issues.length === 0 && <EmptyState icon={ListChecks} title={t("records.empty.title")} hint={t("records.empty.hint")} />}
      {issues.length > 0 && visible.length === 0 && <EmptyState icon={ListChecks} title={t("records.none.filtered")} />}
      {visible.map((issue) => {
        const related = jobs.filter((j) => j.issue_id === issue.issue_id);
        const archived = issue.provenance ?? null;
        const live = !archived && LIVE.has(issue.status);
        return (
          <Card key={issue.issue_id}>
            <CardHeader>
              <div className="flex flex-wrap items-center gap-2">
                <CardTitle className="text-base">{issue.title}</CardTitle>
                <Badge variant="secondary">{label(issue.priority)}</Badge>
                <StatusBadge value={issue.status} />
                {archived && (
                  <Badge
                    variant="warning"
                    title={t("records.archive.title", {
                      deployment: archived.source_deployment,
                      exporter: archived.exported_by,
                      exported: dateTime(archived.exported_at),
                      importer: archived.imported_by,
                      imported: dateTime(archived.imported_at),
                    })}
                  >
                    <Archive className="h-3 w-3" />
                    {t("records.archive")}
                  </Badge>
                )}
                <span className="ml-auto text-xs text-muted-foreground">{dateTime(issue.created_at)}</span>
              </div>
              <CardDescription>{issue.description}</CardDescription>
              <div className="mt-2 flex flex-wrap items-center gap-2">
                <LinkButton href={traceHash(issue.issue_id)} title={t("records.trace.title")}>
                  <Waypoints />
                  {t("records.trace")}
                </LinkButton>
                <LinkButton href={api.sessionDownloadUrl(issue.issue_id, loadOperator())} title={t("records.export.title")} download>
                  <Download />
                  {t("records.export")}
                </LinkButton>
                {live && (
                  <>
                    <Input className="ml-auto h-8 max-w-xs text-xs" placeholder={t("records.closingComment")} value={comments[issue.issue_id] ?? ""} onChange={(e) => setComments((c) => ({ ...c, [issue.issue_id]: e.target.value }))} />
                    <Button size="sm" disabled={busy === issue.issue_id} onClick={() => close(issue, "resolved")} title={t("btn.resolve.title")}>
                      <CheckCircle2 />
                      {t("btn.resolve")}
                    </Button>
                    <Button size="sm" variant="outline" disabled={busy === issue.issue_id} onClick={() => close(issue, "cancelled")} title={t("btn.cancel.title")}>
                      <XCircle />
                      {t("btn.cancel")}
                    </Button>
                  </>
                )}
              </div>
            </CardHeader>
            <CardContent>
              {related.length === 0 && <p className="text-sm text-muted-foreground">{t("records.noJob")}</p>}
              {related.length > 0 && (
                <ul className="divide-y rounded-lg border bg-muted/20">
                  {related.map((job) => (
                    <li key={job.job_id} className="p-3">
                      <div className="flex flex-wrap items-center gap-2 text-sm">
                        <a className="font-mono text-xs text-primary underline-offset-4 hover:underline" href={traceHash(issue.issue_id, job.job_id)} title={t("records.trace.title")}>
                          {t("records.job", { id: `${job.job_id.slice(0, 8)}…` })}
                        </a>
                        <span className="text-muted-foreground">·</span>
                        <span className="text-xs">{label(job.team_kind)}</span>
                        <StatusBadge value={job.status} />
                        {job.result && <Badge variant="outline">{label(job.result.outcome)}</Badge>}
                        {(job.earlier_passes?.length ?? 0) > 0 && <Badge variant="outline">{t("records.pass", { n: (job.earlier_passes?.length ?? 0) + 1 })}</Badge>}
                        {job.revises_job_id && <Badge variant="outline">{t("records.revises", { id: `${job.revises_job_id.slice(0, 8)}…` })}</Badge>}
                        {job.supersedes_job_id && <Badge variant="outline">{t("records.supersedes", { id: `${job.supersedes_job_id.slice(0, 8)}…` })}</Badge>}
                        {job.continues_job_id && <Badge variant="outline">{t("records.follows", { id: `${job.continues_job_id.slice(0, 8)}…` })}</Badge>}
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
                                {f.origin.kind === "stalled_job" && <>{t("records.feedback.stalled", { probes: f.origin.requested_probe_ids.join(", "), summary: f.origin.summary })}</>}
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
                          <div className="mt-1 flex flex-wrap items-center gap-3">
                            <a className="inline-flex items-center gap-1 text-xs text-primary underline-offset-4 hover:underline" href={traceHash(issue.issue_id, job.job_id)}>
                              <Waypoints className="h-3.5 w-3.5" />
                              {t("records.trace")}
                            </a>
                            {job.result.artifact_ids.map((id) => (
                              <a key={id} className="font-mono text-xs text-muted-foreground underline-offset-4 hover:underline" href={`/api/artifacts/${id}/body`} target="_blank" rel="noreferrer">
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
