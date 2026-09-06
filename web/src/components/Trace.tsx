import { AlertTriangle, Archive, ArrowLeft, Bot, ChevronRight, Download, Info, Radio, Terminal, User, Waypoints, Wrench } from "lucide-react";
import { Fragment, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { api, streamEvents } from "../api";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { loadOperator } from "../lib/prefs";
import type { ActionRun, EventRecord, Job, SessionBundle, TraceStep, Transcript, TranscriptEntry, TurnRecord } from "../types";
import { Page } from "./Shell";
import { StatusBadge } from "./status";
import { Alert, Badge, Card, CardContent, CardDescription, CardHeader, CardTitle, EmptyState } from "./ui";

const LIVE_JOB = new Set(["queued", "running"]);

type Relation = "initial" | "probes" | "revision" | "followUp";

/** How a pass relates to the one before it in the chain. */
function relationOf(job: Job): { kind: Relation; from?: string } {
  if (job.continues_job_id) return { kind: "followUp", from: job.continues_job_id };
  if (job.revises_job_id) return { kind: "revision", from: job.revises_job_id };
  if (job.supersedes_job_id) return { kind: "probes", from: job.supersedes_job_id };
  return { kind: "initial" };
}

function short(id: string): string {
  return `${id.slice(0, 8)}…`;
}

function ms(iso: string): number {
  return new Date(iso).getTime();
}

function fmtDuration(millis: number): string {
  if (!Number.isFinite(millis) || millis < 0) return "—";
  if (millis < 1000) return `${Math.round(millis)} ms`;
  if (millis < 60_000) return `${(millis / 1000).toFixed(1)} s`;
  return `${Math.floor(millis / 60_000)} min ${Math.round((millis % 60_000) / 1000)} s`;
}

function pretty(value: unknown): string {
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function actorTone(actor: string): string {
  if (actor === "human") return "text-primary";
  if (actor === "agent-team" || actor === "scheduler-policy") return "text-purple-600 dark:text-purple-400";
  if (actor === "agents-platform") return "text-amber-600 dark:text-amber-400";
  return "text-muted-foreground";
}

/** Long text folded to its first lines, with a control to unfold it. */
function Clamp({ text, lines = 14, mono }: { text: string; lines?: number; mono?: boolean }) {
  const [open, setOpen] = useState(false);
  const { t } = useT();
  const all = text.split("\n");
  const long = all.length > lines || text.length > 1_200;
  const shown = open || !long ? text : all.slice(0, lines).join("\n").slice(0, 1_200);
  return (
    <div>
      <pre className={cn("whitespace-pre-wrap [overflow-wrap:anywhere] text-xs leading-relaxed", mono ? "font-mono" : "font-sans")}>{shown}</pre>
      {long && (
        <button className="mt-1 cursor-pointer text-xs text-primary underline-offset-4 hover:underline" onClick={() => setOpen((o) => !o)}>
          {open ? t("trace.less") : t("trace.more", { n: all.length })}
        </button>
      )}
    </div>
  );
}

/** A folded section (instructions, the View) that opens in place. */
function Fold({ title, children }: { title: string; children: ReactNode }) {
  return (
    <details className="group rounded-lg border bg-muted/20">
      <summary className="flex cursor-pointer items-center gap-2 px-3 py-2 text-sm font-medium">
        <ChevronRight className="h-4 w-4 transition-transform group-open:rotate-90" />
        {title}
      </summary>
      <div className="border-t px-3 py-2">{children}</div>
    </details>
  );
}

type Row = {
  index: number;
  entry: TranscriptEntry;
  output?: { index: number; entry: TranscriptEntry };
  /** Set on the first entry of a model turn, when the stored transcript has turn records. */
  turn?: TurnRecord;
  /** Wall time this row covers: a tool from call to output, anything else until the next entry. */
  duration: number;
};

/** Pairs each tool call with its output and computes what every row took. */
function buildRows(entries: TranscriptEntry[], turns: TurnRecord[] | undefined, endAt: number | null): Row[] {
  const rows: Row[] = [];
  const open = new Map<string, number>();
  const turnAt = new Map<number, TurnRecord>();
  for (const turn of turns ?? []) turnAt.set(turn.first_entry, turn);
  entries.forEach((entry, index) => {
    const item = entry.item;
    if (item.type === "tool_output") {
      const at = open.get(item.call_id);
      if (at !== undefined) {
        rows[at].output = { index, entry };
        open.delete(item.call_id);
        return;
      }
    }
    if (item.type === "tool_call") open.set(item.call_id, rows.length);
    rows.push({ index, entry, turn: turnAt.get(index), duration: 0 });
  });
  rows.forEach((row, i) => {
    const start = ms(row.entry.at);
    const next = rows[i + 1] ? ms(rows[i + 1].entry.at) : (endAt ?? Date.now());
    row.duration = row.output ? ms(row.output.entry.at) - start : next - start;
  });
  return rows;
}

function TrustBadge({ trust }: { trust: string }) {
  const { t } = useT();
  if (trust === "untrusted") return <Badge variant="warning">{t("trace.untrusted")}</Badge>;
  if (trust === "mixed") return <Badge variant="outline">{t("trace.mixed")}</Badge>;
  return null;
}

/** One entry of the transcript, with its paired output when it is a tool call. */
function EntryCard({ row, maxDuration, truncated }: { row: Row; maxDuration: number; truncated: Set<number> }) {
  const { t, time } = useT();
  const item = row.entry.item;
  const kind = item.type;
  const isError = item.type === "tool_output" ? item.is_error : row.output ? (row.output.entry.item as { is_error?: boolean }).is_error : false;
  const refused = isError && row.output && /^refused/.test(pretty((row.output.entry.item as { output?: unknown }).output ?? "").replace(/^\{\s*"error":\s*"/, ""));
  const untrusted = ("trust" in item && item.trust === "untrusted") || (row.output && "trust" in row.output.entry.item && row.output.entry.item.trust === "untrusted");
  const Icon = kind === "user_input" ? User : kind === "assistant_text" ? Bot : kind === "notice" ? Info : kind === "tool_call" ? Wrench : Terminal;
  const tone =
    kind === "user_input"
      ? "border-primary/30 bg-primary/5"
      : kind === "notice"
        ? "border-amber-500/40 bg-amber-500/5"
        : isError
          ? "border-red-500/40 bg-red-500/5"
          : kind === "assistant_text"
            ? "bg-card"
            : "bg-muted/20";
  const width = maxDuration > 0 ? Math.max(6, Math.round(100 * Math.sqrt(row.duration / maxDuration))) : 6;
  const cut = truncated.has(row.index) || (row.output ? truncated.has(row.output.index) : false);
  return (
    <div className="grid grid-cols-[4.5rem_1fr] gap-3">
      <div className="pt-2 text-right">
        <div className="font-mono text-[11px] tabular-nums text-muted-foreground">{time(row.entry.at)}</div>
        <div className="mt-1.5 ml-auto h-1.5 rounded-full bg-primary/40" style={{ width: `${width}%` }} title={fmtDuration(row.duration)} />
        <div className="mt-0.5 font-mono text-[10px] tabular-nums text-muted-foreground">{fmtDuration(row.duration)}</div>
      </div>
      <div className={cn("min-w-0 rounded-lg border p-3", tone, untrusted && "border-dashed border-amber-500/60")}>
        <div className="mb-1.5 flex flex-wrap items-center gap-2 text-xs">
          <Icon className="h-3.5 w-3.5 text-muted-foreground" />
          <span className="font-medium uppercase tracking-wide text-muted-foreground">{t(`trace.kind.${kind}` as "trace.kind.tool_call")}</span>
          {"tool" in item && <span className="font-mono font-semibold">{item.tool}</span>}
          {isError && <Badge variant="danger">{refused ? t("trace.refused") : t("trace.error")}</Badge>}
          {"trust" in item && <TrustBadge trust={item.trust} />}
          {row.output && "trust" in row.output.entry.item && <TrustBadge trust={row.output.entry.item.trust} />}
          {cut && <Badge variant="outline">{t("trace.truncated")}</Badge>}
          <span className="ml-auto font-mono text-[10px] text-muted-foreground">#{row.index}</span>
        </div>
        {(item.type === "user_input" || item.type === "assistant_text" || item.type === "notice") && <Clamp text={item.text} />}
        {item.type === "tool_call" && <Clamp text={pretty(item.arguments)} mono lines={10} />}
        {item.type === "tool_output" && <Clamp text={pretty(item.output)} mono />}
        {row.output && row.output.entry.item.type === "tool_output" && (
          <div className="mt-2 border-t border-dashed pt-2">
            <div className="mb-1 flex items-center gap-2 text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
              <Terminal className="h-3 w-3" />
              {t("trace.kind.tool_output")}
              <span className="ml-auto font-mono normal-case tracking-normal">#{row.output.index}</span>
            </div>
            <Clamp text={pretty(row.output.entry.item.output)} mono />
          </div>
        )}
      </div>
    </div>
  );
}

function TurnDivider({ turn }: { turn: TurnRecord }) {
  const { t } = useT();
  const latency = ms(turn.finished_at) - ms(turn.started_at);
  const tokens = turn.usage.input_tokens + turn.usage.output_tokens;
  return (
    <div className="flex items-center gap-2 pl-[5.25rem] text-[11px] text-muted-foreground">
      <span className="font-medium text-foreground">{t("trace.turn", { n: turn.turn })}</span>
      <span>· {fmtDuration(latency)}</span>
      {turn.usage.requests_without_usage === 0 && <span>· {t("trace.tokens", { n: tokens.toLocaleString() })}</span>}
      {turn.retries > 0 && <Badge variant="warning">{t("trace.turn.retries", { n: turn.retries, ies: turn.retries === 1 ? "y" : "ies" })}</Badge>}
      {turn.wrap_up && <Badge variant="warning">{t("trace.turn.wrapUp")}</Badge>}
      <span className="h-px flex-1 bg-border" />
      <span title={turn.offered_tools.join(", ")}>{t("trace.turn.tools", { n: turn.offered_tools.length })}</span>
    </div>
  );
}

/** The turn-by-turn view of one pass: stored transcript when there is one, live steps until then. */
function PassTrace({ job, transcript, steps, live, endAt }: { job: Job; transcript: Transcript | null | "gone"; steps: TraceStep[]; live: boolean; endAt: number | null }) {
  const { t } = useT();
  const bottom = useRef<HTMLDivElement>(null);
  const stored = transcript && transcript !== "gone" ? transcript : null;
  const entries: TranscriptEntry[] = useMemo(() => {
    if (stored) return stored.entries;
    return [...steps].sort((a, b) => a.index - b.index).map((s) => ({ at: s.at, item: s.item }));
  }, [stored, steps]);
  const truncated = useMemo(() => new Set(stored ? [] : steps.filter((s) => s.truncated).map((s) => s.index)), [stored, steps]);
  const rows = useMemo(() => buildRows(entries, stored?.turns, live ? null : endAt), [entries, stored, live, endAt]);
  const maxDuration = rows.reduce((m, r) => Math.max(m, r.duration), 0);

  useEffect(() => {
    if (live) bottom.current?.scrollIntoView({ block: "nearest" });
  }, [live, rows.length]);

  const turns = stored?.turns ?? [];
  const toolCalls = entries.filter((e) => e.item.type === "tool_call").length;
  const retries = turns.reduce((n, turn) => n + turn.retries, 0);
  const wrappedUp = turns.some((turn) => turn.wrap_up);
  const first = entries[0] ? ms(entries[0].at) : ms(job.created_at);
  const last = live ? Date.now() : (endAt ?? (entries.length ? ms(entries[entries.length - 1].at) : first));

  if (!stored && !live && steps.length === 0) {
    if (transcript === "gone") return <Alert icon={AlertTriangle}>{t("trace.transcript.gone")}</Alert>;
    return <p className="text-sm text-muted-foreground">{t("trace.transcript.none")}</p>;
  }
  return (
    <div className="grid gap-4">
      <div className="flex flex-wrap gap-x-5 gap-y-1 text-xs text-muted-foreground">
        <span>
          <b className="text-foreground">{turns.length || (live ? "…" : "—")}</b> {t("trace.stat.turns")}
        </span>
        <span>
          <b className="text-foreground">{toolCalls}</b> {t("trace.stat.tools")}
        </span>
        <span>
          <b className="text-foreground">{entries.length}</b> {t("trace.stat.entries")}
        </span>
        <span>
          <b className="text-foreground">{fmtDuration(last - first)}</b> {t("trace.stat.duration")}
        </span>
        {retries > 0 && (
          <span>
            <b className="text-foreground">{retries}</b> {t("trace.stat.retries")}
          </span>
        )}
        {job.usage && job.usage.requests_without_usage === 0 && <span>{t("trace.tokens", { n: (job.usage.input_tokens + job.usage.output_tokens).toLocaleString() })}</span>}
        {wrappedUp && <Badge variant="warning">{t("trace.wrappedUp")}</Badge>}
      </div>
      {stored && (
        <Fold title={t("trace.instructions")}>
          <Clamp text={stored.instructions} lines={8} />
        </Fold>
      )}
      <div className="grid gap-3">
        {rows.length === 0 && <p className="text-sm text-muted-foreground">{t("trace.waiting")}</p>}
        {rows.map((row) => (
          <Fragment key={row.index}>
            {row.turn && <TurnDivider turn={row.turn} />}
            <EntryCard row={row} maxDuration={maxDuration} truncated={truncated} />
          </Fragment>
        ))}
        <div ref={bottom} />
      </div>
    </div>
  );
}

function ActionRow({ action }: { action: ActionRun }) {
  const { t, status: label } = useT();
  return (
    <li className="grid gap-1 p-3 text-xs">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono font-semibold">{action.runbook_id}</span>
        <span className="text-muted-foreground">on</span>
        <span className="font-mono">{action.target_ids.join(", ")}</span>
        <StatusBadge value={action.status} />
        <Badge variant="outline">
          {label(action.approval)}
          {action.approved_by && t("approval.by", { who: action.approved_by })}
        </Badge>
        {action.dry_run && <Badge variant="outline">dry-run</Badge>}
        {action.verification_evidence && <Badge variant={action.verification_evidence === "strong" ? "success" : "warning"}>{t(`evidence.${action.verification_evidence}`)}</Badge>}
      </div>
      <p className="text-muted-foreground">{action.reason}</p>
      {action.denial && (
        <p>
          <span className="font-medium">{t("kv.denialReason")}:</span> {action.denial.reason}
          {action.denial.comment && <> — “{action.denial.comment}”</>}
        </p>
      )}
      {action.execution_summary && (
        <p>
          <span className="font-medium">{t("kv.execution")}:</span> {action.execution_summary}
        </p>
      )}
      {action.verification_summary && (
        <p>
          <span className="font-medium">{t("kv.verification")}:</span> {action.verification_summary}
        </p>
      )}
    </li>
  );
}

export function Trace({ issueId, jobId, tick }: { issueId: string; jobId?: string; tick: number }) {
  const { t, status: label, dateTime, time } = useT();
  const [bundle, setBundle] = useState<SessionBundle | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState<string | undefined>(jobId);
  const [steps, setSteps] = useState<Record<string, TraceStep[]>>({});
  const [liveEvents, setLiveEvents] = useState<EventRecord[]>([]);
  const [connected, setConnected] = useState(false);
  const reload = useRef<number | null>(null);

  const load = useCallback(async () => {
    try {
      setBundle(await api.session(issueId));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [issueId]);

  // Load once, then follow the event stream: forwarded transcript entries feed the live
  // transcript directly; anything else means a record changed, so the session is re-read.
  useEffect(() => {
    let stop: (() => void) | null = null;
    let cancelled = false;
    api
      .session(issueId)
      .then((initial) => {
        if (cancelled) return;
        setBundle(initial);
        const after = initial.events.length ? initial.events[initial.events.length - 1].sequence : 0;
        stop = streamEvents(after, (event) => {
          if (event.issue_id !== issueId) return;
          if (event.kind === "team.step" && event.job_id) {
            const step = (event.payload as { step?: TraceStep } | undefined)?.step;
            if (step) setSteps((all) => ({ ...all, [event.job_id!]: [...(all[event.job_id!] ?? []), step] }));
            return;
          }
          setLiveEvents((all) => [...all, event]);
          if (reload.current) window.clearTimeout(reload.current);
          reload.current = window.setTimeout(() => void load(), 400);
        });
        setConnected(true);
      })
      .catch((e: Error) => setError(e.message));
    return () => {
      cancelled = true;
      stop?.();
      if (reload.current) window.clearTimeout(reload.current);
    };
  }, [issueId, load]);

  const jobs = useMemo(() => (bundle ? [...bundle.jobs].sort((a, b) => a.created_at.localeCompare(b.created_at)) : []), [bundle]);
  const hasLive = jobs.some((job) => LIVE_JOB.has(job.status));

  // Without a stream, a running pass still refreshes with the console's own tick.
  useEffect(() => {
    if (hasLive && !connected) void load();
  }, [tick, hasLive, connected, load]);

  const selected = useMemo(() => jobs.find((job) => job.job_id === picked) ?? jobs[jobs.length - 1], [jobs, picked]);
  const artifacts = useMemo(() => new Map((bundle?.artifacts ?? []).map((a) => [a.artifact.artifact_id, a])), [bundle]);
  const events = useMemo(() => {
    const seen = new Set<number>();
    return [...(bundle?.events ?? []), ...liveEvents].filter((e) => e.kind !== "team.step" && !seen.has(e.sequence) && seen.add(e.sequence));
  }, [bundle, liveEvents]);

  // The pass's transcript: a model-backed pass stores one as a DiagnosticBundle; a deterministic
  // pass has none, and a bundle whose body is missing or not JSON is "gone".
  const transcriptOf = (job: Job): Transcript | null | "gone" => {
    const bundles = (job.result?.artifact_ids ?? []).map((id) => artifacts.get(id)).filter((a) => a?.artifact.kind === "diagnostic_bundle");
    for (const a of bundles) {
      if (a && a.body.encoding === "json" && a.body.content && typeof a.body.content === "object" && "entries" in a.body.content) return a.body.content as Transcript;
    }
    return bundles.length ? "gone" : null;
  };
  const viewOf = (job: Job): unknown => {
    const a = artifacts.get(job.snapshot_view.artifact_id);
    return a && a.body.encoding === "json" ? a.body.content : null;
  };
  const actionsOf = (job: Job) => (bundle?.action_runs ?? []).filter((a) => a.originating_job_id === job.job_id);
  // When the Team was last heard from on this Job: humans may review it long after it ended.
  const endOf = (job: Job): number | null => {
    const own = events.filter((e) => e.job_id === job.job_id && (e.kind.startsWith("team.") || e.kind === "model.usage"));
    return own.length ? ms(own[own.length - 1].occurred_at) : null;
  };

  if (error && !bundle) {
    return (
      <Page icon={Waypoints} title={t("trace.title")}>
        <Alert icon={AlertTriangle}>{t("trace.notFound", { error })}</Alert>
      </Page>
    );
  }
  if (!bundle) {
    return (
      <Page icon={Waypoints} title={t("trace.title")}>
        <p className="text-sm text-muted-foreground">{t("trace.loading")}</p>
      </Page>
    );
  }

  const issue = bundle.issue;
  const archive = issue.provenance ?? null;
  return (
    <Page
      icon={Waypoints}
      title={issue.title}
      subtitle={
        <div className="grid gap-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-mono text-xs">{issue.issue_id}</span>
            <Badge variant="secondary">{label(issue.priority)}</Badge>
            <StatusBadge value={issue.status} />
            <span>{dateTime(issue.created_at)}</span>
            {archive && (
              <Badge variant="warning">
                <Archive className="h-3 w-3" />
                {t("trace.archive.badge")}
              </Badge>
            )}
            {hasLive && (
              <Badge variant={connected ? "success" : "outline"}>
                <Radio className="h-3 w-3" />
                {t("trace.live")}
              </Badge>
            )}
          </div>
          <p>{issue.description}</p>
          {archive && <p className="text-xs">{t("trace.archive.note", { deployment: archive.source_deployment, exporter: archive.exported_by, exported: dateTime(archive.exported_at), importer: archive.imported_by, imported: dateTime(archive.imported_at) })}</p>}
        </div>
      }
      actions={
        <>
          <a href="#records" className="inline-flex h-8 items-center gap-1 rounded-md px-3 text-xs font-medium hover:bg-accent hover:text-accent-foreground [&_svg]:size-4">
            <ArrowLeft />
            {t("trace.back")}
          </a>
          <a href={api.sessionDownloadUrl(issue.issue_id, loadOperator())} download className="inline-flex h-8 items-center gap-1 rounded-md border border-input bg-background px-3 text-xs font-medium shadow-xs hover:bg-accent hover:text-accent-foreground [&_svg]:size-4" title={t("records.export.title")}>
            <Download />
            {t("trace.export")}
          </a>
        </>
      }
    >
      {error && (
        <Alert icon={AlertTriangle}>
          <p>{error}</p>
        </Alert>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t("trace.chain")}</CardTitle>
          <CardDescription>{t("trace.chain.desc")}</CardDescription>
        </CardHeader>
        <CardContent>
          {jobs.length === 0 && <EmptyState icon={Waypoints} title={t("records.noJob")} />}
          <div className="flex items-stretch gap-1 overflow-x-auto pb-1">
            {jobs.map((job, i) => {
              const relation = relationOf(job);
              const live = LIVE_JOB.has(job.status);
              const end = endOf(job);
              const isSelected = selected?.job_id === job.job_id;
              return (
                <Fragment key={job.job_id}>
                  {i > 0 && (
                    <div className="flex shrink-0 flex-col items-center justify-center px-1 text-[10px] text-muted-foreground" title={relation.from ? t(`trace.relation.${relation.kind}.long` as "trace.relation.probes.long", { id: short(relation.from) }) : undefined}>
                      <ChevronRight className="h-4 w-4" />
                      <span className="whitespace-nowrap">{t(`trace.relation.${relation.kind}` as "trace.relation.initial")}</span>
                    </div>
                  )}
                  <button
                    onClick={() => {
                      setPicked(job.job_id);
                      window.history.replaceState(null, "", `#trace/${issueId}/${job.job_id}`);
                    }}
                    className={cn("w-60 shrink-0 cursor-pointer rounded-lg border p-3 text-left transition-colors hover:bg-accent/40", isSelected && "border-primary ring-1 ring-primary")}
                  >
                    <div className="flex items-center gap-2">
                      <span className="text-sm font-semibold">{t("trace.pass", { n: i + 1 })}</span>
                      <StatusBadge value={job.status} />
                      {live && <Radio className="h-3.5 w-3.5 animate-pulse text-emerald-500" />}
                    </div>
                    <div className="mt-1 flex flex-wrap gap-x-2 text-[11px] text-muted-foreground">
                      <span>{job.result ? label(job.result.outcome) : label(job.status)}</span>
                      {job.usage && <span>· {t("trace.tokens", { n: (job.usage.input_tokens + job.usage.output_tokens).toLocaleString() })}</span>}
                      {!job.usage && !live && job.result && <span>· {t("trace.noModel")}</span>}
                      <span>· {fmtDuration((live ? Date.now() : (end ?? ms(job.created_at))) - ms(job.created_at))}</span>
                    </div>
                    <div className="mt-1 truncate text-xs" title={job.result?.summary}>
                      {job.result?.summary ?? time(job.created_at)}
                    </div>
                  </button>
                </Fragment>
              );
            })}
          </div>
        </CardContent>
      </Card>

      {selected && (
        <div className="grid gap-4 xl:grid-cols-3">
          <Card className="xl:col-span-2">
            <CardHeader>
              <CardTitle className="flex items-center gap-2 text-base">
                {t("trace.transcript")}
                <span className="font-mono text-xs font-normal text-muted-foreground">{t("records.job", { id: short(selected.job_id) })}</span>
                {LIVE_JOB.has(selected.status) ? (
                  <Badge variant="success">
                    <Radio className="h-3 w-3" />
                    {t("trace.live")}
                  </Badge>
                ) : (
                  <Badge variant="outline">{t("trace.ended")}</Badge>
                )}
              </CardTitle>
              <CardDescription>{LIVE_JOB.has(selected.status) ? t("trace.transcript.live") : t("trace.transcript.stored")}</CardDescription>
            </CardHeader>
            <CardContent className="grid gap-3">
              {viewOf(selected) !== null && (
                <Fold title={t("trace.view")}>
                  <Clamp text={pretty(viewOf(selected))} mono lines={20} />
                </Fold>
              )}
              <PassTrace job={selected} transcript={transcriptOf(selected)} steps={steps[selected.job_id] ?? []} live={LIVE_JOB.has(selected.status)} endAt={endOf(selected)} />
            </CardContent>
          </Card>

          <div className="grid content-start gap-4">
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t("trace.result")}</CardTitle>
              </CardHeader>
              <CardContent className="grid gap-2 text-sm">
                {!selected.result && <p className="text-muted-foreground">{t("trace.result.none")}</p>}
                {selected.result && (
                  <>
                    <div className="flex flex-wrap items-center gap-2">
                      <Badge variant="outline">{label(selected.result.outcome)}</Badge>
                      {selected.result.follow_up_requested && <Badge variant="outline">{t("trace.relation.followUp")}</Badge>}
                    </div>
                    <p>{selected.result.summary}</p>
                    {selected.result.unresolved_questions.length > 0 && (
                      <ul className="list-disc pl-5 text-xs text-muted-foreground">
                        {selected.result.unresolved_questions.map((q, i) => (
                          <li key={i}>{q}</li>
                        ))}
                      </ul>
                    )}
                    {selected.result.requested_probes && selected.result.requested_probes.length > 0 && (
                      <ul className="text-xs text-muted-foreground">
                        {selected.result.requested_probes.map((p, i) => (
                          <li key={i}>
                            <span className="font-mono">{p.probe_id}</span> on <span className="font-mono">{p.target_ids.join(", ")}</span> — {p.reason}
                          </li>
                        ))}
                      </ul>
                    )}
                  </>
                )}
                {selected.feedback.length > 0 && (
                  <div className="mt-1 grid gap-1.5">
                    <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">{t("trace.feedback")}</span>
                    {selected.feedback.map((f) => (
                      <div key={f.feedback_id} className="rounded-md border border-primary/30 bg-primary/5 p-2 text-xs">
                        <span className="font-medium">{f.reviewer}</span>
                        {f.comment && <> · “{f.comment}”</>}
                        <div className="text-muted-foreground">{f.origin.kind.replace(/_/g, " ")}</div>
                      </div>
                    ))}
                  </div>
                )}
              </CardContent>
            </Card>

            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t("trace.actions")}</CardTitle>
              </CardHeader>
              <CardContent>
                {actionsOf(selected).length === 0 && <p className="text-sm text-muted-foreground">{t("trace.actions.none")}</p>}
                {actionsOf(selected).length > 0 && (
                  <ul className="divide-y rounded-lg border bg-muted/20">
                    {actionsOf(selected).map((action) => (
                      <ActionRow key={action.action_run_id} action={action} />
                    ))}
                  </ul>
                )}
              </CardContent>
            </Card>

            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t("trace.events")}</CardTitle>
                <CardDescription>{t("trace.events.desc")}</CardDescription>
              </CardHeader>
              <CardContent className="p-0">
                <div className="max-h-[32rem] overflow-y-auto font-mono text-[11px]">
                  {events.map((e) => (
                    <div key={e.sequence} className={cn("grid grid-cols-[3.5rem_5.5rem_1fr] gap-2 border-b border-dashed px-4 py-1.5 last:border-b-0", e.job_id === selected.job_id && "bg-primary/5")} title={e.kind}>
                      <span className="tabular-nums text-muted-foreground">{e.occurred_at.slice(11, 19)}</span>
                      <span className={cn("truncate", actorTone(e.actor))}>{e.actor}</span>
                      <span className="whitespace-pre-wrap [overflow-wrap:anywhere]">{e.summary}</span>
                    </div>
                  ))}
                </div>
              </CardContent>
            </Card>
          </div>
        </div>
      )}
    </Page>
  );
}
