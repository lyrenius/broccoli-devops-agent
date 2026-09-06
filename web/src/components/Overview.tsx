import { Activity, AlertTriangle, Camera, EyeOff, Inbox as InboxIcon, LayoutDashboard, ListChecks, Server } from "lucide-react";
import { useEffect, useState } from "react";
import { api } from "../api";
import { useT } from "../i18n";
import type { Issue, Snapshot, Status } from "../types";
import { Page } from "./Shell";
import { RunningCard, SpendTile, UsageCard } from "./Spend";
import { StatusBadge } from "./status";
import { Alert, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, EmptyState, StatTile } from "./ui";

const LIVE = new Set(["open", "investigating", "waiting_for_human", "mitigating", "verifying"]);

export function Overview({ tick, status, onChanged }: { tick: number; status: Status | null; onChanged: () => void }) {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [issues, setIssues] = useState<Issue[]>([]);
  const [missing, setMissing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { t, status: label, kind, age } = useT();

  useEffect(() => {
    api
      .latestSnapshot()
      .then((s) => {
        setSnapshot(s);
        setMissing(false);
      })
      .catch(() => setMissing(true));
    api.issues().then(setIssues).catch(() => undefined);
  }, [tick]);

  const capture = async () => {
    setBusy(true);
    setError(null);
    try {
      setSnapshot(await api.capture());
      setMissing(false);
      onChanged();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const healthy = snapshot?.resources.filter((r) => r.health === "healthy").length ?? 0;
  const total = snapshot?.resources.length ?? 0;
  const liveIssues = issues.filter((i) => LIVE.has(i.status)).length;
  const stale = snapshot ? Date.now() - new Date(snapshot.created_at).getTime() > 10 * 60 * 1000 : false;
  const frozenAfterRecovery = status?.recovery && status.mode !== "running";

  return (
    <Page
      icon={LayoutDashboard}
      title={t("overview.title")}
      subtitle={
        snapshot
          ? t("overview.subtitle.snapshot", { age: age(snapshot.created_at), cause: label(snapshot.cause), rev: snapshot.topology_revision })
          : t("overview.subtitle.default")
      }
      actions={
        <Button onClick={capture} disabled={busy}>
          <Camera />
          {busy ? t("overview.capturing") : t("overview.capture")}
        </Button>
      }
    >
      {error && (
        <Alert icon={AlertTriangle}>
          <p className="font-medium">{t("overview.captureFailed")}</p>
          <p className="mt-0.5 text-muted-foreground">{error}</p>
        </Alert>
      )}
      {frozenAfterRecovery && status && (
        <Alert tone="warning" icon={AlertTriangle}>
          <p className="font-medium">{t("overview.recovered.title", { mode: label(status.mode) })}</p>
          <p className="mt-0.5">
            {t("overview.recovered.body", {
              count: status.recovery!.interrupted_job_ids.length + status.recovery!.interrupted_action_ids.length,
              previous: status.recovery!.previous_mode !== "running" ? t("overview.recovered.previous", { mode: label(status.recovery!.previous_mode) }) : "",
            })}
          </p>
        </Alert>
      )}
      {stale && (
        <Alert tone="info" icon={AlertTriangle}>
          {t("overview.stale")}
        </Alert>
      )}

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-5">
        <StatTile label={t("stat.healthy")} value={snapshot ? `${healthy} / ${total}` : "—"} icon={Server} tone={snapshot && healthy < total ? "warn" : "default"} />
        <StatTile
          label={t("stat.inbox")}
          value={status?.inbox.total ?? "—"}
          icon={InboxIcon}
          tone={status && status.inbox.total > 0 ? "alert" : "default"}
          hint={status ? t("stat.inboxHint", { requests: status.inbox.permission_requests, denied: status.inbox.permission_denied, failed: status.inbox.failed_jobs + status.inbox.failed_actions }) : undefined}
        />
        <StatTile label={t("stat.openIssues")} value={liveIssues} icon={ListChecks} hint={t("stat.total", { count: issues.length })} />
        <StatTile label={t("stat.events")} value={status?.counts.events ?? "—"} icon={Activity} hint={status ? t("stat.eventsHint", { jobs: status.counts.jobs, actions: status.counts.actions }) : undefined} />
        <SpendTile usage={status?.usage} />
      </div>

      {/* What is happening right now and what it has cost: the two live facts a Snapshot cannot show. */}
      <div className="grid gap-6 lg:grid-cols-2">
        <RunningCard running={status?.running ?? []} onChanged={onChanged} />
        <UsageCard usage={status?.usage ?? null} />
      </div>

      <div className="grid gap-6 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              <Server className="h-4 w-4" />
              {t("resources.title")}
            </CardTitle>
            <CardDescription>{t("resources.desc")}</CardDescription>
          </CardHeader>
          <CardContent>
            {missing && <EmptyState icon={Camera} title={t("resources.empty.title")} hint={t("resources.empty.hint")} />}
            {snapshot && (
              <div className="overflow-x-auto rounded-lg border">
                <table className="w-full text-sm">
                  <thead>
                    <tr className="border-b bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
                      <th className="whitespace-nowrap px-3 py-2 font-medium">{t("col.resource")}</th>
                      <th className="whitespace-nowrap px-3 py-2 font-medium">{t("col.kind")}</th>
                      <th className="whitespace-nowrap px-3 py-2 font-medium">{t("col.health")}</th>
                      <th className="w-full px-3 py-2 font-medium">{t("col.signals")}</th>
                      <th className="whitespace-nowrap px-3 py-2 text-right font-medium">{t("col.latency")}</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y">
                    {snapshot.resources.map((r) => {
                      const latency = r.metrics.find((m) => m.name.endsWith(".latency"));
                      const signals = r.metrics.filter((m) => !m.name.startsWith("probe."));
                      return (
                        <tr key={r.resource_id} className="transition-colors hover:bg-accent/30">
                          <td className="whitespace-nowrap px-3 py-2 font-mono text-xs font-medium">{r.resource_id}</td>
                          <td className="whitespace-nowrap px-3 py-2 text-muted-foreground">{kind(r.kind)}</td>
                          <td className="whitespace-nowrap px-3 py-2">
                            <StatusBadge value={r.health} />
                          </td>
                          <td className="px-3 py-2 font-mono text-xs text-muted-foreground [overflow-wrap:anywhere]">
                            {signals.length === 0 ? "—" : signals.map((m) => `${m.name} ${Number.isInteger(m.value) ? m.value : m.value.toFixed(1)}${m.unit === "s" ? "s" : ""}`).join(" · ")}
                          </td>
                          <td className="whitespace-nowrap px-3 py-2 text-right font-mono text-xs tabular-nums">{latency ? `${latency.value.toFixed(0)} ms` : "—"}</td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              <EyeOff className="h-4 w-4" />
              {t("gaps.title")}
            </CardTitle>
            <CardDescription>{t("gaps.desc")}</CardDescription>
          </CardHeader>
          <CardContent>
            {snapshot && snapshot.coverage_gaps.length === 0 && <p className="text-sm text-muted-foreground">{t("gaps.none")}</p>}
            {snapshot && snapshot.coverage_gaps.length > 0 && (
              <ul className="divide-y rounded-lg border bg-card">
                {snapshot.coverage_gaps.map((gap, i) => (
                  <li key={i} className="p-3">
                    <div className="flex items-center justify-between gap-2">
                      <span className="truncate font-mono text-xs font-medium">{gap.resource_id}</span>
                      <span className="shrink-0 font-mono text-[11px] text-muted-foreground">{gap.probe_id}</span>
                    </div>
                    <p className="mt-0.5 text-xs text-muted-foreground">{gap.reason}</p>
                  </li>
                ))}
              </ul>
            )}
            {!snapshot && <p className="text-sm text-muted-foreground">—</p>}
          </CardContent>
        </Card>
      </div>
    </Page>
  );
}
