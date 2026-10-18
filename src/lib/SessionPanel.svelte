<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { cubicOut } from 'svelte/easing';
  import type { TransitionConfig } from 'svelte/transition';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { listen } from '@tauri-apps/api/event';
  import {
    dismissSessionNotification,
    getSessionPanelPreferences,
    hideSessionPanel,
    markSessionRead,
    openDesktopSession,
    openCompanionSlot,
    pressKey,
    setCompanionPage,
    setSessionNotificationOverlay,
    setSessionPanelCollapsed,
    setSessionPanelTab,
    setSessionPanelTabOrder,
    type OpenSessionOutcome
  } from '$lib/api';
  import type {
    AppSnapshot,
    AttentionClass,
    DesktopSession,
    SessionNotification,
    SessionTab
  } from '$lib/types';
  import NotificationCard from '$lib/SessionNotification.svelte';
  import { t, tb, locale } from '$lib/i18n';

  let { snapshot = null }: { snapshot?: AppSnapshot | null } = $props();

  let collapsed = $state(false);
  let storedTab = $state<SessionTab>('attention');
  let storedTabOrder = $state<SessionTab[]>(['attention','codex','others']);
  const requestedTab = $derived(snapshot?.companion_navigation?.active ? (snapshot.companion_navigation.tab ?? 'attention') : storedTab);
  const tabOrder = $derived(snapshot?.companion_navigation?.active ? (snapshot.companion_navigation.tab_order ?? storedTabOrder) : storedTabOrder);
  let editingTabs = $state(false);
  let savingTabs = $state(false);
  const tabLabel = (tab:SessionTab) => { void $locale; return tab === 'codex' ? 'Codex' : t(tab === 'attention' ? 'tab_attention' : 'tab_others'); };
  const isCodex = (row:DesktopSession) => row.agent_id === 'codex' || row.agent_id === 'chatgpt';
  let busyIdentity = $state('');
  let feedback = $state('');
  let feedbackTimer: ReturnType<typeof setTimeout> | undefined;
  let requestSequence = 0;
  let disposed = false;
  let collapsing = $state(false);
  // Row keys in the order the user is currently looking at, while hovered.
  let frozenOrder = $state<string[] | null>(null);
  let closedGroups = $state<string[]>([]);
  let cardHovered = $state(false);
  let panelVisible = $state(false);
  // Countdown bookkeeping is deliberately not reactive: only the two effects
  // below read it, and a re-render must not restart a running timer.
  const CARD_VISIBLE_MS = 5000;
  let cardRemainingMs = CARD_VISIBLE_MS;
  let shownEventId = '';
  let overlayActive: boolean | undefined;
  let mountedCardSlots = $state(0);

  // Actions live until the outgoing DOM is actually removed. Counting mounted
  // slots avoids an effect/outrostart race that would shrink then regrow the
  // native window at the first frame of an exit.
  function reserveCardSpace(_node: HTMLElement) {
    mountedCardSlots++;
    return { destroy() { mountedCardSlots--; } };
  }
  let reducedMotion = $state(false);

  // Only opacity and a small visual offset change; layout/click targets stay put.
  function appear(node: Element): TransitionConfig {
    (node as HTMLElement).inert = false;
    return {
      duration: reducedMotion ? 0 : 180,
      easing: cubicOut,
      css: (t) => `opacity: ${t}; transform: translateY(${(1 - t) * 4}px)`
    };
  }

  function disappear(node: Element): TransitionConfig {
    // Svelte retains outgoing keyed nodes during outro. They must never remain
    // clickable (or keyboard accessible) after their event is revoked.
    (node as HTMLElement).inert = true;
    return { duration: reducedMotion || !panelVisible || frozenOrder ? 0 : 140, css: (t) => `opacity: ${t}` };
  }

  function rowAppear(_node: Element): TransitionConfig {
    return {
      duration: reducedMotion || !panelVisible || frozenOrder ? 0 : 140,
      css: (t) => `opacity: ${t}`
    };
  }

  const allRows = $derived((snapshot?.desktop_sessions ?? []).filter(
    (row) => row.status !== 'offline' && !!row.identity
  ));
  const summary = $derived({
    working: allRows.filter((row) => !row.stale && row.status === 'working').length,
    waiting: allRows.filter((row) => !row.stale && row.attention === 'needs_input').length,
    unread: allRows.filter((row) => row.unread).length,
    error: allRows.filter((row) => !row.stale && row.status === 'error').length
  });
  const attentionRows = $derived(allRows.filter((row) => row.attention));
  const visibleTabs = $derived(tabOrder.filter(tab => tab !== 'attention' || attentionRows.length > 0));
  const activeTab = $derived(visibleTabs.includes(requestedTab) ? requestedTab : (visibleTabs.find(tab => allRows.some(row => tab === 'codex' ? isCodex(row) : tab === 'others' ? !isCodex(row) : !!row.attention)) ?? visibleTabs[0]));
  const viewAll = $derived(activeTab !== 'attention');
  const groupByProject = $derived(activeTab !== 'others');
  const navigation = $derived(snapshot?.companion_navigation);
  const navigating = $derived(navigation?.active ?? false);
  const codexRows = $derived(allRows.filter(isCodex));
  const otherRows = $derived(allRows.filter(row=>!isCodex(row)));
  const tabRows = $derived(activeTab === 'attention' ? attentionRows : activeTab === 'codex' ? codexRows : otherRows);
  const viewRows = $derived(navigating ? (navigation?.slots ?? []) : tabRows);
  function tabCount(tab:SessionTab) { return tab === 'attention' ? attentionRows.length : tab === 'codex' ? codexRows.length : otherRows.length; }
  $effect(() => { if(navigation?.active) collapsed=false; });
  // Rust already decided which events are new; the panel only renders the
  // result. While the pointer is in the list the card stays out, so browsing
  // keeps its rows and click targets (§9.1).
  const pendingCards = $derived(snapshot?.session_notifications ?? []);
  const shownCard = $derived(!panelVisible || frozenOrder || navigating ? null : (pendingCards[0] ?? null));
  const shownCardId = $derived(shownCard?.event_id ?? '');
  const moreCards = $derived(
    Math.max(0, pendingCards.length - 1) + (snapshot?.notification_overflow ?? 0)
  );
  const counts = $derived(
    snapshot?.attention_counts ?? { needs_input: 0, error: 0, done: 0 }
  );
  // The collapsed bar carries the same numbers as the queue, as text only: a
  // 44px window has no room for chips and the count must read without color.
  const collapsedLabel = $derived.by(() => {
    const parts: string[] = [];
    if (counts.needs_input) parts.push(`${t('attention_needs_input')} ${counts.needs_input}`);
    if (counts.error) parts.push(`${t('attention_error')} ${counts.error}`);
    if (counts.done) parts.push(`${t('attention_done')} ${counts.done}`);
    if (parts.length) return parts.join(' · ');
    return summary.working > 0 ? `${t('status_working')} ${summary.working}` : t('attention_none');
  });

  const orderedRows = $derived.by(() => {
    const order = navigating ? null : frozenOrder;
    if (!order) return viewRows;
    const rank = new Map(order.map((key, index) => [key, index]));
    // State text updates in place; positions hold until the pointer leaves.
    const frozenRows = allRows.filter((row) => rank.has(row.row_key));
    const incoming = viewRows.filter((row) => !rank.has(row.row_key));
    return [...frozenRows, ...incoming].sort(
      (a, b) =>
        (rank.get(a.row_key) ?? order.length) - (rank.get(b.row_key) ?? order.length)
    );
  });

  type Group = {
    key: string;
    name: string;
    hint: string | null;
    counts: Record<AttentionClass, number>;
    rows: DesktopSession[];
  };

  const groups = $derived.by(() => {
    const byKey = new Map<string, Group>();
    for (const row of orderedRows) {
      // Project groups simplify Codex and the attention queue. Hardware slots
      // keep their captured order; their project name stays on each row.
      const key = navigating ? 'slots' : groupByProject ? (row.project_id ?? '') : `agent:${row.agent_short}`;
      const name = groupByProject ? (row.project_name ?? t('ungrouped')) : row.agent_short;
      let group = byKey.get(key);
      if (!group) {
        group = {
          key,
          name,
          hint: groupByProject ? (row.project_hint ?? null) : null,
          counts: { needs_input: 0, error: 0, done: 0 },
          rows: []
        };
        byKey.set(key, group);
      }
      group.rows.push(row);
      if (row.attention) group.counts[row.attention] += 1;
    }
    const list = [...byKey.values()];
    const nameUses = new Map<string, number>();
    for (const group of list) nameUses.set(group.name, (nameUses.get(group.name) ?? 0) + 1);
    // The parent-directory hint only disambiguates same-name projects.
    for (const group of list) if ((nameUses.get(group.name) ?? 0) < 2) group.hint = null;
    return list;
  });

  function statusLabel(status: string): string {
    // Read the locale store so switching languages re-derives the labels.
    void $locale;
    switch (status) {
      case 'offline': return t('status_offline');
      case 'idle': return t('status_idle');
      case 'working': return t('status_working');
      case 'done': return t('status_done');
      // The backend serializes NeedsInput as "needsinput"; the declared UI
      // type keeps "needs_input". Accept both without a repo-wide rename.
      case 'needsinput':
      case 'needs_input': return t('status_needs_input');
      case 'error': return t('status_error');
      default: return t('status_unknown');
    }
  }

  function attentionLabel(row: DesktopSession): string {
    void $locale;
    if (row.stale) return t('state_expired');
    if (row.error_acknowledged) return t('error_acknowledged');
    switch (row.attention) {
      case 'needs_input': return t('attention_needs_input');
      case 'error': return t('attention_error');
      case 'done': return t('attention_done');
      default: return statusLabel(row.status);
    }
  }

  function relTime(ms: number): string {
    void $locale;
    if (!ms) return '';
    const seconds = Math.max(0, Math.floor((Date.now() - ms) / 1000));
    if (seconds < 45) return t('time_just_now');
    if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
    if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
    return `${Math.floor(seconds / 86400)}d`;
  }

  function showFeedback(message: string) {
    feedback = message;
    if (feedbackTimer) clearTimeout(feedbackTimer);
    if (message) feedbackTimer = setTimeout(() => (feedback = ''), 4000);
  }

  async function open(row: DesktopSession, card?: SessionNotification) {
    if (!row.identity) return;
    if (navigation?.active) {
      if (row.stale) { showFeedback(t('notify_superseded')); return; }
      const index = navigation.slots.findIndex((slot) => slot.identity === row.identity);
      if (index >= 0) await openCompanionSlot(index, navigation.revision);
      return;
    }
    const sequence = ++requestSequence;
    busyIdentity = row.identity;
    showFeedback('');
    try {
      const outcome: OpenSessionOutcome = await openDesktopSession(row.identity, card ? { kind: card.kind, version: card.event_version } : undefined);
      if (disposed || sequence !== requestSequence) return;
      if (outcome === 'target_gone') showFeedback(t('open_target_gone'));
      else if (outcome === 'focus_failed') showFeedback(t('open_focus_failed'));
      else if (outcome === 'switched_not_raised') showFeedback(t('open_switched_not_raised'));
      else if (outcome === 'superseded' && card) showFeedback(t('notify_superseded'));
      if (
        card &&
        (outcome === 'opened' ||
          outcome === 'switched_not_raised' ||
          outcome === 'target_gone' ||
          outcome === 'superseded')
      )
        dismissCard(card);
    } catch (reason) {
      if (!disposed && sequence === requestSequence) showFeedback(String(reason));
    } finally {
      if (!disposed && sequence === requestSequence) busyIdentity = '';
    }
  }

  async function markRead(row: DesktopSession) {
    if (!row.identity || busyIdentity) return;
    busyIdentity = row.identity;
    const sequence = ++requestSequence;
    try {
      await markSessionRead(row.identity, row.unread_since_ms);
    } catch (reason) {
      if (!disposed && sequence === requestSequence) showFeedback(String(reason));
    } finally {
      if (!disposed && sequence === requestSequence) busyIdentity = '';
    }
  }

  function chooseTab(tab:SessionTab) {
    if(activeTab===tab) return;
    const previous=activeTab;
    if(!navigating) storedTab=tab; frozenOrder=null;
    setSessionPanelTab(tab).catch(reason=>{storedTab=previous;showFeedback(String(reason));});
  }
  function tabKeydown(event:KeyboardEvent,tab:SessionTab) {
    const index=visibleTabs.indexOf(tab);
    const target=event.key==='ArrowLeft' ? Math.max(0,index-1) : event.key==='ArrowRight' ? Math.min(visibleTabs.length-1,index+1) : event.key==='Home' ? 0 : event.key==='End' ? visibleTabs.length-1 : -1;
    if(target<0) return;
    event.preventDefault(); const next=visibleTabs[target]; chooseTab(next);
    document.getElementById(`session-tab-${next}`)?.focus();
  }
  async function moveTab(index:number,delta:number) {
    const target=index+delta;
    if(savingTabs || target<0 || target>=tabOrder.length) return;
    const next=[...tabOrder]; [next[index],next[target]]=[next[target],next[index]];
    savingTabs=true;
    try { await setSessionPanelTabOrder(next); storedTabOrder=next; }
    catch(reason) { showFeedback(String(reason)); }
    finally {savingTabs=false;}
  }

  function toggleGroup(key: string) {
    closedGroups = closedGroups.includes(key)
      ? closedGroups.filter((item) => item !== key)
      : [...closedGroups, key];
  }

  function freeze() {
    if (!navigating && !frozenOrder) frozenOrder = viewRows.map((row) => row.row_key);
  }

  function unfreeze() {
    frozenOrder = null;
  }

  async function toggleCollapsed() {
    if (navigating || collapsing) return;
    collapsing = true;
    const next = !collapsed;
    try {
      await setSessionPanelCollapsed(next);
      collapsed = next;
    } catch (reason) {
      showFeedback(String(reason));
    } finally {
      collapsing = false;
    }
  }

  function beginDrag(event: MouseEvent) {
    if (event.button !== 0) return;
    const target = event.target as HTMLElement | null;
    if (target?.closest('button')) return;
    getCurrentWindow().startDragging().catch(() => {});
  }

  function closePanel() {
    hideSessionPanel().catch(() => {});
  }

  // The version Rust keyed this row's event on, mirrored for the click check:
  // a card must open or dismiss exactly the round it was raised for. Only a real
  // source edge counts, so 0 means "no event" rather than an observation time.
  function attentionVersion(row: DesktopSession): number {
    return row.attention === 'done' ? row.unread_since_ms : row.event_at_ms;
  }

  function dismissCard(card: SessionNotification) {
    dismissSessionNotification(card.event_id).catch(() => {});
  }

  // One card, one countdown: a poll that reports the same event must not
  // restart it, and hovering banks the remaining time instead of resetting it.
  $effect(() => {
    const currentId = shownCardId;
    const current = untrack(() => shownCard);
    const hovered = cardHovered;
    if (!currentId || !current) {
      // Hiding or browsing pauses the same event rather than consuming it.
      cardHovered = false;
      return;
    }
    if (current.event_id !== shownEventId) {
      shownEventId = current.event_id;
      cardRemainingMs = CARD_VISIBLE_MS;
    }
    if (hovered) return;
    const started = Date.now();
    const timer = setTimeout(() => dismissCard(current), Math.max(0, cardRemainingMs));
    return () => {
      clearTimeout(timer);
      cardRemainingMs -= Date.now() - started;
    };
  });

  // The collapsed bar is 44px, so a card needs the window to grow for a moment.
  // The saved collapse preference stays as the user left it.
  $effect(() => {
    const next = panelVisible && (!!shownCard || mountedCardSlots > 0);
    if (next === overlayActive) return;
    overlayActive = next;
    setSessionNotificationOverlay(next).catch(() => {});
  });

  async function openFromCard(card: SessionNotification) {
    const row = (snapshot?.desktop_sessions ?? []).find(
      (item) => item.identity === card.key
    );
    if (
      !row ||
      row.attention !== card.kind ||
      attentionVersion(row) !== card.event_version
    ) {
      // The target moved on while the card was up. Say so, and never open or
      // acknowledge a newer round through a stale card.
      showFeedback(t('notify_superseded'));
      dismissCard(card);
      return;
    }
    await open(row, card);
  }

  function showAttentionQueue() {
    chooseTab('attention');
    if (collapsed) void toggleCollapsed();
  }

  onMount(() => {
    const motion = window.matchMedia('(prefers-reduced-motion: reduce)');
    const updateMotion = () => { reducedMotion = motion.matches; };
    updateMotion();
    motion.addEventListener('change', updateMotion);
    let stopVisibility = () => {};
    let stopPreferences = () => {};
    let stopClick = () => {};
    let preferencesRevision = 0;
    let visibilityRevision = 0;
    listen<import('$lib/api').SessionPanelPreferences>('session_panel_preferences_changed', (event) => {
      preferencesRevision++;
      collapsed = event.payload.collapsed;
      storedTab = event.payload.tab ?? 'attention';
      storedTabOrder = event.payload.tab_order ?? ['attention','codex','others'];
      frozenOrder = null;
    }).then((unlisten) => { if (disposed) unlisten(); else stopPreferences = unlisten; }).catch(() => {});
    listen<string>('desktop_notification_click', (event) => {
      if (event.payload === 'focus_failed') showFeedback(t('open_focus_failed'));
      else if (event.payload === 'switched_not_raised') showFeedback(t('open_switched_not_raised'));
      else if (event.payload === 'target_gone') showFeedback(t('open_target_gone'));
      else if (event.payload === 'superseded' || event.payload === 'expired') showFeedback(t('notify_superseded'));
    }).then((unlisten) => { if (disposed) unlisten(); else stopClick = unlisten; }).catch(() => {});
    listen<boolean>('session_panel_visibility_changed', (event) => {
      visibilityRevision++;
      panelVisible = event.payload;
    }).then(async (unlisten) => {
      if (disposed) { unlisten(); return; }
      stopVisibility = unlisten;
      const revision = visibilityRevision;
      const visible = await getCurrentWindow().isVisible();
      if (!disposed && revision === visibilityRevision) panelVisible = visible;
    }).catch(() => {});
    getSessionPanelPreferences()
      .then((preferences) => {
        if (disposed || preferencesRevision !== 0) return;
        collapsed = preferences.collapsed;
        storedTab = preferences.tab ?? 'attention';
        storedTabOrder = preferences.tab_order ?? ['attention','codex','others'];
      })
      .catch(() => {});
    return () => {
      disposed = true;
      motion.removeEventListener('change', updateMotion);
      stopVisibility();
      stopPreferences();
      stopClick();
      if (feedbackTimer) clearTimeout(feedbackTimer);
    };
  });
