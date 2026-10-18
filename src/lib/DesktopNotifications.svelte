<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import { locale } from '$lib/i18n';
  type View = { enabled: boolean; available: boolean; permission: string; last_error: string | null; last_submission_ms: number | null; last_click: string | null };
  let view = $state<View | null>(null);
  let error = $state('');
  let busy = $state(false);
  const zh = $derived($locale === 'zh');
  const permissionLabel = $derived(view?.permission === 'granted' ? (zh ? '已授权' : 'Allowed')
    : view?.permission === 'denied' ? (zh ? '已拒绝，请在 macOS 系统设置 → 通知中开启' : 'Denied. Enable in macOS System Settings → Notifications')
    : view?.available ? (zh ? '尚未授权' : 'Not requested') : (zh ? '请使用打包后的 macOS 应用' : 'Use the packaged macOS app'));
  const clickLabel = $derived(({ opened: zh ? '已打开会话' : 'Session opened', attention_list: zh ? '已显示关注列表' : 'Attention list opened',
    superseded: zh ? '提醒已过期，保留当前会话状态' : 'Event expired; current state preserved', expired: zh ? '提醒已过期，已显示关注列表' : 'Event expired; attention list opened',
    target_gone: zh ? '目标已消失，已显示关注列表' : 'Target gone; attention list opened', focus_failed: zh ? '跳转失败，请从关注列表重试' : 'Focus failed; retry from the list' } as Record<string,string>)[view?.last_click ?? ''] ?? '');
  async function refresh() {
    if (!('__TAURI_INTERNALS__' in window)) return;
    try { view = await invoke<View>('get_desktop_notifications'); } catch (reason) { error = String(reason); }
  }
  async function act(command: string, args?: Record<string, unknown>) {
    busy = true; error = '';
    try { await invoke(command, args); await refresh(); }
    catch (reason) { error = String(reason); }
    finally { busy = false; }
  }
  onMount(() => {
    let disposed = false;
    let stop = () => {};
    listen<View>('desktop_notification_status_changed', (event) => { if (!disposed) view = event.payload; })
      .then(unlisten => { if (disposed) unlisten(); else stop = unlisten; }).catch(() => {});
    void refresh();
    // Permission can change in System Settings while this page is open.
    const timer = setInterval(refresh, 5000);
    return () => { disposed = true; stop(); clearInterval(timer); };
  });
</script>

<section class="notifications">
  <div class="heading"><h2>{zh ? '桌面通知' : 'Desktop notifications'}</h2><span>{permissionLabel}</span></div>
  <p>{zh ? '浮窗隐藏时提醒任务完成、需要输入和错误；通知不抢焦点。未授权时仍保留关注列表与计数。' : 'Notify on completion, input requests and errors while the panel is hidden. Notifications preserve focus; sessions remain in the attention list.'}</p>
  <div class="controls">
    <label><input type="checkbox" checked={view?.enabled ?? false} disabled={!view || busy} onchange={(event) => act('set_desktop_notifications', { enabled: event.currentTarget.checked })}/>{zh ? '隐藏时使用系统通知' : 'System notifications while hidden'}</label>
    {#if view?.available && view.permission === 'not_requested'}
      <button disabled={busy} onclick={() => act('request_desktop_notification_permission')}>{zh ? '授权通知' : 'Allow notifications'}</button>
    {/if}
    <button disabled={busy || !view?.available || view.permission !== 'granted'} onclick={() => act('test_desktop_notification')}>{zh ? '发送测试通知' : 'Send test notification'}</button>
  </div>
  {#if view?.last_submission_ms}<small>{zh ? '最近提交到系统：' : 'Last submitted: '}{new Date(view.last_submission_ms).toLocaleTimeString()} · {zh ? '显示受系统通知设置及勿扰控制' : 'Presentation follows system settings and Focus'}</small>{/if}
  {#if clickLabel}<small role="status">{clickLabel}</small>{/if}
  {#if error || view?.last_error}<p class="error" role="alert">{error || view?.last_error}</p>{/if}
</section>

<style>
  .notifications { padding: var(--space-4); border: 1px solid var(--border); border-radius: var(--radius); background: var(--bg-panel); }
  .heading { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  h2 { font-size: var(--fs-md); font-weight: 650; margin: 0; }
  .heading span { font-size: var(--fs-xs); color: var(--status-waiting); font-weight: 600; }
  p { color: var(--text-mid); font-size: var(--fs-sm); line-height: 1.6; margin-top: 8px; }
  .controls { display: flex; gap: 12px; align-items: center; flex-wrap: wrap; margin-top: 14px; }
  label { display: flex; gap: 8px; align-items: center; font-size: var(--fs-sm); color: var(--text-mid); }
  label input { width: 15px; height: 15px; accent-color: var(--accent); }
  button { background: var(--bg-inset); color: var(--text); border: 1px solid var(--border-strong); border-radius: var(--radius-sm); padding: 7px 12px; font-size: var(--fs-sm); cursor: pointer; transition: .15s ease; }
  button:hover:not(:disabled) { border-color: var(--accent-dim); background: var(--bg-elevated); }
  button:disabled { opacity: .5; cursor: default; }
  small { display: block; margin-top: 10px; font-size: var(--fs-xs); color: var(--text-dim); }
  .error { color: var(--status-error); font-size: var(--fs-xs); margin-top: 10px; }
</style>
