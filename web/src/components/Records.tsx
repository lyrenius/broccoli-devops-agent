import { useEffect, useState } from "react";
import { api } from "../api";
import type { Issue, Job } from "../types";

const LIVE = new Set(["open", "investigating", "waiting_for_human", "mitigating", "verifying"]);

function loadOperator(): string {
  try {
    return localStorage.getItem("broccoli.operator") ?? "operator";
  } catch {
    return "operator";
  }
}

export function Records({ tick, onChanged }: { tick: number; onChanged: () => void }) {
  const [issues, setIssues] = useState<Issue[]>([]);
  const [jobs, setJobs] = useState<Job[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [comments, setComments] = useState<Record<string, string>>({});

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

  return (
    <>
      <h2>Issues ({issues.length})</h2>
      {error && <p className="error">{error}</p>}
      {issues.length === 0 && <p className="muted">No issues filed yet.</p>}
      {issues.map((issue) => {
        const related = jobs.filter((j) => j.issue_id === issue.issue_id);
        return (
          <article key={issue.issue_id} className="card">
            <div className="card-head">
              <strong>{issue.title}</strong>
              <span className="pill">{issue.priority}</span>
              <span className="pill">{issue.status}</span>
              <span className="meta">{new Date(issue.created_at).toLocaleString()}</span>
              {LIVE.has(issue.status) && (
                <span className="issue-actions">
                  <input
                    placeholder="closing comment"
                    value={comments[issue.issue_id] ?? ""}
                    onChange={(e) => setComments((c) => ({ ...c, [issue.issue_id]: e.target.value }))}
                  />
                  <button className="primary" disabled={busy === issue.issue_id} onClick={() => close(issue, "resolved")} title="The problem is fixed or was not a problem">
                    Resolve
                  </button>
                  <button disabled={busy === issue.issue_id} onClick={() => close(issue, "cancelled")} title="Stop working on it without claiming it is fixed">
                    Cancel
                  </button>
                </span>
              )}
            </div>
            <p className="muted" style={{ margin: "0 0 8px" }}>
              {issue.description}
            </p>
            {related.map((job) => (
              <div key={job.job_id} style={{ borderTop: "1px solid var(--border)", paddingTop: 8 }}>
                <div>
                  <span className="mono muted">job {job.job_id.slice(0, 8)}…</span> · {job.team_kind} ·{" "}
                  <span className="pill">{job.status}</span>
                  {job.result && <span className="pill">{job.result.outcome}</span>}
                  {job.revises_job_id && <span className="pill">revises {job.revises_job_id.slice(0, 8)}…</span>}
                  {job.review && (
                    <span className="pill">
                      reviewed by {job.review.reviewer}: {job.review.decision.decision === "sent_upstream" ? "sent upstream" : "acknowledged"}
                    </span>
                  )}
                </div>
                {job.feedback.length > 0 && (
                  <ul className="feedback-list">
                    {job.feedback.map((f) => (
                      <li key={f.feedback_id}>
                        <span className="tag tag-human">feedback from {f.reviewer}</span>{" "}
                        {f.origin.kind === "denied_action" && (
                          <>
                            <span className="mono">{f.origin.runbook_id}</span> was denied: {f.origin.denial.reason}
                            {f.origin.denial.comment && <> — {f.origin.denial.comment}</>}
                          </>
                        )}
                        {f.origin.kind === "failed_action" && (
                          <>
                            <span className="mono">{f.origin.runbook_id}</span> failed: {f.origin.summary}
                            {f.origin.evidence && <pre className="evidence">{f.origin.evidence}</pre>}
                          </>
                        )}
                        {f.origin.kind === "failed_job" && <>the previous job failed: {f.origin.summary}</>}
                        {f.comment && <> · "{f.comment}"</>}
                      </li>
                    ))}
                  </ul>
                )}
                {job.result && (
                  <>
                    <p style={{ margin: "6px 0" }}>{job.result.summary}</p>
                    {job.result.unresolved_questions.length > 0 && (
                      <ul className="muted" style={{ margin: "4px 0", paddingLeft: 18 }}>
                        {job.result.unresolved_questions.map((q, i) => (
                          <li key={i}>{q}</li>
                        ))}
                      </ul>
                    )}
                    {job.result.proposed_actions.length > 0 && (
                      <div className="muted">
                        proposed:{" "}
                        {job.result.proposed_actions.map((p, i) => (
                          <span key={i} className="mono">
                            {p.runbook_id} on {p.target_ids.join(",")}{i < job.result!.proposed_actions.length - 1 ? "; " : ""}
                          </span>
                        ))}
                      </div>
                    )}
                    {job.result.artifact_ids.map((id) => (
                      <a key={id} className="mono" href={`/api/artifacts/${id}/body`} target="_blank" rel="noreferrer" style={{ marginRight: 10 }}>
                        transcript {id.slice(0, 8)}…
                      </a>
                    ))}
                  </>
                )}
              </div>
            ))}
          </article>
        );
      })}
    </>
  );
}
