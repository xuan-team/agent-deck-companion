// agent-deck-companion-opencode-v2 -- managed by Agent Deck Companion.
// OpenCode 2.0.26: Plugin.define is an identity function; a plain definition
// avoids installing a second SDK into the user's config directory.
import { readFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join } from 'node:path';

export default {
  id: 'agent-deck-companion',
  async setup(ctx) {
    const lifetime = new AbortController();
    const sessions = new Map();
    const queue = [];
    const output = new Map();
    const seen = new Set();
    const published = new Set();
    let draining = false;
    let retry;
    const delay = (ms) => new Promise((resolve) => {
      const timer = setTimeout(done, ms);
      function done() { clearTimeout(timer); lifetime.signal.removeEventListener('abort', done); resolve(); }
      lifetime.signal.addEventListener('abort', done, { once: true });
    });
    async function drain() {
      if (draining || lifetime.signal.aborted) return;
      draining = true;
      try {
        const token = (await readFile(join(homedir(), '.agent-deck-companion/hook-token'), 'utf8')).trim();
        while (queue.length && !lifetime.signal.aborted) {
          const packet = queue[0];
          const controller = new AbortController();
          const abort = () => controller.abort();
          lifetime.signal.addEventListener('abort', abort, { once: true });
          const timeout = setTimeout(abort, 700);
          let ok = false;
          try {
            const response = await fetch('http://127.0.0.1:34115/api/hooks/opencode', {
              method: 'POST', signal: controller.signal,
              headers: { 'content-type': 'application/json', 'x-agent-deck-token': token },
              body: JSON.stringify(packet),
            });
            ok = response.ok;
            await response.body?.cancel();
          } finally { clearTimeout(timeout); lifetime.signal.removeEventListener('abort', abort); }
          if (!ok) break;
          const index = queue.indexOf(packet);
          if (index !== -1) queue.splice(index, 1);
        }
      } catch { /* Companion can be closed; never fail the agent's execution. */ }
      finally {
        draining = false;
        if (queue.length && !lifetime.signal.aborted) retry = setTimeout(drain, 750);
      }
    }
    function send(payload) {
      if (lifetime.signal.aborted) return;
      // Keep state transitions ordered. Bounded memory while Companion is shut.
      queue.push(payload);
      if (queue.length > 256) queue.shift();
      clearTimeout(retry);
      void drain();
    }
    async function session(id) {
      if (!sessions.has(id)) {
        const result = await ctx.session.get({ sessionID: id });
        const info = result.data ?? result;
        sessions.set(id, info);
        if (sessions.size > 128) sessions.delete(sessions.keys().next().value);
      }
      return sessions.get(id);
    }
    async function handle(event) {
      if (typeof event === 'string') event = JSON.parse(event);
      const data = event.data;
      if (!data || !event.id || seen.has(event.id)) return;
      const id = data.sessionID ?? data.form?.sessionID;
      if (!id) return;
      const type = event.type;
      let name;
      if (type === 'session.execution.started') name = 'UserPromptSubmit';
      else if (type === 'session.execution.succeeded') name = 'Stop';
      else if (type === 'session.execution.failed') name = 'StopFailure';
      else if (type === 'session.execution.interrupted') name = 'Interrupted';
      else if (type === 'permission.asked' || type === 'form.created') name = 'PermissionRequest';
      else if (type === 'permission.replied' || type === 'form.replied' || type === 'form.cancelled') name = 'PermissionResolved';
      else if (type === 'session.text.delta' && data.delta?.trim()) name = 'Output';
      else if (type === 'session.created') name = 'SessionStart';
      else if (type === 'session.renamed' || type === 'session.moved') name = 'Metadata';
      else if (type === 'session.deleted') name = 'SessionEnd';
      else return;
      seen.add(event.id);
      if (seen.size > 2048) seen.delete(seen.values().next().value);
      if (type === 'session.renamed' || type === 'session.moved') sessions.delete(id);
      const info = type === 'session.deleted' ? sessions.get(id) ?? { id, location: event.location } : await session(id);
      if (!info) return;
      // A shared server hosts many sessions. No inherited HERDR_PANE_ID or PID
      // can identify their terminal. Rust resolves only a discovered native ID.
      let root = info;
      for (let depth = 0; root.parentID && depth < 16; depth++) root = await session(root.parentID);
      if (root.parentID) return;
      // Each loaded location owns one plugin instance, but the public stream
      // includes other locations too. Forward a root only from its owner.
      if (root.location?.directory && root.location.directory !== ctx.location.directory) return;
      // Child lifecycle is not a new root completion. Child blockers propagate.
      if (root.id !== id && !['PermissionRequest', 'PermissionResolved'].includes(name)) return;
      const payload = {
        hook_event_name: name, session_id: root.id, session_title: root.title,
        cwd: root.location?.directory ?? ctx.location.directory, event_id: event.id, event_at_ms: event.created,
        task_id: ['PermissionRequest', 'PermissionResolved'].includes(name) ? `${id}:${data.requestID ?? data.id ?? data.form?.id}` : undefined, opencode_v2: true,
      };
      if (name === 'Output') {
        if (!published.has(root.id)) send({ ...payload, hook_event_name: 'PreToolUse', event_id: `${event.id}:discovery` });
        output.set(root.id, payload);
      }
      else send(payload);
      published.add(root.id);
      if (published.size > 128) published.delete(published.values().next().value);
      if (type === 'session.deleted') sessions.delete(id);
    }
    const outputTimer = setInterval(() => {
      for (const payload of output.values()) send(payload);
      output.clear();
    }, 400);
    // The stream is read-only, and cleanup/reconnect cannot delay the agent.
    void (async () => {
      while (!lifetime.signal.aborted) {
        try {
          for await (const event of ctx.event.subscribe({ signal: lifetime.signal })) {
            if (lifetime.signal.aborted) break;
            try { await handle(event); } catch { /* Deleted session/racing shutdown. */ }
          }
        } catch { /* Reconnect public event subscription. */ }
        if (!lifetime.signal.aborted) await delay(750);
      }
    })();
    return () => {
      lifetime.abort(); clearInterval(outputTimer); clearTimeout(retry);
      output.clear(); queue.length = 0;
    };
  },
};
