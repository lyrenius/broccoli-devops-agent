/** The hash that opens an Issue's trace, on one pass when a Job is given. */
export function traceHash(issueId: string, jobId?: string): string {
  return jobId ? `#trace/${issueId}/${jobId}` : `#trace/${issueId}`;
}
