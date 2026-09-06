import { AlertTriangle, Coins, Loader2, Radio, Square } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, streamEvents } from "../api";
import { useT } from "../i18n";
import { loadOperator } from "../lib/prefs";
import type { EventRecord, RunningPass, UsageTotals } from "../types";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Kv, StatTile } from "./ui";

/** Compact token counts: 1_234_567 reads as 1.23M, which is what a bill is discussed in. */
export function tokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${n}`;
}

/** Money is only ever shown when a price list exists; tokens are shown regardless. */
export function money(usage: UsageTotals): string | null {
  return usage.cost === null ? null : `${usage.cost.toFixed(4)} ${usage.currency ?? ""}`.trim();
}

/** The Overview tile: what the relay has cost, and how close that is to the ceiling. */
export function SpendTile({ usage }: { usage: UsageTotals | undefined }) {
  const { t } = useT();
  const budget = usage?.budget ?? null;
  return (
    <StatTile
      label={t("stat.spend")}
      value={usage ? (money(usage) ?? tokens(usage.total_tokens)) : "—"}
      icon={Coins}
      tone={budget?.exceeded ? "alert" : budget && budget.used_fraction >= 0.8 ? "warn" : "default"}
      hint={
        usage
          ? usage.cost === null
            ? t("stat.spendNoPricing")
            : t("stat.spendHint", { passes: usage.passes, tokens: tokens(usage.total_tokens) })
          : undefined
      }
    />
  );
}

/** The full breakdown: tokens in and out, the cache hit, the cost, and any gap in the record. */
export function UsageCard({ usage }: { usage: UsageTotals | null }) {
  const { t } = useT();
  if (!usage) return null;
  const cost = money(usage);
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
                { k: t("usage.input"), v: <span className="font-mono tabular-nums">{usage.input_tokens.toLocaleString()}</span> },
                { k: t("usage.cached"), v: <span className="font-mono tabular-nums text-muted-foreground">{usage.cached_input_tokens.toLocaleString()}</span> },
                { k: t("usage.output"), v: <span className="font-mono tabular-nums">{usage.output_tokens.toLocaleString()}</span> },
                { k: t("usage.total"), v: <span className="font-mono font-medium tabular-nums">{usage.total_tokens.toLocaleString()}</span> },
                ...(cost ? [{ k: t("usage.cost"), v: <span className="font-mono font-medium tabular-nums">{cost}</span> }] : []),
              ]}
            />
            {usage.by_model.length > 1 && (
              <ul className="divide-y rounded-lg border text-xs">
                {usage.by_model.map((model) => (
                  <li key={model.model} className="flex items-center gap-2 px-3 py-2">
                    <span className="truncate font-mono font-medium">{model.model}</span>
                    <span className="ml-auto shrink-0 font-mono tabular-nums text-muted-foreground">
                      {tokens(model.input_tokens + model.output_tokens)}
                      {model.cost !== null && ` · ${model.cost.toFixed(4)}`}
                    </span>
                  </li>
                ))}
              </ul>
            )}
            {usage.cost === null && <p className="text-xs text-muted-foreground">{t("usage.noPricing")}</p>}
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
export function RunningCard({ running, onChanged }: { running: RunningPass[]; onChanged: () => void }) {
  const { t } = useT();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const interrupt = async (jobId: string) => {
    setBusy(jobId);
    setError(null);
    try {
      await api.cancelJob(jobId, loadOperator());
      onChanged();
    } catch (e) {
      setError(t("running.interruptFailed", { error: (e as Error).message }));
    } finally {
      setBusy(null);
    }
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Loader2 className={`h-4 w-4 ${running.length > 0 ? "animate-spin" : ""}`} />
          {t("running.title")}
        </CardTitle>
        <CardDescription>{t("running.desc")}</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-2">
        {error && (
          <Alert icon={AlertTriangle}>
            <p>{error}</p>
          </Alert>
        )}
        {running.length === 0 && <p className="text-sm text-muted-foreground">{t("running.none")}</p>}
        {running.map((pass) => (
          <div key={pass.job_id} className="flex flex-wrap items-center gap-2 rounded-lg border p-3 text-sm">
            <span className="font-mono text-xs font-medium">{t("running.job", { id: `${pass.job_id.slice(0, 8)}…` })}</span>
            <span className="font-mono text-xs text-muted-foreground">{t("running.since", { time: pass.started_at.slice(11, 19) })}</span>
            <Button size="sm" variant="outline" className="ml-auto" disabled={busy === pass.job_id} onClick={() => interrupt(pass.job_id)}>
              <Square />
              {busy === pass.job_id ? t("running.interrupting") : t("running.interrupt")}
            </Button>
          </div>
        ))}
      </CardContent>
    </Card>
  );
}

/**
 * Live progress for a request the viewer is currently blocked on.
 *
 * The control plane already streams every step as an event, so this subscribes to the same SSE
 * feed rather than inventing a second channel: each Team callback is one line, newest last. It
 * is the answer to "is it stuck, or is it working?" during a pass that takes minutes.
 */
export function LiveProgress({ active }: { active: boolean }) {
  const [lines, setLines] = useState<EventRecord[]>([]);
  const [live, setLive] = useState(false);
  const list = useRef<HTMLDivElement>(null);
  const { t } = useT();

  useEffect(() => {
    if (!active) return;
    setLines([]);
    let stop: (() => void) | null = null;
    let cancelled = false;
    // Start from the current tail so only this request's steps are shown.
    api
      .events(1)
      .then((tail) => {
        if (cancelled) return;
        stop = streamEvents(tail.length ? tail[tail.length - 1].sequence : 0, (event) => {
          if (event.kind === "team.callback" || event.kind === "model.usage") {
            setLines((all) => [...all.slice(-99), event]);
          }
        });
        setLive(true);
      })
      .catch(() => setLive(false));
    return () => {
      cancelled = true;
      stop?.();
      setLive(false);
    };
  }, [active]);

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
          {t("progress.live")}
        </Badge>
      </div>
      <div ref={list} className="max-h-48 overflow-y-auto rounded-md border bg-muted/30 p-2 font-mono text-xs">
        {lines.length === 0 && <p className="text-muted-foreground">{t("progress.waiting")}</p>}
        {lines.map((line) => (
          <div key={line.sequence} className="flex gap-2 py-0.5">
            <span className="shrink-0 tabular-nums text-muted-foreground">{line.occurred_at.slice(11, 19)}</span>
            <span className="whitespace-pre-wrap break-words">{line.summary}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
