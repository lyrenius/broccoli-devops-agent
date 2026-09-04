import { useState } from "react";
import { api } from "../api";
import type { ActionRun, Issue, Job } from "../types";

export function Report({ onChanged }: { onChanged: () => void }) {
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [reporter, setReporter] = useState("operator");
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
    <div className="grid-2">
      <form className="report card" onSubmit={submit}>
        <h2>File a human report</h2>
        <p className="muted" style={{ margin: 0 }}>
          Human reports bypass anomaly detection and default to the top priority. The Operate Team diagnoses from a fresh
          Snapshot; any proposed actions go through the authority matrix.
        </p>
        <label>
          Title
          <input value={title} onChange={(e) => setTitle(e.target.value)} required placeholder="Contestants cannot submit" />
        </label>
        <label>
          What you observed
          <textarea rows={4} value={description} onChange={(e) => setDescription(e.target.value)} required />
        </label>
        <label>
          Reporter
          <input value={reporter} onChange={(e) => setReporter(e.target.value)} />
        </label>
        <label>
          Priority
          <select value={priority} onChange={(e) => setPriority(e.target.value)}>
            <option value="">top (default)</option>
            <option value="critical">critical</option>
            <option value="high">high</option>
            <option value="normal">normal</option>
            <option value="low">low</option>
          </select>
        </label>
        <div>
          <button className="primary" type="submit" disabled={busy}>
            {busy ? "Running the Operate Team…" : "File report"}
          </button>
        </div>
        {error && <p className="error">{error}</p>}
      </form>
      <section className="card result">
        <h2>Outcome</h2>
        {!result && <p className="muted">The diagnosis appears here. A model-backed run takes up to a minute.</p>}
        {result && (
          <>
            <p>
              <span className="pill">{result.issue.priority}</span> <span className="pill">{result.job.status}</span>{" "}
              {result.job.result && <span className="pill">{result.job.result.outcome}</span>}
            </p>
            <pre>{result.job.result?.summary ?? "no result"}</pre>
            {result.job.result && result.job.result.unresolved_questions.length > 0 && (
              <ul className="muted">
                {result.job.result.unresolved_questions.map((q, i) => (
                  <li key={i}>{q}</li>
                ))}
              </ul>
            )}
            {result.actions.length > 0 && (
              <>
                <h2 style={{ marginTop: 12 }}>Actions</h2>
                <ul className="mono" style={{ paddingLeft: 18 }}>
                  {result.actions.map((a) => (
                    <li key={a.action_run_id}>
                      {a.runbook_id} on {a.target_ids.join(",")} → {a.status} ({a.approval})
                    </li>
                  ))}
                </ul>
              </>
            )}
          </>
        )}
      </section>
    </div>
  );
}
