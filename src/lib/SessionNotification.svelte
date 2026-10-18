<script lang="ts">
  import type { SessionNotification } from '$lib/types';
  import { t, tb } from '$lib/i18n';

  let {
    card,
    feedback = '',
    more = 0,
    onopen = () => {},
    ondismiss = () => {},
    onqueue = () => {}
  }: {
    card: SessionNotification;
    feedback?: string;
    /** Events still waiting behind this one, including queue overflow. */
    more?: number;
    onopen?: () => void;
    ondismiss?: () => void;
    onqueue?: () => void;
  } = $props();

  const STATE_KEYS = {
    needs_input: 'attention_needs_input',
    error: 'attention_error',
    done: 'attention_done'
  } as const;
</script>

<div class="card">
  <div class="head">
    <i class="dot {card.kind}">{#if card.kind === 'done'}✓{/if}</i>
    <span class="state">{t(STATE_KEYS[card.kind])}</span>
    {#if card.project_name}<span class="project">{card.project_name}</span>{/if}
    <button class="close" title={t('notify_dismiss')} onclick={ondismiss}>✕</button>
  </div>
  <div class="title" title={`${card.agent_short} / ${card.source}`}>{card.label || '—'}</div>
  <div class="foot">
    <span class="tag" class:error={!!feedback} title={feedback || undefined}>
      {feedback || `${card.agent_short} / ${tb(card.source)}`}
    </span>
    <span class="actions">
      {#if more > 0}
        <button class="more" onclick={onqueue}>
          {t('notify_more', { count: String(more) })}
        </button>
      {/if}
      <button class="open" onclick={onopen}>{t('notify_open')}</button>
    </span>
  </div>
</div>

<style>
  .card {
    border: 1px solid var(--border);
    border-radius: 9px;
    background: var(--bg-elevated);
    padding: 7px 9px 8px;
    box-shadow: 0 6px 18px var(--shadow-strong);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 10px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--border-strong);
    flex: 0 0 auto;
  }
  .dot.needs_input { background: var(--status-waiting); box-shadow: 0 0 5px var(--status-waiting); }
  .dot.error { background: var(--status-error); box-shadow: 0 0 5px var(--status-error); }
  .dot.done { background: transparent; color: var(--status-unread); font: 700 11px/8px ui-sans-serif, sans-serif; }
  .state { color: var(--text-mid); font-weight: 700; }
  .project {
    color: var(--text-dim);
    font: 9px ui-monospace, monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .close {
    margin-left: auto;
    border: 0;
    background: transparent;
    color: var(--text-dim);
    font-size: 10px;
    line-height: 1;
    cursor: pointer;
    padding: 2px 3px;
  }
  .close:hover { color: var(--text); }
  .title {
    margin-top: 3px;
    font-size: 12px;
    font-weight: 700;
    line-height: 1.3;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
    color: var(--text);
  }
  .foot {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 4px;
    color: var(--text-dim);
    font-size: 9px;
  }
  .tag { font-family: ui-monospace, monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
  .tag.error { color: var(--error-text); }
  .actions {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-left: auto;
  }
  .open,
  .more {
    border: 1px solid var(--border);
    border-radius: 999px;
    background: transparent;
    color: var(--text-mid);
    font-size: 9px;
    padding: 1px 9px;
    cursor: pointer;
    white-space: nowrap;
  }
  .open:hover,
  .more:hover { background: var(--accent-dim); color: var(--text); }
  .close, .open, .more { transition: background-color 140ms ease, color 140ms ease; }
  @media (prefers-reduced-motion: reduce) {
    .close, .open, .more { transition: none; }
  }
</style>
