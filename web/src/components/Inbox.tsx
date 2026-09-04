import { useEffect, useState } from "react";
import { api } from "../api";
import type { ActionRun } from "../types";

function tone(status: string): string {
  switch (status) {
    case "succeeded":
      return "tone-good";
    case "waiting_for_approval":
    case "verifying":
    case "running":
      return "tone-warn";
    case "failed":
    case "verification_failed":
    case "cancelled":
      return "tone-bad";
    default:
      return "tone-unknown";
  }
}

export function Inbox({ tick, dryRun, onChanged }: { tick: number; dryRun: boolean; onChanged: () => void }) {
  const [actions, setActions] = useState<ActionRun[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api.actions().then(setActions).catch((e) => setError((e as Error).message));
  }, [tick]);

  const decide = async (id: string, approve: boolean) => {
    setBusy(id);
    setError(null);
    try {
      const updated = approve ? await api.approve(id) : await api.reject(id);
      setActions((list) => list.map((a) => (a.action_run_id === id ? updated : a)));
      onChanged();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(null);
    }
  };

  const waiting = actions.filter((a) => a.status === "waiting_for_approval");
  const others = actions.filter((a) => a.status !== "waiting_for_approval").reverse();

  return (
    <>
      {dryRun && <div className="notice">Platform is in dry-run: approved actions are rendered and recorded, not executed.</div>}
      {error && <p className="error">{error}</p>}
      <h2>Waiting for a human ({waiting.length})</h2>
      {waiting.length === 0 && <p className="muted">Nothing to decide.</p>}
      {waiting.map((a) => (
        <article key={a.action_run_id} className="card inbox-item">
          <div className="card-head">
            <span className="runbook">{a.runbook_id}</span>
            <span className="targets">on {a.target_ids.join(", ")}</span>
            <span className="meta">{new Date(a.created_at).toLocaleTimeString()}</span>
          </div>
          <dl className="kv">
            <dt>Why</dt>
            <dd>{a.reason || "—"}</dd>
            <dt>Expected effect</dt>
            <dd>{a.expected_effect || "—"}</dd>
            <dt>Verification</dt>
            <dd className="muted">every target must be Healthy in the after-Snapshot</dd>
          </dl>
          <div className="actions">
            <button className="primary" disabled={busy === a.action_run_id} onClick={() => decide(a.action_run_id, true)}>
              Approve and run
            </button>
            <button className="danger" disabled={busy === a.action_run_id} onClick={() => decide(a.action_run_id, false)}>
              Reject
            </button>
          </div>
        </article>
      ))}

      <h2 style={{ marginTop: 24 }}>History</h2>
      {others.length === 0 && <p className="muted">No decided actions yet.</p>}
      {others.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>Runbook</th>
              <th>Targets</th>
              <th>Status</th>
              <th>Approval</th>
              <th>Verification</th>
            </tr>
          </thead>
          <tbody>
            {others.map((a) => (
              <tr key={a.action_run_id}>
                <td className="mono">{a.runbook_id}</td>
                <td className="mono muted">{a.target_ids.join(", ")}</td>
                <td>
                  <span className={`status ${tone(a.status)}`}>{a.status}</span>
                </td>
                <td className="muted">{a.approval}</td>
                <td className="muted">{a.verification_summary ?? "—"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  );
}
