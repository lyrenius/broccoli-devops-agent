/** The hash that opens an Issue's trace, on one pass when a Job is given. */
export function traceHash(issueId: string, jobId?: string): string {
  return jobId ? `#trace/${issueId}/${jobId}` : `#trace/${issueId}`;
}

/** Return from a trace to the last records search in this browser tab. */
export function recordsReturnHash(): string {
  try {
    const saved = sessionStorage.getItem("broccoli.records.location");
    if (saved && /^#records(?:\?|$)/.test(saved)) return saved;
  } catch { /* No browser storage. */ }
  return "#records";
}
