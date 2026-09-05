import { Radio, ScrollText } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, streamEvents } from "../api";
import type { EventRecord } from "../types";
import { Page } from "./Shell";
import { Badge, Card, CardContent } from "./ui";

function actorTone(actor: string): string {
  if (actor === "human") return "text-primary";
  if (actor === "agent-team" || actor === "scheduler-policy") return "text-purple-600 dark:text-purple-400";
  if (actor === "agents-platform") return "text-amber-600 dark:text-amber-400";
  return "text-muted-foreground";
}

export function Events() {
  const [events, setEvents] = useState<EventRecord[]>([]);
  const [live, setLive] = useState(false);
  const list = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let stop: (() => void) | null = null;
    let cancelled = false;
    api
      .events(200)
      .then((initial) => {
        if (cancelled) return;
        setEvents(initial);
        const after = initial.length ? initial[initial.length - 1].sequence : 0;
        stop = streamEvents(after, (event) => {
          setEvents((list) => [...list.slice(-499), event]);
        });
        setLive(true);
      })
      .catch(() => setLive(false));
    return () => {
      cancelled = true;
      stop?.();
    };
  }, []);

  useEffect(() => {
    const node = list.current;
    if (node) node.scrollTop = node.scrollHeight;
  }, [events.length]);

  return (
    <Page
      icon={ScrollText}
      title="Events"
      subtitle="The append-only log every component writes to. Model output is recorded as data, never as authority."
      actions={
        <Badge variant={live ? "success" : "outline"}>
          <Radio className="h-3 w-3" />
          {live ? "live" : "not connected"}
        </Badge>
      }
    >
      <Card>
        <CardContent className="p-0">
          <div ref={list} className="max-h-[calc(100vh-14rem)] overflow-y-auto font-mono text-xs">
            {events.length === 0 && <p className="p-6 text-sm text-muted-foreground">No events yet.</p>}
            {events.map((e) => (
              <div key={e.sequence} className="grid grid-cols-[3.5rem_5rem_18rem_8rem_1fr] gap-3 border-b border-dashed px-4 py-1.5 last:border-b-0 hover:bg-accent/30">
                <span className="text-right text-muted-foreground tabular-nums">{e.sequence}</span>
                <span className="tabular-nums">{e.occurred_at.slice(11, 19)}</span>
                <span className="truncate text-primary" title={e.kind}>
                  {e.kind}
                </span>
                <span className={`truncate ${actorTone(e.actor)}`}>{e.actor}</span>
                <span className="whitespace-pre-wrap break-words">{e.summary}</span>
              </div>
            ))}
          </div>
        </CardContent>
      </Card>
    </Page>
  );
}
