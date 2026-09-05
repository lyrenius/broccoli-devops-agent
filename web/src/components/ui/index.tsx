import type { ButtonHTMLAttributes, HTMLAttributes, InputHTMLAttributes, ReactNode, TextareaHTMLAttributes } from "react";
import type { LucideIcon } from "lucide-react";
import { cn } from "../../lib/cn";

/* The primitives below carry the same class recipes as Broccoli's web-sdk (shadcn-style), so the
   console reads as one family with the judge's own admin pages while depending on none of it. */

export function Card({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("rounded-xl border bg-card text-card-foreground shadow-sm", className)} {...props} />;
}
export function CardHeader({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("flex flex-col space-y-1.5 p-6", className)} {...props} />;
}
export function CardTitle({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("font-semibold leading-none tracking-tight", className)} {...props} />;
}
export function CardDescription({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("text-sm text-muted-foreground", className)} {...props} />;
}
export function CardContent({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("p-6 pt-0", className)} {...props} />;
}

type BadgeVariant = "default" | "secondary" | "destructive" | "outline" | "success" | "warning" | "danger";
const BADGE: Record<BadgeVariant, string> = {
  default: "border-transparent bg-primary text-primary-foreground shadow-sm",
  secondary: "border-transparent bg-secondary text-secondary-foreground",
  destructive: "border-transparent bg-destructive text-destructive-foreground shadow-sm",
  outline: "text-foreground",
  success: "border-emerald-500/40 text-emerald-600 dark:text-emerald-400",
  warning: "border-amber-500/40 text-amber-600 dark:text-amber-400",
  danger: "border-red-500/40 text-red-600 dark:text-red-400",
};
export function Badge({ variant = "default", className, ...props }: HTMLAttributes<HTMLSpanElement> & { variant?: BadgeVariant }) {
  return (
    <span
      className={cn("inline-flex items-center gap-1 whitespace-nowrap rounded-md border px-2.5 py-0.5 text-xs font-semibold transition-colors", BADGE[variant], className)}
      {...props}
    />
  );
}

type ButtonVariant = "default" | "destructive" | "outline" | "secondary" | "ghost";
type ButtonSize = "default" | "sm" | "icon";
const BUTTON: Record<ButtonVariant, string> = {
  default: "bg-primary text-primary-foreground shadow-sm hover:bg-primary/90",
  destructive: "bg-destructive text-destructive-foreground shadow-xs hover:bg-destructive/90",
  outline: "border border-input bg-background shadow-xs hover:bg-accent hover:text-accent-foreground",
  secondary: "bg-secondary text-secondary-foreground shadow-xs hover:bg-secondary/80",
  ghost: "hover:bg-accent hover:text-accent-foreground",
};
const SIZE: Record<ButtonSize, string> = {
  default: "h-9 px-4 py-2",
  sm: "h-8 rounded-md px-3 text-xs",
  icon: "h-9 w-9",
};
export function Button({
  variant = "default",
  size = "default",
  className,
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: ButtonVariant; size?: ButtonSize }) {
  return (
    <button
      className={cn(
        "inline-flex cursor-pointer items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition-colors focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0",
        BUTTON[variant],
        SIZE[size],
        className,
      )}
      {...props}
    />
  );
}

export function Input({ className, ...props }: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        "flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-base shadow-xs transition-colors placeholder:text-muted-foreground focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50 md:text-sm",
        className,
      )}
      {...props}
    />
  );
}
export function Textarea({ className, ...props }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return (
    <textarea
      className={cn(
        "flex min-h-[60px] w-full rounded-md border border-input bg-transparent px-3 py-2 text-base shadow-xs placeholder:text-muted-foreground focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50 md:text-sm",
        className,
      )}
      {...props}
    />
  );
}
export function Select({ className, ...props }: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select
      className={cn(
        "flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-xs focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring",
        className,
      )}
      {...props}
    />
  );
}

/** A label above a control, as Broccoli's forms lay them out. */
export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="grid gap-1.5">
      <span className="text-sm font-medium">{label}</span>
      {children}
      {hint && <span className="text-xs text-muted-foreground">{hint}</span>}
    </label>
  );
}

/** The four-up number tiles of the admin System page. */
export function StatTile({ label, value, icon: Icon, tone = "default", hint }: { label: string; value: ReactNode; icon: LucideIcon; tone?: "default" | "alert" | "warn"; hint?: string }) {
  const color = tone === "alert" ? "text-destructive" : tone === "warn" ? "text-amber-600 dark:text-amber-400" : "text-foreground";
  return (
    <Card>
      <CardContent className="pt-6">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <p className="text-xs font-medium uppercase tracking-wide text-muted-foreground">{label}</p>
            <p className={cn("mt-2 text-3xl font-semibold tabular-nums", color)}>{value}</p>
            {hint && <p className="mt-1 text-xs text-muted-foreground">{hint}</p>}
          </div>
          <Icon className={cn("h-5 w-5 shrink-0", tone === "alert" ? "text-destructive" : "text-muted-foreground")} />
        </div>
      </CardContent>
    </Card>
  );
}

/** Dashed placeholder for an empty list. */
export function EmptyState({ icon: Icon, title, hint }: { icon: LucideIcon; title: string; hint?: string }) {
  return (
    <div className="rounded-lg border border-dashed bg-muted/20 p-8 text-center">
      <Icon className="mx-auto mb-3 h-8 w-8 text-muted-foreground" />
      <p className="text-sm font-medium">{title}</p>
      {hint && <p className="mt-1 text-xs text-muted-foreground">{hint}</p>}
    </div>
  );
}

/** The pill-style filter of the DLQ page. */
export function Segmented<T extends string>({ value, options, onChange }: { value: T; options: { id: T; label: string; count?: number }[]; onChange: (id: T) => void }) {
  return (
    <div className="flex gap-1 rounded-md border bg-muted/30 p-0.5 text-xs">
      {options.map((opt) => (
        <button
          key={opt.id}
          onClick={() => onChange(opt.id)}
          className={cn(
            "cursor-pointer rounded px-3 py-1 transition-colors",
            value === opt.id ? "bg-background font-medium text-foreground shadow-sm" : "text-muted-foreground hover:text-foreground",
          )}
        >
          {opt.label}
          {opt.count !== undefined && <span className="ml-1.5 tabular-nums text-muted-foreground">{opt.count}</span>}
        </button>
      ))}
    </div>
  );
}

/** Key–value rows inside a card. */
export function Kv({ rows }: { rows: { k: string; v: ReactNode }[] }) {
  return (
    <dl className="mt-3 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1.5 text-sm">
      {rows.map((row) => (
        <div key={row.k} className="contents">
          <dt className="text-muted-foreground">{row.k}</dt>
          <dd className="min-w-0 break-words">{row.v}</dd>
        </div>
      ))}
    </dl>
  );
}

/** Inline alert, as Broccoli renders a failed request. */
export function Alert({ tone = "destructive", icon: Icon, children }: { tone?: "destructive" | "warning" | "info"; icon: LucideIcon; children: ReactNode }) {
  const styles = {
    destructive: "border-destructive/40 bg-destructive/5 text-destructive",
    warning: "border-amber-500/40 bg-amber-500/5 text-amber-700 dark:text-amber-400",
    info: "border-primary/30 bg-primary/5 text-foreground",
  }[tone];
  return (
    <div className={cn("flex items-start gap-3 rounded-md border p-3 text-sm", styles)}>
      <Icon className="mt-0.5 h-4 w-4 shrink-0" />
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  );
}
