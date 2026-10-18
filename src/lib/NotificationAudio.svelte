<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from '@tauri-apps/api/core';
  import { locale } from './i18n';

  type AudioView = {
    status: string; detail: string; progress_percent: number;
    clips: { slot: number; name: string; duration_ms: number; bytes: number; synced: boolean }[];
  };
  let view: AudioView | null = null;
  let error = '';
  let busy = false;
  $: zh = $locale === 'zh';
  $: labels = zh ? ['任务完成', '需要输入', '发生错误'] : ['Completion', 'Needs input', 'Error'];
  $: statusLabel = ({ disconnected: zh ? '未连接' : 'Disconnected', syncing: zh ? '正在同步' : 'Syncing',
    ready: zh ? '已就绪' : 'Ready', unsupported: zh ? '需要更新键盘固件' : 'Keyboard firmware update required',
    error: zh ? '同步失败' : 'Sync failed' } as Record<string, string>)[view?.status ?? 'disconnected'];

  async function refresh() {
    if (!('__TAURI_INTERNALS__' in window)) return;
    try { view = await invoke<AudioView>('get_notification_audio'); }
    catch (reason) { error = String(reason); }
  }
  async function change(slot: number, event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    busy = true; error = '';
    try {
      if (file.size > 4_000_000) throw new Error(zh ? '音频文件过大，请选择不超过 3 秒的 WAV' : 'Choose a WAV up to 3 seconds');
      const wav = Array.from(new Uint8Array(await file.arrayBuffer()));
      view = await invoke<AudioView>('set_notification_audio', { slot, name: file.name, wav });
    } catch (reason) { error = String(reason); }
    finally { busy = false; input.value = ''; }
  }
  async function reset(slot: number) {
    busy = true; error = '';
    try { view = await invoke<AudioView>('set_notification_audio', { slot, name: '', wav: null }); }
    catch (reason) { error = String(reason); }
    finally { busy = false; }
  }
  async function preview(slot: number) {
    error = '';
    try { await invoke('play_notification_audio', { slot }); }
    catch (reason) { error = String(reason); }
  }
  async function retry() {
    error = '';
    try { await invoke('retry_notification_audio'); await refresh(); }
    catch (reason) { error = String(reason); }
  }
  onMount(() => {
    refresh();
    const timer = window.setInterval(refresh, 1000);
    return () => window.clearInterval(timer);
  });
</script>

<section class="audio-settings">
  <div class="heading">
    <div><h2>{zh ? '键盘通知音' : 'Keyboard notification sounds'}</h2>
      <p>{zh ? '可选：用最长 3 秒的 PCM 16-bit WAV 替换键盘提示音。试听直接在键盘扬声器播放；替换的音频尚未同步时，键盘先播放自身内置提示音。' : 'Optional: replace keyboard sounds with PCM 16-bit WAV clips up to 3 seconds. Previews play on the keyboard speaker; until a replacement is cached, the keyboard plays its built-in sound.'}</p></div>
    <span class:ready={view?.status === 'ready'}>{statusLabel}</span>
  </div>
  {#if view}
    <div class="clips">
      {#each view.clips as clip}
        <article>
          <div class="title"><strong>{labels[clip.slot]}</strong><span>{clip.synced ? (zh ? '已缓存' : 'Cached') : (zh ? '待同步' : 'Pending')}</span></div>
          <p class="filename" title={clip.name}>{clip.name}</p>
          <small>{(clip.duration_ms / 1000).toFixed(2)} s · {(clip.bytes / 1000).toFixed(1)} KB</small>
          <div class="actions">
            <label class:disabled={busy}>{zh ? '更换 WAV' : 'Choose WAV'}<input type="file" accept=".wav,audio/wav,audio/x-wav" disabled={busy} onchange={(event) => change(clip.slot, event)} /></label>
            <button disabled={busy || view.status === 'disconnected'} onclick={() => preview(clip.slot)}>{zh ? '设备试听' : 'Play on keyboard'}</button>
            <button disabled={busy} onclick={() => reset(clip.slot)}>{zh ? '恢复默认' : 'Restore default'}</button>
          </div>
        </article>
      {/each}
    </div>
    {#if view.status === 'syncing'}<progress max="100" value={view.progress_percent}></progress>{/if}
    <div class="detail"><p>{view.detail}</p><button disabled={busy || view.status === 'disconnected' || view.status === 'syncing'} onclick={retry}>{zh ? '重新同步' : 'Retry sync'}</button></div>
    <p class="note">{zh ? '声音保存在电脑上；键盘重启后自动重新缓存。未缓存时使用设备原有提示音。' : 'Clips are saved on this computer and cached again after a keyboard restart. The keyboard uses its existing sounds until caching completes.'}</p>
  {:else}
    <p>{zh ? '请在桌面应用中配置键盘通知音。' : 'Configure keyboard sounds in the desktop app.'}</p>
  {/if}
  {#if error}<p class="error" role="alert">{error}</p>{/if}
</section>

<style>
  .audio-settings { padding: var(--space-4); border: 1px solid var(--border); border-radius: var(--radius); background: var(--bg-panel); color: var(--text); }
  .heading, .title, .detail { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
  .heading { align-items: flex-start; }
  h2 { font-size: var(--fs-md); font-weight: 650; margin: 0; }
  p { font-size: var(--fs-sm); color: var(--text-mid); line-height: 1.6; margin-top: 6px; }
  .heading > span, .title > span { font-size: var(--fs-xs); font-weight: 600; color: var(--status-waiting); white-space: nowrap; }
  .heading > span.ready { color: var(--accent); }
  .clips { display: grid; grid-template-columns: repeat(3, 1fr); gap: 12px; margin-top: 14px; }
  article { background: var(--bg-inset); border: 1px solid var(--border); border-radius: var(--radius-sm); padding: 14px; min-width: 0; }
  strong { font-size: var(--fs-sm); }
  .filename { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font: var(--fs-2xs) var(--font-mono); }
  small, .note { font-size: var(--fs-xs); color: var(--text-dim); }
  .actions { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 12px; }
  button, label {
    background: var(--bg-inset); color: var(--text); border: 1px solid var(--border-strong);
    border-radius: var(--radius-xs); padding: 7px 10px; font: inherit; font-size: var(--fs-xs);
    cursor: pointer; transition: .15s ease;
  }
  button:hover:not(:disabled) { border-color: var(--accent-dim); background: var(--bg-elevated); }
  button:disabled, .disabled { opacity: .45; cursor: default; }
  label { position: relative; }
  input { position: absolute; inset: 0; width: 100%; height: 100%; margin: 0; opacity: 0; cursor: pointer; }
  label:focus-within { outline: 2px solid var(--accent); outline-offset: 1px; }
  progress { width: 100%; margin-top: 12px; accent-color: var(--accent); }
  .detail { margin-top: 12px; align-items: flex-start; }
  .note { margin-top: 10px; line-height: 1.6; }
  .error { color: var(--status-error); font-size: var(--fs-xs); margin-top: 10px; }
  @media (max-width: 760px) { .clips { grid-template-columns: 1fr; } }
</style>
