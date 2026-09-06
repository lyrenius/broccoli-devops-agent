import { AlertTriangle, Coins } from "lucide-react";
import { useT } from "../i18n";
import type { UsageTotals } from "../types";
import { Alert, Card, CardContent, CardDescription, CardHeader, CardTitle, Kv, StatTile } from "./ui";

/** Compact token counts: 1_234_567 reads as 1.23M, which is what a bill is discussed in. */
export function tokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${n}`;
}

/** Money is only ever shown when a price list exists; tokens are shown regardless. */
export function money(usage: UsageTotals): string | null {
  return usage.cost === null ? null : `${usage.cost.toFixed(4)} ${usage.currency ?? ""}`.trim();
}

/** The Overview tile: what the relay has cost, and how close that is to the ceiling. */
export function SpendTile({ usage }: { usage: UsageTotals | undefined }) {
  const { t } = useT();
  const budget = usage?.budget ?? null;
  return (
    <StatTile
      label={t("stat.spend")}
      value={usage ? (money(usage) ?? tokens(usage.total_tokens)) : "—"}
      icon={Coins}
      tone={budget?.exceeded ? "alert" : budget && budget.used_fraction >= 0.8 ? "warn" : "default"}
      hint={
        usage
          ? usage.cost === null
            ? t("stat.spendNoPricing")
            : t("stat.spendHint", { passes: usage.passes, tokens: tokens(usage.total_tokens) })
          : undefined
      }
    />
  );
}

/** The full breakdown: tokens in and out, the cache hit, the cost, and any gap in the record. */
export function UsageCard({ usage }: { usage: UsageTotals | null }) {
  const { t } = useT();
  if (!usage) return null;
  const cost = money(usage);
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Coins className="h-4 w-4" />
          {t("usage.title")}
        </CardTitle>
        <CardDescription>{t("usage.desc")}</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-3">
        {usage.passes === 0 && <p className="text-sm text-muted-foreground">{t("usage.none")}</p>}
        {usage.passes > 0 && (
          <>
            <Kv
              rows={[
                { k: t("usage.input"), v: <span className="font-mono tabular-nums">{usage.input_tokens.toLocaleString()}</span> },
                { k: t("usage.cached"), v: <span className="font-mono tabular-nums text-muted-foreground">{usage.cached_input_tokens.toLocaleString()}</span> },
                { k: t("usage.output"), v: <span className="font-mono tabular-nums">{usage.output_tokens.toLocaleString()}</span> },
                { k: t("usage.total"), v: <span className="font-mono font-medium tabular-nums">{usage.total_tokens.toLocaleString()}</span> },
                ...(cost ? [{ k: t("usage.cost"), v: <span className="font-mono font-medium tabular-nums">{cost}</span> }] : []),
              ]}
            />
            {usage.by_model.length > 1 && (
              <ul className="divide-y rounded-lg border text-xs">
                {usage.by_model.map((model) => (
                  <li key={model.model} className="flex items-center gap-2 px-3 py-2">
                    <span className="truncate font-mono font-medium">{model.model}</span>
                    <span className="ml-auto shrink-0 font-mono tabular-nums text-muted-foreground">
                      {tokens(model.input_tokens + model.output_tokens)}
                      {model.cost !== null && ` · ${model.cost.toFixed(4)}`}
                    </span>
                  </li>
                ))}
              </ul>
            )}
            {usage.cost === null && <p className="text-xs text-muted-foreground">{t("usage.noPricing")}</p>}
            {usage.requests_without_usage > 0 && (
              <p className="text-xs text-amber-600 dark:text-amber-400">{t("usage.gap", { count: usage.requests_without_usage })}</p>
            )}
            {usage.budget && (
              <div className="grid gap-1.5">
                <div className="flex items-center justify-between text-xs text-muted-foreground">
                  <span>{t("usage.budget", { percent: Math.round(usage.budget.used_fraction * 100) })}</span>
                </div>
                <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                  <div
                    className={`h-full rounded-full ${usage.budget.exceeded ? "bg-destructive" : usage.budget.used_fraction >= 0.8 ? "bg-amber-500" : "bg-primary"}`}
                    style={{ width: `${Math.min(100, usage.budget.used_fraction * 100)}%` }}
                  />
                </div>
              </div>
            )}
            {usage.budget?.exceeded && (
              <Alert tone="warning" icon={AlertTriangle}>
                <p>{t("usage.budgetExceeded")}</p>
                {usage.budget.reason && <p className="mt-0.5 text-muted-foreground">{usage.budget.reason}</p>}
              </Alert>
            )}
          </>
        )}
      </CardContent>
    </Card>
  );
}
