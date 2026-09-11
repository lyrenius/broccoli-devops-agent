import type { EventRecord, TraceStep } from "../types";

/** Reconnection may replay events already delivered or included in a refreshed session. */
export function mergeEvents(left: EventRecord[], right: EventRecord[]): EventRecord[] {
  return [...new Map([...left, ...right].map((event) => [event.sequence, event])).values()]
    .sort((a, b) => a.sequence - b.sequence);
}

export function mergeSteps(left: TraceStep[], right: TraceStep[]): TraceStep[] {
  return [...new Map([...left, ...right].map((step) => [step.index, step])).values()]
    .sort((a, b) => a.index - b.index);
}
