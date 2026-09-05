import { AlertTriangle, FilePlus2, Send } from "lucide-react";
import { useState } from "react";
import { api } from "../api";
import { loadOperator } from "../lib/prefs";
import type { ActionRun, Issue, Job } from "../types";
import { Page } from "./Shell";
import { StatusBadge } from "./status";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Field, Input, Select, Textarea } from "./ui";

export function Report({ onChanged }: { onChanged: () => void }) {
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [reporter, setReporter] = useState(loadOperator);
  const [priority, setPriority] = useState<string>("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<{ issue: Issue; job: Job; actions: ActionRun[] } | null>(null);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      setResult(await api.report({ title, description, reporter, priority: priority || undefined }));
      onChanged();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Page icon={FilePlus2} title="File a report" subtitle="Human reports bypass anomaly detection and default to the top priority. The Operate Team diagnoses from a fresh Snapshot; any proposed actions go through the authority matrix.">
      <div className="grid gap-6 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Report</CardTitle>
            <CardDescription>What you saw, in your own words. The Team reads it as data, never as instructions.</CardDescription>
          </CardHeader>
          <CardContent>
            <form className="grid gap-4" onSubmit={submit}>
              <Field label="Title">
                <Input value={title} onChange={(e) => setTitle(e.target.value)} required placeholder="Contestants cannot submit" />
              </Field>
              <Field label="What you observed">
                <Textarea rows={5} value={description} onChange={(e) => setDescription(e.target.value)} required placeholder="Web submissions time out since 10:12; the queue keeps growing" />
              </Field>
              <div className="grid gap-4 sm:grid-cols-2">
                <Field label="Reporter">
                  <Input value={reporter} onChange={(e) => setReporter(e.target.value)} />
                </Field>
                <Field label="Priority" hint="Top is reserved for humans; lower it deliberately for non-urgent reports.">
                  <Select value={priority} onChange={(e) => setPriority(e.target.value)}>
                    <option value="">top (default)</option>
                    <option value="critical">critical</option>
                    <option value="high">high</option>
                    <option value="normal">normal</option>
                    <option value="low">low</option>
                  </Select>
                </Field>
              </div>
              <div>
                <Button type="submit" disabled={busy}>
                  <Send />
                  {busy ? "Running the Operate Team…" : "File report"}
                </Button>
              </div>
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
            <CardTitle className="text-base">Outcome</CardTitle>
            <CardDescription>{result ? "The Team's diagnosis and what happened to its proposals." : "The diagnosis appears here. A model-backed run takes up to a minute."}</CardDescription>
          </CardHeader>
          <CardContent>
            {!result && <p className="text-sm text-muted-foreground">—</p>}
            {result && (
              <div className="grid gap-3">
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="secondary">{result.issue.priority.replace(/_/g, " ")}</Badge>
                  <StatusBadge value={result.issue.status} />
                  <StatusBadge value={result.job.status} />
                  {result.job.result && <Badge variant="outline">{result.job.result.outcome.replace(/_/g, " ")}</Badge>}
                </div>
                <pre className="whitespace-pre-wrap rounded-md border bg-muted/30 p-3 font-mono text-xs">{result.job.result?.summary ?? "no result"}</pre>
                {result.job.result && result.job.result.unresolved_questions.length > 0 && (
                  <ul className="list-disc pl-5 text-xs text-muted-foreground">
                    {result.job.result.unresolved_questions.map((q, i) => (
                      <li key={i}>{q}</li>
                    ))}
                  </ul>
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
