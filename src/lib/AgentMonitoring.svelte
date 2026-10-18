<script lang="ts">
  import { onMount } from 'svelte';
  import { dshDiagnostics, setAgentEnabled } from '$lib/api';
  import { locale, t } from '$lib/i18n';
  import type { AppSnapshot, DshDiagnostics } from '$lib/types';

  let {
    snapshot = null,
    onsnapshot
  }: {
    snapshot?: AppSnapshot | null;
    onsnapshot?: (next: AppSnapshot) => void;
  } = $props();

  const zh = $derived($locale === 'zh');
  // The Companion key is the app itself and is never switchable.
  const agents = $derived((snapshot?.agents ?? []).filter((agent) => agent.id !== 'companion'));
  const dshAgent = $derived(agents.find((agent) => agent.id === 'dsh') ?? null);

  let busy = $state('');
  let error = $state('');
  let diagnostics = $state<DshDiagnostics | null>(null);
  let diagnosing = $state(false);

  async function toggle(agentId: string, enabled: boolean) {
    busy = agentId;
    error = '';
    try {
      onsnapshot?.(await setAgentEnabled(agentId, enabled));
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = '';
    }
  }

  async function diagnose() {
    if (!('__TAURI_INTERNALS__' in window)) return;
    diagnosing = true;
    error = '';
    try {
      diagnostics = await dshDiagnostics();
    } catch (reason) {
      error = String(reason);
    } finally {
      diagnosing = false;
    }
  }

  // The Settings tab is only mounted while it is open, so one probe per visit
  // is enough; the button re-runs it after the user starts DSH.
  onMount(() => void diagnose());

  function yesNo(value: boolean) {
    return value ? (zh ? '是' : 'yes') : (zh ? '否' : 'no');
  }

  function bridgeLabel(): string {
    if (!diagnostics) return zh ? '尚未检测' : 'Not checked';
    if (diagnostics.bridge_tabs === null) {
      return zh ? `不可用 · ${diagnostics.bridge_error ?? ''}` : `unavailable · ${diagnostics.bridge_error ?? ''}`;
    }
    const browsers = diagnostics.bridge_browsers.length
      ? ` (${diagnostics.bridge_browsers.join(', ')})`
      : '';
    if (diagnostics.bridge_tabs === 0) {
      return zh ? '已加载，但没有打开的 DSH 页面' : 'loaded, no DSH page open';
    }
    return zh
      ? `已连接 ${diagnostics.bridge_tabs} 个页面${browsers}`
      : `${diagnostics.bridge_tabs} page(s) connected${browsers}`;
  }

  /** Push channel state. Only ever non-zero after the user restarts DSH with
   *  the updated plugin, so "0" means "polling only", not an error. */
  function pushLabel(): string {
    if (!diagnostics) return zh ? '尚未检测' : 'Not checked';
    if (!diagnostics.pushes) {
      return zh ? '未收到推送（仍每 5 秒轮询）' : 'no pushes yet (still polling every 5s)';
    }
    const age = Math.max(0, Date.now() - diagnostics.last_push_at_ms);
    const ago = age < 1000 ? (zh ? '刚刚' : 'just now') : zh ? `${Math.round(age / 1000)} 秒前` : `${Math.round(age / 1000)}s ago`;
    return zh
      ? `已收到 ${diagnostics.pushes} 次推送 · 最近 ${ago}（轮询降为兜底）`
      : `${diagnostics.pushes} pushes · last ${ago} (poll is a fallback)`;
  }
</script>

