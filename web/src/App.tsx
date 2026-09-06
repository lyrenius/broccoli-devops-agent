import { useCallback, useEffect, useState } from "react";
import { api } from "./api";
import type { Status } from "./types";
import { LocaleProvider } from "./i18n";
import { Shell, type Tab } from "./components/Shell";
import { Overview } from "./components/Overview";
import { Inbox } from "./components/Inbox";
import { Records } from "./components/Records";
import { Events } from "./components/Events";
import { Report } from "./components/Report";
import { Trace } from "./components/Trace";

const TABS: Tab[] = ["overview", "inbox", "records", "events", "report"];

/** Where the console is: a tab, or the trace of one Issue (optionally opened on one pass). */
type Route = { tab: Tab } | { tab: "records"; trace: { issueId: string; jobId?: string } };

function routeFromHash(): Route {
  const hash = window.location.hash.replace("#", "");
  const trace = /^trace\/([0-9a-f-]{36})(?:\/([0-9a-f-]{36}))?$/i.exec(hash);
  if (trace) return { tab: "records", trace: { issueId: trace[1], jobId: trace[2] } };
  return { tab: (TABS as string[]).includes(hash) ? (hash as Tab) : "overview" };
}

export default function App() {
  const [route, setRoute] = useState<Route>(routeFromHash);
  const tab = route.tab;
  const [status, setStatus] = useState<Status | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  const setTab = (next: Tab) => {
    window.location.hash = next;
    setRoute({ tab: next });
  };

  useEffect(() => {
    const onHash = () => setRoute(routeFromHash());
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
    <LocaleProvider agentLanguage={status?.language ?? null}>
      <Shell tab={tab} onTab={setTab} status={status} error={error} onChanged={refresh}>
        {tab === "overview" && <Overview tick={tick} status={status} onChanged={refresh} />}
        {tab === "inbox" && <Inbox tick={tick} status={status} onChanged={refresh} />}
        {tab === "records" && !("trace" in route) && <Records tick={tick} onChanged={refresh} />}
        {"trace" in route && <Trace key={route.trace.issueId} issueId={route.trace.issueId} jobId={route.trace.jobId} tick={tick} />}
        {tab === "events" && <Events />}
        {tab === "report" && <Report onChanged={refresh} />}
      </Shell>
    </LocaleProvider>
  );
}
