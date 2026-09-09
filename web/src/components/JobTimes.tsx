import { useT } from "../i18n";
import type { Job } from "../types";

export function JobTimes({ job }: { job: Job }) {
  const { t, dateTime } = useT();
  return <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
    <time dateTime={job.created_at}>{t("job.created", { time: dateTime(job.created_at) })}</time>
    {job.started_at && <time dateTime={job.started_at}>{t("job.started", { time: dateTime(job.started_at) })}</time>}
    {job.completed_at && <time dateTime={job.completed_at}>{t("job.ended", { time: dateTime(job.completed_at) })}</time>}
  </div>;
}
