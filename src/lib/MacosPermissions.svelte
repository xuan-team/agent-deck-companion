<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { locale } from './i18n';
  type Kind = 'accessibility' | 'input_monitoring';
  type Permissions = { supported: boolean; accessibility: boolean; input_monitoring: boolean };

  const kinds: Kind[] = ['accessibility', 'input_monitoring'];
  let status: Permissions | null = null;
  let error = '';
  let hint = '';
  let busy = false;
  let polling = false;
  let disposed = false;
  $: zh = $locale === 'zh';
  $: grantedCount = status ? kinds.filter((kind) => status![kind]).length : 0;
  $: allGranted = grantedCount === kinds.length;
  $: names = {
    accessibility: zh ? '辅助功能 · 发送快捷键' : 'Accessibility · send shortcuts',
    input_monitoring: zh ? '输入监控 · 全局硬件按键' : 'Input Monitoring · global hardware keys'
  } as Record<Kind, string>;
  async function refresh() {
    try { status = await invoke<Permissions>('get_macos_permissions'); }
    catch (e) { error = String(e); }
  }
  async function pollUntilGranted(kind: Kind) {
    polling = true;
    try {
      // TCC shows prompts asynchronously, and granting Input Monitoring may
      // only take effect after the system dialog closes.
      for (let attempt = 0; attempt < 40 && !disposed; attempt++) {
        await new Promise((resolve) => setTimeout(resolve, 500));
        await refresh();
        if (status?.[kind]) { hint = ''; return; }
      }
      if (!disposed) {
        hint = kind === 'input_monitoring'
          ? (zh
            ? '如果系统没有弹出授权提示，请点「打开设置」，在「输入监控」中启用 Agent Deck Companion；如果列表里没有，点 + 添加 /Applications 里的应用。授权后若仍未生效，请重新启动应用。'
            : 'If macOS did not show a prompt, open Settings and enable Agent Deck Companion under Input Monitoring; if it is not listed, use + to add it from /Applications. Restart the app if it still does not take effect.')
          : (zh
            ? '如果系统没有弹出授权提示，请点「打开设置」，在「辅助功能」中手动启用 Agent Deck Companion。'
            : 'If macOS did not show a prompt, open Settings and enable Agent Deck Companion under Accessibility.');
      }
    } finally { polling = false; }
  }
  async function request(kind: Kind, openSettings: boolean) {
    busy = true; error = ''; hint = '';
    try {
      status = await invoke<Permissions>('request_macos_permission', { kind, openSettings });
      if (!openSettings && !status![kind]) void pollUntilGranted(kind);
    }
    catch (e) { error = String(e); }
    finally { busy = false; }
  }
  onMount(() => {
    if (!('__TAURI_INTERNALS__' in window)) return;
    void refresh();
    window.addEventListener('focus', refresh);
    return () => { disposed = true; window.removeEventListener('focus', refresh); };
  });
</script>

{#if status?.supported}
  <section class="privacy" aria-label={zh ? 'macOS 权限' : 'macOS permissions'}>
    <div class="privacy-head">
      <div class="privacy-title"><h2>{zh ? 'macOS 权限' : 'macOS permissions'}</h2></div>
      <span class="overall" class:granted={allGranted}>
        {allGranted ? (zh ? '已全部授权' : 'All granted') : `${grantedCount}/${kinds.length} ${zh ? '已授权' : 'granted'}`}
      </span>
    </div>
    <p>{zh ? '按需授权：基础状态查看不需要这些权限；只有发送快捷键、部分窗口定位或硬件输入才会用到。' : 'Grant access as needed for shortcuts, some window locators, or hardware input. Basic status viewing needs neither permission.'}</p>
    <div class="perm-list">
      {#each kinds as kind}
        <div class="permission">
          <span class="perm-name">{names[kind]}</span>
          <span class="perm-state" class:granted={status[kind]}>{status[kind] ? (zh ? '已授权' : 'Granted') : (zh ? '未授权' : 'Not granted')}</span>
          <div class="perm-actions">
            {#if !status[kind]}<button disabled={busy || polling} onclick={() => request(kind, false)}>{zh ? '申请权限' : 'Request access'}</button>{/if}
            <button disabled={busy} onclick={() => request(kind, true)}>{zh ? '打开设置' : 'Open settings'}</button>
          </div>
        </div>
      {/each}
    </div>
    {#if !allGranted}
      <small>{zh ? '在系统隐私设置中启用 Agent Deck Companion。若授权输入监控，请退出并重新启动应用；回到此窗口可刷新权限状态。' : 'Enable Agent Deck Companion in Privacy settings. Quit and reopen after granting Input Monitoring. Return to this window to refresh access status.'}</small>
    {/if}
    {#if hint}<p class="hint" role="status">{hint}</p>{/if}
    {#if error}<p class="error" role="alert">{error}</p>{/if}
  </section>
{/if}

<style>
  .privacy { margin: 0; padding: var(--space-4); border: 1px solid var(--border); border-radius: var(--radius); background: var(--bg-panel); }
  .privacy-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .privacy-title { display: flex; align-items: center; gap: 8px; }
  .privacy-title::before { content: ''; width: 7px; height: 7px; border-radius: 50%; background: var(--accent); box-shadow: 0 0 7px var(--accent); }
  h2 { margin: 0; font-size: var(--fs-md); font-weight: 650; }
  .overall { font-size: var(--fs-xs); font-weight: 600; color: var(--status-waiting); }
  .overall.granted { color: var(--accent); }
  p { color: var(--text-mid); font-size: var(--fs-xs); line-height: 1.6; margin-top: 8px; }
  .perm-list { margin-top: 12px; border-top: 1px solid var(--border); }
  .permission { display: flex; align-items: center; gap: 12px; padding: 10px 0; border-bottom: 1px solid var(--border); }
  .permission:last-child { border-bottom: 0; padding-bottom: 2px; }
  .perm-name { flex: 1; min-width: 0; font-size: var(--fs-sm); color: var(--text); }
  .perm-state { font-size: var(--fs-2xs); text-transform: uppercase; letter-spacing: .05em; color: var(--text-dim); }
  .perm-state.granted { color: var(--accent); }
  .perm-actions { display: flex; gap: 6px; flex-shrink: 0; }
  button {
    color: var(--text); background: var(--bg-inset); border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm); padding: 6px 10px; font-size: var(--fs-xs); cursor: pointer; transition: .15s ease;
  }
  button:hover:not(:disabled) { border-color: var(--accent-dim); background: var(--bg-elevated); }
  button:disabled { opacity: .45; cursor: default; }
  small { display: block; margin-top: 10px; font-size: var(--fs-xs); line-height: 1.6; color: var(--text-dim); }
  .hint { margin: 8px 0 0; font-size: var(--fs-xs); line-height: 1.6; color: var(--status-waiting); }
  .error { margin: 8px 0 0; font-size: var(--fs-xs); color: var(--status-error); }
</style>
