import { AlertTriangle, Radio, ScrollText } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, streamEvents } from "../api";
import { useT } from "../i18n";
import type { ActionRun, EventRecord } from "../types";
import { Page } from "./Shell";
import { EventLinks, eventTraceHref } from "./EventLinks";
import { Alert, Badge, Button, Card, CardContent, Input } from "./ui";

const PAGE_SIZE = 200;
function actorTone(actor: string): string {
  if (actor === "human") return "text-primary";
  if (actor === "agent-team" || actor === "scheduler-policy") return "text-purple-600 dark:text-purple-400";
  if (actor === "agents-platform") return "text-amber-600 dark:text-amber-400";
  return "text-muted-foreground";
}
function mergeEvents(left: EventRecord[], right: EventRecord[]) {
  return [...new Map([...left, ...right].map((event) => [event.sequence, event])).values()].sort((a, b) => a.sequence - b.sequence);
}
export function Events() {
  const [events, setEvents] = useState<EventRecord[]>([]);
  const [actions, setActions] = useState<ActionRun[]>([]);
  const [live, setLive] = useState(false);
  const [query, setQuery] = useState("");
  const [issueId, setIssueId] = useState("");
  const [filter, setFilter] = useState({ q: "", issue_id: "" });
  const [more, setMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [loadingEarlier, setLoadingEarlier] = useState(false);
  const [follow, setFollow] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const list = useRef<HTMLDivElement>(null);
  const scrollOlder = useRef(false);
  const generation = useRef(0);
  const { t, dateTime } = useT();

  useEffect(() => {
    let stop: (() => void) | null = null;
    let cancelled = false;
    generation.current += 1;
    setLoading(true); setLoadingEarlier(false); setError(null); setEvents([]); setMore(false); setLive(false); setFollow(true);
    void api.actions().then((items) => { if (!cancelled) setActions(items); }).catch(() => undefined);
    api.events(PAGE_SIZE, filter).then((initial) => {
      if (cancelled) return;
      setEvents(initial); setMore(initial.length === PAGE_SIZE);
      const after = initial.at(-1)?.sequence ?? 0;
      stop = streamEvents(after, (event) => {
        if (cancelled) return;
        const haystack = [event.kind, event.actor, event.summary, event.issue_id, event.job_id, event.action_run_id].join(" ").toLowerCase();
        if ((!filter.issue_id || event.issue_id === filter.issue_id) && (!filter.q || haystack.includes(filter.q.toLowerCase()))) setEvents((current) => mergeEvents(current, [event]));
        if (event.action_run_id && !event.job_id) void api.actions().then((items) => { if (!cancelled) setActions(items); }).catch(() => undefined);
      }, (connected) => { if (!cancelled) setLive(connected); });
    }).catch((e: Error) => { if (!cancelled) setError(e.message); }).finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; stop?.(); };
  }, [filter]);

  useEffect(() => {
    const node = list.current;
    if (!node) return;
    if (scrollOlder.current) { node.scrollTop = 0; scrollOlder.current = false; }
    else if (follow) node.scrollTop = node.scrollHeight;
  }, [events, follow]);

  const loadEarlier = async () => {
    const currentGeneration = generation.current;
    setLoadingEarlier(true); setError(null);
    try {
      const older = await api.events(PAGE_SIZE, { ...filter, before: events[0]?.sequence });
      if (currentGeneration !== generation.current) return;
      setFollow(false); scrollOlder.current = true;
      setEvents((current) => mergeEvents(older, current)); setMore(older.length === PAGE_SIZE);
    } catch (e) { if (currentGeneration === generation.current) setError((e as Error).message); }
    finally { if (currentGeneration === generation.current) setLoadingEarlier(false); }
  };

  return <Page icon={ScrollText} title={t("events.title")} subtitle={t("events.subtitle")} actions={<Badge variant={live ? "success" : "outline"}><Radio className="h-3 w-3" />{live ? t("events.live") : t("events.disconnected")}</Badge>}>
    <form className="flex flex-wrap items-center gap-2" onSubmit={(event) => { event.preventDefault(); setFilter({ q: query.trim(), issue_id: issueId.trim() }); }}>
      <Input type="search" className="min-w-48 flex-1" aria-label={t("events.search")} placeholder={t("events.search")} value={query} onChange={(event) => setQuery(event.target.value)} />
      <Input className="w-80 max-w-full" aria-label={t("events.issueFilter")} placeholder={t("events.issueFilter")} value={issueId} onChange={(event) => setIssueId(event.target.value)} />
      <Button size="sm" type="submit">{t("events.apply")}</Button>
      <Button size="sm" variant="outline" type="button" onClick={() => { setQuery(""); setIssueId(""); setFilter({ q: "", issue_id: "" }); }}>{t("events.clear")}</Button>
    </form>
    {error && <Alert icon={AlertTriangle}>{error}</Alert>}
    <div className="flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
      <span>{loading ? t("events.loading") : t("events.range", { n: events.length, first: events[0]?.sequence ?? "—", last: events.at(-1)?.sequence ?? "—" })}</span>
      <label className="flex items-center gap-1.5"><input type="checkbox" checked={follow} onChange={(event) => setFollow(event.target.checked)} />{t("events.follow")}</label>
      {more && <Button size="sm" variant="outline" disabled={loading || loadingEarlier} onClick={() => void loadEarlier()}>{t(loadingEarlier ? "events.loading" : "events.earlier")}</Button>}
    </div>
    <Card><CardContent className="p-0">
      <div ref={list} onScroll={(event) => { const node = event.currentTarget; setFollow(node.scrollHeight - node.scrollTop - node.clientHeight < 48); }} className="max-h-[calc(100vh-20rem)] overflow-auto font-mono text-xs">
        {!loading && events.length === 0 && !error && <p className="p-6 text-sm text-muted-foreground">{t("events.none")}</p>}
        {events.map((event) => <div key={event.sequence} className="grid grid-cols-[3.5rem_12rem_14rem_7rem_minmax(12rem,1fr)] gap-3 border-b border-dashed px-4 py-1.5 last:border-b-0 hover:bg-accent/30">
          <span className="text-right text-muted-foreground tabular-nums">{event.sequence}</span>
          <time className="tabular-nums" dateTime={event.occurred_at}>{dateTime(event.occurred_at)}</time>
          {eventTraceHref(event, actions) ? <a className="truncate text-primary hover:underline" title={event.kind} href={eventTraceHref(event, actions)}>{event.kind}</a> : <span className="truncate text-muted-foreground" title={event.kind}>{event.kind}</span>}
          <span className={`truncate ${actorTone(event.actor)}`}>{event.actor}</span>
          <div><span className="whitespace-pre-wrap break-words">{event.summary}</span><EventLinks event={event} actions={actions} /></div>
        </div>)}
      </div>
    </CardContent></Card>
  </Page>;
}
