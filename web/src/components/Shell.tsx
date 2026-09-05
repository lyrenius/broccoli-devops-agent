import type { ReactNode } from "react";
import type { LucideIcon } from "lucide-react";
import {
  FilePlus2,
  Inbox,
  Languages,
  LayoutDashboard,
  ListChecks,
  Moon,
  Play,
  ScrollText,
  ShieldAlert,
  Snowflake,
  Sprout,
  Sun,
  User,
} from "lucide-react";
import { useState } from "react";
import { api } from "../api";
import { useLocale, useT } from "../i18n";
import type { Key } from "../i18n/en";
import { cn } from "../lib/cn";
import { useOperator, useTheme } from "../lib/prefs";
import type { Status } from "../types";

export type Tab = "overview" | "inbox" | "records" | "events" | "report";

const NAV: { id: Tab; label: Key; icon: LucideIcon }[] = [
  { id: "overview", label: "nav.overview", icon: LayoutDashboard },
  { id: "inbox", label: "nav.inbox", icon: Inbox },
  { id: "records", label: "nav.records", icon: ListChecks },
  { id: "events", label: "nav.events", icon: ScrollText },
  { id: "report", label: "nav.report", icon: FilePlus2 },
];

const MODE_KEY: Record<string, Key> = {
  running: "mode.running",
  dispatch_frozen: "mode.dispatch_frozen",
  fully_frozen: "mode.fully_frozen",
  recovering: "mode.recovering",
};

function modeDot(mode: string): string {
  if (mode === "running") return "bg-emerald-500";
  if (mode === "dispatch_frozen") return "bg-amber-500";
  if (mode === "fully_frozen" || mode === "recovering") return "bg-red-500";
  return "bg-muted-foreground";
}

function MenuButton({
  active,
  icon: Icon,
  label,
  onClick,
  trailing,
  className,
}: {
  active?: boolean;
  icon: LucideIcon;
  label: ReactNode;
  onClick?: () => void;
  trailing?: ReactNode;
  className?: string;
}) {
  return (
    <button
      onClick={onClick}
      data-active={active}
      className={cn(
        "flex h-8 w-full cursor-pointer items-center gap-2 overflow-hidden rounded-md p-2 text-left text-sm outline-hidden transition-colors hover:bg-sidebar-accent hover:text-sidebar-accent-foreground focus-visible:ring-2 focus-visible:ring-sidebar-ring data-[active=true]:bg-sidebar-accent data-[active=true]:font-medium data-[active=true]:text-sidebar-accent-foreground [&>svg]:size-4 [&>svg]:shrink-0",
        className,
      )}
    >
      <Icon className={active ? "text-sidebar-primary" : undefined} />
      <span className="truncate">{label}</span>
      {trailing}
    </button>
  );
}

function GroupLabel({ children }: { children: ReactNode }) {
  return <div className="flex h-8 shrink-0 items-center px-2 text-xs font-medium text-sidebar-foreground/70">{children}</div>;
}