</script>

<section class="panel" class:collapsed class:navigating>
  <header class="panel-header" role="presentation" onmousedown={beginDrag}>
    <button class="control" disabled={collapsing} onclick={toggleCollapsed} title={collapsed ? t('expand_panel') : t('collapse_panel')}>
      {collapsed ? '□' : '—'}
    </button>
    <strong class="panel-title">{t('session_panel_title')}</strong>
    {#if collapsed}
      <p class="bar-counts" title={t('tab_attention')}>
        {#key collapsedLabel}<span in:appear>{collapsedLabel}</span>{/key}
      </p>
    {:else if summary}
      <div class="summary">
        <span class="chip working" title={t('status_working')}>{summary.working}</span>
        <span class="chip waiting" title={t('status_needs_input')}>{summary.waiting}</span>
        <span class="chip unread" title={t('summary_unread')}>{summary.unread}</span>
        <span class="chip error" title={t('status_error')}>{summary.error}</span>
      </div>
    {/if}
    <button class="control" onclick={closePanel} title={t('hide_panel')}>✕</button>
  </header>

  {#if !collapsed}
    <div class="tabs" in:appear role="presentation" onmousedown={beginDrag}>
      <div class="tab-items" role="tablist" aria-label={t('session_tabs')}>
      {#each visibleTabs as tab (tab)}
        <button id={`session-tab-${tab}`} role="tab" tabindex={activeTab===tab ? 0 : -1} onkeydown={(event)=>tabKeydown(event,tab)} aria-selected={activeTab===tab} aria-controls="session-tab-rows" class="tab" class:active={activeTab===tab} onclick={() => chooseTab(tab)}>{tabLabel(tab)} {tabCount(tab)}</button>
      {/each}
      </div>
      <button class="tab-order-toggle" aria-label={t('tab_order')} title={t('tab_order')} aria-expanded={editingTabs} onclick={()=>editingTabs=!editingTabs}>⇄</button>
    </div>
    {#if editingTabs}
      <div class="tab-order-editor" aria-label={t('tab_order')}>
        {#each tabOrder as tab,index (tab)}
          <div><span>{index+1}. {tabLabel(tab)}</span><button disabled={savingTabs || index===0} aria-label={`${tabLabel(tab)} ${t('move_tab_left')}`} onclick={()=>moveTab(index,-1)}>←</button><button disabled={savingTabs || index===tabOrder.length-1} aria-label={`${tabLabel(tab)} ${t('move_tab_right')}`} onclick={()=>moveTab(index,1)}>→</button></div>
        {/each}
      </div>
    {/if}
    {#if navigation?.active}
      <div class="navigation-bar">
        <span>{t('companion_navigation_hint')}</span>
        <button title={t(navigation.encoder_mode === 'pages' ? 'companion_page_encoder_hint' : 'companion_tab_encoder_hint')} onclick={() => pressKey('ENCODER_PRESS')}>{t(navigation.encoder_mode === 'pages' ? 'companion_encoder_pages' : 'companion_encoder_tabs')}</button>
        <button title="K4" disabled={navigation.page === 0} onclick={() => setCompanionPage(navigation.page - 1)}>‹ K4</button>
        <span>{navigation.page + 1}/{navigation.pages}</span>
        <button title="K5" disabled={navigation.page + 1 >= navigation.pages} onclick={() => setCompanionPage(navigation.page + 1)}>K5 ›</button>
      </div>
      {#if !snapshot?.companion_navigation?.hardware_ready}<small class="physical-pending">{t('companion_physical_pending')}</small>{/if}
      {#if navigation.feedback}<p class="feedback" role="status">{navigation.feedback}</p>{/if}
    {/if}

    <div
      class="rows"
      id="session-tab-rows"
      role="tabpanel"
      tabindex="0"
      aria-labelledby={`session-tab-${activeTab}`}
      onmouseenter={freeze}
      onmouseleave={unfreeze}
    >
      {#if allRows.length === 0 && (!navigating || !navigation?.slots.length)}
        <p class="empty">{t('panel_empty')}</p>
      {:else if !viewAll && attentionRows.length === 0 && (!navigating || !navigation?.slots.length)}
        <div class="empty-attention">
          <p>
            {t('empty_attention')}
            {#if summary && summary.working > 0}· {t('status_working')} {summary.working}{/if}
          </p>
          <button class="link" onclick={() => chooseTab('codex')}>{t('view_codex')}</button>
        </div>
      {:else if viewRows.length === 0}
        <p class="empty">{t('tab_empty', {tab:tabLabel(activeTab)})}</p>
      {:else}
        {#each groups as group (group.key)}
          <div class="group">
            <button class="group-header" aria-expanded={navigating || !closedGroups.includes(group.key)} onclick={() => toggleGroup(group.key)} title={groupByProject && !navigating ? (group.key || undefined) : undefined}>
              <i class="chev" class:closed={closedGroups.includes(group.key)} aria-hidden="true">▾</i>
              <span class="gname">{group.name}</span>
              {#if group.hint}<span class="ghint">/ {group.hint}</span>{/if}
              <span class="gcounts">
                {#if group.counts.needs_input > 0}
                  <span class="mini waiting">{t('attention_needs_input')} {group.counts.needs_input}</span>
                {/if}
                {#if group.counts.error > 0}
                  <span class="mini error">{t('attention_error')} {group.counts.error}</span>
                {/if}
                {#if group.counts.done > 0}
                  <span class="mini unread">{t('attention_done')} {group.counts.done}</span>
                {/if}
              </span>
            </button>
            <div class="group-body" class:closed={!navigating && closedGroups.includes(group.key)} inert={!navigating && closedGroups.includes(group.key)} aria-hidden={!navigating && closedGroups.includes(group.key)}>
              <div class="group-content">
              {#each group.rows as row (row.row_key)}
                <div
                  class="row"
                  in:rowAppear
                  class:unread={row.unread}
                  class:stale={row.stale}
                  aria-disabled={navigating && row.stale}
                  class:busy={busyIdentity === row.identity}
                  role="button"
                  tabindex={row.identity ? 0 : -1}
                  title={row.identity
                    ? `${t('open_session_hint', { label: row.label })} · ${row.display_name} · ${row.host}`
                    : `${t('open_no_identity')} · ${row.host}`}
                  onclick={() => open(row)}
                  onkeydown={(event) => {
                    if (event.target !== event.currentTarget) return;
                    if (row.identity && (event.key === 'Enter' || event.key === ' ')) {
                      event.preventDefault();
                      open(row);
                    }
                  }}
                >
                  <div class="title">{#if navigating}<b class="slot-number">{(navigation?.slot_keys ?? ['K1','K2','K3','K6','K7','K8'])[navigation?.slots.findIndex((slot) => slot.identity === row.identity) ?? -1]}</b>{/if}{row.label || '—'}</div>
                  <div class="meta">
                    {#key `${row.status}:${row.attention ?? ''}:${attentionVersion(row)}`}
                      <i class={`dot ${row.status}`} class:attention-pulse={panelVisible && !!row.attention && attentionVersion(row) > 0}>
                        {#if row.attention === 'done'}<span class="done-check">✓</span>{/if}
                      </i>
                    {/key}
                    <span class="state">{attentionLabel(row)}</span>
                    {#if activeTab !== 'codex'}<span class="tag">{row.agent_short} / {tb(row.source)}</span>{/if}
                    {#if isCodex(row) && row.project_name && (navigating || !groupByProject)}<span class="project-tag" title={[row.project_name,row.project_hint,row.project_id].filter(Boolean).join(' · ')}>{row.project_name}</span>{/if}
                    <span class="time activity" title={t('time_activity_coverage', {coverage: row.activity?.coverage ?? 'unknown', source: row.activity?.activity_source ?? '—'})}>
                      {t('last_activity')} {row.activity?.last_activity_at ? relTime(row.activity.last_activity_at) : '—'}
                      {#if row.activity?.coverage === 'terminal_inferred'}<span aria-label={t('activity_inferred')}>≈</span>{/if}
                    </span>
                    {#if row.attention && row.event_at_ms}
                    <span class="time" title={row.event_at_ms ? (row.event_is_activity ? t('time_last_activity') : t('time_status_change')) : t('time_first_observed')}>
                      · {relTime(row.event_at_ms)}
                    </span>
                    {/if}
                    {#if !row.identity}<span class="noc">{t('cannot_locate')}</span>{/if}
                    {#if row.unread}<i class="mark" title={t('summary_unread')}>●</i>{/if}
                    {#if row.attention === 'done' && row.identity}
                      <button
                        class="mark-read"
                        onclick={(event) => {
                          event.stopPropagation();
                          markRead(row);
                        }}
                      >{t('mark_read')}</button>
                    {/if}
                  </div>
                </div>
              {/each}
              </div>
            </div>
          </div>
        {/each}
      {/if}
    </div>
    {#if feedback}<p class="feedback">{feedback}</p>{/if}
  {/if}

  {#each shownCard ? [shownCard] : [] as card (card.event_id)}
    <div
      class="card-slot"
      in:appear
      out:disappear
      use:reserveCardSpace
      onoutrostart={() => { cardHovered = false; }}
      role="status"
      aria-live="polite"
      onmouseenter={() => (cardHovered = true)}
      onmouseleave={() => (cardHovered = false)}
    >
      <NotificationCard
        card={card}
        feedback={feedback}
        more={moreCards}
        onopen={() => openFromCard(card)}
        ondismiss={() => dismissCard(card)}
        onqueue={showAttentionQueue}
      />
    </div>
  {/each}
</section>

<style>
  /* Colours come from the shared theme tokens in app.css; the panel only
     shapes layout. It never defines its own palette. */
  .tab-items {display:flex;flex:1;}
  .tab-order-toggle { color: inherit; border: 0; background: transparent; padding: 0 7px; cursor: pointer; }
  .tab-order-editor { padding: 5px 10px; font-size: 10px; border-bottom: 1px solid var(--border); }
  .tab-order-editor > div { display:flex; gap:6px; align-items:center; padding:2px 0; }
  .tab-order-editor span { flex:1; }
  .tab-order-editor button { color:inherit; border:1px solid var(--border-strong); border-radius:4px; background:transparent; }
  .project-tag { max-width:110px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; color:var(--text-dim); }
  .physical-pending { display: block; padding: 0 8px 3px; color: var(--text-dim); font-size: 9px; }
  .slot-number { display: inline-block; color: var(--accent); border: 1px solid var(--border-strong); border-radius: 4px; padding: 0 5px; margin-right: 6px; }
  .navigation-bar { display: flex; align-items: center; gap: 5px; flex-wrap: wrap; padding: 4px 8px; font-size: 9px; }
  .navigation-bar > span:first-child { flex: 1; }
  .navigation-bar button { color: inherit; background: transparent; border: 1px solid var(--border-strong); border-radius: 4px; }
  .navigating .group-header { display: none; }
  .navigating .row { padding: 7px 9px; }

  .panel {
    position: relative;
    width: 100vw;
    height: 100vh;
    display: flex;
    flex-direction: column;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-panel);
    color: var(--text);
    overflow: hidden;
    font-size: 11px;
  }
  .panel-header {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 10px;
    cursor: grab;
    border-bottom: 1px solid var(--border);
    background: var(--bg-inset);
    flex: 0 0 auto;
  }
  .collapsed .panel-header { border-bottom: 0; }
  .panel-title { font-size: 12px; letter-spacing: .02em; }
  .control {
    width: 22px;
    height: 22px;
    flex: 0 0 auto;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: transparent;
    color: var(--text-mid);
    font-size: 11px;
    line-height: 1;
    cursor: pointer;
    padding: 0;
  }
  .control:hover { background: var(--accent-dim); }
  .summary { display: flex; gap: 4px; margin-left: auto; }
  .bar-counts {
    margin: 0 0 0 auto;
    min-width: 0;
    font-size: 10px;
    font-weight: 700;
    color: var(--text-mid);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .bar-counts span { display: inline-block; }
  /* The card floats over the list instead of pushing rows down, so browsing
     keeps its scroll position and click targets. */
  .card-slot {
    z-index: 2;
    position: absolute;
    left: 8px;
    right: 8px;
    top: 68px;
  }
  .collapsed .card-slot { top: 48px; }
  .chip {
    min-width: 18px;
    padding: 1px 5px;
    border-radius: 999px;
    font: 700 9px ui-monospace, monospace;
    text-align: center;
    background: var(--session-row);
    color: var(--text-dim);
  }
  .chip.working { color: var(--status-working); }
  .chip.waiting { color: var(--status-waiting); }
  .chip.unread { color: var(--status-unread); }
  .chip.error { color: var(--status-error); }
  .tabs {
    display: flex;
    gap: 6px;
    padding: 6px 8px 0;
    flex: 0 0 auto;
    cursor: grab;
  }
  .tab {
    border: 1px solid var(--border);
    border-radius: 999px;
    background: transparent;
    color: var(--text-dim);
    font-weight: 700;
    font-size: 9.5px;
    padding: 3px 10px;
    cursor: pointer;
  }
  .tab.active {
    background: var(--bg-inset);
    border-color: var(--accent);
    color: var(--text);
  }
  .rows { flex: 1; overflow-y: auto; padding: 4px 6px 8px; }
  .group { margin-top: 4px; }
  .group-header {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    padding: 4px 6px;
    border: 0;
    background: transparent;
    color: var(--text-mid);
    font-size: 10px;
    cursor: pointer;
    text-align: left;
  }
  .group-header:hover { color: var(--text); }
  .chev { font-style: normal; width: 10px; color: var(--text-dim); transform: rotate(0deg); transition: transform 160ms ease; }
  .chev.closed { transform: rotate(-90deg); }
  /* Retain keyed rows while folding so reopening does not replay attention
     animations. Inert removes click and keyboard targets immediately. */
  .group-body { display: grid; grid-template-rows: 1fr; transition: grid-template-rows 160ms ease; }
  .group-body.closed { grid-template-rows: 0fr; }
  .group-content { min-height: 0; overflow: hidden; }
  .gname { font-weight: 700; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .ghint { color: var(--text-dim); font: 9px ui-monospace, monospace; }
  .gcounts { display: flex; gap: 4px; margin-left: auto; }
  .mini { font: 700 8.5px ui-monospace, monospace; white-space: nowrap; }
  .mini.waiting { color: var(--status-waiting); }
  .mini.error { color: var(--status-error); }
  .mini.unread { color: var(--status-unread); }
  .row {
    display: block;
    width: 100%;
    padding: 8px 10px;
    margin: 3px 0;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--session-row);
    color: inherit;
    text-align: left;
    cursor: pointer;
  }
  .row:hover { background: var(--session-hover); border-color: var(--accent); }
  .row:active { background: var(--session-hover); }
  .row.busy { opacity: .6; }
  .row.unread { border-color: var(--accent); }
  .row.stale { opacity: .55; cursor: default; }
  .title {
    font-size: 12px;
    font-weight: 700;
    line-height: 1.35;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
    color: var(--text);
  }
  .meta {
    display: flex;
    align-items: center;
    gap: 7px;
    margin-top: 4px;
    color: var(--text-dim);
    font-size: 9.5px;
    flex-wrap: wrap;
  }
  .dot { width: 7px; height: 7px; border-radius: 50%; background: var(--border-strong); flex: 0 0 auto; }
  .dot.working { background: var(--status-working); box-shadow: 0 0 5px var(--status-working); }
  .dot.idle { background: var(--accent-glow); }
  .dot.done { background: transparent; color: var(--status-unread); }
  .done-check { display: block; font: 700 10px/7px ui-sans-serif, sans-serif; }
  .attention-pulse { animation: attention-pulse 500ms ease-out 1; }
  @keyframes attention-pulse {
    0% { box-shadow: 0 0 0 0 currentColor; }
    100% { box-shadow: 0 0 0 5px transparent; }
  }
  .dot.needsinput, .dot.needs_input { background: var(--status-waiting); color: var(--status-waiting); box-shadow: 0 0 5px var(--status-waiting); }
  .dot.error { background: var(--status-error); color: var(--status-error); box-shadow: 0 0 5px var(--status-error); }
  .dot.offline, .dot.unknown { background: var(--border-strong); }
  .state { color: var(--text-mid); font-weight: 700; }
  .tag { font: 9px ui-monospace, monospace; }
  .time { font: 9px ui-monospace, monospace; }
  .noc { color: var(--status-waiting); }
  .mark { color: var(--accent); font-size: 8px; font-style: normal; }
  .mark-read {
    margin-left: auto;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: transparent;
    color: var(--text-mid);
    font-size: 9px;
    padding: 1px 7px;
    cursor: pointer;
  }
  .mark-read:hover { background: var(--accent-dim); color: var(--text); }
  .empty { color: var(--text-dim); text-align: center; padding: 24px 14px; line-height: 1.6; }
  .empty-attention {
    color: var(--text-dim);
    text-align: center;
    padding: 22px 14px;
    line-height: 1.8;
  }
  .link {
    border: 1px solid var(--border);
    border-radius: 999px;
    background: transparent;
    color: var(--text-mid);
    font-size: 10px;
    padding: 2px 12px;
    cursor: pointer;
  }
  .link:hover { border-color: var(--accent); color: var(--text); }
  .feedback {
    flex: 0 0 auto;
    margin: 0;
    padding: 6px 10px;
    border-top: 1px solid var(--error-border);
    background: var(--error-bg);
    color: var(--error-text);
    font-size: 10px;
  }
  .control, .tab, .row, .mark-read, .link {
    transition: background-color 140ms ease, border-color 140ms ease, color 140ms ease;
  }
  @media (prefers-reduced-motion: reduce) {
    /* Also suppress an already-running Web Animation if the OS preference
       changes mid-transition. */
    .card-slot, .bar-counts span, .tabs { opacity: 1 !important; transform: none !important; }
    .row { opacity: 1 !important; }
    .row.busy { opacity: .6 !important; }
    .row.stale { opacity: .55 !important; }
    .group-body, .chev { transition: none; }
    .attention-pulse { animation: none; }
    .control, .tab, .row, .mark-read, .link { transition: none; }
  }
</style>
