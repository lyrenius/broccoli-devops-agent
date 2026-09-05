import { Badge } from "./ui";

/** Maps a health, status, or mode string onto Broccoli's badge tones. */
export function tone(value: string): "success" | "warning" | "danger" | "outline" {
  switch (value) {
    case "healthy":
    case "succeeded":
    case "running":
    case "resolved":
    case "completed":
    case "approved":
    case "not_required":
      return "success";
    case "degraded":
    case "waiting_for_approval":
    case "waiting_for_human":
    case "dispatch_frozen":
    case "pending":
    case "verifying":
    case "mitigating":
    case "investigating":
    case "open":
      return "warning";
    case "down":
    case "failed":
    case "verification_failed":
    case "cancelled":
    case "fully_frozen":
    case "rejected":
      return "danger";
    default:
      return "outline";
  }
}

export function StatusBadge({ value, className }: { value: string; className?: string }) {
  return (
    <Badge variant={tone(value)} className={className}>
      <span className="font-mono font-medium">{value.replace(/_/g, " ")}</span>
    </Badge>
  );
}

export function EvidenceBadge({ evidence }: { evidence: "dry_run" | "weak" | "strong" | null }) {
  if (!evidence) return null;
  const variant = evidence === "strong" ? "success" : evidence === "weak" ? "warning" : "outline";
  return (
    <Badge variant={variant}>
      <span className="font-normal">{evidence.replace("_", " ")} evidence</span>
    </Badge>
  );
}
