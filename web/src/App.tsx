import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import type { Status } from "./types";
import { Shell, type Tab } from "./components/Shell";
import { Overview } from "./components/Overview";
import { Inbox } from "./components/Inbox";
import { Records } from "./components/Records";
import { Events } from "./components/Events";
import { Report } from "./components/Report";

const TABS: Tab[] = ["overview", "inbox", "records", "events", "report"];

function tabFromHash(): Tab {
  const hash = window.location.hash.replace("#", "");
  return (TABS as string[]).includes(hash) ? (hash as Tab) : "overview";
}

export default function App() {
  const [tab, setTabState] = useState<Tab>(tabFromHash);
  const [status, setStatus] = useState<Status | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  const setTab = (next: Tab) => {
    window.location.hash = next;
    setTabState(next);
  };

  useEffect(() => {
    const onHash = () => setTabState(tabFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

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
    <Shell tab={tab} onTab={setTab} status={status} error={error} onChanged={refresh}>
      {tab === "overview" && <Overview tick={tick} status={status} onChanged={refresh} />}
      {tab === "inbox" && <Inbox tick={tick} status={status} onChanged={refresh} />}
      {tab === "records" && <Records tick={tick} onChanged={refresh} />}
      {tab === "events" && <Events />}
      {tab === "report" && <Report onChanged={refresh} />}
    </Shell>
  );
}
