import { Activity, AlertTriangle, Camera, EyeOff, Inbox as InboxIcon, LayoutDashboard, ListChecks, Server } from "lucide-react";
import { useEffect, useState } from "react";
import { api } from "../api";
import { ageOf } from "../lib/prefs";
import type { Issue, Snapshot, Status } from "../types";
import { Page } from "./Shell";
import { StatusBadge } from "./status";
import { Alert, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, EmptyState, StatTile } from "./ui";

const LIVE = new Set(["open", "investigating", "waiting_for_human", "mitigating", "verifying"]);

export function Overview({ tick, status, onChanged }: { tick: number; status: Status | null; onChanged: () => void }) {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [issues, setIssues] = useState<Issue[]>([]);
  const [missing, setMissing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
      title="Overview"
      subtitle={
        snapshot
          ? `Snapshot captured ${ageOf(snapshot.created_at)} · ${snapshot.cause.replace(/_/g, " ")} · topology ${snapshot.topology_revision}`
          : "The latest Snapshot of the deployment, with its coverage gaps."
      }
      actions={
        <Button onClick={capture} disabled={busy}>
          <Camera />
          {busy ? "Capturing…" : "Capture now"}
        </Button>
      }
    >
      {error && (
        <Alert icon={AlertTriangle}>
          <p className="font-medium">Capture failed</p>
          <p className="mt-0.5 text-muted-foreground">{error}</p>
        </Alert>
      )}
      {frozenAfterRecovery && status && (
        <Alert tone="warning" icon={AlertTriangle}>
          <p className="font-medium">Recovered from a restart; the Scheduler is {status.mode.replace(/_/g, " ")}.</p>
          <p className="mt-0.5">
            {status.recovery!.interrupted_job_ids.length + status.recovery!.interrupted_action_ids.length} interrupted item(s) were put in the Inbox
            {status.recovery!.previous_mode !== "running" && <> · the previous process was {status.recovery!.previous_mode.replace(/_/g, " ")}</>}. Check the Inbox, then press Resume in the sidebar.
          </p>
        </Alert>
      )}
      {stale && (
        <Alert tone="info" icon={AlertTriangle}>
          This Snapshot is old; the probes may have changed since. Capture now for the current picture.
        </Alert>
      )}

      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatTile label="Healthy resources" value={snapshot ? `${healthy} / ${total}` : "—"} icon={Server} tone={snapshot && healthy < total ? "warn" : "default"} />
        <StatTile label="Inbox" value={status?.inbox.total ?? "—"} icon={InboxIcon} tone={status && status.inbox.total > 0 ? "alert" : "default"} hint={status ? `${status.inbox.permission_requests} requests · ${status.inbox.permission_denied} denied · ${status.inbox.failed_jobs + status.inbox.failed_actions} failed` : undefined} />
        <StatTile label="Open issues" value={liveIssues} icon={ListChecks} hint={`${issues.length} total`} />
        <StatTile label="Events" value={status?.counts.events ?? "—"} icon={Activity} hint={status ? `${status.counts.jobs} jobs · ${status.counts.actions} actions` : undefined} />
      </div>

      <div className="grid gap-6 lg:grid-cols-3">
        <Card className="lg:col-span-2">
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              <Server className="h-4 w-4" />
              Resources
            </CardTitle>
            <CardDescription>Every resource in the topology as the Collector last observed it.</CardDescription>
          </CardHeader>
          <CardContent>
            {missing && <EmptyState icon={Camera} title="No Snapshot yet" hint="Capture one to see the deployment." />}
            {snapshot && (
              <div className="overflow-x-auto rounded-lg border">
                <table className="w-full text-sm">
                  <thead>
                    <tr className="border-b bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
                      <th className="px-3 py-2 font-medium">Resource</th>
                      <th className="px-3 py-2 font-medium">Kind</th>
                      <th className="px-3 py-2 font-medium">Health</th>
                      <th className="px-3 py-2 font-medium">Signals</th>
                      <th className="px-3 py-2 text-right font-medium">Latency</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y">
                    {snapshot.resources.map((r) => {
                      const latency = r.metrics.find((m) => m.name.endsWith(".latency"));
                      const signals = r.metrics.filter((m) => !m.name.startsWith("probe."));
                      return (
                        <tr key={r.resource_id} className="transition-colors hover:bg-accent/30">
                          <td className="px-3 py-2 font-mono text-xs font-medium">{r.resource_id}</td>
                          <td className="px-3 py-2 text-muted-foreground">{r.kind.replace(/_/g, " ")}</td>
                          <td className="px-3 py-2">
                            <StatusBadge value={r.health} />
                          </td>
                          <td className="px-3 py-2 font-mono text-xs text-muted-foreground">
                            {signals.length === 0 ? "—" : signals.map((m) => `${m.name} ${Number.isInteger(m.value) ? m.value : m.value.toFixed(1)}${m.unit === "s" ? "s" : ""}`).join(" · ")}
                          </td>
                          <td className="px-3 py-2 text-right font-mono text-xs tabular-nums">{latency ? `${latency.value.toFixed(0)} ms` : "—"}</td>
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
              Coverage gaps
            </CardTitle>
            <CardDescription>What the Collector could not observe. A gap is a fact, never assumed healthy.</CardDescription>
          </CardHeader>
          <CardContent>
            {snapshot && snapshot.coverage_gaps.length === 0 && <p className="text-sm text-muted-foreground">None — every resource was observed.</p>}
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
