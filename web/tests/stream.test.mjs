import test from 'node:test';
import assert from 'node:assert/strict';
import { loadTs } from './load-ts.mjs';

const { streamEvents } = await loadTs('../src/api.ts');
const { mergeEvents, mergeSteps } = await loadTs('../src/lib/trace-stream.ts');

test('stream reports loss and restoration; disposal stops late updates', () => {
  const original = globalThis.EventSource;
  let source;
  globalThis.EventSource = class extends EventTarget {
    constructor(url) { super(); this.url = url; source = this; }
    close() { this.closed = true; }
    log(value) { this.dispatchEvent(Object.assign(new Event('log'), { data: JSON.stringify(value) })); }
  };
  try {
    const connections = [], received = [];
    const stop = streamEvents(42, event => received.push(event), value => connections.push(value));
    assert.equal(source.url, '/api/events/stream?after=42');
    source.dispatchEvent(new Event('open'));
    source.log({ sequence: 43 });
    source.dispatchEvent(new Event('error'));
    source.dispatchEvent(new Event('open'));
    source.log({ sequence: 44 });
    assert.deepEqual(connections, [true, false, true]);
    assert.deepEqual(received.map(event => event.sequence), [43, 44]);
    stop(); source.log({ sequence: 45 }); source.dispatchEvent(new Event('error'));
    assert.equal(source.closed, true);
    assert.equal(received.length, 2);
    assert.deepEqual(connections, [true, false, true]);
  } finally { globalThis.EventSource = original; }
});

test('replayed events and steps merge without duplicates or ordering gaps', () => {
  const events = mergeEvents([{ sequence: 12 }, { sequence: 10 }], [{ sequence: 11 }, { sequence: 12 }, { sequence: 13 }]);
  assert.deepEqual(events.map(event => event.sequence), [10, 11, 12, 13]);
  const steps = mergeSteps([{ index: 2 }, { index: 0 }], [{ index: 1 }, { index: 2 }, { index: 3 }]);
  assert.deepEqual(steps.map(step => step.index), [0, 1, 2, 3]);
});

test('permanently closed HTTP stream reconnects from last received sequence', async () => {
  const original = globalThis.EventSource;
  const sources = [];
  globalThis.EventSource = class extends EventTarget {
    static CLOSED = 2;
    constructor(url) { super(); this.url = url; this.readyState = 0; sources.push(this); }
    close() { this.readyState = 2; }
  };
  let stop;
  try {
    const connections = [];
    stop = streamEvents(10, () => {}, value => connections.push(value));
    sources[0].dispatchEvent(Object.assign(new Event('log'), { data: JSON.stringify({ sequence: 15 }) }));
    sources[0].readyState = 2;
    sources[0].dispatchEvent(new Event('error'));
    await new Promise(resolve => setTimeout(resolve, 1100));
    assert.equal(sources.length, 2);
    assert.equal(sources[1].url, '/api/events/stream?after=15');
    sources[1].dispatchEvent(new Event('open'));
    assert.deepEqual(connections, [false, true]);
    stop();
    sources[1].dispatchEvent(new Event('error'));
    assert.deepEqual(connections, [false, true]);
  } finally { stop?.(); globalThis.EventSource = original; }
});
