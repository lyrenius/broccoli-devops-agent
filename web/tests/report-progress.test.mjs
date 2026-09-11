import test from 'node:test';
import assert from 'node:assert/strict';
import { loadTs } from './load-ts.mjs';
const { ReportProgressScope } = await loadTs('../src/lib/report-progress.ts');

const event = (sequence, kind, fields = {}) => ({ sequence, event_id: `event-${sequence}`, kind, issue_id: null, job_id: null, payload: {}, ...fields });
test('interleaved reports bind only their own admission and progress, including replay', () => {
  const a = new ReportProgressScope('report-a'), b = new ReportProgressScope('report-b');
  const events = [
    event(1, 'team.callback', { issue_id: 'unrelated', job_id: 'other' }),
    event(2, 'human.issue_reported', { payload: { report_id: 'report-a' } }),
    event(3, 'human.issue_reported', { payload: { report_id: 'report-b' } }),
    event(4, 'scheduler.issue_created', { issue_id: 'issue-b', payload: { source_event_id: 'event-3' } }),
    event(5, 'scheduler.issue_created', { issue_id: 'issue-a', payload: { source_event_id: 'event-2' } }),
    event(6, 'model.usage'), // Background snapshot review has no Job binding.
    event(7, 'team.callback', { issue_id: 'issue-a', job_id: 'job-a' }),
    event(8, 'team.callback', { issue_id: 'issue-b', job_id: 'job-b' }),
    event(9, 'model.usage', { issue_id: 'issue-a', job_id: 'job-a' }),
    event(10, 'scheduler.job_cancelled', { issue_id: 'issue-b', job_id: 'job-b' }),
  ];
  assert.deepEqual(events.filter(e => a.accept(e)).map(e => e.sequence), [5, 7, 9]);
  assert.deepEqual(events.filter(e => b.accept(e)).map(e => e.sequence), [4, 8, 10]);
  assert.equal(a.issueId, 'issue-a'); assert.equal(a.jobId, 'job-a');
  assert.equal(b.issueId, 'issue-b'); assert.equal(b.jobId, 'job-b');
  assert.equal(events.filter(e => a.accept(e)).length, 0, 'native SSE replay must not duplicate progress');
});

test('unbound progress is never attributed to a report', () => {
  const scope = new ReportProgressScope('not-admitted-yet');
  assert.equal(scope.accept(event(1, 'team.callback', { issue_id: 'some-issue', job_id: 'some-job' })), false);
  assert.equal(scope.issueId, null);
});
