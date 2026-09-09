import { AlertTriangle, FilePlus2, Send } from "lucide-react";
import { useEffect, useState } from "react";
import { api } from "../api";
import { useT } from "../i18n";
import { loadOperator } from "../lib/prefs";
import { traceHash } from "../lib/routes";
import type { ActionRun, Issue, Job, PassOutcome } from "../types";
import { Page } from "./Shell";
import { LiveProgress } from "./Spend";
import { StatusBadge } from "./status";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Field, Input, Select, Textarea } from "./ui";

const DRAFT_KEY = "broccoli.report.draft.v1";
function loadDraft() {
  const fallback = { title: "", description: "", reporter: loadOperator(), priority: "" };
  try {
    const saved = JSON.parse(sessionStorage.getItem(DRAFT_KEY) ?? "null");
    if (!saved || typeof saved !== "object") return fallback;
    return {
      title: typeof saved.title === "string" ? saved.title : "",
      description: typeof saved.description === "string" ? saved.description : "",
      reporter: typeof saved.reporter === "string" ? saved.reporter : fallback.reporter,
      priority: ["", "critical", "high", "normal", "low"].includes(saved.priority) ? saved.priority as string : "",
    };
  } catch { return fallback; }
}

export function Report({ onChanged }: { onChanged: () => void }) {
  const [draft, setDraft] = useState(loadDraft);
  const { title, description, reporter, priority } = draft;
  useEffect(() => {
    try { sessionStorage.setItem(DRAFT_KEY, JSON.stringify(draft)); } catch { /* Private browsing may disable storage. */ }
  }, [draft]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{ issue: Issue; job: Job; actions: ActionRun[]; passes: PassOutcome[] } | null>(null);
  const { t, status: label, dateTime } = useT();

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      setResult(await api.report({ title, description, reporter, priority: priority || undefined }));
      setDraft((current) => current.title === title && current.description === description && current.reporter === reporter && current.priority === priority ? { ...current, title: "", description: "", priority: "" } : current);
      onChanged();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Page icon={FilePlus2} title={t("report.title")} subtitle={t("report.subtitle")}>
      <div className="grid gap-6 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t("report.card.title")}</CardTitle>
            <CardDescription>{t("report.card.desc")}</CardDescription>
          </CardHeader>
          <CardContent>
            <form className="grid gap-4" onSubmit={submit}>
              <Field label={t("field.title")}>
                <Input value={title} onChange={(e) => setDraft((current) => ({ ...current, title: e.target.value }))} required placeholder={t("field.title.placeholder")} />
              </Field>
              <Field label={t("field.observed")}>
                <Textarea rows={5} value={description} onChange={(e) => setDraft((current) => ({ ...current, description: e.target.value }))} required placeholder={t("field.observed.placeholder")} />
              </Field>
              <div className="grid gap-4 sm:grid-cols-2">
                <Field label={t("field.reporter")}>
                  <Input value={reporter} onChange={(e) => setDraft((current) => ({ ...current, reporter: e.target.value }))} />
                </Field>
                <Field label={t("field.priority")} hint={t("field.priority.hint")}>
                  <Select value={priority} onChange={(e) => setDraft((current) => ({ ...current, priority: e.target.value }))}>
                    <option value="">{t("priority.top")}</option>
                    <option value="critical">{t("priority.critical")}</option>
                    <option value="high">{t("priority.high")}</option>
                    <option value="normal">{t("priority.normal")}</option>
                    <option value="low">{t("priority.low")}</option>
                  </Select>
                </Field>
              </div>
              <div>
                <Button type="submit" disabled={busy}>
                  <Send />
                  {busy ? t("btn.filing") : t("btn.file")}
                </Button>
              </div>
              {/* A model-backed report blocks for minutes; the steps stream in while it runs. */}
              <LiveProgress active={busy} />
              {error && (
                <Alert icon={AlertTriangle}>
                  <p>{error}</p>
                </Alert>
              )}
            </form>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t("outcome.title")}</CardTitle>
            <CardDescription>{result ? t("outcome.desc.done") : t("outcome.desc.pending")}</CardDescription>
          </CardHeader>
          <CardContent>
            {!result && <p className="text-sm text-muted-foreground">—</p>}
            {result && (
              <div className="grid gap-3">
                <a className="font-medium text-primary hover:underline" href={traceHash(result.issue.issue_id)}>{result.issue.title}</a>
                <div className="grid gap-1 text-xs text-muted-foreground">
                  <time dateTime={result.issue.created_at}>{t("feedback.issueCreated", { time: dateTime(result.issue.created_at) })}</time>
                  <a className="text-primary hover:underline" href={traceHash(result.issue.issue_id, result.job.job_id)}>{t("records.job", { id: `…${result.job.job_id.slice(-8)}` })} · {t("records.trace")}</a>
                  <time dateTime={result.job.created_at}>{t("feedback.passStarted", { time: dateTime(result.job.created_at) })}</time>
                </div>
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="secondary">{label(result.issue.priority)}</Badge>
                  <StatusBadge value={result.issue.status} />
                  <StatusBadge value={result.job.status} />
                  {result.job.result && <Badge variant="outline">{label(result.job.result.outcome)}</Badge>}
                </div>
                <pre className="whitespace-pre-wrap rounded-md border bg-muted/30 p-3 font-mono text-xs">{result.job.result?.summary ?? t("outcome.noResult")}</pre>
                {result.job.result && result.job.result.unresolved_questions.length > 0 && (
                  <ul className="list-disc pl-5 text-xs text-muted-foreground">
                    {result.job.result.unresolved_questions.map((q, i) => (
                      <li key={i}>{q}</li>
                    ))}
                  </ul>
                )}
                {result.passes.length > 1 && (
                  <p className="text-xs text-muted-foreground">
                    {result.passes.map((pass, index) => <a key={pass.job.job_id} className="mr-3 text-primary hover:underline" href={traceHash(result.issue.issue_id, pass.job.job_id)}>{t("trace.pass", { n: index + 1 })}: {label(pass.stop)}</a>)}
                  </p>
                )}
                {result.actions.length > 0 && (
                  <ul className="divide-y rounded-lg border">
                    {result.actions.map((a) => (
                      <li key={a.action_run_id} className="flex flex-wrap items-center gap-2 p-3 text-sm">
                        <span className="font-mono text-xs font-medium">{a.runbook_id}</span>
                        <span className="font-mono text-xs text-muted-foreground">on {a.target_ids.join(",")}</span>
                        <StatusBadge value={a.status} className="ml-auto" />
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            )}
          </CardContent>
        </Card>
      </div>
    </Page>
  );
}
