import { AlertTriangle, Coins, Loader2, Radio, Square, Waypoints } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, streamEvents } from "../api";
import { useT } from "../i18n";
import { loadOperator } from "../lib/prefs";
import { traceHash } from "../lib/routes";
import { ReportProgressScope, type ReportProgressRequest } from "../lib/report-progress";
import type { ActiveOperation, EventRecord, RunningPass, UsageTotals } from "../types";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Kv, StatTile } from "./ui";

/** One unit throughout the console: one Mtok is one million tokens. */
export function tokens(n: number): string {
  return `${(n / 1_000_000).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })} Mtoks`;
}

/** Cumulative model usage and proximity to the configured budget. */
export function SpendTile({ usage }: { usage: UsageTotals | undefined }) {
  const { t } = useT();
  const budget = usage?.budget ?? null;
  const noCalls = usage?.passes === 0;
  return (
    <StatTile
      label={t("usage.title")}
      value={usage ? tokens(usage.total_tokens) : "—"}
      icon={Coins}
      tone={budget?.exceeded ? "alert" : budget && budget.used_fraction >= 0.8 ? "warn" : "default"}
      hint={
        usage
          ? noCalls
            ? t("usage.none")
            : t("stat.usageHint", { requests: usage.requests })
          : undefined
      }
    />
  );
}

