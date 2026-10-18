import { invoke } from '@tauri-apps/api/core';
import type { AppSnapshot, HookIntegrationState, SessionTab } from './types';

const browserFallback = async <T>(path: string, init?: RequestInit): Promise<T> => {
  const response = await fetch(`http://127.0.0.1:34115${path}`, {
    ...init,
    headers: { 'content-type': 'application/json', ...(init?.headers ?? {}) }
  });
  if (!response.ok) throw new Error(await response.text());
  return response.json() as Promise<T>;
};

const inTauri = () => '__TAURI_INTERNALS__' in window;

export const getState = () =>
  inTauri() ? invoke<AppSnapshot>('get_state') : browserFallback<AppSnapshot>('/api/state');

export const pressKey = (key: string) =>
  inTauri()
    ? invoke<AppSnapshot>('send_hardware_event', { event: { type: 'key_down', key } })
    : browserFallback<AppSnapshot>('/api/hardware/key', {
        method: 'POST',
        body: JSON.stringify({ key })
      });

export const rotateKnob = (delta: number) =>
  inTauri()
    ? invoke<AppSnapshot>('send_hardware_event', { event: { type: 'encoder', delta } })
    : browserFallback<AppSnapshot>('/api/hardware/encoder', {
        method: 'POST',
        body: JSON.stringify({ delta })
      });

export const herdrAgentAction = (paneId: string, action: 'focus' | 'continue' | 'stop') => {
  if (!inTauri()) throw new Error('Herdr 触屏操作仅在 Tauri 桌面应用中可用');
  return invoke<AppSnapshot>('herdr_agent_action', { paneId, action });
};

export type OpenSessionOutcome =
  | 'opened'
  | 'switched_not_raised'
  | 'target_gone'
  | 'superseded'
  | 'focus_failed';

export const openDesktopSession = (identity: string, expected?: { kind: import('./types').AttentionClass; version: number }) => {
  if (!inTauri()) throw new Error('按会话打开仅在 Tauri 桌面应用中可用');
  return invoke<OpenSessionOutcome>('open_desktop_session', { identity, expectedKind: expected?.kind, expectedVersion: expected?.version });
};

export interface SessionPanelPreferences {
  x: number | null;
  y: number | null;
  collapsed: boolean;
  visible: boolean;
  show_all: boolean;
  tab: SessionTab;
  tab_order: SessionTab[];
}

export const getSessionPanelPreferences = () => {
  return inTauri() ? invoke<SessionPanelPreferences>('get_session_panel_preferences') : browserFallback<SessionPanelPreferences>('/api/session-panel/preferences');
};

export const setSessionPanelCollapsed = (collapsed: boolean) => {
  if (!inTauri()) throw new Error('会话浮窗折叠仅在 Tauri 桌面应用中可用');
  return invoke<void>('set_session_panel_collapsed', { collapsed });
};

export const hideSessionPanel = () => {
  if (!inTauri()) throw new Error('会话浮窗仅在 Tauri 桌面应用中可用');
  return invoke<void>('hide_session_panel');
};

export const showSessionPanel = () => {
  if (!inTauri()) throw new Error('会话浮窗仅在 Tauri 桌面应用中可用');
  return invoke<void>('show_session_panel');
};

export const markSessionRead = (identity: string, unreadSinceMs: number) => {
  if (!inTauri()) throw new Error('标记已读仅在 Tauri 桌面应用中可用');
  return invoke<boolean>('mark_session_read', { identity, unreadSinceMs });
};

export const setSessionPanelView = (showAll: boolean) => {
  if (!inTauri()) throw new Error('会话浮窗视图偏好仅在 Tauri 桌面应用中可用');
  return invoke<void>('set_session_panel_view', { showAll });
};

/** Hide a card without acknowledging the session: it cannot clear unread or
 *  resolve a wait, which only 标记已读 and the source may do. */
export const dismissSessionNotification = (eventId: string) => {
  if (!inTauri()) throw new Error('通知卡片仅在 Tauri 桌面应用中可用');
  return invoke<boolean>('dismiss_session_notification', { eventId });
};

/** Grow the collapsed bar for one card, then put it back. */
export const setSessionNotificationOverlay = (active: boolean) => {
  if (!inTauri()) throw new Error('会话浮窗仅在 Tauri 桌面应用中可用');
  return invoke<void>('set_session_notification_overlay', { active });
};

export const setMockStatus = (agentId: string, status: string) =>
  browserFallback<AppSnapshot>('/api/mock/status', {
    method: 'POST',
    body: JSON.stringify({ agent_id: agentId, status })
  });

export const clearMocks = () =>
  browserFallback<AppSnapshot>('/api/mock/reset', { method: 'POST', body: '{}' });

export const listHookIntegrations = () =>
  inTauri()
    ? invoke<HookIntegrationState[]>('list_hook_integrations')
    : browserFallback<HookIntegrationState[]>('/api/hook-integrations');

