import { useEffect, useState } from "react";
import { api } from "../api";
import type { Snapshot } from "../types";

function tone(health: string): string {
  switch (health) {
    case "healthy":
      return "tone-good";
    case "degraded":
      return "tone-warn";
    case "down":
      return "tone-bad";
    default:
      return "tone-unknown";
  }
}

export function Overview({ tick, onChanged }: { tick: number; onChanged: () => void }) {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
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

  const age = (iso: string): string => {
    const seconds = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000));
    if (seconds < 90) return `${seconds}s ago`;
    if (seconds < 5400) return `${Math.round(seconds / 60)} min ago`;
    return `${Math.round(seconds / 3600)} h ago`;
  };

  return (
    <div className="grid-2">
      <section className="card">
        <div className="card-head">
          <h2>Latest Snapshot</h2>
          {snapshot && (
            <span className="meta" title={new Date(snapshot.created_at).toLocaleString()}>
              captured {age(snapshot.created_at)} · {snapshot.cause} · rev {snapshot.topology_revision}
            </span>
          )}
          <div className="spacer" style={{ flex: 1 }} />
          <button className="primary" disabled={busy} onClick={capture}>
            {busy ? "Capturing…" : "Capture now"}
          </button>
        </div>
        {error && <p className="error">capture failed: {error}</p>}
        {missing && <p className="muted">No Snapshot yet. Capture one to see the deployment.</p>}
        {snapshot && Date.now() - new Date(snapshot.created_at).getTime() > 10 * 60 * 1000 && (
          <p className="muted">This Snapshot is old; the probes may have changed since. Capture now for the current picture.</p>
        )}
        {snapshot && (
          <table>
            <thead>
              <tr>
                <th>Resource</th>
                <th>Kind</th>
                <th>Health</th>
                <th>Signals</th>
                <th className="num">Latency</th>
              </tr>
            </thead>
            <tbody>
              {snapshot.resources.map((r) => {
                const latency = r.metrics.find((m) => m.name.endsWith(".latency"));
                const signals = r.metrics.filter((m) => !m.name.startsWith("probe."));
                return (
                  <tr key={r.resource_id}>
                    <td className="mono">{r.resource_id}</td>
                    <td className="muted">{r.kind}</td>
                    <td>
                      <span className={`health ${tone(r.health)}`}>{r.health}</span>
                    </td>
                    <td className="muted mono signals">
                      {signals.length === 0
                        ? "—"
                        : signals.map((m) => `${m.name} ${Number.isInteger(m.value) ? m.value : m.value.toFixed(1)}${m.unit === "s" ? "s" : ""}`).join(" · ")}
                    </td>
                    <td className="num">{latency ? `${latency.value.toFixed(0)} ms` : "—"}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </section>
      <section className="card">
        <h2>Coverage gaps</h2>
        {snapshot && snapshot.coverage_gaps.length === 0 && <p className="muted">None — every resource was observed.</p>}
        {snapshot &&
          snapshot.coverage_gaps.map((gap, i) => (
            <div key={i} style={{ padding: "6px 0", borderBottom: "1px solid var(--border)" }}>
              <span className="mono">{gap.resource_id}</span> <span className="muted">· probe {gap.probe_id}</span>
              <div className="muted">{gap.reason}</div>
            </div>
          ))}
        {!snapshot && <p className="muted">—</p>}
      </section>
    </div>
  );
}