/** Input, output, cached usage, and any gap in the record. */
export function UsageCard({ usage }: { usage: UsageTotals | null }) {
  const { t } = useT();
  if (!usage) return null;
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Coins className="h-4 w-4" />
          {t("usage.title")}
        </CardTitle>
        <CardDescription>{t("usage.desc")}</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-3">
        {usage.passes === 0 && <p className="text-sm text-muted-foreground">{t("usage.none")}</p>}
        {usage.passes > 0 && (
          <>
            <Kv
              rows={[
                { k: t("usage.input"), v: <span className="font-mono tabular-nums">{tokens(usage.input_tokens)}</span> },
                { k: t("usage.cached"), v: <span className="font-mono tabular-nums text-muted-foreground">{tokens(usage.cached_input_tokens)}</span> },
                { k: t("usage.output"), v: <span className="font-mono tabular-nums">{tokens(usage.output_tokens)}</span> },
                { k: t("usage.total"), v: <span className="font-mono font-medium tabular-nums">{tokens(usage.total_tokens)}</span> },
              ]}
            />
            {usage.by_model.length > 1 && (
              <ul className="divide-y rounded-lg border text-xs">
                {usage.by_model.map((model) => (
                  <li key={model.model} className="flex items-center gap-2 px-3 py-2">
                    <span className="truncate font-mono font-medium">{model.model}</span>
                    <span className="ml-auto shrink-0 font-mono tabular-nums text-muted-foreground">
                      {tokens(model.input_tokens + model.output_tokens)}
                    </span>
                  </li>
                ))}
              </ul>
            )}
            {usage.requests_without_usage > 0 && (
              <p className="text-xs text-amber-600 dark:text-amber-400">{t("usage.gap", { count: usage.requests_without_usage })}</p>
            )}
            {usage.budget && (
              <div className="grid gap-1.5">
                <div className="flex items-center justify-between text-xs text-muted-foreground">
                  <span>{t("usage.budget", { percent: Math.round(usage.budget.used_fraction * 100) })}</span>
                </div>
                <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                  <div
                    className={`h-full rounded-full ${usage.budget.exceeded ? "bg-destructive" : usage.budget.used_fraction >= 0.8 ? "bg-amber-500" : "bg-primary"}`}
                    style={{ width: `${Math.min(100, usage.budget.used_fraction * 100)}%` }}
                  />
                </div>
              </div>
            )}
            {usage.budget?.exceeded && (
              <Alert tone="warning" icon={AlertTriangle}>
                <p>{t("usage.budgetExceeded")}</p>
                {usage.budget.reason && <p className="mt-0.5 text-muted-foreground">{usage.budget.reason}</p>}
              </Alert>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}

/** The passes in flight, each with the button that stops it. */
export function RunningCard({ operations, running = [], onChanged }: { operations?: ActiveOperation[]; running?: RunningPass[]; onChanged: () => void }) {
  const { t, dateTime, age } = useT();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 1000); return () => clearInterval(timer); }, []);
  const activities: ActiveOperation[] = operations ?? running.map(pass => ({ operation_id: pass.job_id, kind: "job", phase: "model", started_at: pass.started_at, issue_id: pass.issue_id, job_id: pass.job_id, action_run_id: null, cancel_requested: false }));
  const interrupt = async (id: string) => {
    setBusy(id); setError(null);
    try {
      if (operations !== undefined) await api.cancelOperation(id, loadOperator());
      else await api.cancelJob(id, loadOperator());
      onChanged();
    } catch (e) { setError(t("running.interruptFailed", { error: (e as Error).message })); }
    finally { setBusy(null); }
  };
  return <Card>
    <CardHeader>
      <CardTitle className="flex items-center gap-2 text-base"><Loader2 className={`h-4 w-4 ${activities.length ? "animate-spin" : ""}`} />{t("running.title")}</CardTitle>
      <CardDescription>{t("running.operationsDesc")}</CardDescription>
    </CardHeader>
    <CardContent className="grid gap-2">
      {error && <Alert icon={AlertTriangle}>{error}</Alert>}
      {!activities.length && <p className="text-sm text-muted-foreground">{t("running.none")}</p>}
      {activities.map(op => {
        const pass = running.find(pass => pass.job_id === op.job_id);
        return <div key={op.operation_id} className="flex flex-wrap items-center gap-2 rounded-lg border p-3 text-sm">
          <span className="font-medium">{t(`running.phase.${op.phase}`)}</span>
          <span className="text-xs text-muted-foreground">{t("running.since", { time: dateTime(op.started_at) })}</span>
          <span className="text-xs text-muted-foreground">{t("running.elapsed", { seconds: Math.max(0, Math.floor((now - Date.parse(op.started_at)) / 1000)) })}</span>
          {op.issue_id && <a className="ml-auto inline-flex items-center gap-1 text-xs underline [&_svg]:size-4" href={traceHash(op.issue_id, op.job_id ?? undefined)}><Waypoints />{t("running.trace")}</a>}
          <Button size="sm" variant="outline" disabled={busy === op.operation_id || op.cancel_requested} onClick={() => interrupt(op.operation_id)}><Square />{busy === op.operation_id || op.cancel_requested ? t("running.interrupting") : t("running.interrupt")}</Button>
          {pass && <div className="w-full border-t pt-2 text-xs">
            <p className="whitespace-pre-wrap break-words">{pass.last_progress ?? t("progress.waiting")}</p>
            {pass.last_progress_at && <p className="mt-1 text-muted-foreground" title={dateTime(pass.last_progress_at)}>{t("running.lastProgress", { age: age(pass.last_progress_at) })}</p>}
            {now - Date.parse(pass.last_progress_at ?? pass.started_at) >= 120_000 && <p className="mt-1 text-amber-600 dark:text-amber-400">{t("running.quiet")}</p>}
          </div>}
        </div>;
      })}
    </CardContent>
  </Card>;
}

/**
 * Live progress for a request the viewer is currently blocked on.
 *
 * The control plane already streams every step as an event, so this subscribes to the same SSE
 * feed rather than inventing a second channel: each Team callback is one line, newest last. It
 * is the answer to "is it stuck, or is it working?" during a pass that takes minutes.
 */
export function LiveProgress({ active, request }: { active: boolean; request: ReportProgressRequest | null }) {
  const [lines, setLines] = useState<EventRecord[]>([]);
  const [live, setLive] = useState(false);
  const [target, setTarget] = useState<{ issueId: string; jobId?: string } | null>(null);
  const list = useRef<HTMLDivElement>(null);
  const { t, dateTime } = useT();

  useEffect(() => {
    setLines([]); setTarget(null); setLive(false);
    if (!active || !request) return;
    let cancelled = false;
    const scope = new ReportProgressScope(request.reportId);
    const stop = streamEvents(request.after, (event) => {
      if (cancelled) return;
      if (scope.accept(event)) setLines((all) => [...all.slice(-99), event]);
      if (scope.issueId) {
        const issueId = scope.issueId, jobId = scope.jobId ?? undefined;
        setTarget((current) => current?.issueId === issueId && current?.jobId === jobId ? current : { issueId, jobId });
      }
    }, (connected) => { if (!cancelled) setLive(connected); });
    return () => { cancelled = true; stop(); };
  }, [active, request]);

  useEffect(() => {
    const node = list.current;
    if (node) node.scrollTop = node.scrollHeight;
  }, [lines.length]);

  if (!active) return null;
  return (
    <div className="grid gap-2">
      <div className="flex items-center gap-2">
        <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">{t("progress.title")}</span>
        <Badge variant={live ? "success" : "outline"}>
          <Radio className="h-3 w-3" />
          {live ? t("progress.live") : t("events.disconnected")}
        </Badge>
        {target && <a className="ml-auto text-xs text-primary hover:underline" href={traceHash(target.issueId, target.jobId)}>{t("records.trace")}</a>}
      </div>
      <div ref={list} className="max-h-48 overflow-y-auto rounded-md border bg-muted/30 p-2 font-mono text-xs">
        {lines.length === 0 && <p className="text-muted-foreground">{t("progress.waiting")}</p>}
        {lines.map((line) => (
          <div key={line.sequence} className="flex gap-2 py-0.5">
            <span className="shrink-0 tabular-nums text-muted-foreground">{dateTime(line.occurred_at)}</span>
            <span className="whitespace-pre-wrap break-words">{line.summary}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
