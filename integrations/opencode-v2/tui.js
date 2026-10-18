// agent-deck-companion-opencode-v2 -- managed by Agent Deck Companion.
import { readFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join } from 'node:path';

// The server is shared, while this plugin runs in the actual TUI process.
// Reading the router/data cache never reads or refreshes a terminal viewport.
export default {
  id: 'agent-deck-companion',
  setup(ctx) {
    const pane = process.env.HERDR_PANE_ID;
    const socket = process.env.HERDR_SOCKET_PATH;
    if (process.env.HERDR_ENV !== '1' || !pane || !socket) return;
    let last;
    let pending = false;
    let epochPending = false;
    let epoch;
    const lifetime = new AbortController();
    async function sync() {
      if (pending || lifetime.signal.aborted) return;
      const route = ctx.ui.router.current();
      if (route.type !== 'session') { last = undefined; return; }
      const info = ctx.data.session.get(route.sessionID);
      if (!info || info.parentID) return;
      const family = ctx.data.session.family(info.id);
      const blocked = family.some(id => ctx.data.session.permission.list(id)?.length
        || ctx.data.session.form.list(id)?.length);
      const working = ctx.data.session.status(info.id) === 'running';
      const status = blocked ? 'needs_input' : working ? 'working' : info.outcome === 'failed' ? 'error' : 'idle';
      const signature = JSON.stringify([info.id, info.title, status]);
      if (last === signature) return;
      pending = true;
      const controller = new AbortController();
      const abort = () => controller.abort();
      lifetime.signal.addEventListener('abort', abort, { once: true });
      const timeout = setTimeout(abort, 700);
      try {
        const token = (await readFile(join(homedir(), '.agent-deck-companion/hook-token'), 'utf8')).trim();
        const response = await fetch('http://127.0.0.1:34115/api/hooks/opencode', {
          method: 'POST', signal: controller.signal,
          headers: { 'content-type': 'application/json', 'x-agent-deck-token': token,
            'x-agent-deck-herdr-pane': pane, 'x-agent-deck-herdr-socket': socket },
          body: JSON.stringify({ opencode_v2: true, hook_event_name: 'SessionBind',
            session_id: info.id, session_title: info.title, native_status: status,
            cwd: info.location?.directory, event_id: `tui:${info.id}:${Date.now()}` }),
        });
        if (response.ok) last = signature;
        await response.body?.cancel();
      } catch { /* Reattempt on next cache tick, without blocking the TUI. */ }
      finally { clearTimeout(timeout); lifetime.signal.removeEventListener('abort', abort); pending = false; }
    }
    void sync().catch(() => {});
    const timer = setInterval(() => { void sync().catch(() => {}); }, 250);
    async function checkInstance() {
      if (epochPending || lifetime.signal.aborted) return;
      epochPending = true;
      const controller = new AbortController();
      const abort = () => controller.abort();
      lifetime.signal.addEventListener('abort', abort, { once: true });
      const timeout = setTimeout(abort, 700);
      try {
        const response = await fetch('http://127.0.0.1:34115/api/instance', { signal: controller.signal });
        if (!response.ok) { await response.body?.cancel(); return; }
        const next = await response.json();
        if (typeof next === 'number' && next !== epoch) { epoch = next; last = undefined; }
      } catch { /* Companion is shut; retry without sending cached events. */ }
      finally { clearTimeout(timeout); lifetime.signal.removeEventListener('abort', abort); epochPending = false; }
    }
    void checkInstance();
    const epochTimer = setInterval(() => { void checkInstance(); }, 1000);
    return () => { lifetime.abort(); clearInterval(timer); clearInterval(epochTimer); };
  },
};
