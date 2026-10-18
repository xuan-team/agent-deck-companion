import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import vm from 'node:vm';

import { apply } from '../lib/index.js';

function host() {
  const routes = new Map();
  const listeners = new Map();
  const ctx = {
    connection: { fetch: { register(route) { routes.set(route.path, route.fetch); } } },
    effect() {},
    on(event, handler) { listeners.set(event, handler); },
  };
  apply(ctx);
  return routes;
}

test('closed browser stream is removed from the tab count', async () => {
  const routes = host();
  const abort = new AbortController();
  const stream = routes.get('/api/deck/stream')(
    new Request('http://localhost/api/deck/stream', { signal: abort.signal }),
  );
  const reader = stream.body.getReader();
  await reader.read();
  assert.equal((await routes.get('/api/deck/tabs')()).status, 200);
  assert.equal((await (await routes.get('/api/deck/tabs')()).json()).tabs, 1);

  abort.abort();
  assert.equal((await (await routes.get('/api/deck/tabs')()).json()).tabs, 0);
  assert.equal((await reader.read()).done, true);
});

test('long poll delivers a session and waits for its acknowledgement', async () => {
  const routes = host();
  const clientId = 'a'.repeat(32);
  const nonce = 'abc123';
  const next = routes.get('/api/deck/next')(
    new Request(`http://localhost/api/deck/next?clientId=${clientId}`),
  );
  assert.equal((await (await routes.get('/api/deck/tabs')()).json()).tabs, 1);
  const selection = routes.get('/api/deck/select')(
    new Request('http://localhost/api/deck/select', {
      method: 'POST',
      body: JSON.stringify({ sessionId: 'session-one', nonce }),
    }),
  );
  assert.deepEqual((await (await next).json()).command, { sessionId: 'session-one', nonce });
  await routes.get('/api/deck/ack')(
    new Request('http://localhost/api/deck/ack', {
      method: 'POST', body: JSON.stringify({ nonce }),
    }),
  );
  assert.equal((await (await selection).json()).acked, true);
});

test('a second selection waits for the next poll and reports a client rejection', async () => {
  const routes = host();
  const clientId = 'b'.repeat(32);
  const firstPoll = routes.get('/api/deck/next')(
    new Request(`http://localhost/api/deck/next?clientId=${clientId}`),
  );
  const firstSelect = routes.get('/api/deck/select')(
    new Request('http://localhost/api/deck/select', {
      method: 'POST', body: JSON.stringify({ sessionId: 'session-one', nonce: 'abc' }),
    }),
  );
  await firstPoll;
  const secondSelect = routes.get('/api/deck/select')(
    new Request('http://localhost/api/deck/select', {
      method: 'POST', body: JSON.stringify({ sessionId: 'session-two', nonce: 'def' }),
    }),
  );
  await routes.get('/api/deck/ack')(
    new Request('http://localhost/api/deck/ack', {
      method: 'POST', body: JSON.stringify({ nonce: 'abc' }),
    }),
  );
  assert.equal((await (await firstSelect).json()).acked, true);
  const next = await routes.get('/api/deck/next')(
    new Request(`http://localhost/api/deck/next?clientId=${clientId}`),
  );
  assert.deepEqual((await next.json()).command, { sessionId: 'session-two', nonce: 'def' });
  await routes.get('/api/deck/nack')(
    new Request('http://localhost/api/deck/nack', {
      method: 'POST', body: JSON.stringify({ nonce: 'def', error: 'unknown session' }),
    }),
  );
  const rejected = await (await secondSelect).json();
  assert.equal(rejected.reason, 'client-rejected');
  assert.equal(rejected.detail, 'unknown session');
});

test('client acknowledges only a successful session open', async () => {
  const source = readFileSync(new URL('../lib/client.js', import.meta.url), 'utf8');
  const posts = [];
  const polls = [];
  let opened = [];
  let shouldThrow = false;
  let selectedPanel;
  let plugin;
  let dispose;
  const window = {
    __ModuleLoader__: { load(value) { plugin = value.factory(); } },
    AbortController,
    crypto: { getRandomValues(bytes) { bytes.fill(10); return bytes; } },
    location: { hash: '', pathname: '/', search: '' },
    history: { replaceState() {} },
    addEventListener() {},
    fetch(url, options) {
      if (url.startsWith('/api/deck/next?')) {
        return new Promise((resolve) => polls.push(resolve));
      }
      posts.push({ url, options });
      return Promise.resolve({ ok: true });
    },
    setTimeout,
    clearTimeout,
  };
  const errors = [];
  vm.runInNewContext(source, { window, console: { error(...args) { errors.push(args); } } });
  assert.deepEqual(Array.from(plugin.inject), ['sessions', 'layout']);
  plugin.apply({
    get(name) {
      if (name === 'uiWorkspace') return void 0;
      assert.equal(name, 'sessions');
      return { open(id) { if (shouldThrow) throw Error('failed'); opened.push(id); } };
    },
    layout: { selectPanel(value) { selectedPanel = value; } },
    effect(start) { dispose = start(); },
  });

  const command = { sessionId: 'session-one', nonce: 'abc' };
  polls.shift()({ ok: true, json: async () => ({ command }) });
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(opened, ['session-one']);
  assert.equal(selectedPanel, null);
  assert.equal(posts.length, 1);
  assert.equal(posts[0].url, '/api/deck/ack');

  shouldThrow = true;
  polls.shift()({ ok: true, json: async () => ({ command }) });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(posts.length, 2);
  assert.equal(posts[1].url, '/api/deck/nack');
  assert.match(posts[1].options.body, /failed/);
  assert.equal(errors.length, 1);
  dispose();
});