export function Shell({
  tab,
  onTab,
  status,
  error,
  onChanged,
  children,
}: {
  tab: Tab;
  onTab: (tab: Tab) => void;
  status: Status | null;
  error: string | null;
  onChanged: () => void;
  children: ReactNode;
}) {
  const [theme, toggleTheme] = useTheme();
  const [operator, setOperator] = useOperator();
  const [busy, setBusy] = useState(false);
  const { t, status: statusLabel } = useT();
  const { locale, setLocale } = useLocale();
  const mode = status?.mode ?? "unknown";

  const transition = async (name: "freeze-dispatch" | "freeze-all" | "resume") => {
    setBusy(true);
    try {
      await api.transition(name);
      onChanged();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-screen w-full">
      <aside className="sticky top-0 flex h-screen w-64 shrink-0 flex-col border-r border-sidebar-border bg-sidebar text-sidebar-foreground">
        <div className="flex flex-col gap-2 p-2">
          <div className="flex items-center gap-2 rounded-md p-2">
            <span className="flex size-8 items-center justify-center rounded-md bg-sidebar-primary text-sidebar-primary-foreground">
              <Sprout className="size-4" />
            </span>
            <div className="flex flex-col leading-none">
              <span className="font-semibold">Broccoli</span>
              <span className="mt-0.5 text-xs text-sidebar-foreground/70">{t("brand.tagline")}</span>
            </div>
          </div>
        </div>

        <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-auto p-2">
          <div className="flex flex-col">
            <GroupLabel>{t("group.controlPlane")}</GroupLabel>
            <ul className="flex flex-col gap-1">
              {NAV.map((item) => (
                <li key={item.id}>
                  <MenuButton
                    active={tab === item.id}
                    icon={item.icon}
                    label={t(item.label)}
                    onClick={() => onTab(item.id)}
                    trailing={
                      item.id === "inbox" && status && status.inbox.total > 0 ? (
                        <span className="ml-auto rounded-md bg-sidebar-primary px-1.5 text-[11px] font-bold leading-5 text-sidebar-primary-foreground tabular-nums">
                          {status.inbox.total}
                        </span>
                      ) : undefined
                    }
                  />
                </li>
              ))}
            </ul>
          </div>

          <div className="flex flex-col">
            <GroupLabel>{t("group.scheduler")}</GroupLabel>
            <div className="flex items-center gap-2 px-2 py-1.5 text-sm">
              <span className={cn("size-2 rounded-full", modeDot(mode))} />
              <span className="font-medium">{MODE_KEY[mode] ? t(MODE_KEY[mode]) : t("mode.unknown")}</span>
            </div>
            <ul className="flex flex-col gap-1">
              <li>
                <MenuButton icon={Snowflake} label={t("scheduler.freezeDispatch")} onClick={() => void transition("freeze-dispatch")} className={cn((busy || mode === "dispatch_frozen") && "pointer-events-none opacity-50")} />
              </li>
              <li>
                <MenuButton icon={ShieldAlert} label={t("scheduler.freezeAll")} onClick={() => void transition("freeze-all")} className={cn((busy || mode === "fully_frozen") && "pointer-events-none opacity-50")} />
              </li>
              <li>
                <MenuButton icon={Play} label={t("scheduler.resume")} onClick={() => void transition("resume")} className={cn((busy || mode === "running") && "pointer-events-none opacity-50")} />
              </li>
            </ul>
            {status && (
              <div className="mt-2 grid gap-1 px-2 text-xs text-sidebar-foreground/70">
                <div className="truncate" title={status.deployment.name}>
                  {status.deployment.name} · {statusLabel(status.deployment.operation_mode)}
                </div>
                <div className="truncate" title={status.team_backend}>
                  {status.team_backend.replace(/ via .*\)$/, ")")}
                </div>
                <div className={cn("font-medium", status.dry_run ? "text-amber-600 dark:text-amber-400" : "text-red-600 dark:text-red-400")}>
                  {status.dry_run ? t("platform.dryRun") : t("platform.live")}
                </div>
              </div>
            )}
            {error && <div className="mt-2 px-2 text-xs text-destructive">{t("api.unreachable", { error })}</div>}
          </div>
        </div>

        <div className="flex flex-col gap-1 border-t border-sidebar-border p-2">
          <MenuButton
            icon={Languages}
            label={t("locale.name")}
            onClick={() => setLocale(locale === "en" ? "zh-CN" : "en")}
            className="bg-sidebar-accent/50"
            trailing={<span className="ml-auto text-xs text-sidebar-foreground/60">{t("locale.switch")}</span>}
          />
          <MenuButton icon={theme === "light" ? Moon : Sun} label={theme === "light" ? t("theme.dark") : t("theme.light")} onClick={toggleTheme} className="bg-sidebar-accent/50" />
          <label className="flex h-8 items-center gap-2 rounded-md p-2 text-sm">
            <User className="size-4 shrink-0" />
            <input
              value={operator}
              onChange={(e) => setOperator(e.target.value)}
              placeholder={t("operator.placeholder")}
              title={t("operator.title")}
              className="w-full min-w-0 bg-transparent outline-hidden placeholder:text-sidebar-foreground/50"
            />
          </label>
        </div>
      </aside>

      <main className="flex min-w-0 flex-1 flex-col">{children}</main>
    </div>
  );
}

/** Page frame with Broccoli's sticky header: icon, title, subtitle, actions. */
export function Page({
  icon: Icon,
  title,
  subtitle,
  actions,
  children,
}: {
  icon: LucideIcon;
  title: string;
  subtitle?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="p-6">
      <div className="sticky top-0 z-10 -mx-6 -mt-6 mb-4 border-b bg-background px-6 pb-4 pt-6">
        <div className="flex items-center gap-4">
          <div className="flex min-w-0 items-center gap-3">
            <Icon className="h-6 w-6 shrink-0 text-primary" />
            <h1 className="truncate text-2xl font-bold tracking-tight">{title}</h1>
          </div>
          {actions && <div className="ml-auto flex shrink-0 items-center gap-2">{actions}</div>}
        </div>
        {subtitle && <div className="mt-3 text-sm text-muted-foreground">{subtitle}</div>}
      </div>
      <div className="flex flex-col gap-4">{children}</div>
    </div>
  );
}
