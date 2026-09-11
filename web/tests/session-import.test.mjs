import test from 'node:test';
import assert from 'node:assert/strict';
import { loadTs } from './load-ts.mjs';
const { api } = await loadTs('../src/api.ts');

test('session import preserves float notation and integers beyond JavaScript precision', async (t) => {
  const source = '{"artifacts":[{"body":{"encoding":"json","content":{"metric":0.0,"counter":9007199254740993}}}]}';
  assert.notEqual(JSON.stringify(JSON.parse(source)), source, 'browser reserialization corrupts hashed bodies');
  let sent;
  t.mock.method(globalThis, 'fetch', async (path, init) => {
    sent = { path, ...init };
    return { ok: true, json: async () => ({ jobs: 2 }) };
  });
  assert.deepEqual(await api.importSession(source, 'test operator'), { jobs: 2 });
  assert.equal(sent.path, '/api/sessions/import?by=test%20operator');
  assert.equal(sent.method, 'POST');
  assert.equal(sent.headers['Content-Type'], 'application/json');
  assert.equal(sent.body, source);
});
