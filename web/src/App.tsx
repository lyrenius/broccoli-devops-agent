import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import type { Status } from "./types";
import { TopBar } from "./components/TopBar";
import { Overview } from "./components/Overview";
import { Inbox } from "./components/Inbox";
import { Records } from "./components/Records";
import { Events } from "./components/Events";
import { Report } from "./components/Report";

type Tab = "overview" | "inbox" | "records" | "events" | "report";

const TABS: { id: Tab; label: string }[] = [
  { id: "overview", label: "Overview" },
  { id: "inbox", label: "Approval inbox" },
  { id: "records", label: "Issues & jobs" },
  { id: "events", label: "Events" },
  { id: "report", label: "File a report" },
];

export default function App() {
  const [tab, setTab] = useState<Tab>("overview");
  const [status, setStatus] = useState<Status | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  const refresh = useCallback(async () => {
    try {
      setStatus(await api.status());
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
    setTick((t) => t + 1);
  }, []);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), 3000);
    return () => clearInterval(timer);
  }, [refresh]);

  return (
    <>
      <TopBar status={status} error={error} onChanged={refresh} />
      <nav className="tabs">
        {TABS.map((t) => (
          <button key={t.id} className={tab === t.id ? "active" : ""} onClick={() => setTab(t.id)}>
            {t.label}
            {t.id === "inbox" && status && status.counts.actions_waiting > 0 && (
              <span className="badge">{status.counts.actions_waiting}</span>
            )}
          </button>
        ))}
      </nav>
      <main>
        {tab === "overview" && <Overview tick={tick} onChanged={refresh} />}
        {tab === "inbox" && <Inbox tick={tick} dryRun={status?.dry_run ?? true} onChanged={refresh} />}
        {tab === "records" && <Records tick={tick} />}
        {tab === "events" && <Events />}
        {tab === "report" && <Report onChanged={refresh} />}
      </main>
    </>
  );
}
