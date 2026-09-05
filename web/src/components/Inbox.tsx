import { useEffect, useState } from "react";
import { api } from "../api";
import type { ActionRun, Inbox as InboxData, Job, Revision } from "../types";

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

/** Mirrors the runner's inbox membership rule, so History shows exactly what the inbox does not. */
function inInbox(a: ActionRun): boolean {
  if (a.status === "waiting_for_approval") return true;
  if (a.review !== null) return false;
  return a.denial !== null || a.status === "failed" || a.status === "verification_failed";
}

const EMPTY: InboxData = { permission_requests: [], permission_denied: [], failed_jobs: [], failed_actions: [] };

function loadOperator(): string {
  try {
    return localStorage.getItem("broccoli.operator") ?? "operator";
  } catch {
    return "operator";
  }
}

export function Inbox({ tick, dryRun, onChanged }: { tick: number; dryRun: boolean; onChanged: () => void }) {
  const [inbox, setInbox] = useState<InboxData>(EMPTY);
  const [history, setHistory] = useState<ActionRun[]>([]);
  const [operator, setOperator] = useState(loadOperator);
  const [comments, setComments] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revision, setRevision] = useState<Revision | null>(null);

  useEffect(() => {
    api.inbox().then(setInbox).catch((e) => setError((e as Error).message));
    api
      .actions()
      .then((all) => setHistory(all.filter((a) => !inInbox(a)).reverse()))
      .catch(() => undefined);
  }, [tick]);

  useEffect(() => {
    try {
      localStorage.setItem("broccoli.operator", operator);
    } catch {
      // storage unavailable; the name still applies for this session
    }
  }, [operator]);

  const comment = (id: string) => comments[id] ?? "";
  const setComment = (id: string, value: string) => setComments((c) => ({ ...c, [id]: value }));
  const by = operator.trim() || "operator";

  const run = async (id: string, work: () => Promise<Revision | null | undefined>) => {
    setBusy(id);
    setError(null);
    try {
      const result = await work();
      if (result) setRevision(result);
      setComment(id, "");
      onChanged();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(null);
    }
  };

  const approve = (a: ActionRun) => run(a.action_run_id, async () => void (await api.approve(a.action_run_id, by)));
  const reject = (a: ActionRun) => run(a.action_run_id, async () => void (await api.reject(a.action_run_id, by, comment(a.action_run_id))));
  const reviewAction = (a: ActionRun, decision: "acknowledge" | "send_upstream") =>
    run(a.action_run_id, async () => (await api.reviewAction(a.action_run_id, by, decision, comment(a.action_run_id))).revision);
  const reviewJob = (j: Job, decision: "acknowledge" | "send_upstream") =>
    run(j.job_id, async () => (await api.reviewJob(j.job_id, by, decision, comment(j.job_id))).revision);

  const reviewButtons = (id: string, send: () => void, ack: () => void) => (
    <div className="actions">
      <button className="primary" disabled={busy === id} onClick={send} title="A revising Job runs now with the reason and your comment as input">
        Send back upstream
      </button>
      <button disabled={busy === id} onClick={ack}>
        Acknowledge
      </button>
    </div>
  );

  const feedbackBox = (id: string, placeholder: string) => (
    <label className="feedback">
      <span className="muted">Comment for the next pass</span>
      <textarea rows={2} value={comment(id)} placeholder={placeholder} onChange={(e) => setComment(id, e.target.value)} />
    </label>
  );

  return (
    <>
      <div className="inbox-toolbar">
        <label>
          <span className="muted">Deciding as</span>
          <input value={operator} onChange={(e) => setOperator(e.target.value)} placeholder="your name" />
        </label>
        {dryRun && <div className="notice">Platform is in dry-run: approved actions are rendered and recorded, not executed.</div>}
      </div>
      {error && <p className="error">{error}</p>}
      {revision && (
        <div className="notice revision">
          <strong>Revision ran.</strong> Job {revision.job.job_id.slice(0, 8)}… is {revision.job.status}
          {revision.job.result && <> — {revision.job.result.summary}</>}
          {revision.actions.length > 0 && (
            <ul className="mono" style={{ margin: "6px 0 0", paddingLeft: 18 }}>
              {revision.actions.map((a) => (
                <li key={a.action_run_id}>
                  {a.runbook_id} on {a.target_ids.join(",")} → {a.status} ({a.approval})
                </li>
              ))}
            </ul>
          )}
          <button style={{ marginLeft: 12 }} onClick={() => setRevision(null)}>
            Dismiss
          </button>
        </div>
      )}

      <h2>
        Permission requests <span className="count">{inbox.permission_requests.length}</span>
      </h2>
      {inbox.permission_requests.length === 0 && <p className="muted">Nothing waits for approval.</p>}
      {inbox.permission_requests.map((a) => (
        <article key={a.action_run_id} className="card inbox-item request">
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
          <label className="feedback">
            <span className="muted">Comment (recorded with a rejection; the agent sees it if the denial is sent back)</span>
            <textarea rows={2} value={comment(a.action_run_id)} onChange={(e) => setComment(a.action_run_id, e.target.value)} />
          </label>
          <div className="actions">
            <button className="primary" disabled={busy === a.action_run_id} onClick={() => approve(a)}>
              Approve and run
            </button>
            <button className="danger" disabled={busy === a.action_run_id} onClick={() => reject(a)}>
              Reject
            </button>
          </div>
        </article>
      ))}

      <h2 style={{ marginTop: 24 }}>
        Permission denied <span className="count">{inbox.permission_denied.length}</span>
      </h2>
      {inbox.permission_denied.length === 0 && <p className="muted">No denial awaits review.</p>}
      {inbox.permission_denied.map((a) => (
        <article key={a.action_run_id} className="card inbox-item denied">
          <div className="card-head">
            <span className="runbook">{a.runbook_id}</span>
            <span className="targets">on {a.target_ids.join(", ")}</span>
            <span className={`tag ${a.denial?.source === "human" ? "tag-human" : "tag-rule"}`}>
              denied by {a.denial?.source === "human" ? a.denial.decided_by ?? "a human" : "rule"}
            </span>
            <span className="meta">{a.denial && new Date(a.denial.decided_at).toLocaleTimeString()}</span>
          </div>
          <dl className="kv">
            <dt>Proposed because</dt>
            <dd>{a.reason || "—"}</dd>
            <dt>Denial reason</dt>
            <dd>{a.denial?.reason ?? "—"}</dd>
            {a.denial?.comment && (
              <>
                <dt>Comment</dt>
                <dd>{a.denial.comment}</dd>
              </>
            )}
          </dl>
          {feedbackBox(a.action_run_id, "What should the next pass do differently?")}
          {reviewButtons(a.action_run_id, () => reviewAction(a, "send_upstream"), () => reviewAction(a, "acknowledge"))}
        </article>
      ))}

      <h2 style={{ marginTop: 24 }}>
        Failed <span className="count">{inbox.failed_jobs.length + inbox.failed_actions.length}</span>
      </h2>
      {inbox.failed_jobs.length + inbox.failed_actions.length === 0 && <p className="muted">No failure awaits review.</p>}
      {inbox.failed_jobs.map((j) => (
        <article key={j.job_id} className="card inbox-item failed">
          <div className="card-head">
            <span className="runbook">job {j.job_id.slice(0, 8)}…</span>
            <span className="tag tag-rule">job failed</span>
            <span className="meta">{new Date(j.created_at).toLocaleTimeString()}</span>
          </div>
          <p style={{ margin: "4px 0" }}>{j.result?.summary ?? "no result was recorded"}</p>
          {j.result?.artifact_ids.map((id) => (
            <a key={id} className="mono" href={`/api/artifacts/${id}/body`} target="_blank" rel="noreferrer" style={{ marginRight: 10 }}>
              transcript {id.slice(0, 8)}…
            </a>
          ))}
          {feedbackBox(j.job_id, "Anything the next pass should know?")}
          {reviewButtons(j.job_id, () => reviewJob(j, "send_upstream"), () => reviewJob(j, "acknowledge"))}
        </article>
      ))}
      {inbox.failed_actions.map((a) => (
        <article key={a.action_run_id} className="card inbox-item failed">
          <div className="card-head">
            <span className="runbook">{a.runbook_id}</span>
            <span className="targets">on {a.target_ids.join(", ")}</span>
            <span className={`status ${tone(a.status)}`}>{a.status}</span>
          </div>
          <dl className="kv">
            <dt>Proposed because</dt>
            <dd>{a.reason || "—"}</dd>
            <dt>Execution</dt>
            <dd>{a.execution_summary ?? "—"}</dd>
            <dt>Verification</dt>
            <dd>{a.verification_summary ?? "not reached"}</dd>
          </dl>
          {feedbackBox(a.action_run_id, "What should the next pass do differently?")}
          {reviewButtons(a.action_run_id, () => reviewAction(a, "send_upstream"), () => reviewAction(a, "acknowledge"))}
        </article>
      ))}

      <h2 style={{ marginTop: 24 }}>History</h2>
      {history.length === 0 && <p className="muted">No decided actions yet.</p>}
      {history.length > 0 && (
        <table>
          <thead>
            <tr>
              <th>Runbook</th>
              <th>Targets</th>
              <th>Status</th>
              <th>Approval</th>
              <th>Denial / verification</th>
              <th>Review</th>
            </tr>
          </thead>
          <tbody>
            {history.map((a) => (
              <tr key={a.action_run_id}>
                <td className="mono">{a.runbook_id}</td>
                <td className="mono muted">{a.target_ids.join(", ")}</td>
                <td>
                  <span className={`status ${tone(a.status)}`}>{a.status}</span>
                </td>
                <td className="muted">
                  {a.approval}
                  {a.approved_by && <> by {a.approved_by}</>}
                </td>
                <td className="muted">
                  {a.denial
                    ? `${a.denial.reason}${a.denial.comment ? ` — ${a.denial.comment}` : ""}`
                    : a.verification_summary ?? a.execution_summary ?? "—"}
                  {a.verification_evidence && <span className={`tag evidence-${a.verification_evidence}`}>{a.verification_evidence} evidence</span>}
                </td>
                <td className="muted">
                  {a.review
                    ? `${a.review.reviewer}: ${a.review.decision.decision === "sent_upstream" ? `sent upstream (job ${a.review.decision.job_id.slice(0, 8)}…)` : "acknowledged"}`
                    : "—"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  );
}