test('client prefers the 0.1.7 uiWorkspace navigation API', async () => {
  const source = readFileSync(new URL('../lib/client.js', import.meta.url), 'utf8');
  const posts = [];
  const polls = [];
  const opened = [];
  let plugin;
  let dispose;
  const window = {
    __ModuleLoader__: { load(value) { plugin = value.factory(); } },
    AbortController,
    crypto: { getRandomValues(bytes) { bytes.fill(10); return bytes; } },
    location: { hash: '', pathname: '/', search: '' },
    history: { replaceState() {} },
    addEventListener() {},
    fetch(url, options) {
      if (url.startsWith('/api/deck/next?')) return new Promise((resolve) => polls.push(resolve));
      posts.push({ url, options });
      return Promise.resolve({ ok: true });
    },
    setTimeout,
    clearTimeout,
  };
  vm.runInNewContext(source, { window, console: { error() {} } });
  plugin.apply({
    get(name) {
      if (name === 'sessions') return { list: {} };
      assert.equal(name, 'uiWorkspace');
      return { openSession(id) { opened.push(id); } };
    },
    layout: { selectPanel() {} },
    effect(start) { dispose = start(); },
  });

  polls.shift()({ ok: true, json: async () => ({ command: { sessionId: 'session-two', nonce: 'abc' } }) });
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(opened, ['session-two']);
  assert.equal(posts[0].url, '/api/deck/ack');
  dispose();
});

test('a session event pushes a look-now trigger to the companion', async () => {
  const home = await mkdtemp(join(tmpdir(), 'deck-bridge-'));
  await writeFile(join(home, 'hook-token'), 'test-token\n');
  process.env.AGENT_DECK_COMPANION_HOME = home;
  // The token path is fixed at module load, so this test needs its own copy.
  const module = await import(`../lib/index.js?push=${Date.now()}`);

  const listeners = new Map();
  const calls = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (url, request) => {
    calls.push({ url, request });
    return { ok: true, status: 200 };
  };
  try {
    module.apply({
      connection: { fetch: { register() {} } },
      effect() {},
      on(event, handler) { listeners.set(event, handler); },
    });

    listeners.get('api-session/status')('session-one');
    assert.equal(calls.length, 0, 'a burst is coalesced, not sent per event');
    await new Promise((resolve) => setTimeout(resolve, 700));

    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, 'http://127.0.0.1:34115/api/dsh/push');
    assert.equal(calls[0].request.headers['x-agent-deck-token'], 'test-token');
    const body = JSON.parse(calls[0].request.body);
    assert.equal(body.reason, 'status');
    assert.deepEqual(body.sessions, ['session-one']);

    // An unparsable session id is dropped instead of being forwarded.
    listeners.get('api-session/removed')('../../etc/passwd');
    await new Promise((resolve) => setTimeout(resolve, 700));
    assert.deepEqual(JSON.parse(calls[1].request.body).sessions, ['session-one']);
    assert.equal(JSON.parse(calls[1].request.body).reason, 'removed');
  } finally {
    globalThis.fetch = originalFetch;
    delete process.env.AGENT_DECK_COMPANION_HOME;
  }
});

test('a missing companion token never reaches the network', async () => {
  const home = await mkdtemp(join(tmpdir(), 'deck-bridge-'));
  process.env.AGENT_DECK_COMPANION_HOME = home;
  const module = await import(`../lib/index.js?notoken=${Date.now()}`);

  const listeners = new Map();
  let fetched = 0;
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => {
    fetched += 1;
    return { ok: true, status: 200 };
  };
  try {
    module.apply({
      connection: { fetch: { register() {} } },
      effect() {},
      on(event, handler) { listeners.set(event, handler); },
    });
    listeners.get('api-session/status')('session-one');
    await new Promise((resolve) => setTimeout(resolve, 700));
    assert.equal(fetched, 0);
  } finally {
    globalThis.fetch = originalFetch;
    delete process.env.AGENT_DECK_COMPANION_HOME;
  }
});
