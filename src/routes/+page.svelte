<script lang="ts">
  import MacosPermissions from '$lib/MacosPermissions.svelte';
  import { onMount } from 'svelte';
  import AgentMonitoring from '$lib/AgentMonitoring.svelte';
  import NotificationAudio from '$lib/NotificationAudio.svelte';
  import DesktopNotifications from '$lib/DesktopNotifications.svelte';
  import SessionPanel from '$lib/SessionPanel.svelte';
  import SessionHome from '$lib/SessionHome.svelte';
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import {
    assignAgentSlot,
    getState,
    installAgentHooks,
    installDshPlugin,
    listHookIntegrations,
    hideFunctionKeyHint,
    herdrAgentAction,
    pressKey,
    rotateKnob,
    resetAgentBindings,
    resetKeyBinding,
    sendMockHook,
    setHintPreferences,
    setKeyBinding,
    setUiLocale,
    setUiTheme,
    showFunctionKeyHint,
    uninstallAgentHooks,
    uninstallDshPlugin
  } from '$lib/api';
  import type { AgentState, AgentStatus, AppSnapshot, FunctionKeyState, HookIntegrationState, UiTheme } from '$lib/types';
  import { t, tb, locale, setLocale, type Locale } from '$lib/i18n';

  $: statusLabel = {
    offline: t('status_offline'),
    idle: t('status_idle'),
    working: t('status_working'),
    done: t('status_done'),
    needs_input: t('status_needs_input'),
    error: t('status_error'),
    unknown: t('status_unknown')
  } as Record<AgentStatus, string>;
  $: defaultFunctionKeys = Array.from({ length: 10 }, (_, index) => ({
    key: `K${index + 1}`, label: '—', hint: t('no_agent_hint'), supported: false,
    action_id: null, source: 'no_agent', customized: false
  }));

  let snapshot: AppSnapshot | null = null;
  let error = '';
  let loading = true;
  let integrations: HookIntegrationState[] = [];
  let busyAgent = '';
  let viewReady = false;
  let hintView = false;
  let panelView = false;
  let configMode = false;
  let activeTab = 'sessions';
  let devOpen = '';
  let herdrJobIndex = 0;

  // One palette for every window. The order here is the only thing the UI
  // decides; the colours live in src/app.css and the choice is persisted by
  // Rust, so the session panel and hint window just mirror `ui_theme`.
  const themes: UiTheme[] = ['classic', 'modern', 'tech', 'fresh'];
  let localTheme: UiTheme = 'modern';
  $: currentTheme = snapshot?.ui_theme ?? localTheme;
  // Also stamp the palette on <html> so the window chrome (scrollbars, the
  // page background behind the app) matches, not just the app subtree.
  function applyTheme(id: UiTheme) {
    if (typeof document === 'undefined') return;
    const root = document.documentElement;
    for (const name of themes) root.classList.toggle(`theme-${name}`, name === id);
  }
  $: applyTheme(currentTheme);
  function setTheme(id: UiTheme) {
    if (!themes.includes(id)) return;
    localTheme = id;
    if (typeof localStorage !== 'undefined') localStorage.setItem('deck-theme', id);
    setUiTheme(id).catch((reason) => { error = String(reason); });
  }

  async function beginWindowDrag(event: MouseEvent) {
    if (event.button !== 0) return;
    const target = event.target as HTMLElement | null;
    if (target?.closest('button, input, select, textarea, a')) return;
    try {
      await getCurrentWindow().startDragging();
    } catch (reason) {
      console.warn('window drag unavailable', reason);
    }
  }

  function minimizeWindow() { getCurrentWindow().minimize(); }
  function closeWindow() { getCurrentWindow().close(); }

  let refreshGeneration = 0;
  let disposed = false;
  async function refresh() {
    const generation = ++refreshGeneration;
    try {
      const next = await getState();
      if (disposed || generation !== refreshGeneration) return;
      snapshot = next;
      error = '';
    } catch (reason) {
      if (!disposed && generation === refreshGeneration) error = String(reason);
    } finally {
      loading = false;
    }
  }

  async function refreshHooks() {
    try {
      integrations = await listHookIntegrations();
    } catch (reason) {
      error = String(reason);
    }
  }

  async function toggleHooks(integration: HookIntegrationState) {
    busyAgent = integration.agent_id;
    try {
      // A plugin entry is maintained by DSH's own installer, not by editing an
      // agent config, so it has its own command.
      integrations = integration.kind === 'plugin'
        ? (integration.installed ? await uninstallDshPlugin() : await installDshPlugin())
        : integration.installed
          ? await uninstallAgentHooks(integration.agent_id)
          : await installAgentHooks(integration.agent_id);
      error = '';
    } catch (reason) {
      error = String(reason);
    } finally {
      busyAgent = '';
    }
  }

  async function mockHook(agentId: string, event: string) {
    try {
      snapshot = await sendMockHook(agentId, event);
      error = '';
    } catch (reason) {
      error = String(reason);
    }
  }

  async function handleKey(key: string) {
    try {
      snapshot = await pressKey(key);
      error = '';
    } catch (reason) {
      error = String(reason);
    }
  }

  async function handleKnobRotate(delta: number) {
    if (!delta) return;
    try {
      snapshot = await rotateKnob(delta);
      error = '';
    } catch (reason) {
      error = String(reason);
    }
  }

  function knobWheel(event: WheelEvent) {
    event.preventDefault();
    void handleKnobRotate(event.deltaY < 0 ? 1 : -1);
  }

  function activeAgent(): AgentState | null {
    return snapshot?.agents.find((item) => item.id === snapshot?.active_agent) ?? null;
  }

  // Legacy-mode templates only track state referenced textually in the
  // expression; calling activeAgent()/jobCounts() directly compiles to an
  // untracked derived that never recomputes when snapshot changes.
  $: active = snapshot ? activeAgent() : null;
  $: counts = snapshot ? jobCounts() : { working: 0, waiting: 0, error: 0 };
  $: slots = snapshot?.agent_slots ?? [];

  function herdrAgents() {
    return snapshot?.herdr?.agents ?? [];
  }

  function activePanels() {
    const lcdOrder = (snapshot?.sessions ?? []).filter((row) => row.id).map((row) => row.id);
    const rank = (id: string) => { const index = lcdOrder.indexOf(id); return index < 0 ? Number.MAX_SAFE_INTEGER : index; };
    return (snapshot?.active_panels ?? [])
      .filter((panel) => {
        const belongs = snapshot?.active_agent === 'herdr'
          ? panel.transport.kind === 'herdr'
          : panel.agent_id === snapshot?.active_agent;
        return belongs && panel.status !== 'offline';
      })
      .sort((a, b) => rank(a.id) - rank(b.id) || a.slot - b.slot);
  }

  function selectedActivePanel() {
    return activePanels()[Math.min(herdrJobIndex, Math.max(0, activePanels().length - 1))] ?? null;
  }

  function selectedHerdrAgent() {
    const agents = herdrAgents();
    return agents.length ? agents[Math.min(herdrJobIndex, agents.length - 1)] : null;
  }

  function cycleHerdrJob(delta: number) {
    const count = herdrAgents().length;
    if (count) herdrJobIndex = (herdrJobIndex + delta + count) % count;
  }

  function activePanelSlot(actionId?: string | null): number | null {
    const match = actionId?.match(/^active_panel\.(\d+)$/);
    return match ? Number(match[1]) : null;
  }

  function panelForKey(item: FunctionKeyState) {
    const slot = activePanelSlot(item.action_id);
    return slot === null ? null : activePanels()[slot - 1] ?? null;
  }

  async function touchHerdr(action: 'focus' | 'continue' | 'stop') {
    const selected = selectedHerdrAgent();
    if (!selected) return;
    try { snapshot = await herdrAgentAction(selected.pane_id, action); error = ''; }
    catch (reason) { error = String(reason); }
  }

  function jobCounts() {
    if (snapshot?.active_agent === 'herdr') {
      return herdrAgents().reduce((result, agent) => {
        if (agent.status === 'working') result.working++;
        else if (agent.status === 'blocked' || agent.status === 'done') result.waiting++;
        else if (agent.status === 'unknown') result.error++;
        return result;
      }, { working: 0, waiting: 0, error: 0 });
    }
    const panels = activePanels();
    return panels.reduce((result, panel) => {
      result.working += panel.jobs_working;
      result.waiting += panel.jobs_waiting;
      result.error += panel.jobs_error;
      return result;
    }, { working: 0, waiting: 0, error: 0 });
  }

  async function updateHintPreferences(autoShow: boolean, pinned: boolean) {
    try {
      snapshot = await setHintPreferences(autoShow, pinned);
      error = '';
    } catch (reason) {
      error = String(reason);
    }
  }

  async function closeHint() {
    if (snapshot?.hint_preferences.pinned) {
      await updateHintPreferences(snapshot.hint_preferences.auto_show, false);
    }
    await hideFunctionKeyHint();
  }

  async function showHintNow() {
    try {
      await showFunctionKeyHint();
      error = '';
    } catch (reason) {
      error = String(reason);
    }
  }

  function toggleLocale() {
    const next = $locale === 'zh' ? 'en' : 'zh';
    setLocale(next);
    // The menu-bar menu is native, so Rust has to render it in the same
    // language. Failures only mean the tray keeps its previous labels.
    setUiLocale(next).catch((reason) => { error = String(reason); });
  }

  function agentSlots() {
    return snapshot?.agent_slots ?? [];
  }

  async function configureAgentSlot(slot: string, agentId: string) {
    try {
      snapshot = await assignAgentSlot(slot, agentId);
      error = '';
    } catch (reason) {
      error = String(reason);
    }
  }

  async function configureFunctionKey(key: string, actionId: string) {
    if (!snapshot?.active_agent || !snapshot.active_surface) return;
    try {
      snapshot = actionId === '__inherit__'
        ? await resetKeyBinding(snapshot.active_agent, snapshot.active_surface, key)
        : await setKeyBinding(snapshot.active_agent, snapshot.active_surface, key, actionId);
      error = '';
    } catch (reason) {
      error = String(reason);
    }
  }

  async function restoreAgentDefaults() {
    if (!snapshot?.active_agent) return;
    try {
      snapshot = await resetAgentBindings(snapshot.active_agent);
      error = '';
    } catch (reason) {
      error = String(reason);
    }
  }

  onMount(() => {
    const savedTheme = localStorage.getItem('deck-theme');
    if (savedTheme && (themes as string[]).includes(savedTheme)) localTheme = savedTheme as UiTheme;
    const urlTab = new URLSearchParams(window.location.search).get('tab');
    if (urlTab && ['sessions', 'sim', 'hooks', 'testing'].includes(urlTab)) activeTab = urlTab;
    hintView = '__TAURI_INTERNALS__' in window && getCurrentWindow().label === 'function-hint';
    panelView = '__TAURI_INTERNALS__' in window && getCurrentWindow().label === 'session-panel';
    if (!hintView && !panelView) {
      // Seed Rust with the language this window already resolved (saved
      // choice or system locale) so a fresh install gets a matching tray.
      setUiLocale($locale).catch(() => {});
      refreshHooks();
    }
    let stopListening = () => {};
    let liveEvents = false;
    let lastFallback = 0;
    if ('__TAURI_INTERNALS__' in window) {
      listen<AppSnapshot>('state_changed', (event) => {
        ++refreshGeneration;
        snapshot = event.payload;
      }).then((unlisten) => { if (disposed) unlisten(); else { stopListening = unlisten; liveEvents = true; } });
    }
    refresh().finally(() => (viewReady = true));
    // Rust pushes live state to every window. Poll only as recovery, rather
    // than projecting the same snapshot in three WebViews every 1.5 seconds.
    const timer = window.setInterval(() => {
      if (!liveEvents || Date.now() - lastFallback >= 10000) {
        lastFallback = Date.now();
        void refresh();
      }
    }, 1500);
    return () => {
      disposed = true;
      window.clearInterval(timer);
      stopListening();
    };
  });