export const installAgentHooks = (agentId: string) => {
  if (!inTauri()) throw new Error('请在 Tauri 桌面应用中安装 hooks');
  return invoke<HookIntegrationState[]>('install_agent_hooks', { agentId });
};

export const uninstallAgentHooks = (agentId: string) => {
  if (!inTauri()) throw new Error('请在 Tauri 桌面应用中卸载 hooks');
  return invoke<HookIntegrationState[]>('uninstall_agent_hooks', { agentId });
};

/** The DSH profile plugin is installed by DSH's own CLI, which can take a
 *  while on a cold `npx` cache; it runs off the main thread. */
export const installDshPlugin = () => {
  if (!inTauri()) throw new Error('请使用打包后的桌面应用安装 DSH 插件');
  return invoke<HookIntegrationState[]>('install_dsh_plugin');
};

export const uninstallDshPlugin = () => {
  if (!inTauri()) throw new Error('请使用打包后的桌面应用卸载 DSH 插件');
  return invoke<HookIntegrationState[]>('uninstall_dsh_plugin');
};

export const sendMockHook = (agentId: string, hookEventName: string) =>
  browserFallback<AppSnapshot>(`/api/mock/hooks/${agentId}`, {
    method: 'POST',
    body: JSON.stringify({
      hook_event_name: hookEventName,
      session_id: `mock-${agentId}`,
      cwd: '/tmp/agent-deck-mock'
    })
  });


export const setHintPreferences = (autoShow: boolean, pinned: boolean) => {
  if (!inTauri()) throw new Error('功能键浮层仅在 Tauri 桌面应用中可用');
  return invoke<AppSnapshot>('set_hint_preferences', { autoShow, pinned });
};

export const showFunctionKeyHint = () => {
  if (!inTauri()) throw new Error('功能键浮层仅在 Tauri 桌面应用中可用');
  return invoke<void>('show_function_key_hint');
};

export const hideFunctionKeyHint = () => {
  if (!inTauri()) throw new Error('功能键浮层仅在 Tauri 桌面应用中可用');
  return invoke<void>('hide_function_key_hint');
};

export const assignAgentSlot = (slot: string, agentId: string) => {
  if (!inTauri()) throw new Error('Agent 槽位配置仅在 Tauri 桌面应用中可用');
  return invoke<AppSnapshot>('assign_agent_slot', { slot, agentId });
};

export const setKeyBinding = (agentId: string, surface: string, key: string, actionId: string) => {
  if (!inTauri()) throw new Error('功能键配置仅在 Tauri 桌面应用中可用');
  return invoke<AppSnapshot>('set_key_binding', { agentId, surface, key, actionId });
};

export const resetKeyBinding = (agentId: string, surface: string, key: string) => {
  if (!inTauri()) throw new Error('功能键配置仅在 Tauri 桌面应用中可用');
  return invoke<AppSnapshot>('reset_key_binding', { agentId, surface, key });
};

export const resetAgentBindings = (agentId: string) => {
  if (!inTauri()) throw new Error('功能键配置仅在 Tauri 桌面应用中可用');
  return invoke<AppSnapshot>('reset_agent_bindings', { agentId });
};

export const setAgentEnabled = (agentId: string, enabled: boolean) => {
  if (!inTauri()) throw new Error('Agent 监控开关仅在 Tauri 桌面应用中可用');
  return invoke<AppSnapshot>('set_agent_enabled', { agentId, enabled });
};

export const dshDiagnostics = () => {
  if (!inTauri()) throw new Error('DSH 诊断仅在 Tauri 桌面应用中可用');
  return invoke<import('./types').DshDiagnostics>('dsh_diagnostics');
};

export const setCompanionPage = (page: number) => invoke<void>('set_companion_page', { page });
export const openCompanionSlot = (index: number, revision: number) => invoke<void>('open_companion_slot', { index, revision });

export const setSessionPanelTab = (tab:SessionTab) => inTauri()
 ? invoke<void>('set_session_panel_tab',{tab})
 : browserFallback<void>('/api/session-panel/tab',{method:'POST',body:JSON.stringify({tab})});
export const setSessionPanelTabOrder = (order:SessionTab[]) => inTauri()
 ? invoke<void>('set_session_panel_tab_order',{order})
 : browserFallback<void>('/api/session-panel/tab-order',{method:'POST',body:JSON.stringify({order})});

export const setUiTheme = (theme: import('./types').UiTheme) => inTauri()
 ? invoke<void>('set_ui_theme',{theme})
 : browserFallback<void>('/api/ui-theme',{method:'POST',body:JSON.stringify({theme})});

/** Mirrors the WebView locale to Rust so the native tray menu uses the same
 *  language as the companion window. */
export const setUiLocale = (locale: 'zh' | 'en') => inTauri()
 ? invoke<void>('set_ui_locale',{locale})
 : browserFallback<void>('/api/ui-locale',{method:'POST',body:JSON.stringify({locale})});
