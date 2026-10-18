<script lang="ts">
  import { locale, t } from '$lib/i18n';
  import { openDesktopSession, showSessionPanel } from '$lib/api';
  import type { AppSnapshot, DesktopSession } from '$lib/types';

  let { snapshot = null }: { snapshot?: AppSnapshot | null } = $props();
  const zh = $derived($locale === 'zh');
  const rows = $derived((snapshot?.desktop_sessions ?? []).filter(row => row.status !== 'offline'));
  let busy = $state(false);
  let feedback = $state('');
  async function open(row?: DesktopSession) {
    busy = true;
    feedback = '';
    try {
      if (!row) await showSessionPanel();
      else if (row.identity) {
        const outcome = await openDesktopSession(row.identity);
        if (outcome === 'target_gone') feedback = t('open_target_gone');
        else if (outcome === 'focus_failed') feedback = t('open_focus_failed');
        else if (outcome === 'switched_not_raised') feedback = t('open_switched_not_raised');
        else if (outcome === 'superseded') feedback = t('notify_superseded');
      }
    } catch (reason) { feedback = String(reason); }
    finally { busy = false; }
  }
</script>

<section class="home">
  <div class="intro">
    <h2>{zh ? '让多个 Agent 会话保持井然有序' : 'Keep your agent sessions in view'}</h2>
    <p>{zh ? '查看运行状态、处理通知、点击回到对应会话。无需购买或连接 AgentDeck 硬件。' : 'Track activity, handle notifications, and click to return to a session. No AgentDeck hardware required.'}</p>
    <button class="primary" disabled={busy} onclick={() => open()}>{zh ? '打开会话浮窗' : 'Open session panel'}</button>
    <p class="note">{zh ? '浮窗可收起、隐藏，也可从菜单栏重新显示。首次使用：启动 Agent → 按需安装 Hooks → 在「通知」中启用桌面提醒。' : 'Collapse or hide the panel and reopen it from the menu bar. To get started: launch an agent → install its hooks if needed → enable desktop alerts in Notifications.'}</p>
  </div>
  <div class="sessions">
    <div class="sessions-head">
      <h2>{t('session_panel_title')}</h2>
      <span class="count">{rows.length}</span>
    </div>
    {#if !rows.length}<p class="empty">{t('panel_empty')}</p>{/if}
    <div class="session-list">
      {#each rows as row (row.row_key)}
        <button class="session" disabled={busy || !row.identity} title={row.identity ? t('open_session_hint', { label: row.label }) : t('open_no_identity')} onclick={() => open(row)}>
          <span class="session-main">
            <strong>{row.label || row.display_name}</strong>
            <small>{row.agent_short} · {row.project_name ?? t('ungrouped')}</small>
          </span>
          <span class="session-state">
            <span class="status {row.stale ? 'unknown' : row.status}">{row.stale ? t('state_expired') : t(`status_${row.status}`)}</span>
            {#if row.unread}<span class="unread">{t('attention_done')}</span>{/if}
          </span>
        </button>
      {/each}
    </div>
  </div>
  {#if feedback}<p class="feedback" role="status">{feedback}</p>{/if}
</section>

<style>
  .home { display: grid; gap: 12px; }
  .intro, .sessions {
    padding: var(--space-4); border: 1px solid var(--border);
    border-radius: var(--radius); background: var(--bg-panel);
  }
  h2 { margin: 0; font-size: var(--fs-lg); font-weight: 650; }
  p { color: var(--text-mid); font-size: var(--fs-sm); line-height: 1.7; margin-top: 8px; }
  .note { color: var(--text-dim); font-size: var(--fs-xs); }
  .primary {
    margin-top: 14px; color: var(--on-accent); background: var(--accent); border: 1px solid var(--accent);
    border-radius: var(--radius-sm); padding: 9px 14px; font-size: var(--fs-sm); font-weight: 700;
    cursor: pointer; transition: .15s ease;
  }
  .primary:hover:not(:disabled) { filter: brightness(1.07); }
  .primary:disabled { opacity: .5; cursor: default; }
  .sessions-head { display: flex; align-items: center; gap: 8px; margin-bottom: 10px; }
  .sessions-head h2 { font-size: var(--fs-md); }
  .count {
    min-width: 22px; padding: 1px 8px; border-radius: var(--radius-pill);
    background: var(--bg-inset); border: 1px solid var(--border);
    color: var(--text-mid); font: 700 var(--fs-2xs) var(--font-mono); text-align: center;
  }
  .empty { margin: 0; }
  .session-list { display: flex; flex-direction: column; gap: 8px; }
  .session {
    width: 100%; display: flex; align-items: center; justify-content: space-between; gap: 12px;
    padding: 10px 12px; text-align: left; font-size: var(--fs-sm);
    border: 1px solid var(--border); border-radius: var(--radius-sm);
    background: var(--bg-inset); color: var(--text); cursor: pointer; transition: .15s ease;
  }
  .session:hover:not(:disabled) { border-color: var(--accent-dim); background: var(--bg-elevated); }
  .session:disabled { opacity: .55; cursor: default; }
  .session-main { min-width: 0; }
  strong { display: block; font-size: var(--fs-md); overflow-wrap: anywhere; }
  small { display: block; color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); margin-top: 3px; overflow-wrap: anywhere; }
  .session-state { display: flex; align-items: center; gap: 8px; flex: 0 0 auto; }
  .status { font-size: var(--fs-xs); font-weight: 600; color: var(--text-mid); white-space: nowrap; }
  .status.working { color: var(--status-working); }
  .status.idle, .status.ready { color: var(--accent); }
  .status.done { color: var(--status-unread); }
  .status.needs_input, .status.needsinput { color: var(--status-waiting); }
  .status.error { color: var(--status-error); }
  .status.offline, .status.unknown { color: var(--text-dim); }
  .unread {
    padding: 1px 8px; border-radius: var(--radius-pill);
    background: color-mix(in srgb, var(--status-unread) 16%, transparent);
    color: var(--status-unread); font-size: var(--fs-2xs); font-weight: 600; white-space: nowrap;
  }
  .feedback {
    margin: 0; padding: 8px 12px; border: 1px solid var(--error-border); border-radius: var(--radius-sm);
    background: var(--error-bg); color: var(--error-text); font-size: var(--fs-xs);
  }
  @media (max-width: 760px) {
    .session { align-items: flex-start; }
    .session-state { flex-direction: column; align-items: flex-end; gap: 4px; }
  }
</style>