</script>

<svelte:head><title>Agent Deck Companion</title></svelte:head>

{#if !viewReady}
  <div class="boot-view"></div>
{:else if hintView}
  <main class="hint-window theme-root theme-{currentTheme}">
    <section class="hint-card">
      <header class="hint-header">
        <div class="hint-agent">
          <i class="status-dot {active?.status ?? 'offline'}"></i>
          <div>
            <strong>{active?.display_name ?? t('no_active_agent')}</strong>
            <small>{snapshot?.active_surface?.replace('_', ' ').toUpperCase() ?? t('select_agent')}</small>
          </div>
        </div>
        <div class="hint-controls">
          <button
            class:chosen={snapshot?.hint_preferences.pinned}
            aria-label={snapshot?.hint_preferences.pinned ? t('unpin') : t('pin')}
            title={snapshot?.hint_preferences.pinned ? t('unpin') : t('pin')}
            onclick={() => updateHintPreferences(snapshot?.hint_preferences.auto_show ?? true, !snapshot?.hint_preferences.pinned)}
          >{snapshot?.hint_preferences.pinned ? t('pinned') : t('not_pinned')}</button>
          <button aria-label={t('close_hint')} title={t('close_hint')} onclick={closeHint}>×</button>
        </div>
      </header>
      <div class="hint-key-grid">
        {#each snapshot?.function_keys ?? defaultFunctionKeys as item}
          {@const slot = activePanelSlot(item.action_id)}
          {@const panel = panelForKey(item)}
          <div class:unsupported={slot !== null ? !panel : !item.supported} class="hint-key">
            <small>{item.key}</small>
            <strong>{slot !== null ? item.key : tb(item.label)}</strong>
            <span>{slot !== null ? (panel ? t('activate', { label: panel.label }) : t('no_active_panel_short')) : tb(item.hint)}</span>
          </div>
        {/each}
      </div>
      <footer class="hint-footer">{t('hint_footer')}</footer>
    </section>
  </main>
{:else if panelView}
  <main class="panel-window theme-root theme-{currentTheme}">
    <SessionPanel {snapshot} />
  </main>
{:else}
<main class="companion-main theme-root theme-{currentTheme}">
  <header data-tauri-drag-region role="presentation" onmousedown={beginWindowDrag}>
    <div class="brand">
      <span class="brand-mark" aria-hidden="true"></span>
      <div class="brand-text">
        <h1>Agent Deck <span>Companion</span></h1>
        <p class="subtitle">{t('subtitle')}</p>
      </div>
    </div>
    <div class="header-right">
      <div class="theme-selector" role="group" aria-label={t('theme_label')}>
        {#each themes as theme}
          <button
            class="theme-dot theme-{theme}"
            class:active={currentTheme === theme}
            aria-label={t(`theme_${theme}`)}
            aria-pressed={currentTheme === theme}
            title={t(`theme_${theme}`)}
            onclick={() => setTheme(theme)}
          ></button>
        {/each}
      </div>
      <span class="system-state">
        <span class:connected={!!snapshot && !error} class="pulse"></span>
        {loading ? t('scanning') : ($locale === 'zh' ? '本机会话管理' : 'Local session manager')}
      </span>
      <button class="lang-toggle" onclick={toggleLocale}>{t('lang_label')}</button>
      <div class="window-controls">
        <button class="win-btn" aria-label="Minimize" onclick={minimizeWindow}>─</button>
        <button class="win-btn win-close" aria-label="Close" onclick={closeWindow}>✕</button>
      </div>
    </div>
  </header>

  {#if error}<div class="error">{error}</div>{/if}

  <nav class="tab-bar" aria-label={$locale === 'zh' ? '主视图' : 'Main views'}>
    <button class:active={activeTab === 'sessions'} onclick={() => activeTab = 'sessions'}>
      <span aria-hidden="true">▤</span> {t('session_panel_title')}
    </button>
    <button class:active={activeTab === 'hooks'} onclick={() => activeTab = 'hooks'}>
      <span aria-hidden="true">⚡</span> Hooks
    </button>
    <button
      class:active={activeTab === 'sim'}
      title={$locale === 'zh' ? 'AgentDeck 硬件（可选）' : 'AgentDeck hardware (optional)'}
      onclick={() => activeTab = 'sim'}
    >
      <span aria-hidden="true">◉</span> {$locale === 'zh' ? '硬件' : 'Hardware'}
    </button>
    <button class:active={activeTab === 'testing'} onclick={() => activeTab = 'testing'}>
      <span aria-hidden="true">⚙</span> {$locale === 'zh' ? '设置' : 'Settings'}
    </button>
  </nav>

  {#if activeTab === 'sessions'}
    <SessionHome {snapshot} />
  {:else if activeTab === 'sim'}
    <article class="panel hardware-intro">
      <h2>{$locale === 'zh' ? 'AgentDeck 键盘配置 · 可选扩展' : 'AgentDeck keyboard configuration · optional'}</h2>
      <p>{$locale === 'zh' ? '仅 AgentDeck（ESP32）用户需要此页。请在菜单栏启用「ESP32 硬件支持」，再连接设备；A1–A5 切换 Agent，可将一个槽位绑定到 Companion，快速管理会话浮窗。下方布局也可用于模拟测试。' : 'For AgentDeck (ESP32) owners. Enable “ESP32 硬件支持” in the menu bar, then connect your device. A1–A5 select agents; assign a slot to Companion to navigate the session panel. The layout below also supports simulation.'}</p>
      <p>{$locale === 'zh' ? '关闭硬件支持后，重启应用可完整停止监听。无需硬件时，直接使用「会话」与「通知」。' : 'After disabling hardware support, restart to fully stop listeners. Use Sessions and Notifications when working without a deck.'}</p>
      {#if snapshot?.companion_navigation && !snapshot.companion_navigation.hardware_ready}
        <p>{t('companion_physical_pending')}</p>
      {/if}
    </article>
    <MacosPermissions />
  <!-- Hardware Simulator -->
  <section class="console-section">
    <div class="section-heading">
      <div class="deck-editor-heading">
        <small>{t('config_label', { profile: tb(snapshot?.active_profile ?? 'default') })}</small>
        <button class:active={configMode} class="secondary" onclick={() => (configMode = !configMode)}>
          {configMode ? t('done_configuring') : t('configure_keys')}
        </button>
      </div>
    </div>
    {#if configMode}
      <div class="config-tip">
        <span>{t('select_slot')}</span>
        {#if snapshot?.active_agent}
          <button class="text-button" onclick={restoreAgentDefaults}>{t('restore_defaults', { name: active?.display_name ?? '' })}</button>
        {/if}
      </div>
    {/if}
    <div class="deck">
      <div class="deck-screws"><i></i><i></i><i></i><i></i></div>
      <div class="agent-key-row">
        {#if slots.length === 0}
          <div class="agent-slots-unavailable">{loading ? t('scanning') : t('agent_slots_unavailable')}</div>
        {:else}
          {#each slots as slot}
            {@const agent = snapshot?.agents.find((item) => item.id === slot.agent_id)}
            {#if configMode}
              <div class="agent-slot-editor">
                <button class:chosen={snapshot?.active_agent === slot.agent_id} class="deck-key agent-key" onclick={() => handleKey(slot.key)}>
                  <small>{slot.key}</small><span>{agent?.display_name ?? slot.agent_id}</span><i class="status-dot {agent?.status ?? 'offline'}"></i>
                </button>
                <select aria-label={`${slot.key} Agent`} value={slot.agent_id} onchange={(event) => configureAgentSlot(slot.key, event.currentTarget.value)}>
                  {#each snapshot?.agents ?? [] as option}
                    <option value={option.id}>{option.display_name}</option>
                  {/each}
                </select>
              </div>
            {:else}
              <button class:chosen={snapshot?.active_agent === slot.agent_id} class="deck-key agent-key" onclick={() => handleKey(slot.key)}>
                <small>{slot.key}</small><span>{agent?.display_name ?? slot.agent_id}</span><i class="status-dot {agent?.status ?? 'offline'}"></i>
              </button>
            {/if}
          {/each}
        {/if}
      </div>
      <div class="deck-middle">
        <div class="lcd lcd-v2">
          {#if active}
            <!-- Agent page header only: session rows live in the Sessions view. -->
            <div class="lcd-header">
              <div class="lcd-header-left">
                <strong>{active.display_name}</strong>
                <span class="lcd-status-dot {active.status}"></span>
                <span class="lcd-status-text {active.status}">{(statusLabel[active.status] ?? t('status_unknown')).toUpperCase()}</span>
              </div>
              <span class="lcd-jobs">{counts.working}W {counts.waiting}I {counts.error}E</span>
            </div>
            <!-- K6-K10 function key labels -->
            <div class="lcd-fn-labels">
              {#each snapshot?.function_keys?.slice(5, 10) ?? [] as fk}
                <div class="lcd-fn-label">{tb(fk.label) || '—'}</div>
              {/each}
            </div>
          {:else}
            <div class="lcd-empty"><strong>NO ACTIVE AGENT</strong><span>PRESS A1–A5</span></div>
          {/if}
        </div>
        <div class="knob-wrap">
          <button
            class="knob"
            aria-label={snapshot?.companion_navigation?.active ? t(snapshot.companion_navigation.encoder_mode === 'pages' ? 'companion_page_encoder_hint' : 'companion_tab_encoder_hint') : t('knob_title')}
            title={snapshot?.companion_navigation?.active ? t(snapshot.companion_navigation.encoder_mode === 'pages' ? 'companion_page_encoder_hint' : 'companion_tab_encoder_hint') : t('knob_hint')}
            onclick={() => handleKey('ENCODER_PRESS')}
            onwheel={knobWheel}
          ><i style={`transform: rotate(${(snapshot?.device.knob_position ?? 1) * 90 - 135}deg)`}></i></button>
          <small class="knob-value">
            {#if snapshot?.companion_navigation?.active}
              {snapshot.companion_navigation.encoder_mode === 'pages' ? `${t('companion_encoder_pages')} ${snapshot.companion_navigation.page + 1}/${snapshot.companion_navigation.pages}` : `${t('companion_encoder_tabs')} · ${snapshot.companion_navigation.tab === 'codex' ? 'Codex' : t(snapshot.companion_navigation.tab === 'others' ? 'tab_others' : 'tab_attention')}`}
            {:else if snapshot?.active_agent === 'chatgpt'}
              {`${t('target')} ${snapshot.device.reasoning_level.toUpperCase()} · ${snapshot.device.reasoning_actual ? `${t('actual')} ${snapshot.device.reasoning_actual.toUpperCase()}` : snapshot.device.reasoning_sync.toUpperCase()}`}
            {:else}
              {t('scroll_rotate')}
            {/if}
          </small>
          {#if snapshot?.active_agent === 'chatgpt'}
            <small class:mode-warning={snapshot.device.native_encoder && snapshot.device.encoder_mode !== 'reasoning'} class="knob-mode">
              {snapshot.device.native_encoder
                ? `NATIVE · ${snapshot.device.encoder_mode?.toUpperCase() ?? 'MODE NOT SET'}`
                : t('gui_fallback')}
            </small>
          {/if}
        </div>
      </div>
      <div class="function-grid">
        {#each snapshot?.function_keys ?? defaultFunctionKeys as item}
          {#if configMode && snapshot?.active_agent}
            <div class:customized={item.customized} class="function-key-editor">
              <div><small>{item.key}</small><strong>{tb(item.label)}</strong><span>{tb(item.hint)}</span></div>
              <select
                aria-label={`${item.key} ${t('function')}`}
                value={item.customized ? (item.action_id ?? 'disabled') : '__inherit__'}
                onchange={(event) => configureFunctionKey(item.key, event.currentTarget.value)}
              >
                <option value="__inherit__">{t('inherit')} · {tb(item.label)}</option>
                {#each snapshot.action_catalog as action}
                  <option value={action.action_id}>{tb(action.label)} · {tb(action.hint)}</option>
                {/each}
                <option value="disabled">{t('disable_key')}</option>
              </select>
            </div>
          {:else}
            <button class="deck-key fn-key" disabled={!item.supported} title={tb(item.hint)} onclick={() => handleKey(item.key)}>
              <small>{item.key}</small>
              <span>{tb(item.label)}</span>
            </button>
          {/if}
        {/each}
      </div>
    </div>
  </section>
    <article class="panel device-panel">
      <div class="panel-title"><div><span>⬡</span><h2>{t('hardware_device')}</h2></div><small>{t('vendor_hid')}</small></div>
      <div class="agent-meta">
        <span class="badge {snapshot?.device.connected ? 'active' : 'offline'}">
          {snapshot?.device.connected ? t('connected') : t('not_detected')}
        </span>
        <small>{snapshot?.device.port ?? t('plug_hint')}</small>
      </div>
      <div class="event-log"><span>{t('last_event')}</span><code>{snapshot?.device.last_event ?? t('waiting_input')}</code></div>
    </article>
    <NotificationAudio />
  {:else if activeTab === 'hooks'}
  <section class="hook-section">
    <div class="section-heading"><div><span>⚡</span><h2>{t('agent_hooks')}</h2></div><p>{t('hooks_desc')}</p></div>
    <div class="hook-list">
      {#each integrations as integration}
        <div class="hook-item" class:installed={integration.installed}>
          <div class="hook-row">
            <i class="hook-dot"></i>
            <div class="hook-name">
              <strong>
                {integration.display_name}
                <span class="hook-kind">{integration.kind === 'plugin' ? ($locale === 'zh' ? 'DSH 插件' : 'DSH plugin') : 'Hooks'}</span>
              </strong>
              <code title={integration.config_path}>{integration.config_path}</code>
            </div>
            <span class="hook-state">{integration.installed ? t('hook_connected') : t('hook_not_installed')}</span>
            <button class={integration.installed ? 'secondary' : 'primary'} disabled={busyAgent === integration.agent_id} onclick={() => toggleHooks(integration)}>
              {busyAgent === integration.agent_id ? t('processing') : integration.installed ? t('uninstall') : integration.kind === 'plugin' ? t('install_plugin') : t('install_hooks')}
            </button>
            {#if integration.kind !== 'plugin'}
              <button
                class="hook-dev-toggle"
                class:active={devOpen === integration.agent_id}
                aria-label={$locale === 'zh' ? '开发测试' : 'Developer testing'}
                title={$locale === 'zh' ? '开发测试' : 'Developer testing'}
                onclick={() => (devOpen = devOpen === integration.agent_id ? '' : integration.agent_id)}
              >⋯</button>
            {/if}
          </div>
          <p class="hook-detail">{integration.detail}</p>
          {#if devOpen === integration.agent_id}
            <div class="hook-dev">
              <button class="secondary" onclick={() => mockHook(integration.agent_id, 'UserPromptSubmit')}>{t('mock_running')}</button>
              <button class="secondary" onclick={() => mockHook(integration.agent_id, 'PermissionRequest')}>{t('mock_waiting')}</button>
              <button class="secondary" onclick={() => mockHook(integration.agent_id, 'Stop')}>{t('mock_done')}</button>
            </div>
          {/if}
        </div>
      {/each}
    </div>
  </section>

  {:else if activeTab === 'testing'}
  <AgentMonitoring {snapshot} onsnapshot={(next) => (snapshot = next)} />
  <MacosPermissions />
  <DesktopNotifications />

  {/if}

  {#if activeTab === 'sim'}
  <section class="hint-settings panel">
    <div>
      <span class="settings-kicker">FUNCTION KEY HINT</span>
      <h2>{t('function_key_hint')}</h2>
      <p>{t('hint_desc')}</p>
    </div>
    <label class="toggle-control">
      <input
        type="checkbox"
        checked={snapshot?.hint_preferences.auto_show ?? true}
        onchange={(event) => updateHintPreferences(event.currentTarget.checked, snapshot?.hint_preferences.pinned ?? false)}
      />
      <span>{t('auto_show')}</span>
    </label>
    <label class="toggle-control">
      <input
        type="checkbox"
        checked={snapshot?.hint_preferences.pinned ?? false}
        onchange={(event) => updateHintPreferences(snapshot?.hint_preferences.auto_show ?? true, event.currentTarget.checked)}
      />
      <span>{t('pin_hint')}</span>
    </label>
    <button class="secondary show-hint-button" onclick={showHintNow}>{t('show_now')}</button>
  </section>
  {/if}

  <footer><span>POC v0.1</span><span>{$locale === 'zh' ? '本地优先 · 硬件可选' : 'Local first · Hardware optional'}</span></footer>
</main>
{/if}

<style>
  /* Shared tokens live in app.css; this block only shapes the main window. */
  .hardware-intro { font-size: var(--fs-sm); line-height: 1.65; }
  .hardware-intro h2 { font-size: var(--fs-lg); margin-bottom: 8px; }
  .hardware-intro p { color: var(--text-mid); margin-top: 6px; }

  .companion-main {
    min-height: 100vh;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 14px max(16px, calc((100vw - 900px) / 2)) 10px;
    background:
      radial-gradient(circle at 88% -12%, color-mix(in srgb, var(--accent) 12%, transparent), transparent 46%),
      var(--bg);
  }
  .companion-main > header[data-tauri-drag-region] { cursor: grab; }
  .companion-main > header[data-tauri-drag-region]:active { cursor: grabbing; }
  .companion-main > footer { margin-top: auto; }

  header { display: flex; align-items: center; justify-content: space-between; gap: 16px; }
  .brand { display: flex; align-items: center; gap: 11px; min-width: 0; }
  .brand-mark {
    width: 22px; height: 22px; border-radius: 7px; flex: 0 0 auto;
    background: linear-gradient(140deg, var(--accent), var(--accent-dim));
    box-shadow: 0 0 14px color-mix(in srgb, var(--accent) 45%, transparent);
  }
  .brand-text { min-width: 0; }
  h1 { font-size: var(--fs-lg); font-weight: 650; letter-spacing: -.01em; line-height: 1.15; white-space: nowrap; }
  h1 span { color: var(--text-dim); font-weight: 450; }
  .subtitle {
    color: var(--text-dim); font-size: var(--fs-xs); line-height: 1.35; margin-top: 2px;
    white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  }
  .header-right { display: flex; align-items: center; gap: 10px; flex: 0 0 auto; }
  .theme-selector { display: flex; gap: 5px; align-items: center; }
  .theme-dot {
    width: 13px; height: 13px; border-radius: 50%; border: 2px solid transparent;
    padding: 0; cursor: pointer; opacity: .55; transition: .15s ease;
    background: var(--accent);
  }
  .theme-dot:hover { opacity: .85; }
  .theme-dot.active { opacity: 1; border-color: var(--text); box-shadow: 0 0 7px var(--accent); }
  .system-state {
    display: flex; align-items: center; gap: 6px;
    border: 1px solid var(--border); border-radius: var(--radius-pill);
    padding: 4px 10px; color: var(--text-mid); font-size: var(--fs-xs); white-space: nowrap;
  }
  .lang-toggle {
    color: var(--text-mid); background: var(--bg-panel); border: 1px solid var(--border);
    border-radius: var(--radius-pill); padding: 4px 9px; font-size: var(--fs-xs); font-weight: 600;
    letter-spacing: .03em; transition: .15s ease;
  }
  .lang-toggle:hover { background: var(--accent-soft); border-color: var(--accent-dim); color: var(--text); }
  .window-controls { display: flex; gap: 5px; }
  .win-btn {
    width: 20px; height: 20px; border-radius: 50%; border: 1px solid var(--border);
    background: var(--bg-panel); color: var(--text-mid); font-size: 9px;
    display: flex; align-items: center; justify-content: center; padding: 0; transition: .15s ease;
  }
  .win-btn:hover { background: var(--accent-soft); border-color: var(--accent-dim); color: var(--text); }
  .win-close:hover { background: var(--status-error); color: var(--on-accent); border-color: var(--status-error); }

  .pulse, .status-dot { display: inline-block; border-radius: 50%; background: var(--text-dim); width: 8px; height: 8px; flex: 0 0 auto; }
  .pulse.connected, .status-dot.ready, .status-dot.idle { background: var(--accent-glow); box-shadow: 0 0 8px var(--accent-glow); }
  .status-dot.working, .status-dot.active { background: var(--status-working); box-shadow: 0 0 8px var(--status-working); }
  .status-dot.done { background: var(--status-unread); box-shadow: 0 0 8px var(--status-unread); }
  .status-dot.waiting, .status-dot.needs_input, .status-dot.needsinput { background: var(--status-waiting); box-shadow: 0 0 8px var(--status-waiting); }
  .status-dot.error { background: var(--status-error); box-shadow: 0 0 8px var(--status-error); }

  .error {
    color: var(--error-text); background: var(--error-bg); border: 1px solid var(--error-border);
    padding: 8px 12px; border-radius: var(--radius-sm); font-size: var(--fs-xs);
  }

  .tab-bar {
    display: flex; gap: 4px; padding: 4px;
    background: var(--bg-inset); border: 1px solid var(--border); border-radius: var(--radius);
  }
  .tab-bar button {
    flex: 1 1 0; min-width: 0; display: flex; align-items: center; justify-content: center; gap: 6px;
    padding: 8px 10px; background: transparent; border: 0; border-radius: var(--radius-sm);
    color: var(--text-dim); font-size: var(--fs-sm); font-weight: 600; white-space: nowrap;
    transition: .15s ease;
  }
  .tab-bar button span { color: var(--accent); font-size: 11px; opacity: .85; }
  .tab-bar button:hover { color: var(--text-mid); background: var(--bg-panel); }
  .tab-bar button.active {
    color: var(--text); background: var(--bg-panel);
    box-shadow: inset 0 0 0 1px var(--border-strong), 0 4px 12px var(--shadow-soft);
  }

  .panel { background: var(--bg-panel); border: 1px solid var(--border); border-radius: var(--radius); padding: var(--space-4); }
  .panel-title {
    display: flex; align-items: center; justify-content: space-between; gap: 8px;
    padding-bottom: 10px; margin-bottom: 12px; border-bottom: 1px solid var(--border);
  }
  .panel-title > div { display: flex; align-items: center; gap: 8px; }
  .panel-title span { color: var(--accent); font-size: 11px; }
  .panel-title h2 { font-size: var(--fs-md); font-weight: 650; }
  .panel-title small { color: var(--text-dim); font-size: var(--fs-xs); }

  .section-heading { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; margin-bottom: 12px; }
  .section-heading > div { display: flex; align-items: center; gap: 8px; }
  .section-heading span { color: var(--accent); font-size: 11px; }
  .section-heading h2 { font-size: var(--fs-md); font-weight: 650; }
  .section-heading p { color: var(--text-dim); font-size: var(--fs-xs); margin: 0; }

  button { cursor: pointer; }
  button:disabled { cursor: not-allowed; opacity: .45; }
  .secondary {
    color: var(--text); background: var(--bg-inset); border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm); padding: 8px 12px; font-size: var(--fs-sm); font-weight: 550;
    transition: .15s ease;
  }
  .secondary:hover:not(:disabled) { border-color: var(--accent-dim); background: var(--bg-elevated); }
  .primary {
    color: var(--on-accent); background: var(--accent); border: 1px solid var(--accent);
    border-radius: var(--radius-sm); padding: 8px 12px; font-size: var(--fs-sm); font-weight: 700;
    transition: .15s ease;
  }
  .primary:hover:not(:disabled) { filter: brightness(1.07); }
  .text-button { border: 0; background: transparent; color: var(--accent); font-size: var(--fs-xs); padding: 6px 0; }
  .text-button:hover { text-decoration: underline; }

  select, input {
    width: 100%; color: var(--text); background: var(--bg-inset);
    border: 1px solid var(--border-strong); border-radius: var(--radius-xs);
    padding: 6px 8px; font-size: var(--fs-xs);
  }
  select:focus-visible, input:focus-visible, button:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }

  /* Status badges (device panel) */
  .agent-meta { display: flex; flex-direction: column; align-items: flex-end; gap: 3px; }
  .badge { font-size: var(--fs-2xs); text-transform: uppercase; letter-spacing: .07em; font-weight: 600; color: var(--text-mid); }
  .badge.active { color: var(--status-working); }
  .badge.offline { color: var(--text-dim); }

  /* Hardware deck */
  .console-section { display: flex; flex-direction: column; gap: 12px; }
  .deck-editor-heading { display: flex; align-items: center; gap: 10px; }
  .deck-editor-heading small { color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); }
  .deck-editor-heading button { border-radius: var(--radius-sm); padding: 6px 12px; font-size: var(--fs-xs); }
  .deck-editor-heading button.active { color: var(--on-accent); background: var(--accent); border-color: var(--accent); }
  .config-tip {
    max-width: 640px; margin: 0 auto; padding: 8px 12px; border: 1px solid var(--border);
    border-radius: var(--radius-sm); background: var(--bg-inset);
    display: flex; align-items: center; justify-content: space-between; gap: 10px;
    color: var(--text-mid); font-size: var(--fs-xs);
  }
  .config-tip .text-button { padding: 0; white-space: nowrap; }

  .deck {
    position: relative; max-width: 660px; width: 100%; margin: 0 auto; padding: 16px;
    border: 1px solid var(--border-strong); border-radius: var(--radius);
    background: linear-gradient(145deg, var(--device-body), var(--device-body-2) 62%);
    box-shadow: 0 18px 44px var(--shadow-strong), inset 0 1px var(--inset-highlight);
  }
  .deck-screws { position: absolute; inset: 8px; display: flex; justify-content: space-between; pointer-events: none; }
  .deck-screws i { width: 4px; height: 4px; border-radius: 50%; background: var(--device-screw); box-shadow: 0 0 0 1px var(--border-strong); }
  .deck-screws i:nth-child(3), .deck-screws i:nth-child(4) { align-self: flex-end; }
  .agent-key-row, .function-grid { display: grid; grid-template-columns: repeat(5, 1fr); gap: 7px; }
  .agent-slots-unavailable {
    grid-column: 1 / -1; height: 42px; display: grid; place-items: center;
    color: var(--text-dim); font: var(--fs-xs) var(--font-mono);
    border: 1px dashed var(--border-strong); border-radius: var(--radius-sm);
  }
  .deck-key {
    color: var(--text-mid); background: linear-gradient(var(--device-key-top), var(--device-key-bottom));
    border: 1px solid var(--border-strong); border-bottom-color: var(--device-key-edge);
    border-radius: var(--radius-sm); box-shadow: 0 3px 0 var(--device-key-shadow), inset 0 1px var(--inset-highlight);
    transition: .12s ease;
  }
  .deck-key:active, .deck-key.chosen {
    transform: translateY(2px); box-shadow: 0 1px 0 var(--device-key-shadow), inset 0 0 10px var(--accent-soft);
    border-color: var(--accent-dim);
  }
  .agent-key { width: 100%; height: 42px; position: relative; }
  .agent-key span { display: block; font-size: var(--fs-2xs); font-weight: 700; text-transform: uppercase; }
  .agent-key > small { position: absolute; left: 5px; top: 4px; color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); }
  .agent-key .status-dot { position: absolute; top: 5px; right: 6px; width: 6px; height: 6px; }
  .agent-slot-editor { min-width: 0; display: grid; gap: 5px; }
  .agent-slot-editor select { padding: 4px 4px; font-size: var(--fs-2xs); }

  .deck-middle { display: grid; grid-template-columns: 1fr 80px; align-items: center; gap: 14px; padding: 16px 22px; }
  .lcd {
    height: 92px; color: var(--lcd-text); padding: 7px 9px; border: 4px solid var(--device-key-edge); border-radius: var(--radius-sm);
    background: radial-gradient(circle at 72% 0, var(--lcd-bg-a), var(--lcd-bg-b) 62%);
    box-shadow: inset 0 0 16px #000, 0 0 0 1px var(--border-strong);
    font-family: var(--font-mono); overflow: hidden; display: flex; flex-direction: column;
  }
  .lcd strong { display: block; font-size: var(--fs-xs); margin: 0; color: var(--lcd-accent); }
  .lcd-header { display: flex; align-items: center; justify-content: space-between; padding-bottom: 6px; border-bottom: 1px solid var(--lcd-border); }
  .lcd-header-left { display: flex; align-items: center; gap: 5px; }
  .lcd-header strong { font-size: var(--fs-sm); color: var(--lcd-text-strong); }
  .lcd-status-dot { width: 6px; height: 6px; border-radius: 50%; display: inline-block; background: var(--text-dim); }
  .lcd-status-dot.ready { background: var(--accent-glow); box-shadow: 0 0 5px var(--accent-glow); }
  .lcd-status-dot.active, .lcd-status-dot.working { background: var(--status-working); box-shadow: 0 0 5px var(--status-working); }
  .lcd-status-dot.done { background: var(--status-unread); box-shadow: 0 0 5px var(--status-unread); }
  .lcd-status-dot.waiting, .lcd-status-dot.needs_input { background: var(--status-waiting); box-shadow: 0 0 5px var(--status-waiting); }
  .lcd-status-dot.error { background: var(--status-error); box-shadow: 0 0 5px var(--status-error); }
  .lcd-status-text { font-size: var(--fs-2xs); font-weight: 700; letter-spacing: .08em; color: var(--status-working); }
  .lcd-status-text.waiting, .lcd-status-text.needs_input { color: var(--status-waiting); }
  .lcd-status-text.error { color: var(--status-error); }
  .lcd-status-text.ready { color: var(--accent); }
  .lcd-jobs { font-size: var(--fs-2xs); color: var(--lcd-text-dim); letter-spacing: .06em; font-family: var(--font-mono); }
  .lcd-fn-labels { display: grid; grid-template-columns: repeat(5, 1fr); gap: 2px; padding-top: 4px; margin-top: auto; }
  .lcd-fn-label { background: var(--lcd-fn-bg); border-radius: 2px; text-align: center; padding: 3px 1px; font-size: var(--fs-2xs); color: var(--lcd-fn-text); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .lcd-empty { height: 100%; display: grid; place-content: center; text-align: center; flex: 1; gap: 3px; }
  .lcd-empty strong { color: var(--lcd-text-strong); font-size: var(--fs-sm); }
  .lcd-empty span { color: var(--lcd-text-faint); font-size: var(--fs-2xs); }
  .knob-wrap { display: grid; gap: 6px; justify-items: center; align-content: center; }
  .knob {
    width: 58px; height: 58px; border-radius: 50%; border: 1px solid var(--border-strong);
    background: repeating-radial-gradient(circle, var(--knob-a) 0 3px, var(--knob-b) 4px 6px);
    box-shadow: 0 5px 8px var(--shadow-strong), inset 0 2px var(--inset-highlight); justify-self: center; cursor: ns-resize;
  }
  .knob i { display: block; width: 2px; height: 16px; background: var(--accent); margin: -25px auto 0; transform-origin: 50% 25px; transition: transform .15s ease; }
  .knob-value { color: var(--accent); font: 700 var(--fs-2xs) var(--font-mono); letter-spacing: .05em; text-align: center; }
  .knob-mode { color: var(--text-dim); font: 700 var(--fs-2xs) var(--font-mono); letter-spacing: .05em; text-align: center; }
  .knob-mode.mode-warning { color: var(--status-waiting); }
  .fn-key { height: 42px; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 2px; }
  .fn-key small { color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); }
  .fn-key span { font-size: var(--fs-xs); font-weight: 700; }
  .function-key-editor { min-width: 0; padding: 7px; border: 1px solid var(--border); border-radius: var(--radius-sm); background: var(--bg-inset); }
  .function-key-editor.customized { border-color: var(--accent-dim); box-shadow: inset 0 0 8px var(--accent-soft); }
  .function-key-editor > div { display: grid; grid-template-columns: auto 1fr; column-gap: 5px; min-height: 26px; }
  .function-key-editor small { color: var(--accent); font: 700 var(--fs-2xs) var(--font-mono); }
  .function-key-editor strong { overflow: hidden; text-overflow: ellipsis; color: var(--text); font-size: var(--fs-xs); white-space: nowrap; }
  .function-key-editor span { grid-column: 1 / -1; overflow: hidden; text-overflow: ellipsis; color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); white-space: nowrap; }
  .function-key-editor select { margin-top: 5px; padding: 4px; font-size: var(--fs-2xs); }

  /* Device + notification detail */
  .device-panel .agent-meta { align-items: flex-start; margin: 4px 0 0; }
  .device-panel .agent-meta small { color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); }
  .event-log { background: var(--bg-inset); border: 1px solid var(--border); border-radius: var(--radius-sm); padding: 10px 12px; margin-top: 12px; }
  .event-log span { color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); letter-spacing: .1em; display: block; margin-bottom: 4px; }
  .event-log code { color: var(--text-mid); font-size: var(--fs-xs); word-break: break-all; }

  /* Hooks — compact list so the page fits a laptop without scrolling. */
  .hook-section { display: flex; flex-direction: column; }
  .hook-list { display: flex; flex-direction: column; gap: 8px; }
  .hook-item { border: 1px solid var(--border); border-radius: var(--radius-sm); background: var(--bg-panel); overflow: hidden; }
  .hook-item.installed { border-color: color-mix(in srgb, var(--accent) 35%, var(--border)); }
  .hook-row { display: flex; align-items: center; gap: 12px; padding: 10px 14px; }
  .hook-dot { width: 8px; height: 8px; border-radius: 50%; background: var(--text-dim); flex: 0 0 auto; }
  .hook-item.installed .hook-dot { background: var(--accent); box-shadow: 0 0 7px var(--accent); }
  .hook-name { flex: 1; min-width: 0; }
  .hook-name strong { display: block; font-size: var(--fs-md); }
  .hook-kind {
    margin-left: 6px; padding: 1px 6px; border-radius: var(--radius-pill);
    border: 1px solid var(--border-strong); background: var(--bg-inset);
    color: var(--text-dim); font: 600 var(--fs-2xs) var(--font-mono);
    letter-spacing: .04em; vertical-align: middle;
  }
  .hook-detail { margin: 0; padding: 0 14px 10px 34px; color: var(--text-dim); font-size: var(--fs-xs); line-height: 1.5; }
  .hook-name code { display: block; color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-top: 2px; }
  .hook-state { flex: 0 0 auto; color: var(--text-dim); font: 600 var(--fs-2xs) var(--font-mono); letter-spacing: .04em; }
  .hook-item.installed .hook-state { color: var(--accent); }
  .hook-row > button { flex: 0 0 auto; }
  .hook-dev-toggle { width: 30px; padding: 8px 0; text-align: center; font-size: var(--fs-md); line-height: 1; }
  .hook-dev-toggle.active { border-color: var(--accent-dim); color: var(--accent); }
  .hook-dev { display: flex; gap: 6px; padding: 0 14px 12px 34px; }
  .hook-dev button { flex: 1 1 0; padding: 6px 8px; font-size: var(--fs-2xs); }

  /* Hint preferences */
  .hint-settings { display: flex; align-items: center; gap: 16px; flex-wrap: wrap; padding: var(--space-4); }
  .hint-settings > div:first-child { flex: 1 1 240px; }
  .settings-kicker { color: var(--accent); font: 700 var(--fs-2xs) var(--font-mono); letter-spacing: .16em; display: block; margin-bottom: 6px; }
  .hint-settings h2 { font-size: var(--fs-md); }
  .hint-settings p { color: var(--text-dim); font-size: var(--fs-xs); margin-top: 4px; }
  .toggle-control { display: flex; align-items: center; gap: 7px; margin: 0; white-space: nowrap; color: var(--text-mid); font-size: var(--fs-sm); }
  .toggle-control input { width: 15px; height: 15px; margin: 0; padding: 0; accent-color: var(--accent); }
  .show-hint-button { padding: 7px 12px; white-space: nowrap; }

  footer {
    display: flex; justify-content: space-between;
    color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); padding-top: 4px;
  }

  /* Auxiliary windows */
  .boot-view { width: 100vw; height: 100vh; background: var(--bg); }
  .hint-window { width: 100vw; height: 100vh; min-height: 0; padding: 10px; background: radial-gradient(circle at 90% 0, color-mix(in srgb, var(--accent) 12%, transparent), transparent 38%), var(--bg); overflow: hidden; }
  .panel-window { width: 100vw; height: 100vh; min-height: 0; padding: 0; background: transparent; overflow: hidden; }
  .hint-card { height: 100%; padding: 11px 13px 8px; border: 1px solid var(--accent-dim); border-radius: var(--radius); background: radial-gradient(circle at 90% 0, color-mix(in srgb, var(--accent) 8%, transparent), transparent 38%), var(--bg-panel); box-shadow: inset 0 1px var(--inset-highlight); overflow: hidden; }
  .hint-header { height: 36px; margin: 0 0 8px; display: flex; align-items: center; justify-content: space-between; }
  .hint-agent { display: flex; align-items: center; gap: 11px; }
  .hint-agent .status-dot { flex: 0 0 auto; }
  .hint-agent strong { display: block; font-size: var(--fs-md); line-height: 1.1; }
  .hint-agent small { color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); letter-spacing: .1em; }
  .hint-controls { display: flex; gap: 6px; }
  .hint-controls button { height: 27px; color: var(--text-mid); background: var(--bg-elevated); border: 1px solid var(--border-strong); border-radius: var(--radius-xs); padding: 0 10px; font-size: var(--fs-sm); }
  .hint-controls button:last-child { width: 27px; padding: 0; font-size: 16px; line-height: 1; }
  .hint-controls button.chosen { color: var(--accent); border-color: var(--accent-dim); background: var(--accent-soft); }
  .hint-key-grid { display: grid; grid-template-columns: repeat(5, 1fr); gap: 6px; }
  .hint-key { min-width: 0; height: 50px; padding: 7px 8px; border: 1px solid var(--border-strong); border-bottom-color: var(--border); border-radius: var(--radius-sm); background: linear-gradient(var(--bg-elevated), var(--bg-inset)); box-shadow: inset 0 1px var(--inset-highlight); display: grid; grid-template-columns: auto 1fr; grid-template-rows: auto auto; column-gap: 6px; align-content: center; }
  .hint-key small { color: var(--accent); font: 700 var(--fs-xs) var(--font-mono); }
  .hint-key strong { min-width: 0; color: var(--text); font-size: var(--fs-xs); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .hint-key span { grid-column: 1 / -1; color: var(--text-dim); font: var(--fs-2xs) var(--font-mono); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-top: 3px; }
  .hint-key.unsupported { opacity: .36; }
  .hint-footer { display: block; margin: 6px 0 0; text-align: right; color: var(--text-dim); font-size: var(--fs-2xs); }

  @media (max-width: 760px) {
    .companion-main { padding: 12px 12px 8px; }
    .brand { min-width: 0; }
    .subtitle { white-space: normal; }
    .header-right .system-state { display: none; }
    .tab-bar button { padding: 8px 6px; font-size: var(--fs-xs); }
    .tab-bar button span { display: none; }
    .hint-settings > div:first-child { flex-basis: 100%; }
    .deck { padding: 12px 10px; }
    .agent-key-row, .function-grid { gap: 5px; }
    .deck-middle { padding: 10px 6px; grid-template-columns: 1fr 52px; }
    .knob { width: 46px; height: 46px; }
    .knob i { margin-top: -19px; transform-origin: 50% 19px; }
    .lcd { padding: 6px; height: 104px; }
    .agent-key span { font-size: 9px; }
    .section-heading p { display: none; }
  }
</style>