<section class="monitoring">
  <div class="heading">
    <h2>{zh ? 'Agent 监控' : 'Agent monitoring'}</h2>
    <span>{agents.filter((agent) => agent.enabled).length}/{agents.length}</span>
  </div>
  <p>
    {zh
      ? '关闭某个 Agent 后不再检测它的进程与状态，其会话会从列表和键盘灯中移除；A1–A5 槽位绑定保留，重新打开即恢复。'
      : 'Turning an agent off stops its process detection and status polling, and removes its sessions from the lists and the deck lights. Its A1–A5 slot binding is kept and comes back when you re-enable it.'}
  </p>

  <ul class="agent-list">
    {#each agents as agent}
      <li class:off={!agent.enabled}>
        <label class="toggle-control">
          <input
            type="checkbox"
            checked={agent.enabled}
            disabled={busy === agent.id}
            aria-label={`${agent.display_name} ${zh ? '监控' : 'monitoring'}`}
            onchange={(event) => toggle(agent.id, event.currentTarget.checked)}
          />
          <span class="name">{agent.display_name}<code>{agent.id}</code></span>
        </label>
        <span class="state">
          <i class="status-dot {agent.running ? agent.status : 'offline'}"></i>
          {agent.enabled ? (t(`status_${agent.status}`) ?? '') : zh ? '已关闭' : 'off'}
          {#if agent.enabled && agent.running && agent.pid !== null}
            <small>pid {agent.pid}</small>
          {/if}
        </span>
      </li>
    {/each}
  </ul>

  {#if dshAgent?.enabled}
    <div class="diagnostics">
      <div class="diag-head">
        <h3>{zh ? 'DSH 集成' : 'DSH integration'}</h3>
        <button class="secondary" disabled={diagnosing} onclick={diagnose}>
          {diagnosing ? t('processing') : zh ? '重新检测' : 'Check again'}
        </button>
      </div>
      {#if diagnostics}
        <dl>
          <div><dt>{zh ? '进程' : 'Process'}</dt><dd>{yesNo(diagnostics.process_running)}</dd></div>
          <div><dt>{zh ? 'Web 地址' : 'Web URL'}</dt><dd>{diagnostics.web_url ?? (zh ? 'dsh web 未运行' : 'dsh web is not running')}</dd></div>
          <div><dt>{zh ? '本地会话' : 'Stored sessions'}</dt><dd>{diagnostics.storage_sessions}</dd></div>
          <div><dt>{zh ? 'Connection RPC' : 'Connection RPC'}</dt><dd>{diagnostics.api_error ? (zh ? `失败 · ${diagnostics.api_error}` : `failed · ${diagnostics.api_error}`) : `${diagnostics.api_sessions.length} ${zh ? '个会话' : 'session(s)'}`}</dd></div>
          <div><dt>{zh ? 'Deck 桥接' : 'Deck bridge'}</dt><dd>{bridgeLabel()}</dd></div>
          <div><dt>{zh ? '插件推送' : 'Plugin push'}</dt><dd>{pushLabel()}</dd></div>
          <div><dt>{zh ? '凭据' : 'Credentials'}</dt><dd>{diagnostics.credentials ? '~/.credentials.yaml' : zh ? '缺失 · ~/.credentials.yaml' : 'missing · ~/.credentials.yaml'}</dd></div>
          <div><dt>{zh ? 'DSH 目录' : 'DSH home'}</dt><dd class="wrap">{diagnostics.home}</dd></div>
        </dl>
        <p class="hint">
          {zh
            ? 'K1–K3 切换会话需要先在 DSH 的 web profile 里安装 dsh-deck-bridge 并重启 DSH，然后刷新浏览器页面；状态与进程检测不依赖桥接。'
            : 'Switching sessions with K1–K3 needs the dsh-deck-bridge plugin installed in the DSH web profile, a DSH restart and a browser refresh. Status and process detection work without it.'}
        </p>
      {:else}
        <p class="hint">{zh ? '仅在打包的桌面应用中可用。' : 'Only available in the packaged desktop app.'}</p>
      {/if}
    </div>
  {/if}

  {#if error}<p class="error" role="alert">{error}</p>{/if}
</section>

<style>
  .monitoring { padding: var(--space-4); border: 1px solid var(--border); border-radius: var(--radius); background: var(--bg-panel); }
  .heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  h2 { font-size: var(--fs-md); font-weight: 650; margin: 0; }
  .heading span { color: var(--text-dim); font: 700 var(--fs-2xs) var(--font-mono); }
  p { color: var(--text-mid); font-size: var(--fs-sm); line-height: 1.65; margin-top: 8px; }
  .agent-list { list-style: none; margin: 14px 0 0; padding: 0; display: flex; flex-direction: column; gap: 6px; }
  li { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 8px 12px; border: 1px solid var(--border); border-radius: var(--radius-sm); background: var(--bg-inset); }
  li.off { opacity: .55; }
  .toggle-control { display: flex; align-items: center; gap: 9px; min-width: 0; }
  .toggle-control input { width: 15px; height: 15px; margin: 0; padding: 0; accent-color: var(--accent); }
  .name { display: flex; align-items: baseline; gap: 7px; font-size: var(--fs-sm); color: var(--text); }
  .name code { color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); }
  .state { display: flex; align-items: center; gap: 6px; flex: 0 0 auto; color: var(--text-mid); font-size: var(--fs-xs); }
  .state small { color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); }
  .status-dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; background: var(--text-dim); }
  .status-dot.idle, .status-dot.ready { background: var(--accent-glow); box-shadow: 0 0 8px var(--accent-glow); }
  .status-dot.working { background: var(--status-working); box-shadow: 0 0 8px var(--status-working); }
  .status-dot.done { background: var(--status-unread); box-shadow: 0 0 8px var(--status-unread); }
  .status-dot.needs_input, .status-dot.needsinput { background: var(--status-waiting); box-shadow: 0 0 8px var(--status-waiting); }
  .status-dot.error { background: var(--status-error); box-shadow: 0 0 8px var(--status-error); }
  .diagnostics { margin-top: 14px; padding-top: 12px; border-top: 1px solid var(--border); }
  .diag-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  h3 { margin: 0; font-size: var(--fs-sm); font-weight: 650; }
  button { background: var(--bg-inset); color: var(--text); border: 1px solid var(--border-strong); border-radius: var(--radius-sm); padding: 6px 11px; font-size: var(--fs-xs); cursor: pointer; transition: .15s ease; }
  button:hover:not(:disabled) { border-color: var(--accent-dim); background: var(--bg-elevated); }
  button:disabled { opacity: .5; cursor: default; }
  dl { display: flex; flex-direction: column; gap: 4px; margin: 10px 0 0; }
  dl > div { display: grid; grid-template-columns: 120px 1fr; gap: 10px; align-items: baseline; }
  dt { color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); }
  dd { margin: 0; color: var(--text-mid); font-size: var(--fs-xs); overflow-wrap: anywhere; }
  dd.wrap { font: var(--fs-2xs) var(--font-mono); }
  .hint { color: var(--text-dim); font-size: var(--fs-xs); }
  .error { color: var(--status-error); font-size: var(--fs-xs); margin-top: 10px; }
</style>
