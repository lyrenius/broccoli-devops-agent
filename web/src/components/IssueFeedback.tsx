import { AlertTriangle, MessageSquareQuote } from "lucide-react";
import { useState } from "react";
import { api } from "../api";
import { useT } from "../i18n";
import { loadOperator } from "../lib/prefs";
import type { Revision, WaitingIssue } from "../types";
import { Alert, Button, Textarea } from "./ui";

/** Human input continues the exact pass shown by this card, over a fresh snapshot. */
export function IssueFeedback({ item, onChanged, onRevision }: {
  item: WaitingIssue;
  onChanged: () => void;
  onRevision?: (revision: Revision) => void;
}) {
  const { t } = useT();
  const [comment, setComment] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const send = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!comment.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      const revision = await api.feedbackIssue(item.issue.issue_id, item.job.job_id, loadOperator(), comment);
      setComment("");
      onRevision?.(revision);
      onChanged();
    } catch (e) {
      setError((e as Error).message);
      onChanged();
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className="mt-3 grid gap-2" onSubmit={send}>
      {error && <Alert icon={AlertTriangle}><p>{error}</p></Alert>}
      <Textarea aria-label={t("feedback.comment")} rows={3} value={comment} onChange={(e) => setComment(e.target.value)} placeholder={t("feedback.placeholder")} />
      <div><Button size="sm" disabled={busy || !comment.trim()} type="submit"><MessageSquareQuote />{busy ? t("feedback.sending") : t("feedback.send")}</Button></div>
    </form>
  );
}
