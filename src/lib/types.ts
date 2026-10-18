export type AgentStatus = 'offline' | 'idle' | 'working' | 'done' | 'needs_input' | 'error' | 'unknown';

export interface AgentState {
  id: string;
  display_name: string;
  status: AgentStatus;
  running: boolean;
  pid: number | null;
  executable: string;
  /** The user still wants this agent monitored. A disabled agent keeps its
   *  slot binding but is not detected, polled or listed. */
  enabled: boolean;
  task: string | null;
  workspace: string | null;
  active: boolean;
  unread: boolean;
  sessions: AgentSession[];
}

export interface AgentSession {
  id: string;
  label: string;
  status: AgentStatus;
  unread: boolean;
  focused: boolean;
}

// Floating-panel projection: cross-agent session rows with a stable
// transport-derived identity, independent of the selected agent.
export type AttentionClass = 'needs_input' | 'error' | 'done';

export interface DesktopSession {
  error_acknowledged?: boolean;
  activity?: {
    last_output_at: number | null;
    last_keyboard_at: number | null;
    last_activity_at: number | null;
    last_prompt_submitted_at: number | null;
    last_completed_at: number | null;
    activity_source: string | null;
    coverage: string;
  };
  recent_activity_ms?: number;
  row_key: string;
  identity: string | null;
  agent_id: string;
  display_name: string;
  agent_short: string;
  source: string;
  label: string;
  status: AgentStatus;
  unread: boolean;
  attention: AttentionClass | null;
  /** When the row's status last changed. `updated_at_ms` is a poll stamp. */
  event_at_ms: number;
  observed_since_ms: number;
  event_is_activity: boolean;
  unread_since_ms: number;
  project_id: string | null;
  project_name: string | null;
  project_hint: string | null;
  host: string;
  stale: boolean;
  updated_at_ms: number;
}

export interface DesktopSummary {
  working: number;
  waiting: number;
  unread: number;
  error: number;
}

/** One temporary notification card. Rust decides it from session identity +
 *  kind + source version, so neither a poll nor a broadcast can replay it. */
export interface SessionNotification {
  event_id: string;
  key: string;
  row_key: string;
  kind: AttentionClass;
  agent_short: string;
  source: string;
  label: string;
  project_name: string | null;
  event_version: number;
  created_at_ms: number;
  openable: boolean;
}

/** Actionable attention counts, over the same rows the list shows. */
export interface AttentionCounts {
  needs_input: number;
  error: number;
  done: number;
}

export interface DeviceState {
  mode: 'mock' | 'hid_vendor' | 'hid';
  connected: boolean;
  port: string | null;
  baud_rate: number;
  last_event: string | null;
  knob_position: number;
  reasoning_level: 'low' | 'medium' | 'high' | 'extrhigh';
  reasoning_actual: 'low' | 'medium' | 'high' | 'extrhigh' | null;
  reasoning_sync: 'idle' | 'syncing' | 'confirmed' | 'unavailable' | 'permission_denied' | 'wrong_mode' | 'timeout';
  native_encoder: boolean;
  encoder_mode: 'reasoning' | 'composer-navigation' | 'conversation-scroll' | 'custom' | null;
}

export type SessionTab = 'attention' | 'codex' | 'others';
export interface CompanionNavigation {
  visible_tabs?: SessionTab[];
  encoder_mode?: 'tabs' | 'pages';
  hardware_ready?: boolean;
  active: boolean; visible: boolean; show_all: boolean; tab: SessionTab; tab_order: SessionTab[]; page: number; pages: number;
  revision: number; slots: DesktopSession[]; slot_keys?: string[]; feedback: string | null;
}
export interface AppSnapshot {
  chatgpt_activity_metrics?: { sampled_at_ms: number; scan_ms: number; sample_count: number; detection_p50_ms: number | null; detection_p95_ms: number | null; healthy: boolean; pushed_at_ms: number };
  companion_navigation?: CompanionNavigation;
  agents: AgentState[];
  agent_slots: AgentSlotState[];
  jobs: AgentJob[];
  active_panels: ActivePanel[];
  sessions: AgentSession[];
  desktop_sessions: DesktopSession[];
  desktop_summary: DesktopSummary;
  session_notifications: SessionNotification[];
  notification_overflow: number;
  attention_counts: AttentionCounts;
  active_profile: string;
  active_agent: string | null;
  active_surface: 'desktop' | 'terminal' | 'herdr_pane' | 'web' | 'unknown' | null;
  function_keys: FunctionKeyState[];
  action_catalog: ActionCatalogItem[];
  hint_preferences: HintPreferences;
  ui_theme: UiTheme;
  ui_locale: 'zh' | 'en';
  device: DeviceState;
  attention_count: number;
  attention_urgent: number;
  chatgpt_usage: ChatGptUsage;
  herdr?: HerdrSnapshot;
  updated_at_ms: number;
}

