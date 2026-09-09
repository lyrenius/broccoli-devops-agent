import { useT } from "../i18n";
import { traceHash } from "../lib/routes";
import type { ActionRun, EventRecord } from "../types";

/** Resolve action-only events to the Job that actually proposed the operation. */
export function eventTraceHref(event: EventRecord, actions: ActionRun[] = []): string | undefined {
  const action = actions.find((item) => item.action_run_id === event.action_run_id);
  const issueId = event.issue_id ?? action?.issue_id;
  return issueId ? traceHash(issueId, event.job_id ?? action?.originating_job_id) : undefined;
}

export function EventLinks({ event, actions = [] }: { event: EventRecord; actions?: ActionRun[] }) {
  const { t } = useT();
  const href = eventTraceHref(event, actions);
  return <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-xs">
    {event.issue_id && <a className="text-primary hover:underline" href={traceHash(event.issue_id)}>{t("review.issue", { id: event.issue_id.slice(-8) })}</a>}
    {event.job_id && href && <a className="text-primary hover:underline" href={href}>{t("records.job", { id: `…${event.job_id.slice(-8)}` })}</a>}
    {event.action_run_id && href && <a className="text-primary hover:underline" href={href}>{t("events.action", { id: event.action_run_id.slice(-8) })}</a>}
    {event.artifact_ids.map((id, index) => <a key={id} className="text-primary hover:underline" href={`/api/artifacts/${id}/body`} target="_blank" rel="noreferrer" title={id}>{t("events.evidence", { n: index + 1 })}</a>)}
  </div>;
}
