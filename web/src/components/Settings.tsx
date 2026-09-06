import { AlertTriangle, Check, Lock, Plus, RotateCcw, Save, SlidersHorizontal, Snowflake, Trash2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { api } from "../api";
import { useT } from "../i18n";
import type { Key } from "../i18n/en";
import { loadOperator } from "../lib/prefs";
import type { AgentConfig, RunbookCommand, SettingsPage, Status } from "../types";
import { Page } from "./Shell";
import { Alert, Badge, Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Field, Input, Kv, Textarea } from "./ui";

const FROZEN = new Set(["dispatch_frozen", "fully_frozen"]);

type Draft = AgentConfig;

function clone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

/** The nested object of leaves that differ between `base` and `draft`, in the file's shape. */
function changesBetween(base: unknown, draft: unknown): unknown {
  if (Array.isArray(base) || Array.isArray(draft) || typeof base !== "object" || typeof draft !== "object" || base === null || draft === null) {
    return JSON.stringify(base) === JSON.stringify(draft) ? undefined : draft;
  }
  const out: Record<string, unknown> = {};
  const keys = new Set([...Object.keys(base as object), ...Object.keys(draft as object)]);
  for (const key of keys) {
    const inner = changesBetween((base as Record<string, unknown>)[key], (draft as Record<string, unknown>)[key]);
    if (inner !== undefined) out[key] = inner;
  }
  return Object.keys(out).length ? out : undefined;
}

function countLeaves(value: unknown): number {
  if (Array.isArray(value) || typeof value !== "object" || value === null) return 1;
  return Object.values(value).reduce<number>((n, inner) => n + countLeaves(inner), 0);
}

function NumberField({ label, hint, value, onChange, min = 0, step = 1, disabled }: { label: string; hint?: string; value: number; onChange: (n: number) => void; min?: number; step?: number; disabled?: boolean }) {
  return (
    <Field label={label} hint={hint}>
      <Input type="number" min={min} step={step} value={Number.isFinite(value) ? value : ""} disabled={disabled} onChange={(e) => onChange(e.target.value === "" ? 0 : Number(e.target.value))} />
    </Field>
  );
}

function LinesField({ label, hint, value, onChange, disabled }: { label: string; hint?: string; value: string[]; onChange: (lines: string[]) => void; disabled?: boolean }) {
  return (
    <Field label={label} hint={hint}>
      <Textarea rows={3} className="font-mono text-xs" value={value.join("\n")} disabled={disabled} onChange={(e) => onChange(e.target.value.split("\n").map((l) => l.trim()).filter(Boolean))} />
    </Field>
  );
}

function Section({ title, desc, badge, children }: { title: string; desc: string; badge?: ReactNode; children: ReactNode }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          {title}
          {badge}
        </CardTitle>
        <CardDescription>{desc}</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4">{children}</CardContent>
    </Card>
  );
}