export interface ChatGptUsage {
  model: string;
  input_tokens: number;
  output_tokens: number;
  remaining_tokens: number | null;
  remaining_percent: number | null;
  remaining_5h_percent: number | null;
  remaining_weekly_percent: number | null;
  reset_seconds: number | null;
  reset_time: string | null;
  reset_5h_time: string | null;
  reset_weekly_time: string | null;
  updated_at_ms: number;
  source: string;
}

export type PanelTransport =
  | { kind: 'herdr'; socket_path: string | null; pane_id: string }
  | { kind: 'tmux'; socket_path: string; pane_id: string; pid: number | null }
  | { kind: 'terminal'; pid: number }
  | { kind: 'unknown' };

export interface ActivePanel {
  id: string; agent_id: string; session_id: string; slot: number; label: string;
  status: AgentStatus; cwd: string | null; jobs_working: number; jobs_waiting: number;
  jobs_error: number; updated_at_ms: number; transport: PanelTransport;
}

export interface HerdrAgentInfo {
  pane_id: string; workspace_id: string; tab_id: string; agent: string;
  display_name: string; status: 'idle' | 'working' | 'blocked' | 'done' | 'unknown';
  title: string | null; cwd: string | null; focused: boolean; attention: boolean;
  state_change_seq: number; model: string | null; summary: string | null;
}

export interface HerdrSnapshot {
  connected: boolean; focused_pane_id: string | null; agents: HerdrAgentInfo[];
}

export interface HintPreferences {
  auto_show: boolean;
  pinned: boolean;
}

/** Shared palette id; see `src/app.css` for the actual colours. */
export type UiTheme = 'classic' | 'modern' | 'tech' | 'fresh';

export interface FunctionKeyState {
  key: string;
  label: string;
  hint: string;
  supported: boolean;
  action_id: string | null;
  source: string;
  customized: boolean;
}

export interface AgentSlotState {
  key: string;
  agent_id: string;
}

export interface ActionCatalogItem {
  action_id: string;
  label: string;
  hint: string;
}

export type JobStatus = 'ready' | 'running' | 'waiting' | 'completed' | 'failed' | 'interrupted';

export interface AgentJob {
  id: string;
  agent_id: string;
  session_id: string;
  status: JobStatus;
  cwd: string | null;
  last_event: string;
  updated_at_ms: number;
  agent_type: string | null;
}

export interface HookIntegrationState {
  agent_id: string;
  display_name: string;
  installed: boolean;
  config_path: string;
  detail: string;
  /** `hooks` = lifecycle bridge in an agent's own config;
   *  `plugin` = installed into DSH's profile through DSH's own installer. */
  kind: 'hooks' | 'plugin';
}

/** One DSH session as reported by its authenticated Connection RPC. */
export interface DshApiSession {
  sessionId: string;
  updatedAt: number;
  running: boolean;
  blank: boolean;
  cwd: string | null;
  title: string | null;
}

/** Best-effort picture of the DSH integration for the Settings page. A missing
 *  server, credentials or bridge is reported as a field, not an error. */
export interface DshDiagnostics {
  home: string;
  credentials: boolean;
  process_running: boolean;
  web_url: string | null;
  storage_sessions: number;
  api_sessions: DshApiSession[];
  api_error: string | null;
  bridge_tabs: number | null;
  bridge_browsers: string[];
  bridge_error: string | null;
  /** Pushes accepted from the deck plugin since launch. Non-zero means the
   *  plugin channel works and the 5s poll is only a fallback. */
  pushes: number;
  last_push_at_ms: number;
}
