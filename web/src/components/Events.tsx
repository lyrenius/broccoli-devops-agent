import { useEffect, useRef, useState } from "react";
import { api, streamEvents } from "../api";
import type { EventRecord } from "../types";

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
    // Scroll only the log container, so the top bar and tabs stay in place.
    const node = list.current;
    if (node) node.scrollTop = node.scrollHeight;
  }, [events.length]);

  return (
    <>
      <div className="card-head">
        <h2>Event log</h2>
        <span className={`pill ${live ? "mode-running" : ""}`}>{live ? "live" : "not connected"}</span>
      </div>
      <div className="events" ref={list}>
        {events.map((e) => (
          <div key={e.sequence} className="row">
            <span className="seq">{e.sequence}</span>
            <span>{e.occurred_at.slice(11, 19)}</span>
            <span className="kind">{e.kind}</span>
            <span className="actor">{e.actor}</span>
            <span className="summary">{e.summary}</span>
          </div>
        ))}
      </div>
    </>
  );
}