export function Settings({ status, onChanged }: { status: Status | null; onChanged: () => void }) {
  const { t, status: label } = useT();
  const [page, setPage] = useState<SettingsPage | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const load = useCallback(async () => {
    try {
      const next = await api.settings();
      setPage(next);
      setDraft(clone(next.config));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const mode = status?.mode ?? page?.mode ?? "unknown";
  const frozen = FROZEN.has(mode);
  const changes = useMemo(() => (page && draft ? changesBetween(page.config, draft) : undefined), [page, draft]);
  const changeCount = changes ? countLeaves(changes) : 0;

  const edit = (mutate: (d: Draft) => void) => {
    setDraft((current) => {
      if (!current) return current;
      const next = clone(current);
      mutate(next);
      return next;
    });
  };

  const save = async () => {
    if (!page || !draft || !changes) return;
    let confirm = false;
    if (page.config.platform.dry_run && !draft.platform.dry_run) {
      if (!window.confirm(t("settings.confirmLive"))) return;
      confirm = true;
    }
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      const result = await api.updateSettings(loadOperator(), changes, confirm);
      setNotice(t("settings.saved", { n: result.changes.length, keys: result.changes.map((c) => c.key).join(", ") }));
      await load();
      onChanged();
    } catch (e) {
      setError(t("settings.failed", { error: (e as Error).message }));
    } finally {
      setSaving(false);
    }
  };

  const freeze = async () => {
    try {
      await api.transition("freeze-dispatch");
      onChanged();
      await load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  if (!page || !draft) {
    return (
      <Page icon={SlidersHorizontal} title={t("settings.title")} subtitle={t("settings.subtitle")}>
        {error ? (
          <Alert icon={AlertTriangle}>
            <p>{error}</p>
          </Alert>
        ) : (
          <p className="text-sm text-muted-foreground">{t("settings.loading")}</p>
        )}
      </Page>
    );
  }

  const model = draft.model;
  const policyLocked = !frozen;
  const label_ = (key: Key) => t(key);

  return (
    <Page
      icon={SlidersHorizontal}
      title={t("settings.title")}
      subtitle={
        <>
          {t("settings.subtitle")} <span className="font-mono text-xs">{page.path ? t("settings.path", { path: page.path }) : t("settings.noPath")}</span>
        </>
      }
      actions={
        <>
          <Button variant="outline" size="sm" disabled={!changeCount || saving} onClick={() => setDraft(clone(page.config))}>
            <RotateCcw />
            {t("settings.reset")}
          </Button>
          <Button size="sm" disabled={!changeCount || saving} onClick={() => void save()}>
            <Save />
            {saving ? t("settings.saving") : changeCount ? t("settings.save", { n: changeCount }) : t("settings.noChanges")}
          </Button>
        </>
      }
    >
      {error && (
        <Alert icon={AlertTriangle}>
          <p>{error}</p>
        </Alert>
      )}
      {notice && (
        <Alert tone="info" icon={Check}>
          <p>{notice}</p>
        </Alert>
      )}

      <Section title={t("settings.live.title")} desc={t("settings.live.desc")}>
        <div className="grid gap-4 md:grid-cols-3">
          <NumberField label={label_("settings.f.snapshot_interval")} hint={t("settings.zeroOff")} value={draft.collector.snapshot_interval_secs} onChange={(n) => edit((d) => (d.collector.snapshot_interval_secs = n))} />
          <NumberField label={label_("settings.f.max_auto_passes")} value={draft.agent.max_auto_passes} min={1} onChange={(n) => edit((d) => (d.agent.max_auto_passes = n))} />
          {model && <NumberField label={label_("settings.f.max_inspections")} hint={t("settings.zeroOff")} value={model.max_inspections} onChange={(n) => edit((d) => (d.model!.max_inspections = n))} />}
        </div>
        {model ? (
          <>
            <div className="grid gap-4 md:grid-cols-3">
              <NumberField label={label_("settings.f.max_model_turns")} value={model.max_model_turns} min={1} onChange={(n) => edit((d) => (d.model!.max_model_turns = n))} />
              <NumberField label={label_("settings.f.max_tool_calls")} value={model.max_tool_calls} min={1} onChange={(n) => edit((d) => (d.model!.max_tool_calls = n))} />
              <NumberField label={label_("settings.f.max_tokens_per_run")} hint={t("settings.zeroNoLimit")} value={model.max_tokens_per_run} onChange={(n) => edit((d) => (d.model!.max_tokens_per_run = n))} />
            </div>
            <div>
              <p className="mb-2 text-sm font-medium">{t("settings.f.pricing")}</p>
              <div className="grid gap-4 md:grid-cols-4">
                <NumberField label={label_("settings.f.pricing.input")} step={0.01} value={model.pricing?.input_per_mtok ?? 0} onChange={(n) => edit((d) => (d.model!.pricing = { ...(d.model!.pricing ?? { cached_input_per_mtok: null, output_per_mtok: 0, currency: "USD" }), input_per_mtok: n }))} />
                <NumberField label={label_("settings.f.pricing.cached")} step={0.01} value={model.pricing?.cached_input_per_mtok ?? 0} onChange={(n) => edit((d) => (d.model!.pricing = { ...(d.model!.pricing ?? { input_per_mtok: 0, output_per_mtok: 0, currency: "USD" }), cached_input_per_mtok: n }))} />
                <NumberField label={label_("settings.f.pricing.output")} step={0.01} value={model.pricing?.output_per_mtok ?? 0} onChange={(n) => edit((d) => (d.model!.pricing = { ...(d.model!.pricing ?? { input_per_mtok: 0, cached_input_per_mtok: null, currency: "USD" }), output_per_mtok: n }))} />
                <Field label={label_("settings.f.pricing.currency")}>
                  <Input value={model.pricing?.currency ?? "USD"} onChange={(e) => edit((d) => (d.model!.pricing = { ...(d.model!.pricing ?? { input_per_mtok: 0, cached_input_per_mtok: null, output_per_mtok: 0 }), currency: e.target.value }))} />
                </Field>
              </div>
            </div>
          </>
        ) : (
          <p className="text-sm text-muted-foreground">{t("settings.modelAbsent")}</p>
        )}
        <div className="grid gap-4 md:grid-cols-3">
          <NumberField label={label_("settings.f.budget.tokens")} hint={t("settings.zeroOff")} value={draft.budget.max_total_tokens} onChange={(n) => edit((d) => (d.budget.max_total_tokens = n))} />
          <NumberField label={label_("settings.f.budget.cost")} hint={t("settings.zeroOff")} step={0.01} value={draft.budget.max_total_cost} onChange={(n) => edit((d) => (d.budget.max_total_cost = n))} />
          <p className="self-end pb-2 text-xs text-muted-foreground">{t("settings.f.budget.hint")}</p>
        </div>
      </Section>

      <Section
        title={t("settings.policy.title")}
        desc={t("settings.policy.desc")}
        badge={
          policyLocked ? (
            <Badge variant="warning">
              <Lock className="h-3 w-3" />
              {label(mode)}
            </Badge>
          ) : (
            <Badge variant="success">{label(mode)}</Badge>
          )
        }
      >
        {policyLocked && (
          <Alert tone="warning" icon={Lock}>
            <div className="flex flex-wrap items-center gap-3">
              <span>{t("settings.policy.frozenOnly", { mode: label(mode) })}</span>
              <Button size="sm" variant="outline" onClick={() => void freeze()}>
                <Snowflake />
                {t("settings.policy.freeze")}
              </Button>
            </div>
          </Alert>
        )}
        <label className="flex items-center gap-2 text-sm">
          <input type="checkbox" className="size-4" checked={draft.platform.dry_run} disabled={policyLocked} onChange={(e) => edit((d) => (d.platform.dry_run = e.target.checked))} />
          <span className={draft.platform.dry_run ? "" : "font-medium text-red-600 dark:text-red-400"}>{t("settings.f.dry_run")}</span>
        </label>
        <div className="grid gap-4 md:grid-cols-2">
          <NumberField label={label_("settings.f.command_timeout")} min={1} value={draft.platform.command_timeout_secs} disabled={policyLocked} onChange={(n) => edit((d) => (d.platform.command_timeout_secs = n))} />
          <NumberField label={label_("settings.f.auto_repeat_window")} hint={t("settings.f.auto_repeat_window.hint")} value={draft.platform.auto_repeat_window_secs} disabled={policyLocked} onChange={(n) => edit((d) => (d.platform.auto_repeat_window_secs = n))} />
        </div>
        <div className="grid gap-4 md:grid-cols-2">
          <LinesField label={label_("settings.f.tunable")} hint={t("settings.perLine")} value={draft.platform.classification.tunable_config_keys} disabled={policyLocked} onChange={(v) => edit((d) => (d.platform.classification.tunable_config_keys = v))} />
          <LinesField label={label_("settings.f.contest")} hint={t("settings.perLine")} value={draft.platform.classification.contest_config_keys} disabled={policyLocked} onChange={(v) => edit((d) => (d.platform.classification.contest_config_keys = v))} />
          <LinesField label={label_("settings.f.security")} hint={t("settings.perLine")} value={draft.platform.classification.security_config_keys} disabled={policyLocked} onChange={(v) => edit((d) => (d.platform.classification.security_config_keys = v))} />
          <LinesField label={label_("settings.f.internal_prefixes")} hint={t("settings.perLine")} value={draft.platform.classification.known_internal_address_prefixes} disabled={policyLocked} onChange={(v) => edit((d) => (d.platform.classification.known_internal_address_prefixes = v))} />
        </div>
        <div>
          <p className="text-sm font-medium">{t("settings.f.runbooks")}</p>
          <p className="mb-2 text-xs text-muted-foreground">{t("settings.f.runbooks.hint")}</p>
          <div className="grid gap-2">
            {draft.platform.runbooks.map((runbook: RunbookCommand, i: number) => (
              <div key={i} className="grid grid-cols-[14rem_1fr_auto] items-center gap-2">
                <Input className="font-mono text-xs" placeholder={t("settings.f.runbook.id")} value={runbook.id} disabled={policyLocked} onChange={(e) => edit((d) => (d.platform.runbooks[i].id = e.target.value))} />
                <Input className="font-mono text-xs" placeholder={t("settings.f.runbook.command")} value={runbook.command} disabled={policyLocked} onChange={(e) => edit((d) => (d.platform.runbooks[i].command = e.target.value))} />
                <Button size="icon" variant="ghost" disabled={policyLocked} title={t("settings.f.remove")} onClick={() => edit((d) => d.platform.runbooks.splice(i, 1))}>
                  <Trash2 />
                </Button>
              </div>
            ))}
            <div>
              <Button size="sm" variant="outline" disabled={policyLocked} onClick={() => edit((d) => d.platform.runbooks.push({ id: "", command: "" }))}>
                <Plus />
                {t("settings.f.add_runbook")}
              </Button>
            </div>
          </div>
        </div>
      </Section>

      <Section title={t("settings.startup.title")} desc={t("settings.startup.desc")} badge={<Badge variant="outline">{t("settings.restart")}</Badge>}>
        <Kv
          rows={[
            { k: t("settings.f.language"), v: page.config.agent.language },
            { k: t("settings.f.data_dir"), v: <span className="font-mono text-xs">{page.config.data.dir}</span> },
            { k: t("settings.f.topology"), v: <span className="font-mono text-xs">{page.config.topology.path}</span> },
            { k: t("settings.f.base_url"), v: <span className="font-mono text-xs">{page.config.model?.base_url ?? "—"}</span> },
            { k: t("settings.f.model"), v: <span className="font-mono text-xs">{page.config.model?.model ?? "—"}</span> },
            { k: t("settings.f.wire_api"), v: page.config.model?.wire_api ?? "—" },
            {
              k: t("settings.f.api_key_env"),
              v: page.config.model ? (
                <>
                  <span className="font-mono text-xs">{page.config.model.api_key_env}</span>{" "}
                  <Badge variant={page.config.model.api_key_present ? "success" : "danger"}>{page.config.model.api_key_present ? t("settings.f.key_present") : t("settings.f.key_absent")}</Badge>
                </>
              ) : (
                "—"
              ),
            },
            { k: t("settings.f.timeout"), v: page.config.model ? String(page.config.model.timeout_secs) : "—" },
            { k: t("settings.f.bind"), v: <span className="font-mono text-xs">{page.config.api.bind}</span> },
            { k: t("settings.f.token"), v: page.config.api.token ? t("settings.f.token.set") : t("settings.f.token.none") },
          ]}
        />
      </Section>
    </Page>
  );
}
