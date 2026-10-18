import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as wait } from 'node:timers/promises';

test('v2 events preserve root identity, blockers, output time and cleanup', async () => {
  const home = await mkdtemp(join(tmpdir(), 'agent-deck-opencode-test-'));
  await mkdir(join(home, '.agent-deck-companion'));
  await writeFile(join(home, '.agent-deck-companion/hook-token'), 'test-only-token');
  const source = (await readFile(new URL('./index.js', import.meta.url), 'utf8'))
    .replace("import { homedir } from 'node:os';", `const homedir = () => ${JSON.stringify(home)};`);
  const plugin = (await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`)).default;
  const captured = [];
  const originalFetch = globalThis.fetch;
  let failed = false;
  globalThis.fetch = async (url, options) => {
    assert.equal(url, 'http://127.0.0.1:34115/api/hooks/opencode');
    assert.equal(options.headers['x-agent-deck-token'], 'test-only-token');
    assert.equal(options.headers['x-agent-deck-herdr-pane'], undefined);
    if (failed) throw new Error('Companion temporarily closed');
    captured.push({ payload: JSON.parse(options.body), received: Date.now() });
    return new Response('{}');
  };
  const events = [];
  let notify;
  let cleanup;
  const publish = (type, data, id = `evt_${events.length}_${Date.now()}`) => {
    events.push({ id, type, data, created: Date.now() }); notify?.();
  };
  try {
    cleanup = await plugin.setup({
      location: { directory: '/test/project' },
      session: { get: async ({ sessionID }) => ({ id: sessionID, title: 'Test session', location: { directory: sessionID === 'ses_foreign' ? '/other/project' : '/test/project' }, parentID: sessionID === 'ses_child' ? 'ses_root' : undefined }) },
      event: { async *subscribe({ signal }) {
        let index = 0;
        while (!signal.aborted) {
          if (index < events.length) yield events[index++];
          else await new Promise((resolve) => {
            const done = () => { signal.removeEventListener('abort', done); resolve(); };
            notify = done; signal.addEventListener('abort', done, { once: true });
          });
        }
      } },
    });
    publish('session.execution.started', { sessionID: 'ses_root' }, 'evt_start');
    publish('session.execution.started', { sessionID: 'ses_root' }, 'evt_start');
    publish('permission.asked', { sessionID: 'ses_child', id: 'per_test' });
    publish('permission.replied', { sessionID: 'ses_child', requestID: 'per_test', reply: 'once' });
    for (let i = 0; i < 100; i++) publish('session.text.delta', { sessionID: 'ses_root', delta: `private output ${i}` });
    publish('session.execution.succeeded', { sessionID: 'ses_child' });
    publish('session.execution.succeeded', { sessionID: 'ses_foreign' });
    publish('session.execution.succeeded', { sessionID: 'ses_root' });
    await wait(550);
    const payloads = captured.map(x => x.payload);
    assert.equal(payloads.filter(p => p.hook_event_name === 'UserPromptSubmit').length, 1);
    assert.equal(payloads.filter(p => p.hook_event_name === 'Output').length, 1);
    assert.equal(payloads.filter(p => p.hook_event_name === 'Stop').length, 1);
    const asked = payloads.find(p => p.hook_event_name === 'PermissionRequest');
    const replied = payloads.find(p => p.hook_event_name === 'PermissionResolved');
    assert.equal(asked.session_id, 'ses_root');
    assert.equal(asked.task_id, replied.task_id);
    assert.ok(!JSON.stringify(payloads).includes('private output'));
    assert.ok(captured.every(x => x.received - x.payload.event_at_ms < 1500));
    failed = true;
    publish('session.execution.failed', { sessionID: 'ses_root' });
    await wait(50);
    failed = false;
    await wait(850);
    assert.equal(captured.at(-1).payload.hook_event_name, 'StopFailure');
    cleanup();
    const count = captured.length;
    publish('session.execution.started', { sessionID: 'ses_root' });
    await wait(450);
    assert.equal(captured.length, count);
  } finally {
    cleanup?.(); globalThis.fetch = originalFetch; await rm(home, { recursive: true });
  }
});
