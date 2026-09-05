import { useState } from "react";
import { api } from "../api";
import type { Status } from "../types";

export function TopBar({
  status,
  error,
  onChanged,
}: {
  status: Status | null;
  error: string | null;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const transition = async (name: "freeze-dispatch" | "freeze-all" | "resume") => {
    setBusy(true);
    try {
      await api.transition(name);
      onChanged();
    } finally {
      setBusy(false);
    }
  };
  const mode = status?.mode ?? "unknown";
  return (
    <header className="topbar">
      <div className="brand">
        <span>●</span> Broccoli Ops Console
      </div>
      <span className={`pill mode-${mode}`}>scheduler {mode}</span>
      {status && (
        <span className={`pill ${status.dry_run ? "dry" : "live"}`}>
          {status.dry_run ? "platform dry-run" : "platform LIVE"}
        </span>
      )}
      {status && <span className="pill">{status.team_backend}</span>}
      {status && (
        <span className="pill">
          {status.deployment.name} · {status.deployment.operation_mode}
        </span>
      )}
      {status?.recovery && mode !== "running" && (
        <span className="pill mode-dispatch_frozen" title="Startup recovery reconciled interrupted work; check the inbox, then resume">
          recovered: {status.recovery.interrupted_job_ids.length + status.recovery.interrupted_action_ids.length} interrupted · was {status.recovery.previous_mode}
        </span>
      )}
      {error && <span className="error">API unreachable: {error}</span>}
      <div className="spacer" />
      <div className="controls">
        <button disabled={busy || mode === "dispatch_frozen"} onClick={() => transition("freeze-dispatch")}>
          Freeze dispatch
        </button>
        <button className="danger" disabled={busy || mode === "fully_frozen"} onClick={() => transition("freeze-all")}>
          Freeze all
        </button>
        <button className="primary" disabled={busy || mode === "running"} onClick={() => transition("resume")}>
          Resume
        </button>
      </div>
    </header>
  );
}
