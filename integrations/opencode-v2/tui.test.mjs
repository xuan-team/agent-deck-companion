import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as wait } from 'node:timers/promises';

test('TUI binds only its selected root pane, deduplicates cache ticks, and disposes', async () => {
  const home = await mkdtemp(join(tmpdir(), 'agent-deck-opencode-tui-test-'));
  await mkdir(join(home, '.agent-deck-companion'));
  await writeFile(join(home, '.agent-deck-companion/hook-token'), 'test-only-token');
  const source = (await readFile(new URL('./tui.js', import.meta.url), 'utf8'))
    .replace("import { homedir } from 'node:os';", `const homedir = () => ${JSON.stringify(home)};`);
  const plugin = (await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`)).default;
  const savedEnv = Object.fromEntries(['HERDR_ENV', 'HERDR_PANE_ID', 'HERDR_SOCKET_PATH'].map(k => [k, process.env[k]]));
  Object.assign(process.env, { HERDR_ENV: '1', HERDR_PANE_ID: 'test-pane', HERDR_SOCKET_PATH: '/tmp/test.sock' });
  const originalFetch = globalThis.fetch;
  const captured = [];
  let epoch = 101;
  globalThis.fetch = async (url, options) => {
    if (url.endsWith('/api/instance')) return new Response(JSON.stringify(epoch));
    assert.equal(options.headers['x-agent-deck-herdr-pane'], 'test-pane');
    captured.push(JSON.parse(options.body));
    return new Response('{}');
  };
  let cleanup;
  let id = 'ses_first';
  let pending = [];
  try {
    cleanup = plugin.setup({
      ui: { router: { current: () => ({ type: 'session', sessionID: id }) } },
      data: { session: {
        get: id => ({ id, title: id, parentID: id === 'ses_child' ? 'ses_first' : undefined }),
        family: id => [id], status: () => 'idle',
        permission: { list: () => pending }, form: { list: () => [] },
      } },
    });
    await wait(600);
    assert.ok(captured.length <= 2);
    assert.equal(captured[0].hook_event_name, 'SessionBind');
    assert.equal(captured[0].native_status, 'idle');
    pending = [{ id: 'per_test' }];
    await wait(300);
    assert.equal(captured.at(-1).native_status, 'needs_input');
    id = 'ses_child'; await wait(300);
    const beforeChild = captured.length;
    await wait(300);
    assert.equal(captured.length, beforeChild);
    id = 'ses_second'; await wait(300);
    assert.equal(captured.at(-1).session_id, 'ses_second');
    const beforeRestart = captured.length; epoch = 102;
    await wait(1300);
    assert.equal(captured.length, beforeRestart + 1);
    cleanup(); const count = captured.length;
    id = 'ses_third'; await wait(300);
    assert.equal(captured.length, count);
    process.env.HERDR_ENV = '0';
    assert.equal(plugin.setup({}), undefined);
  } finally {
    cleanup?.(); globalThis.fetch = originalFetch;
    for (const [k, v] of Object.entries(savedEnv)) { if (v === undefined) delete process.env[k]; else process.env[k] = v; }
    await rm(home, { recursive: true });
  }
});
