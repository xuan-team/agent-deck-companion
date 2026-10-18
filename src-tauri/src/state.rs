use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc, Arc, Mutex, RwLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::agent_actions::{ActionCatalogItem, AgentSurface, FunctionKeyState};
use crate::plugin;
use crate::profile;
use crate::vendor_hid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Offline,
    Idle,
    Working,
    Done,
    #[serde(rename = "needs_input", alias = "needsinput")]
    NeedsInput,
    Error,
    Unknown,
}

impl AgentStatus {
    pub(crate) fn priority(self) -> u8 {
        match self {
            AgentStatus::Error => 6,
            AgentStatus::NeedsInput => 5,
            AgentStatus::Done => 4,
            AgentStatus::Working => 3,
            AgentStatus::Idle => 2,
            AgentStatus::Unknown => 1,
            AgentStatus::Offline => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentState {
    pub id: String,
    pub display_name: String,
    pub status: AgentStatus,
    pub running: bool,
    pub pid: Option<u32>,
    pub executable: String,
    /// Whether the user still wants this agent monitored. A disabled agent
    /// keeps its slot binding but is never detected, polled or projected.
    pub enabled: bool,
    pub task: Option<String>,
    pub workspace: Option<String>,
    pub active: bool,
    pub unread: bool,
    pub sessions: Vec<AgentSession>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceState {
    pub mode: String,
    pub connected: bool,
    pub port: Option<String>,
    pub baud_rate: u32,
    pub last_event: Option<String>,
    pub knob_position: u8,
    pub reasoning_level: String,
    pub reasoning_actual: Option<String>,
    pub reasoning_sync: String,
    pub native_encoder: bool,
    pub encoder_mode: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Ready,
    Running,
    Waiting,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentJob {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub status: JobStatus,
    pub cwd: Option<String>,
    pub last_event: String,
    pub updated_at_ms: u128,
    pub agent_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelTransport {
    Herdr {
        socket_path: Option<String>,
        pane_id: String,
    },
    Tmux {
        socket_path: String,
        pane_id: String,
        pid: Option<u32>,
    },
    Terminal {
        pid: u32,
    },
    DshWeb {
        session_id: String,
    },
    /// A ChatGPT desktop thread, activated with the `codex://threads/<id>` link.
    CodexThread {
        thread_id: String,
    },
    Unknown,
}

/// Drop every trace of an agent the user turned off: detection results, panel
/// rows and jobs. Called on each projection so the UI, the tray and the deck
/// never render a disabled agent, whichever thread last wrote the snapshot.
///
/// The `AgentState` itself stays in `agents` with `enabled: false`, because
/// that vector is also the Settings list that can turn it back on.
pub(crate) fn prune_disabled_agents(snapshot: &mut AppSnapshot) {
    let disabled = snapshot
        .agents
        .iter()
        .filter(|agent| !agent.enabled)
        .map(|agent| agent.id.clone())
        .collect::<Vec<_>>();
    if disabled.is_empty() {
        return;
    }
    for agent in snapshot.agents.iter_mut().filter(|agent| !agent.enabled) {
        agent.status = AgentStatus::Offline;
        agent.running = false;
        agent.pid = None;
        agent.task = None;
        agent.workspace = None;
        agent.active = false;
        agent.unread = false;
        agent.sessions.clear();
    }
    snapshot
        .active_panels
        .retain(|panel| !disabled.contains(&panel.agent_id));
    snapshot
        .jobs
        .retain(|job| !disabled.contains(&job.agent_id));
}

/// Stable focus identity for a panel. `ActivePanel.id` is *not* usable here:
/// hook traffic rewrites it whenever an agent reports a different session_id
/// for the same pane (directory name vs UUID, see `hooks::panel_match`), which
/// unlights a Herdr row seconds after it was focused. The transport carries
/// the pane identity the hook dedup itself matches on, so it stays put.
/// Live panels are authoritative; retired native jobs must not keep an
/// agent's header in error after that session has been replaced.
pub(crate) fn refresh_live_agent_statuses(snapshot: &mut AppSnapshot) {
    for agent in &mut snapshot.agents {
        // Herdr's aggregate has its own assigned-agent exclusion rules, and a
        // disabled agent must not be relit by a panel that outlived its switch.
        if agent.id == "herdr" || !agent.enabled {
            continue;
        }
        if let Some(status) = snapshot
            .active_panels
            .iter()
            .filter(|panel| panel.agent_id == agent.id && panel.status != AgentStatus::Offline)
            .map(|panel| panel.status)
            .max_by_key(|status| status.priority())
        {
            agent.running = true;
            agent.status = status;
        }
    }
}

#[cfg(target_os = "macos")]
fn session_is_focused(
    panel: &ActivePanel,
    herdr: &crate::herdr::HerdrSnapshot,
    recorded: Option<&str>,
) -> bool {
    if let PanelTransport::Herdr {
        socket_path,
        pane_id,
    } = &panel.transport
    {
        if herdr.connected && crate::herdr::socket_paths_match(socket_path.as_deref(), None) {
            if let Some(focused) = herdr.focused_pane_id.as_deref() {
                return pane_id == focused;
            }
        }
    }
    recorded.is_some_and(|key| panel_focus_key(panel).as_deref() == Some(key))
}

pub fn panel_focus_key(panel: &ActivePanel) -> Option<String> {
    match &panel.transport {
        PanelTransport::Herdr {
            socket_path,
            pane_id,
        } => {
            #[cfg(target_os = "macos")]
            if !crate::herdr::socket_paths_match(socket_path.as_deref(), None) {
                let socket = crate::herdr::socket_identity(socket_path.as_deref())?;
                return Some(format!("herdr:{}:{pane_id}", socket.display()));
            }
            Some(format!("herdr:{pane_id}"))
        }
        PanelTransport::Tmux {
            socket_path,
            pane_id,
            ..
        } => Some(format!("tmux:{socket_path}:{pane_id}")),
        PanelTransport::Terminal { pid } => Some(format!("terminal:{pid}")),
        PanelTransport::DshWeb { session_id } => Some(format!("dsh:{session_id}")),
        PanelTransport::CodexThread { thread_id } => Some(format!("thread:{thread_id}")),
        PanelTransport::Unknown => None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivePanel {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub slot: u8,
    pub label: String,
    pub status: AgentStatus,
    pub cwd: Option<String>,
    pub jobs_working: u16,
    pub jobs_waiting: u16,
    pub jobs_error: u16,
    pub updated_at_ms: u128,
    /// When `status` last actually changed. `updated_at_ms` is only a liveness
    /// stamp that every poll refreshes, so it must never order or time a row.
    pub status_since_ms: u128,
    /// Source content/prompt time, separate from an observed status transition.
    pub last_activity_ms: u128,
    /// First local observation, never presented as a source activity time.
    pub first_seen_ms: u128,
    pub transport: PanelTransport,
    pub unread: bool,
    /// When `unread` was latched. Zero after the session is acknowledged.
    pub unread_since_ms: u128,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AgentSession {
    pub id: String,
    pub label: String,
    pub status: AgentStatus,
    pub unread: bool,
    /// The session the desktop actually raised most recently for this agent.
    pub focused: bool,
}

/// Bound on recent Desktop thread discovery; the combined floating-panel
/// projection must not truncate other agents or unread rows.
pub(crate) const DESKTOP_SESSION_LIMIT: usize = 12;

/// One desktop session row for the floating panel, independent of the LCD
/// projection and of which agent the deck currently has selected. `identity`
/// is `panel_focus_key` — the stable transport-derived key, never the mutable
/// `ActivePanel.id` a hook can rewrite.
#[derive(Debug, Clone, Serialize)]
pub struct DesktopSession {
    pub error_acknowledged: bool,
    pub activity: crate::session_activity::Activity,
    pub row_key: String,
    pub identity: Option<String>,
    pub agent_id: String,
    pub display_name: String,
    /// Short Agent label for the row chip: Codex / Qwen / Claude / DSH…
    pub agent_short: String,
    /// Short host label paired with `agent_short` ("Desktop", "Herdr"…).
    /// `host` keeps the full text for tooltips and legacy views.
    pub source: String,
    pub label: String,
    pub status: AgentStatus,
    pub unread: bool,
    /// Why this row belongs in the attention queue, or `None` for ordinary
    /// background. Computed once here so the UI, the tray and the later
    /// notification layer share one definition.
    pub attention: Option<AttentionClass>,
    /// When the status behind this row last changed. Orders the attention queue
    /// and feeds the row's relative time; `updated_at_ms` is a poll stamp.
    pub event_at_ms: u128,
    pub observed_since_ms: u128,
    pub event_is_activity: bool,
    /// Completion version observed by the UI, required for acknowledgement.
    pub unread_since_ms: u128,
    /// Project identity: a normalized cwd. Never guessed — rows without a
    /// usable cwd group under 未识别项目.
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    /// Parent directory name, shown only to disambiguate same-name projects.
    pub project_hint: Option<String>,
    pub host: String,
    /// The Herdr host stopped reporting, so this row's status has no live
    /// source any more. Never presented as a current state without a stamp.
    pub stale: bool,
    pub updated_at_ms: u128,
    /// Content/activity recency; never the snapshot polling timestamp.
    pub recent_activity_ms: u128,
}

/// The three states that mean "a human should look at this". Stale hosts and
/// offline rows never qualify: their state cannot be acted on, and a latched
/// Done on a dead row would stick in the queue forever.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionClass {
    NeedsInput,
    Error,
    Done,
}

/// Cross-agent counts for the floating panel header, computed over every
/// projected session, not over the currently selected agent.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DesktopSummary {
    pub working: usize,
    pub waiting: usize,
    pub unread: usize,
    pub error: usize,
}

fn desktop_host_label(
    panel: &ActivePanel,
    herdr_connected: bool,
    focused_pane_id: Option<&str>,
) -> String {
    match &panel.transport {
        PanelTransport::Herdr { pane_id, .. } => {
            if !herdr_connected {
                "Herdr (失联)".into()
            } else if Some(pane_id.as_str()) == focused_pane_id {
                "Herdr · 当前".into()
            } else {
                "Herdr".into()
            }
        }
        PanelTransport::Tmux { .. } => "Tmux".into(),
        PanelTransport::Terminal { .. } => "Terminal".into(),
        PanelTransport::DshWeb { .. } => "DSH".into(),
        PanelTransport::CodexThread { .. } => "Codex Desktop".into(),
        PanelTransport::Unknown => "未知宿主".into(),
    }
}

/// Attention membership is derived from the live reported state: NeedsInput
/// and Error leave only when the source recovers — opening or reading does
/// not resolve them. Done leaves via the unread acknowledgement. Offline and
/// stale rows never enter the queue (§8.2).
fn desktop_attention(panel: &ActivePanel, herdr_connected: bool) -> Option<AttentionClass> {
    let host_gone = !herdr_connected && matches!(&panel.transport, PanelTransport::Herdr { .. });
    if host_gone || panel.status == AgentStatus::Offline {
        return None;
    }
    match panel.status {
        AgentStatus::NeedsInput => Some(AttentionClass::NeedsInput),
        AgentStatus::Error => Some(AttentionClass::Error),
        _ if panel.unread => Some(AttentionClass::Done),
        _ => None,
    }
}

fn desktop_agent_short(agent_id: &str, display_name: &str) -> String {
    match agent_id {
        "chatgpt" | "codex" => "Codex".into(),
        // Herdr reports the Qoder CLI as `qodercli`; both spellings are one agent.
        "qoder" | "qodercli" => "Qoder".into(),
        "qwen" => "Qwen".into(),
        "claude" => "Claude".into(),
        "dsh" => "DSH".into(),
        "herdr" => "Herdr".into(),
        _ => display_name.to_owned(),
    }
}

fn desktop_source_short(panel: &ActivePanel) -> String {
    match &panel.transport {
        PanelTransport::Herdr { .. } => "Herdr".into(),
        PanelTransport::Tmux { .. } => "Tmux".into(),
        PanelTransport::Terminal { .. } => "Terminal".into(),
        PanelTransport::DshWeb { .. } => "DSH".into(),
        PanelTransport::CodexThread { .. } => "Desktop".into(),
        PanelTransport::Unknown => "未知".into(),
    }
}

/// A usable project identity is an absolute cwd, trailing slash trimmed.
/// Never guessed from names; relative or empty paths drop out to 未识别项目.
fn desktop_project_for(cwd: Option<&str>) -> (Option<String>, Option<String>, Option<String>) {
    let Some(raw) = cwd
        .map(str::trim)
        .filter(|path| path.len() > 1 && path.starts_with('/'))
    else {
        return (None, None, None);
    };
    let normalized = raw.trim_end_matches('/').to_owned();
    let mut parts = normalized.split('/').filter(|segment| !segment.is_empty());
    let name = parts.next_back().map(str::to_owned);
    let hint = parts.next_back().map(str::to_owned);
    (Some(normalized), name, hint)
}

/// Only the connected server's current agent panes can disprove a Herdr target.
pub(crate) fn herdr_target_missing(snapshot: &AppSnapshot, transport: &PanelTransport) -> bool {
    let PanelTransport::Herdr {
        socket_path,
        pane_id,
    } = transport
    else {
        return false;
    };
    // The snapshot describes the default server only. Never use its pane list
    // to invalidate a target on another socket, or while disconnected.
    #[cfg(target_os = "macos")]
    if !crate::herdr::socket_paths_match(socket_path.as_deref(), None) {
        return false;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = socket_path;
    snapshot.herdr.connected
        && !snapshot
            .herdr
            .agents
            .iter()
            .any(|agent| agent.pane_id == *pane_id)
}

/// Project the shared panel list into the floating panel's desktop sessions.
/// Offline rows retain unread news but cannot activate an exited target.
pub fn desktop_sessions_for(snapshot: &AppSnapshot) -> Vec<DesktopSession> {
    let herdr_connected = snapshot.herdr.connected;
    let focused_pane_id = snapshot.herdr.focused_pane_id.as_deref();
    let display_names = snapshot
        .agents
        .iter()
        .map(|agent| (agent.id.as_str(), agent.display_name.as_str()))
        .collect::<HashMap<_, _>>();
    let mut identities = HashMap::<String, usize>::new();
    for panel in &snapshot.active_panels {
        if panel.status != AgentStatus::Offline && !herdr_target_missing(snapshot, &panel.transport)
        {
            if let Some(identity) = desktop_focus_identity(panel) {
                *identities.entry(identity).or_default() += 1;
            }
        }
    }
    let mut rows = snapshot
        .active_panels
        .iter()
        .enumerate()
        .filter(|(_, panel)| panel.status != AgentStatus::Offline || panel.unread)
        .filter(|(_, panel)| !herdr_target_missing(snapshot, &panel.transport))
        .map(|(index, panel)| {
            let identity = desktop_focus_identity(panel).filter(|key| {
                panel.status != AgentStatus::Offline && identities.get(key) == Some(&1)
            });
            let display_name = if panel.agent_id == "chatgpt" {
                "Codex Desktop".into()
            } else {
                display_names
                    .get(panel.agent_id.as_str())
                    .map(|name| (*name).to_owned())
                    .unwrap_or_else(|| panel.agent_id.clone())
            };
            let agent_short = desktop_agent_short(&panel.agent_id, display_name.as_str());
            let source = desktop_source_short(panel);
            let error_acknowledged = panel_error_acknowledged(snapshot, panel);
            let attention = desktop_attention(panel, herdr_connected)
                .filter(|kind| *kind != AttentionClass::Error || !error_acknowledged);
            let (project_id, project_name, project_hint) =
                desktop_project_for(panel.cwd.as_deref());
            DesktopSession {
                error_acknowledged,
                activity: Default::default(),
                row_key: identity
                    .clone()
                    .unwrap_or_else(|| format!("unresolved:{index}:{}", panel.id)),
                identity,
                agent_id: panel.agent_id.clone(),
                agent_short,
                source,
                display_name,
                label: panel.label.clone(),
                status: panel.status,
                unread: panel.unread,
                attention,
                event_at_ms: if panel.unread {
                    panel.unread_since_ms
                } else if panel.status_since_ms != 0
                    || matches!(panel.status, AgentStatus::NeedsInput | AgentStatus::Error)
                {
                    panel.status_since_ms
                } else {
                    panel.last_activity_ms
                },
                event_is_activity: !panel.unread
                    && panel.status_since_ms == 0
                    && !matches!(panel.status, AgentStatus::NeedsInput | AgentStatus::Error),
                observed_since_ms: panel.first_seen_ms,
                unread_since_ms: panel.unread_since_ms,
                project_id,
                project_name,
                project_hint,
                host: desktop_host_label(panel, herdr_connected, focused_pane_id),
                stale: !herdr_connected && matches!(&panel.transport, PanelTransport::Herdr { .. }),
                updated_at_ms: panel.updated_at_ms,
                recent_activity_ms: panel.last_activity_ms,
            }
        })
        .collect::<Vec<_>>();
    retain_recent_codex_sessions(&mut rows);
    rows.sort_by(|a, b| {
        priority_of(a)
            .cmp(&priority_of(b))
            .then_with(|| {
                if a.attention.is_some() && b.attention.is_some() {
                    b.event_at_ms.cmp(&a.event_at_ms)
                } else {
                    b.recent_activity_ms.cmp(&a.recent_activity_ms)
                }
            })
            .then_with(|| a.identity.cmp(&b.identity))
    });
    rows
}

/// Both tabs use the same six most recently active Codex rows, before attention filtering.
fn retain_recent_codex_sessions(rows: &mut Vec<DesktopSession>) {
    let mut codex: Vec<_> = rows
        .iter()
        .filter(|row| {
            matches!(row.agent_id.as_str(), "codex" | "chatgpt")
                && row.status != AgentStatus::Offline
                && row.identity.is_some()
        })
        .map(|row| (row.row_key.clone(), row.recent_activity_ms))
        .collect();
    codex.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let keep: std::collections::HashSet<_> =
        codex.into_iter().take(6).map(|(key, _)| key).collect();
    rows.retain(|row| {
        !matches!(row.agent_id.as_str(), "codex" | "chatgpt")
            || row.status == AgentStatus::Offline
            || row.identity.is_none()
            || keep.contains(&row.row_key)
    });
}

/// The POC promises exact thread/pane navigation only. A terminal PID can
/// merely raise its app and must not acknowledge a specific session.
///
/// A `DshWeb` row belongs here: activating it asks the page's own bridge to
/// open that session and only reports success once a tab acknowledged the
/// switch, which is stronger evidence than a window being raised.
fn desktop_focus_identity(panel: &ActivePanel) -> Option<String> {
    match panel.transport {
        PanelTransport::Herdr { .. }
        | PanelTransport::CodexThread { .. }
        | PanelTransport::DshWeb { .. } => panel_focus_key(panel),
        _ => None,
    }
}

fn priority_of(row: &DesktopSession) -> u8 {
    // §8.2 default ranking: waiting for a human beats a broken run, which
    // beats an unviewed result.
    match row.status {
        AgentStatus::NeedsInput => 0,
        AgentStatus::Error => 1,
        _ if row.unread => 2,
        AgentStatus::Working => 3,
        _ => 5,
    }
}

pub fn desktop_summary_for(rows: &[DesktopSession]) -> DesktopSummary {
    DesktopSummary {
        working: rows
            .iter()
            .filter(|row| !row.stale && row.status == AgentStatus::Working)
            .count(),
        waiting: rows
            .iter()
            .filter(|row| !row.stale && row.status == AgentStatus::NeedsInput)
            .count(),
        unread: rows.iter().filter(|row| row.unread).count(),
        error: rows
            .iter()
            .filter(|row| !row.stale && row.status == AgentStatus::Error)
            .count(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatGptUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub remaining_tokens: Option<u64>,
    pub remaining_percent: Option<u8>,
    pub remaining_5h_percent: Option<u8>,
    pub remaining_weekly_percent: Option<u8>,
    pub reset_seconds: Option<u32>,
    pub reset_time: Option<String>,
    pub reset_5h_time: Option<String>,
    pub reset_weekly_time: Option<String>,
    pub updated_at_ms: u128,
    /// When the quota numbers themselves were produced, which is older than
    /// `updated_at_ms` whenever the deck is showing a disk-only fallback.
    pub snapshot_at_ms: u128,
    pub source: String,
}

impl Default for ChatGptUsage {
    fn default() -> Self {
        Self {
            model: "unknown".into(),
            input_tokens: 0,
            output_tokens: 0,
            remaining_tokens: None,
            remaining_percent: None,
            remaining_5h_percent: None,
            remaining_weekly_percent: None,
            reset_seconds: None,
            reset_time: None,
            reset_5h_time: None,
            reset_weekly_time: None,
            updated_at_ms: 0,
            snapshot_at_ms: 0,
            source: "unavailable".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSlotState {
    pub key: String,
    pub agent_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct HintPreferences {
    pub auto_show: bool,
    pub pinned: bool,
}

impl Default for HintPreferences {
    fn default() -> Self {
        Self {
            auto_show: true,
            pinned: false,
        }
    }
}

/// One palette shared by every window. The companion window owns the picker;
/// the session panel and hint window only render the class it broadcasts, so
/// the app can never drift into two competing colour schemes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiTheme {
    Classic,
    Modern,
    Tech,
    Fresh,
}

impl Default for UiTheme {
    fn default() -> Self {
        UiTheme::Modern
    }
}

/// Language of the whole surface: the companion UI and its menu-bar menu.
/// The companion window owns the picker; Rust mirrors the value so strings
/// rendered outside the WebView (tray menu, quit item) use the same language.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiLocale {
    Zh,
    En,
}

impl Default for UiLocale {
    fn default() -> Self {
        UiLocale::En
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AppSnapshot {
    #[serde(skip_serializing)]
    pub acknowledged_errors: HashMap<String, u128>,
    pub companion_navigation: crate::session_navigation::NavigationView,
    pub agents: Vec<AgentState>,
    pub agent_slots: Vec<AgentSlotState>,
    pub jobs: Vec<AgentJob>,
    pub active_panels: Vec<ActivePanel>,
    pub sessions: Vec<AgentSession>,
    pub active_profile: String,
    pub active_agent: Option<String>,
    pub active_surface: Option<AgentSurface>,
    pub function_keys: Vec<FunctionKeyState>,
    pub action_catalog: Vec<ActionCatalogItem>,
    pub hint_preferences: HintPreferences,
    /// Selected palette for every window; the only theme setting in the app.
    #[serde(default)]
    pub ui_theme: UiTheme,
    /// Language for the companion window and the menu-bar menu.
    #[serde(default)]
    pub ui_locale: UiLocale,
    pub device: DeviceState,
    pub attention_count: usize,
    pub attention_urgent: usize,
    pub chatgpt_usage: ChatGptUsage,
    #[cfg(target_os = "macos")]
    pub herdr: crate::herdr::HerdrSnapshot,
    /// Floating-panel projection over all monitored sessions; independent of
    /// `sessions`, which remains the current agent's three-row LCD projection.
    #[serde(default)]
    pub desktop_sessions: Vec<DesktopSession>,
    #[serde(default)]
    pub desktop_summary: DesktopSummary,
    /// Temporary notification cards decided by the coordinator in Rust, so the
    /// panel poll and every broadcast agree on what is a new event (§9.2).
    #[serde(default)]
    pub session_notifications: Vec<crate::notifications::SessionNotification>,
    /// Events dropped from the bounded queue; the attention list stays complete.
    #[serde(default)]
    pub notification_overflow: usize,
    pub chatgpt_activity_metrics: crate::chatgpt_activity::Metrics,
    /// Collapsed-bar counts: one per attention class, zeros hidden by the UI.
    #[serde(default)]
    pub attention_counts: crate::notifications::AttentionCounts,
    pub updated_at_ms: u128,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceEvent {
    #[serde(default)]
    pub navigation_stamp: Option<(u32, u64)>,
    #[serde(rename = "type")]
    pub kind: String,
    pub key: Option<String>,
    pub delta: Option<i32>,
}

pub struct AppRuntime {
    pub navigation: Mutex<crate::session_navigation::Navigator>,
    pub snapshot: RwLock<AppSnapshot>,
    pub config: Mutex<profile::AppConfig>,
    pub chatgpt_activity: crate::chatgpt_activity::Runtime,
    pub session_activity: crate::session_activity::Runtime,
    pub overrides: Mutex<HashMap<String, AgentStatus>>,
    pub hook_statuses: Mutex<HashMap<String, (AgentStatus, u128)>>,
    pub deck_push: Mutex<Option<mpsc::Sender<Instant>>>,
    pub deck_last_payload: Mutex<Option<(Vec<u8>, Instant)>>,
    /// Host receive time for the latest physical Agent selection.
    pub deck_last_selection_at: Mutex<Option<(u16, Instant)>>,
    pub deck_rev: AtomicU32,
    /// Advertised by the BLE HID Hello/Status response for this connection.
    pub fast_deck_supported: AtomicBool,
    /// Keep using Report 6 after a failed large Feature write or missing ACK.
    pub fast_deck_failed: AtomicBool,
    /// Every codepoint the deck's dynamic font is believed to carry right now,
    /// or `None` when any push must resend. The blob's epoch is the hash of this
    /// set, so the set is the only truth about the device's font. It is cleared
    /// whenever `deck_boot_id` disagrees with the boot id on the wire — a
    /// reboot, or this companion process seeing the device for the first time —
    /// but never on a mere reconnect, which leaves the device's installed font
    /// alone (see `vendor_hid::forget_glyphs`). Grows as a union across pushes,
    /// so switching back to a recently seen agent costs no glyph frames.
    pub glyph_installed: Mutex<Option<BTreeSet<u32>>>,
    /// The deck's boot id as last seen in a `status` reply. Kept on the runtime
    /// rather than per link session so a reboot that happens while the cable is
    /// out is still recognised as one when the link comes back.
    pub deck_boot_id: Mutex<Option<u32>>,
    /// Latched after the firmware NAKs `CMD_GLYPHS` as an unknown command, so
    /// an old device is never probed again this session.
    pub glyph_unsupported: AtomicBool,
    /// After a failed transfer, stop pushing glyphs until this instant so a
    /// broken font cannot stall every deck refresh.
    pub glyph_backoff_until: Mutex<Option<Instant>>,
    /// Deck worker's inbox for `CMD_GLYPHS` responses relayed by the reader
    /// thread, which is the only one that reads the device.
    pub glyph_ack: Mutex<Option<mpsc::Sender<(u8, u16, u8)>>>,
    /// Hand-off from the HID reader thread to the serial device-event worker.
    /// Activating an agent costs seconds of AppleScript and subprocess work,
    /// and while that runs nothing is reading the device — which would strand a
    /// glyph ACK the deck worker is blocked on (see `vendor_hid::post_device_event`).
    pub device_event_tx: Mutex<Option<mpsc::Sender<DeviceEvent>>>,
    pub hint_generation: AtomicU64,
    pub focus_generation: AtomicU64,
    /// All OS focus operations share this lock so an older operation cannot
    /// finish after a newer one has already raised its target.
    pub focus_lock: Mutex<()>,
    /// Last successfully focused panel per agent (`agent_id -> panel_focus_key`).
    /// This is the deck's source of truth for the session-row highlight: it
    /// records the desktop's real foreground, not the last activation attempt.
    /// Keyed by `panel_focus_key` because `ActivePanel.id` is rewritten by
    /// hooks whenever an agent re-reports the same pane under a new session id.
    pub focused_panels: Mutex<HashMap<String, String>>,
    pub reasoning_sync: Mutex<()>,
    /// Single-point attention event log behind `session_notifications`. Lives
    /// in Rust because every window renders the same snapshot.
    pub notifications: Mutex<crate::notifications::NotificationCoordinator>,
    pub plugins: Arc<plugin::PluginRegistry>,
    pub notification_audio: Arc<crate::notification_audio::AudioRuntime>,
    pub desktop_notifications: Arc<crate::desktop_notifications::NotificationRuntime>,
    pub chatgpt_usage_last_refresh: Mutex<Instant>,
    /// Latest live quota from the ChatGPT app server, filled by the app-server
    /// worker so `read()` never blocks on a subprocess.
    pub chatgpt_quota: Arc<Mutex<Option<crate::chatgpt_quota::QuotaSnapshot>>>,
    /// Same worker's latest thread list, which carries the desktop's own titles.
    pub chatgpt_threads: Arc<Mutex<Option<crate::chatgpt_quota::ThreadSnapshot>>>,
    /// The app-server worker only keeps a child alive while this is set, which
    /// is while ChatGPT is the active agent.
    pub chatgpt_quota_wanted: Arc<AtomicBool>,
    /// ESP32 deck switch from `hardware.json`, off by default. Deck pushes
    /// respect it live; already-spawned listeners are only released by a
    /// restart, so flipping it on takes effect immediately and off degrades
    /// to "no pushes" until the next launch.
    pub hardware_enabled: AtomicBool,
    /// Guards `hardware_hid::start` + `vendor_hid::start` against running
    /// twice when the tray toggle is flipped on after launch.
    pub hardware_listeners_started: AtomicBool,
    /// Host receive time (ms) of the last `dsh-deck-bridge` push. A fresh push
    /// means the plugin is reporting changes, so the DSH poll degrades to a
    /// fallback instead of the primary source.
    pub dsh_push_at_ms: AtomicU64,
    /// Pushes accepted since launch, surfaced in `dsh_diagnostics` so a user can
    /// see the plugin channel working without reading stderr.
    pub dsh_push_count: AtomicU64,
}

impl AppRuntime {
    /// Whether an agent is currently monitored. An unknown id counts as
    /// monitored so a racing shutdown never silently drops a valid event.
    pub fn agent_enabled(&self, agent_id: &str) -> bool {
        let snapshot = self.snapshot.read().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .map_or(true, |agent| agent.enabled)
    }

    /// Record a push from the DSH deck plugin. The push is a *trigger*, not a
    /// state blob: it says "look now", and the normal reader decides what the
    /// sessions currently are, so normalisation stays in one place.
    pub fn note_dsh_push(&self) {
        self.dsh_push_at_ms.store(
            u64::try_from(now_ms()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.dsh_push_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether the deck plugin pushed within `window`. Used to stretch the DSH
    /// poll while the event channel is demonstrably alive.
    pub fn dsh_push_is_fresh(&self, window: Duration) -> bool {
        let last = self.dsh_push_at_ms.load(Ordering::Relaxed);
        last > 0 && now_ms().saturating_sub(u128::from(last)) < window.as_millis()
    }

    /// `(pushes accepted, last push host time in ms)`, zero when never pushed.
    pub fn dsh_push_stats(&self) -> (u64, u128) {
        (
            self.dsh_push_count.load(Ordering::Relaxed),
            u128::from(self.dsh_push_at_ms.load(Ordering::Relaxed)),
        )
    }

    pub fn desktop_rows(&self, snapshot: &AppSnapshot) -> Vec<DesktopSession> {
        let mut projected = snapshot.clone();
        for panel in &mut projected.active_panels {
            panel.last_activity_ms = self
                .session_activity
                .activity(panel)
                .last_activity_at
                .unwrap_or(0);
        }
        let mut rows = desktop_sessions_for(&projected);
        for row in &mut rows {
            if let Some(panel) = projected.active_panels.iter().find(|panel| {
                row.identity.is_some()
                    && panel_focus_key(panel) == row.identity
                    && panel.agent_id == row.agent_id
            }) {
                row.activity = self.session_activity.activity(panel);
            }
        }
        rows
    }

    pub fn new() -> Self {
        let plugins = Arc::new(plugin::PluginRegistry::load());
        let plugin_ids = plugins.ids().map(str::to_owned).collect::<Vec<_>>();
        let config = profile::load_with_agents(&plugin_ids);
        // A monitoring switch, not a profile edit: the slot assignment below
        // stays untouched so re-enabling restores the previous layout.
        let disabled_agents = crate::agent_preferences::load().disabled;
        let mut agents = [
            ("qwen", "Qwen", "qwen"),
            ("herdr", "Herdr", "herdr"),
            ("codex", "Codex", "codex"),
            ("chatgpt", "ChatGPT", "ChatGPT"),
            ("claude", "Claude", "claude"),
            ("dsh", "DSH", "dsh"),
            ("companion", "Companion", ""),
        ]
        .into_iter()
        .map(|(id, display_name, executable)| AgentState {
            id: id.into(),
            display_name: display_name.into(),
            status: AgentStatus::Offline,
            running: false,
            pid: None,
            executable: executable.into(),
            // The Companion key is the app itself, so it is never switchable.
            enabled: id == "companion" || !disabled_agents.contains(id),
            task: None,
            workspace: None,
            active: false,
            unread: false,
            sessions: Vec::new(),
        })
        .collect::<Vec<_>>();
        agents.extend(
            plugins
                .all()
                .iter()
                .filter(|descriptor| !descriptor.built_in)
                .map(|descriptor| AgentState {
                    id: descriptor.id.clone(),
                    display_name: descriptor.display_name.clone(),
                    status: AgentStatus::Offline,
                    running: false,
                    pid: None,
                    executable: descriptor
                        .detection
                        .process_names
                        .first()
                        .cloned()
                        .unwrap_or_else(|| descriptor.id.clone()),
                    enabled: !disabled_agents.contains(&descriptor.id),
                    task: None,
                    workspace: None,
                    active: false,
                    unread: false,
                    sessions: Vec::new(),
                }),
        );
        let agent_slots = agent_slot_states(&config);

        Self {
            navigation: Mutex::new(crate::session_navigation::Navigator::default()),
            snapshot: RwLock::new(AppSnapshot {
                acknowledged_errors: HashMap::new(),
                companion_navigation: Default::default(),
                agents,
                agent_slots,
                jobs: Vec::new(),
                active_panels: Vec::new(),
                sessions: Vec::new(),
                active_profile: config.active_profile.clone(),
                active_agent: None,
                active_surface: None,
                function_keys: keymap::effective_keys(&config, None, AgentSurface::Unknown),
                action_catalog: Vec::new(),
                hint_preferences: load_hint_preferences(),
                ui_theme: load_ui_theme(),
                ui_locale: load_ui_locale(),
                attention_count: 0,
                attention_urgent: 0,
                chatgpt_usage: ChatGptUsage::default(),
                device: DeviceState {
                    mode: "mock".into(),
                    connected: false,
                    port: None,
                    baud_rate: 115_200,
                    last_event: None,
                    knob_position: 1,
                    reasoning_level: "medium".into(),
                    reasoning_actual: None,
                    reasoning_sync: "idle".into(),
                    native_encoder: chatgpt_state::native_encoder_available(),
                    encoder_mode: chatgpt_state::encoder_mode(),
                },
                #[cfg(target_os = "macos")]
                herdr: crate::herdr::HerdrSnapshot::default(),
                desktop_sessions: Vec::new(),
                desktop_summary: DesktopSummary::default(),
                session_notifications: Vec::new(),
                notification_overflow: 0,
                chatgpt_activity_metrics: Default::default(),
                attention_counts: crate::notifications::AttentionCounts::default(),
                updated_at_ms: now_ms(),
            }),
            config: Mutex::new(config),
            chatgpt_activity: Default::default(),
            session_activity: Default::default(),
            overrides: Mutex::new(HashMap::new()),
            hook_statuses: Mutex::new(HashMap::new()),
            deck_push: Mutex::new(None),
            deck_last_payload: Mutex::new(None),
            deck_last_selection_at: Mutex::new(None),
            deck_rev: AtomicU32::new(0),
            fast_deck_supported: AtomicBool::new(false),
            fast_deck_failed: AtomicBool::new(false),
            glyph_installed: Mutex::new(None),
            deck_boot_id: Mutex::new(None),
            glyph_unsupported: AtomicBool::new(false),
            glyph_backoff_until: Mutex::new(None),
            glyph_ack: Mutex::new(None),
            device_event_tx: Mutex::new(None),
            hint_generation: AtomicU64::new(0),
            focus_generation: AtomicU64::new(0),
            focus_lock: Mutex::new(()),
            focused_panels: Mutex::new(HashMap::new()),
            reasoning_sync: Mutex::new(()),
            notifications: Mutex::new(crate::notifications::NotificationCoordinator::ready()),
            plugins,
            notification_audio: Arc::new(crate::notification_audio::AudioRuntime::new()),
            desktop_notifications: Arc::new(
                crate::desktop_notifications::NotificationRuntime::new(),
            ),
            chatgpt_usage_last_refresh: Mutex::new(Instant::now() - Duration::from_secs(10)),
            chatgpt_quota: Arc::new(Mutex::new(None)),
            chatgpt_threads: Arc::new(Mutex::new(None)),
            chatgpt_quota_wanted: Arc::new(AtomicBool::new(false)),
            hardware_enabled: AtomicBool::new(crate::hardware_mode::load().enabled),
            hardware_listeners_started: AtomicBool::new(false),
            dsh_push_at_ms: AtomicU64::new(0),
            dsh_push_count: AtomicU64::new(0),
        }
    }

    pub fn read(&self) -> AppSnapshot {
        let (chatgpt_is_selected, chatgpt_desktop_running) = self
            .snapshot
            .read()
            .map(|snapshot| {
                (
                    snapshot.active_agent.as_deref() == Some("chatgpt"),
                    // A disabled ChatGPT also stops its thread discovery and the
                    // app-server worker, not just its rows.
                    snapshot
                        .agents
                        .iter()
                        .any(|agent| agent.id == "chatgpt" && agent.running && agent.enabled),
                )
            })
            .unwrap_or((false, false));
        // Thread discovery follows the desktop app, not the deck selection: an
        // open ChatGPT keeps surfacing sessions even while another agent is lit.
        self.chatgpt_quota_wanted
            .store(chatgpt_desktop_running, Ordering::Relaxed);
        let refresh_usage = self
            .chatgpt_usage_last_refresh
            .lock()
            .map(|mut last| {
                if last.elapsed() >= Duration::from_secs(5) {
                    *last = Instant::now();
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        let usage_is_mock = self
            .snapshot
            .read()
            .map(|snapshot| snapshot.chatgpt_usage.source == "mock")
            .unwrap_or(false);
        if refresh_usage && !usage_is_mock && chatgpt_is_selected {
            if let Some(usage) = chatgpt_state::local_usage() {
                let now = now_ms();
                let live = self
                    .chatgpt_quota
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                    .filter(|quota| quota.usable(now));
                let mut snapshot = self
                    .snapshot
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let fields = &mut snapshot.chatgpt_usage;
                // Model and token counters only exist on disk; the app server
                // reports them with a different scope, so they never mix.
                fields.model = usage.model;
                fields.input_tokens = usage.input_tokens;
                fields.output_tokens = usage.output_tokens;
                match live {
                    Some(quota) => {
                        let five_h = quota.five_h.as_ref();
                        fields.remaining_percent = five_h.map(|window| window.remaining_percent);
                        fields.remaining_5h_percent = fields.remaining_percent;
                        fields.remaining_weekly_percent =
                            quota.weekly.as_ref().map(|window| window.remaining_percent);
                        fields.reset_seconds =
                            five_h
                                .and_then(|window| window.resets_at_ms)
                                .map(|reset_ms| {
                                    (reset_ms.saturating_sub(now as u64) / 1_000)
                                        .min(u32::MAX as u64)
                                        as u32
                                });
                        fields.reset_time = five_h
                            .and_then(|window| window.resets_at_ms)
                            .and_then(|reset_ms| chatgpt_state::reset_time(reset_ms / 1_000));
                        fields.reset_5h_time = fields.reset_time.clone();
                        fields.reset_weekly_time = quota
                            .weekly
                            .as_ref()
                            .and_then(|window| window.resets_at_ms)
                            .and_then(|reset_ms| chatgpt_state::reset_date(reset_ms / 1_000));
                        fields.snapshot_at_ms = quota.fetched_at_ms;
                        fields.source = "codex-live".into();
                    }
                    None => {
                        fields.remaining_percent = usage.remaining_5h_percent;
                        fields.remaining_5h_percent = usage.remaining_5h_percent;
                        fields.remaining_weekly_percent = usage.remaining_weekly_percent;
                        fields.reset_seconds = usage.reset_5h_seconds;
                        fields.reset_time = usage.reset_5h_time.clone();
                        fields.reset_5h_time = usage.reset_5h_time;
                        fields.reset_weekly_time = usage.reset_weekly_time;
                        fields.snapshot_at_ms = usage.snapshot_at_ms;
                        fields.source = "codex-local".into();
                    }
                }
                fields.updated_at_ms = now;
            }
        }
        // Desktop ChatGPT has no process-level busy signal. Materialize its
        // recent threads as panels in the real snapshot whenever the desktop is
        // running or already has rows, whether or not it is the selected agent,
        // so the monitor's live-panel path takes precedence over any heuristic
        // and stale placeholders retire when they stop being recent.
        if chatgpt_desktop_running {
            let activity = self.chatgpt_activity.records();
            let recent_chatgpt_sessions = self.recent_chatgpt_sessions();
            let mut guard = self
                .snapshot
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let snapshot = &mut *guard;
            sync_chatgpt_panels(
                &mut snapshot.active_panels,
                &snapshot.jobs,
                &recent_chatgpt_sessions,
                now_ms(),
            );
            for panel in snapshot
                .active_panels
                .iter_mut()
                .filter(|p| p.agent_id == "chatgpt")
            {
                if let Some(record) = activity.get(&panel.session_id) {
                    let status = record.status;
                    panel.jobs_working = u16::from(status == AgentStatus::Working);
                    panel.jobs_waiting = u16::from(status == AgentStatus::NeedsInput);
                    panel.jobs_error = u16::from(status == AgentStatus::Error);
                    report_discovered_panel_status(
                        &mut snapshot.agents,
                        snapshot.active_agent.as_deref(),
                        panel,
                        status,
                    );
                }
            }
        }

        // Serialize capture and observation together. Capturing before the
        // coordinator lock lets a slow older read roll the live event set back.
        let mut coordinator = self.notifications.lock().unwrap_or_else(|p| p.into_inner());
        let mut snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| {
                eprintln!("[state] snapshot lock poisoned, recovering");
                poisoned.into_inner()
            })
            .clone();

        // Single chokepoint: whatever thread last wrote a disabled agent's
        // rows, no projection below may see them.
        prune_disabled_agents(&mut snapshot);
        for panel in &mut snapshot.active_panels {
            panel.last_activity_ms = self
                .session_activity
                .activity(panel)
                .last_activity_at
                .unwrap_or(0);
        }
        refresh_live_agent_statuses(&mut snapshot);
        refresh_agent_unread(&mut snapshot.agents, &snapshot.active_panels);
        let acknowledged = snapshot
            .agents
            .iter()
            .map(|agent| {
                (
                    agent_error_acknowledged(&snapshot, &agent.id),
                    snapshot
                        .active_panels
                        .iter()
                        .any(|p| p.unread && panel_belongs_to_agent(p, &agent.id)),
                )
            })
            .collect::<Vec<_>>();
        for (agent, (ack, has_completion)) in snapshot.agents.iter_mut().zip(acknowledged) {
            if ack {
                agent.unread = has_completion;
            }
        }

        let active_agent = snapshot.active_agent.clone();
        let mut attention_count = 0;
        let mut attention_urgent = 0;
        for agent in &snapshot.agents {
            if agent_error_acknowledged(&snapshot, &agent.id) && !agent.unread {
                continue;
            }
            if Some(agent.id.as_str()) != active_agent.as_deref()
                && (agent.unread
                    || matches!(agent.status, AgentStatus::Error | AgentStatus::NeedsInput))
            {
                attention_count += 1;
                if matches!(agent.status, AgentStatus::NeedsInput | AgentStatus::Error)
                    && !agent_error_acknowledged(&snapshot, &agent.id)
                {
                    attention_urgent += 1;
                }
            }
        }
        snapshot.attention_count = attention_count;
        snapshot.attention_urgent = attention_urgent;

        let active_agent_id = active_agent.as_deref();
        let mut panels = snapshot
            .active_panels
            .iter()
            .filter(|panel| {
                let belongs = active_agent_id.is_some_and(|id| panel_belongs_to_agent(panel, id));
                let selectable = panel.status != AgentStatus::Offline || panel.unread;
                belongs && selectable
            })
            .collect::<Vec<_>>();
        let ordered =
            crate::session_navigation::display_order(&desktop_sessions_for(&snapshot), true);
        panels.retain(|panel| {
            ordered
                .iter()
                .any(|row| row.identity == crate::state::panel_focus_key(panel))
        });
        panels.sort_by_key(|panel| {
            ordered
                .iter()
                .position(|row| row.identity == panel_focus_key(panel))
                .unwrap_or(usize::MAX)
        });
        let focused_panel_key = active_agent_id.and_then(|id| {
            self.focused_panels
                .lock()
                .ok()
                .and_then(|guard| guard.get(id).cloned())
        });
        let mut sessions: Vec<AgentSession> = panels
            .into_iter()
            .take(3)
            .map(|panel| {
                #[cfg(target_os = "macos")]
                let focused =
                    session_is_focused(panel, &snapshot.herdr, focused_panel_key.as_deref());
                #[cfg(not(target_os = "macos"))]
                let focused = focused_panel_key
                    .as_deref()
                    .is_some_and(|key| panel_focus_key(panel).as_deref() == Some(key));
                AgentSession {
                    id: panel.id.clone(),
                    label: panel.label.clone(),
                    status: panel.status,
                    unread: panel_is_unread(panel, now_ms()),
                    focused,
                }
            })
            .collect();
        while sessions.len() < 3 {
            sessions.push(AgentSession {
                id: String::new(),
                label: String::new(),
                status: AgentStatus::Offline,
                unread: false,
                focused: false,
            });
        }
        snapshot.sessions = sessions;
        snapshot.desktop_sessions = desktop_sessions_for(&snapshot);
        for row in &mut snapshot.desktop_sessions {
            if let Some(panel) = snapshot.active_panels.iter().find(|panel| {
                row.identity.is_some()
                    && panel_focus_key(panel) == row.identity
                    && panel.agent_id == row.agent_id
            }) {
                row.activity = self.session_activity.activity(panel);
            }
        }
        snapshot.desktop_summary = desktop_summary_for(&snapshot.desktop_sessions);
        snapshot.companion_navigation = {
            let mut nav = self.navigation.lock().unwrap_or_else(|p| p.into_inner());
            nav.reconcile_tabs(&snapshot.desktop_sessions);
            nav.project(&snapshot.desktop_sessions)
        };
        snapshot.chatgpt_activity_metrics = self.chatgpt_activity.metrics();
        snapshot.companion_navigation.hardware_ready =
            crate::vendor_hid::companion_hardware_ready();
        if snapshot.active_agent.as_deref() == Some("companion") {
            snapshot.function_keys =
                crate::session_navigation::function_keys(&snapshot.companion_navigation);
            snapshot.action_catalog.clear();
        }
        #[cfg(target_os = "macos")]
        let herdr_connected = snapshot.herdr.connected;
        #[cfg(not(target_os = "macos"))]
        let herdr_connected = true;
        let cards = coordinator.observe_with_channel(
            &snapshot.desktop_sessions,
            herdr_connected,
            now_ms(),
            self.desktop_notifications.system_channel(),
        );
        self.desktop_notifications
            .sync(coordinator.take_system_events());
        let overflow = coordinator.overflow();
        let attention_counts = crate::notifications::attention_counts(&snapshot.desktop_sessions);
        snapshot.session_notifications = cards;
        snapshot.notification_overflow = overflow;
        snapshot.attention_counts = attention_counts;

        let config = self
            .config
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let surface = snapshot.active_surface.unwrap_or(AgentSurface::Unknown);
        if snapshot.active_agent.as_deref() != Some("companion") {
            if let Some(id) = snapshot
                .active_agent
                .as_deref()
                .filter(|id| self.plugins.get(id).is_some())
            {
                snapshot.function_keys =
                    plugin_effective_keys_runtime(&self.plugins, &config, id, surface);
                snapshot.action_catalog = plugin_action_catalog(&self.plugins, id, surface);
            } else {
                snapshot.function_keys =
                    keymap::effective_keys(&config, snapshot.active_agent.as_deref(), surface);
                snapshot.action_catalog =
                    keymap::action_catalog(snapshot.active_agent.as_deref(), surface);
            }
        }
        #[cfg(target_os = "macos")]
        if snapshot.active_agent.as_deref() == Some("herdr") {
            let prefix = crate::herdr::configured_prefix();
            for key in &mut snapshot.function_keys {
                key.hint = key.hint.replacen("prefix+", &format!("{prefix} → "), 1);
            }
        }
        snapshot
    }

    /// Remove one card from the display only. Callers must drop the returned
    /// guard before broadcasting, and this never touches `unread`, a wait or an
    /// error — §9.1 keeps closing a card separate from acknowledging a session.
    pub fn dismiss_session_notification(&self, event_id: &str) -> bool {
        let mut coordinator = self
            .notifications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        coordinator.dismiss(event_id)
    }

    /// Recent ChatGPT threads. The app server reports the title the desktop
    /// itself shows and costs no subprocess, so it wins whenever it has
    /// answered recently; the disk query covers the window before the child is
    /// up and running.
    fn recent_chatgpt_sessions(&self) -> Vec<chatgpt_state::LocalSession> {
        let now = now_ms();
        self.chatgpt_threads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .filter(|threads| threads.usable(now) && !threads.sessions.is_empty())
            .map(|threads| threads.sessions)
            .unwrap_or_else(|| chatgpt_state::recent_sessions(DESKTOP_SESSION_LIMIT))
    }
}

fn sync_chatgpt_panels(
    panels: &mut Vec<ActivePanel>,
    jobs: &[AgentJob],
    recent: &[chatgpt_state::LocalSession],
    now: u128,
) {
    let live_sessions = jobs
        .iter()
        .filter(|job| {
            job.agent_id == "chatgpt"
                && matches!(job.status, JobStatus::Running | JobStatus::Waiting)
        })
        .map(|job| job.session_id.clone())
        .collect::<std::collections::HashSet<_>>();
    let recent_ids = recent
        .iter()
        .map(|session| session.id.as_str())
        .collect::<std::collections::HashSet<_>>();

    // Keep a completed thread available for K-row acknowledgement even when
    // it falls out of the desktop's recent list while another agent is active.
    panels.retain(|panel| {
        !(panel.agent_id == "chatgpt"
            && panel.status != AgentStatus::Offline
            && !panel.unread
            && !matches!(
                panel.status,
                AgentStatus::Working | AgentStatus::NeedsInput | AgentStatus::Error
            )
            && !recent_ids.contains(panel.session_id.as_str())
            && !live_sessions.contains(panel.session_id.as_str()))
    });

    let titles = recent
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<HashMap<_, _>>();
    // Hook events provide authoritative live status, but their
    // session_title can be a process/cwd label (for example
    // "agent_deck_companion"). Prefer the desktop thread title.
    for panel in &mut *panels {
        if panel.agent_id != "chatgpt" {
            continue;
        }
        if let Some(session) = titles.get(panel.session_id.as_str()) {
            if panel.status == AgentStatus::Offline {
                panel.status = AgentStatus::Unknown;
            }
            if !session.title.trim().is_empty() {
                panel.label = session.title.clone();
            }
            panel.cwd = Some(session.cwd.clone());
            // The desktop's own content time is the only real age a thread can
            // have; a launch-time discovery must not stand in for it. Take the
            // newer of that and any status change seen since startup.
            panel.last_activity_ms = panel
                .last_activity_ms
                .max(session.last_activity_ms.unwrap_or(0));
            // A panel that already resolves to a real pane keeps that route;
            // only placeholders need the desktop thread to activate against.
            if matches!(
                panel.transport,
                PanelTransport::Unknown | PanelTransport::CodexThread { .. }
            ) {
                panel.transport = PanelTransport::CodexThread {
                    thread_id: session.id.clone(),
                };
            }
        }
    }

    let existing = panels
        .iter()
        .filter(|panel| panel.agent_id == "chatgpt" && panel.status != AgentStatus::Offline)
        .map(|panel| panel.session_id.clone())
        .collect::<std::collections::HashSet<_>>();
    let mut next_slot = panels
        .iter()
        .filter(|panel| panel.agent_id == "chatgpt" && panel.status != AgentStatus::Offline)
        .map(|panel| panel.slot)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    for (index, session) in recent.iter().enumerate() {
        if existing.contains(&session.id) {
            continue;
        }
        // This is a status placeholder, not the Usage overlay title.
        // K8 opens the separate usage page; keep the session list stable.
        let label = if session.title.trim().is_empty() {
            "ChatGPT".to_owned()
        } else {
            session.title.clone()
        };
        panels.push(ActivePanel {
            id: format!("chatgpt:{}", session.id),
            agent_id: "chatgpt".into(),
            session_id: session.id.clone(),
            slot: next_slot.max((index + 1) as u8),
            label,
            // SQLite gives recency/title, but not live execution state.
            // Hook-backed panels carry the authoritative status.
            status: AgentStatus::Unknown,
            cwd: Some(session.cwd.clone()),
            jobs_working: 0,
            jobs_waiting: 0,
            jobs_error: 0,
            updated_at_ms: now,
            status_since_ms: 0,
            last_activity_ms: session.last_activity_ms.unwrap_or(0),
            first_seen_ms: now,
            transport: PanelTransport::CodexThread {
                thread_id: session.id.clone(),
            },
            unread: false,
            unread_since_ms: 0,
        });
        next_slot = next_slot.saturating_add(1);
    }
}

use crate::{chatgpt_state, keymap};

/// The panel a desktop row's stable identity currently points at. Identity
/// lookups go through the transport-derived focus key, never through the
/// mutable `ActivePanel.id` or a row index a refresh can shuffle.
pub fn lookup_panel_by_identity(snapshot: &AppSnapshot, identity: &str) -> Option<ActivePanel> {
    let mut matches = snapshot.active_panels.iter().filter(|panel| {
        panel.status != AgentStatus::Offline
            && !herdr_target_missing(snapshot, &panel.transport)
            && panel_focus_key(panel).as_deref() == Some(identity)
    });
    let panel = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(panel.clone())
}

/// Opening one session acknowledges exactly that row. The agent-level light
/// then re-derives from the rows still unacknowledged, so other unread
/// sessions stay lit.
pub fn panel_error_acknowledged(snapshot: &AppSnapshot, panel: &ActivePanel) -> bool {
    panel.status == AgentStatus::Error
        && snapshot
            .acknowledged_errors
            .get(&crate::session_activity::session_key(panel))
            == Some(&panel.status_since_ms)
}

pub fn agent_error_acknowledged(snapshot: &AppSnapshot, agent_id: &str) -> bool {
    let panels: Vec<_> = snapshot
        .active_panels
        .iter()
        .filter(|p| p.status != AgentStatus::Offline && panel_belongs_to_agent(p, agent_id))
        .collect();
    panels.iter().any(|p| p.status == AgentStatus::Error)
        && !panels.iter().any(|p| p.status == AgentStatus::NeedsInput)
        && panels
            .iter()
            .filter(|p| p.status == AgentStatus::Error)
            .all(|p| panel_error_acknowledged(snapshot, p))
}

pub fn acknowledge_session_unread(
    snapshot: &mut AppSnapshot,
    identity: &str,
    observed: &ActivePanel,
) -> bool {
    if lookup_panel_by_identity(snapshot, identity).is_none() {
        return false;
    }
    let mut error = None;
    let cleared = snapshot
        .active_panels
        .iter_mut()
        .find(|panel| {
            panel.status != AgentStatus::Offline
                && panel_focus_key(panel).as_deref() == Some(identity)
        })
        .is_some_and(|panel| {
            // A completion that arrived during a slow raise has not been seen
            // by the user. Neither a replaced session nor new news is cleared.
            if panel.session_id != observed.session_id
                || panel.unread_since_ms != observed.unread_since_ms
            {
                return false;
            }
            // Opening a newer error that arrived during the raise has not
            // acknowledged it. Status and event version must both still match.
            if observed.status == AgentStatus::Error
                && panel.status == observed.status
                && panel.status_since_ms == observed.status_since_ms
            {
                error = Some((
                    crate::session_activity::session_key(panel),
                    panel.status_since_ms,
                ));
            }
            let had = panel.unread || error.is_some();
            panel.unread = false;
            panel.unread_since_ms = 0;
            had
        });
    if let Some((key, version)) = error {
        if snapshot.acknowledged_errors.len() >= 512
            && !snapshot.acknowledged_errors.contains_key(&key)
        {
            if let Some(old) = snapshot.acknowledged_errors.keys().next().cloned() {
                snapshot.acknowledged_errors.remove(&old);
            }
        }
        snapshot.acknowledged_errors.insert(key, version);
    }
    if cleared {
        let panels = snapshot.active_panels.clone();
        refresh_agent_unread(&mut snapshot.agents, &panels);
    }
    cleared
}

pub fn acknowledge_session_version(
    snapshot: &mut AppSnapshot,
    identity: &str,
    unread_since_ms: u128,
) -> bool {
    let Some(observed) = lookup_panel_by_identity(snapshot, identity) else {
        return false;
    };
    if !observed.unread || observed.unread_since_ms != unread_since_ms {
        return false;
    }
    acknowledge_session_unread(snapshot, identity, &observed)
}

pub fn replace_config(state: &Arc<AppRuntime>, next: profile::AppConfig) {
    *state.config.lock().expect("config poisoned") = next;
    refresh_profile_snapshot(state);
}

pub fn validate_agent_id(state: &Arc<AppRuntime>, agent_id: &str) -> Result<(), String> {
    if state
        .snapshot
        .read()
        .map_err(|_| "snapshot poisoned")?
        .agents
        .iter()
        .any(|agent| agent.id == agent_id)
    {
        Ok(())
    } else {
        Err(format!("unknown Agent: {agent_id}"))
    }
}

pub fn save_profile_for_state(
    state: &Arc<AppRuntime>,
    config: &profile::AppConfig,
) -> Result<(), String> {
    let ids = state.plugins.ids().map(str::to_owned).collect::<Vec<_>>();
    profile::save_with_agents(config, &ids)
}

pub fn panel_belongs_to_agent(panel: &ActivePanel, agent_id: &str) -> bool {
    if agent_id == "herdr" {
        matches!(panel.transport, PanelTransport::Herdr { .. })
    } else {
        panel.agent_id == agent_id
    }
}

/// Derive A-key completion lights from the unacknowledged K rows. Selection
/// alone never changes this result, and opening one row leaves others lit.
fn refresh_agent_unread(agents: &mut [AgentState], panels: &[ActivePanel]) {
    let mut unread_owners = vec![false; agents.len()];
    for panel in panels {
        if !panel.unread {
            continue;
        }
        let owner = agents
            .iter()
            .position(|agent| agent.id == panel.agent_id)
            .or_else(|| {
                agents
                    .iter()
                    .position(|agent| panel_belongs_to_agent(panel, &agent.id))
            });
        if let Some(index) = owner {
            unread_owners[index] = true;
        }
    }
    for (agent, unread) in agents.iter_mut().zip(unread_owners) {
        agent.unread = unread
            || (agent.unread
                && matches!(agent.status, AgentStatus::Error | AgentStatus::NeedsInput));
    }
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Whether a row still counts as "finished and nobody looked at it".
pub fn panel_is_unread(panel: &ActivePanel, _now: u128) -> bool {
    panel.unread
}

/// Record a panel's new status and latch the completion it just produced.
///
/// A task that finishes collapses into `Idle`, which is the same status a row
/// has when it has been sitting unused for an hour, so the deck needs its own
/// memory of the edge: the row stays unread until its session key is pressed.
/// A new task on the row takes the news back.
pub fn report_panel_status(
    agents: &mut [AgentState],
    active_agent: Option<&str>,
    panel: &mut ActivePanel,
    status: AgentStatus,
) {
    let started = status == AgentStatus::Working;
    let completed = panel.status == AgentStatus::Working
        && matches!(status, AgentStatus::Idle | AgentStatus::Done);
    if panel.status != status {
        // Two lifecycle edges may arrive in one millisecond. Keep versions
        // distinct so a new notification cannot reuse an old card's identity.
        panel.status_since_ms = now_ms().max(panel.status_since_ms.saturating_add(1));
    }
    if !started && !completed {
        panel.status = status;
        return;
    }
    panel.unread = completed;
    panel.unread_since_ms = if completed { panel.status_since_ms } else { 0 };
    panel.status = status;
    // Prefer the agent the panel is filed under: a pane-backed panel also
    // belongs to herdr, but the key that owns the agent should carry the news.
    let owner = agents
        .iter()
        .position(|agent| agent.id == panel.agent_id)
        .or_else(|| {
            agents
                .iter()
                .position(|agent| panel_belongs_to_agent(panel, &agent.id))
        });
    let Some(index) = owner else { return };
    if started {
        agents[index].unread = false;
    } else if Some(agents[index].id.as_str()) != active_agent {
        // Only a key the user is not already looking at has news to report.
        agents[index].unread = true;
    }
}

/// Polling a previously unknown state is a first observation, whereas a hook
/// reports a real event even if the previous state was Unknown.
pub fn report_discovered_panel_status(
    agents: &mut [AgentState],
    active_agent: Option<&str>,
    panel: &mut ActivePanel,
    status: AgentStatus,
) {
    let first_observation = panel.status == AgentStatus::Unknown;
    report_panel_status(agents, active_agent, panel, status);
    if first_observation || status == AgentStatus::Unknown {
        panel.status_since_ms = 0;
    }
}

pub fn agent_slot_states(config: &profile::AppConfig) -> Vec<AgentSlotState> {
    profile::AGENT_SLOTS
        .iter()
        .filter_map(|key| {
            config.assigned_agent(key).map(|agent_id| AgentSlotState {
                key: (*key).into(),
                agent_id: agent_id.into(),
            })
        })
        .collect()
}

pub fn parse_surface(value: &str) -> Option<AgentSurface> {
    match value {
        "desktop" => Some(AgentSurface::Desktop),
        "terminal" => Some(AgentSurface::Terminal),
        "herdr_pane" => Some(AgentSurface::HerdrPane),
        "web" => Some(AgentSurface::Web),
        "unknown" => Some(AgentSurface::Unknown),
        _ => None,
    }
}

pub fn refresh_profile_snapshot(state: &Arc<AppRuntime>) {
    let config = state.config.lock().expect("config poisoned").clone();
    let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
        eprintln!("[state] snapshot write lock poisoned, recovering");
        poisoned.into_inner()
    });
    snapshot.active_profile = config.active_profile.clone();
    snapshot.agent_slots = agent_slot_states(&config);
    let active_surface = snapshot.active_surface.unwrap_or(AgentSurface::Unknown);
    let plugin_agent = snapshot
        .active_agent
        .clone()
        .filter(|id| state.plugins.get(id).is_some());
    if let Some(agent_id) = plugin_agent {
        snapshot.function_keys = plugin_effective_keys(state, &config, &agent_id, active_surface);
        snapshot.action_catalog = plugin_action_catalog(&state.plugins, &agent_id, active_surface);
    } else {
        snapshot.function_keys =
            keymap::effective_keys(&config, snapshot.active_agent.as_deref(), active_surface);
        snapshot.action_catalog =
            keymap::action_catalog(snapshot.active_agent.as_deref(), active_surface);
    }
    snapshot.updated_at_ms = now_ms();
}

pub fn plugin_effective_keys(
    state: &Arc<AppRuntime>,
    config: &profile::AppConfig,
    agent_id: &str,
    surface: AgentSurface,
) -> Vec<FunctionKeyState> {
    plugin_effective_keys_runtime(&state.plugins, config, agent_id, surface)
}
fn plugin_effective_keys_runtime(
    plugins: &plugin::PluginRegistry,
    config: &profile::AppConfig,
    agent_id: &str,
    surface: AgentSurface,
) -> Vec<FunctionKeyState> {
    let mut keys = plugins.keys(agent_id, surface);
    let surface_name = keymap::surface_key(surface);
    for key in &mut keys {
        let Some(action) = config.configured_action(agent_id, surface_name, &key.key) else {
            continue;
        };
        key.customized = true;
        match action {
            profile::ConfiguredAction::Disabled => {
                key.supported = false;
                key.label = "UNAVAILABLE".into();
                key.hint = "已禁用".into();
                key.action_id = None;
            }
            profile::ConfiguredAction::SystemAction { action_id }
                if action_id == "voice_input.toggle" =>
            {
                key.label = "VOICE".into();
                key.hint = "系统听写".into();
                key.action_id = Some(action_id.clone());
            }
            profile::ConfiguredAction::AgentAction { action_id } => {
                if let Some(item) = plugins
                    .catalog(agent_id, surface)
                    .into_iter()
                    .find(|item| item.action_id == *action_id)
                {
                    key.label = item.label;
                    key.hint = item.hint;
                    key.action_id = Some(item.action_id);
                    key.supported = true;
                } else {
                    key.supported = false;
                }
            }
            profile::ConfiguredAction::SystemAction { action_id }
                if action_id == "session_panel.toggle" =>
            {
                key.label = "PANEL".into();
                key.hint = "隐藏 / 展示会话浮窗".into();
                key.action_id = Some(action_id.clone());
                key.supported = true;
            }
            profile::ConfiguredAction::SystemAction { .. } => {
                key.supported = false;
            }
            profile::ConfiguredAction::KeyboardShortcut { .. } => {}
        }
    }
    keys
}

pub fn hint_preferences_path() -> Option<PathBuf> {
    dirs::config_dir().map(|root| root.join("Agent Deck Companion").join("function-hint.json"))
}

pub fn load_hint_preferences() -> HintPreferences {
    hint_preferences_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

pub fn save_hint_preferences(preferences: HintPreferences) {
    let Some(path) = hint_preferences_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(contents) = serde_json::to_string_pretty(&preferences) {
        let _ = fs::write(path, contents);
    }
}

pub fn ui_theme_path() -> Option<PathBuf> {
    dirs::config_dir().map(|root| root.join("Agent Deck Companion").join("ui-theme.json"))
}

pub fn load_ui_theme() -> UiTheme {
    ui_theme_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

pub fn save_ui_theme(theme: UiTheme) {
    let Some(path) = ui_theme_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(contents) = serde_json::to_string_pretty(&theme) {
        let _ = fs::write(path, contents);
    }
}

pub fn ui_locale_path() -> Option<PathBuf> {
    dirs::config_dir().map(|root| root.join("Agent Deck Companion").join("ui-locale.json"))
}

pub fn load_ui_locale() -> UiLocale {
    ui_locale_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

pub fn save_ui_locale(locale: UiLocale) {
    let Some(path) = ui_locale_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(contents) = serde_json::to_string_pretty(&locale) {
        let _ = fs::write(path, contents);
    }
}

pub fn update_ui_locale_state(state: &Arc<AppRuntime>, locale: UiLocale) -> UiLocale {
    {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.ui_locale = locale;
        snapshot.updated_at_ms = now_ms();
    }
    save_ui_locale(locale);
    locale
}

pub fn update_ui_theme_state(state: &Arc<AppRuntime>, theme: UiTheme) -> UiTheme {
    {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.ui_theme = theme;
        snapshot.updated_at_ms = now_ms();
    }
    save_ui_theme(theme);
    theme
}

pub fn current_hint_preferences(state: &Arc<AppRuntime>) -> HintPreferences {
    state
        .snapshot
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .hint_preferences
}

pub fn update_hint_preferences_state(
    state: &Arc<AppRuntime>,
    auto_show: bool,
    pinned: bool,
) -> HintPreferences {
    let preferences = HintPreferences { auto_show, pinned };
    let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
        eprintln!("[state] snapshot write lock poisoned, recovering");
        poisoned.into_inner()
    });
    snapshot.hint_preferences = preferences;
    snapshot.updated_at_ms = now_ms();
    drop(snapshot);
    save_hint_preferences(preferences);
    preferences
}

pub fn show_function_hint(app: &AppHandle, state: &Arc<AppRuntime>, transient: bool) {
    use std::{sync::atomic::Ordering, thread, time::Duration};

    crate::function_hint::show(app);
    let generation = state.hint_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let should_hide = transient && !current_hint_preferences(state).pinned;
    if !should_hide {
        return;
    }

    let app = app.clone();
    let state = state.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(3_000));
        if state.hint_generation.load(Ordering::SeqCst) == generation
            && !current_hint_preferences(&state).pinned
        {
            crate::function_hint::hide(&app);
        }
    });
}

pub fn emit_state(app: &AppHandle, state: &Arc<AppRuntime>) {
    let snapshot = state.read();
    crate::tray::update_tray_status(app, &snapshot);
    let _ = app.emit("state_changed", snapshot);
    vendor_hid::request_deck_push(state);
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_switched_off_agent_is_not_detected_and_keeps_no_rows() {
        let runtime = std::sync::Arc::new(AppRuntime::new());
        {
            let mut snapshot = runtime.snapshot.write().unwrap();
            let qwen = snapshot
                .agents
                .iter_mut()
                .find(|agent| agent.id == "qwen")
                .expect("qwen agent");
            qwen.enabled = false;
            qwen.running = true;
            qwen.status = AgentStatus::Working;
            let mut live = chatgpt_panel("session-1", AgentStatus::Working);
            live.agent_id = "qwen".into();
            snapshot.active_panels = vec![live];
            snapshot.jobs = vec![AgentJob {
                id: "qwen:session-1:main".into(),
                agent_id: "qwen".into(),
                session_id: "session-1".into(),
                status: JobStatus::Running,
                cwd: None,
                last_event: "UserPromptSubmit".into(),
                updated_at_ms: 1,
                agent_type: None,
            }];
        }

        crate::monitor::reconcile_processes(&runtime, &sysinfo::System::new());

        let snapshot = runtime.read();
        let qwen = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == "qwen")
            .expect("qwen stays listed so Settings can switch it back on");
        assert!(!qwen.enabled);
        assert!(!qwen.running, "a disabled agent is never detected");
        assert_eq!(qwen.status, AgentStatus::Offline);
        assert!(snapshot.active_panels.is_empty(), "its rows are retired");
        assert!(snapshot.jobs.is_empty(), "its jobs are retired");
        assert!(snapshot.desktop_sessions.is_empty());
    }

    #[test]
    fn codex_tabs_share_six_recent_rows_without_using_poll_stamps_or_attention_priority() {
        let mut panels = (0..9)
            .map(|i| {
                let mut panel = chatgpt_panel(
                    &format!("thread{i}"),
                    if i < 3 {
                        AgentStatus::NeedsInput
                    } else {
                        AgentStatus::Idle
                    },
                );
                panel.transport = PanelTransport::CodexThread {
                    thread_id: panel.session_id.clone(),
                };
                panel.last_activity_ms = 100 + i;
                panel.first_seen_ms = 50_000;
                panel.updated_at_ms = 100_000 - i;
                panel.status_since_ms = 0;
                panel
            })
            .collect::<Vec<_>>();
        // A recent CLI Codex pane shares the Codex group limit; other owners do not.
        let mut cli = herdr_row("cli", AgentStatus::NeedsInput);
        cli.agent_id = "codex".into();
        cli.last_activity_ms = 200;
        panels.push(cli);
        panels.push(herdr_row("qoder", AgentStatus::NeedsInput));
        let snapshot = projection_snapshot(panels);
        let rows = desktop_sessions_for(&snapshot);
        let codex = rows
            .iter()
            .filter(|r| matches!(r.agent_id.as_str(), "codex" | "chatgpt"))
            .collect::<Vec<_>>();
        assert_eq!(codex.len(), 6);
        assert!(codex.iter().all(|r| r.recent_activity_ms >= 104));
        assert!(rows
            .iter()
            .any(|r| r.identity.as_deref() == Some("herdr:qoder")));
        let attention = crate::session_navigation::display_order(&rows, false);
        assert!(!attention.iter().any(|r| r.agent_id == "chatgpt"));
        assert!(attention.iter().any(|r| r.agent_id == "codex"));
        assert_eq!(snapshot.active_panels.len(), 11); // Display limit never deletes source state.
    }

    #[test]
    fn needs_input_uses_frontend_status_name_and_accepts_legacy_hooks() {
        assert_eq!(
            serde_json::to_string(&super::AgentStatus::NeedsInput).unwrap(),
            "\"needs_input\""
        );
        for name in ["needs_input", "needsinput"] {
            assert_eq!(
                serde_json::from_value::<super::AgentStatus>(serde_json::json!(name)).unwrap(),
                super::AgentStatus::NeedsInput
            );
        }
    }
    use super::*;
    use crate::chatgpt_state::LocalSession;

    fn chatgpt_panel(session_id: &str, status: AgentStatus) -> ActivePanel {
        ActivePanel {
            id: format!("chatgpt:{session_id}"),
            agent_id: "chatgpt".into(),
            session_id: session_id.into(),
            slot: 1,
            label: "stale label".into(),
            status,
            cwd: None,
            jobs_working: 0,
            jobs_waiting: 0,
            jobs_error: 0,
            updated_at_ms: 0,
            status_since_ms: 0,
            last_activity_ms: 0,
            first_seen_ms: 0,
            transport: PanelTransport::Unknown,
            unread: false,
            unread_since_ms: 0,
        }
    }

    fn local_session(id: &str, title: &str) -> LocalSession {
        LocalSession {
            id: id.into(),
            title: title.into(),
            cwd: "/tmp/project".into(),
            last_activity_ms: Some(4_000),
        }
    }

    fn deck_agent(id: &str) -> AgentState {
        AgentState {
            id: id.into(),
            display_name: id.into(),
            status: AgentStatus::Idle,
            running: true,
            pid: None,
            executable: id.into(),
            enabled: true,
            task: None,
            workspace: None,
            active: false,
            unread: false,
            sessions: Vec::new(),
        }
    }

    fn herdr_pane_panel(id: &str, status: AgentStatus) -> ActivePanel {
        ActivePanel {
            id: id.into(),
            agent_id: "qodercli".into(),
            transport: PanelTransport::Herdr {
                socket_path: None,
                pane_id: "wK:p1".into(),
            },
            ..chatgpt_panel(id, status)
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_herdr_focus_overrides_the_previous_key_selection() {
        let first = herdr_pane_panel("old", AgentStatus::Idle);
        let mut second = first.clone();
        second.transport = PanelTransport::Herdr {
            socket_path: None,
            pane_id: "wK:p2".into(),
        };
        let recorded = panel_focus_key(&first);
        let mut herdr = crate::herdr::HerdrSnapshot {
            connected: true,
            focused_pane_id: Some("wK:p2".into()),
            agents: vec![],
        };
        assert!(!session_is_focused(&first, &herdr, recorded.as_deref()));
        assert!(session_is_focused(&second, &herdr, recorded.as_deref()));
        herdr.focused_pane_id = Some("another-agent-pane".into());
        assert!(!session_is_focused(&first, &herdr, recorded.as_deref()));
        herdr.connected = false;
        assert!(session_is_focused(&first, &herdr, recorded.as_deref()));
    }

    #[test]
    fn retired_failed_jobs_do_not_poison_live_agent_header() {
        let mut snapshot = AppRuntime::new().snapshot.read().unwrap().clone();
        let mut panel = chatgpt_panel("live", AgentStatus::Idle);
        panel.agent_id = "qwen".into();
        snapshot.active_panels = vec![panel];
        snapshot.jobs = vec![AgentJob {
            id: "qwen:retired:main".into(),
            agent_id: "qwen".into(),
            session_id: "retired".into(),
            status: JobStatus::Failed,
            cwd: None,
            last_event: "StopFailure".into(),
            updated_at_ms: 1,
            agent_type: None,
        }];
        snapshot
            .agents
            .iter_mut()
            .find(|a| a.id == "qwen")
            .unwrap()
            .status = AgentStatus::Error;
        refresh_live_agent_statuses(&mut snapshot);
        assert_eq!(
            snapshot
                .agents
                .iter()
                .find(|a| a.id == "qwen")
                .unwrap()
                .status,
            AgentStatus::Idle
        );
    }

    #[test]
    fn a_focus_key_survives_the_hooks_rewriting_its_panel_id() {
        let mut panel = herdr_pane_panel("herdr:wK:p1", AgentStatus::Working);
        let key = panel_focus_key(&panel);
        assert_eq!(key.as_deref(), Some("herdr:wK:p1"));

        panel.id = "qodercli:session-uuid".into();
        panel.session_id = "session-uuid".into();
        assert_eq!(panel_focus_key(&panel), key);

        assert_eq!(
            panel_focus_key(&chatgpt_panel("x", AgentStatus::Idle)),
            None
        );
    }

    #[test]
    fn a_finished_task_stays_latched_until_its_key_is_pressed() {
        let mut agents = vec![deck_agent("qwen")];
        let mut panel = chatgpt_panel("a", AgentStatus::Working);

        report_panel_status(&mut agents, Some("qwen"), &mut panel, AgentStatus::Idle);
        assert!(
            panel.unread,
            "a completion that collapses into Idle must still be reported"
        );

        // A new task on the same row makes the old news stale.
        report_panel_status(&mut agents, Some("qwen"), &mut panel, AgentStatus::Working);
        assert!(!panel.unread);

        // Only a working row can complete: settling down and retiring are not.
        let mut idle = chatgpt_panel("b", AgentStatus::Idle);
        report_panel_status(&mut agents, Some("qwen"), &mut idle, AgentStatus::Idle);
        assert!(!idle.unread);
        report_panel_status(&mut agents, Some("qwen"), &mut idle, AgentStatus::Offline);
        assert!(!idle.unread);
    }

    #[test]
    fn fast_completion_rounds_have_distinct_notification_versions() {
        let mut panel = chatgpt_panel("a", AgentStatus::Working);
        panel.status_since_ms = now_ms() + 1_000;
        report_panel_status(&mut [], None, &mut panel, AgentStatus::Idle);
        let first = panel.unread_since_ms;
        report_panel_status(&mut [], None, &mut panel, AgentStatus::Working);
        report_panel_status(&mut [], None, &mut panel, AgentStatus::Idle);
        assert!(panel.unread_since_ms > first);
    }

    #[test]
    fn the_first_look_at_a_discovered_row_does_not_backdate_its_age() {
        let mut agents = vec![deck_agent("qwen")];
        // Discovered from the desktop's threads: the status is not observed yet
        // and the age comes from the thread's own content time.
        let mut panel = chatgpt_panel("a", AgentStatus::Unknown);
        panel.last_activity_ms = 4_000;

        report_discovered_panel_status(&mut agents, Some("qwen"), &mut panel, AgentStatus::Idle);
        assert_eq!(
            panel.status_since_ms, 0,
            "a first observation must not turn an old thread into a new one"
        );

        // The next genuine change is a real edge and takes the current time.
        report_panel_status(&mut agents, Some("qwen"), &mut panel, AgentStatus::Working);
        assert!(panel.status_since_ms > 4_000);
    }

    #[test]
    fn agent_completion_stays_lit_until_each_session_is_acknowledged() {
        let mut agents = vec![deck_agent("herdr"), deck_agent("qwen")];
        let mut first = herdr_pane_panel("qodercli:a", AgentStatus::Working);
        let mut second = herdr_pane_panel("qodercli:b", AgentStatus::Working);
        report_panel_status(&mut agents, Some("qwen"), &mut first, AgentStatus::Idle);
        report_panel_status(&mut agents, Some("herdr"), &mut second, AgentStatus::Idle);
        let mut panels = vec![first, second];
        refresh_agent_unread(&mut agents, &panels);
        assert!(agents[0].unread, "the owning A key shows both completions");
        assert!(!agents[1].unread);

        agents[0].active = true;
        refresh_agent_unread(&mut agents, &panels);
        assert!(agents[0].unread, "selecting A does not acknowledge K rows");

        panels[0].unread = false;
        refresh_agent_unread(&mut agents, &panels);
        assert!(agents[0].unread, "one more session still needs attention");
        panels[1].unread = false;
        refresh_agent_unread(&mut agents, &panels);
        assert!(!agents[0].unread);
    }

    #[test]
    fn an_unwatched_completion_does_not_expire_by_time() {
        let mut agents = vec![deck_agent("qwen")];
        let mut panel = chatgpt_panel("a", AgentStatus::Working);
        report_panel_status(&mut agents, Some("herdr"), &mut panel, AgentStatus::Idle);

        let latched = panel.unread_since_ms;
        assert!(panel_is_unread(&panel, latched));
        assert!(panel_is_unread(&panel, latched + 24 * 60 * 60 * 1000));
        panel.unread = false;
        assert!(!panel_is_unread(&panel, latched));
    }

    #[test]
    fn a_new_task_takes_the_news_back_off_the_key() {
        let mut agents = vec![deck_agent("herdr")];
        let mut pane = herdr_pane_panel("qodercli:a", AgentStatus::Working);
        report_panel_status(&mut agents, Some("qwen"), &mut pane, AgentStatus::Idle);
        assert!(agents[0].unread);

        report_panel_status(&mut agents, Some("qwen"), &mut pane, AgentStatus::Working);
        assert!(!agents[0].unread, "the old completion is not news any more");
        assert!(!pane.unread);
    }

    #[test]
    fn sync_materializes_unknown_placeholders_and_refreshes_titles() {
        let mut panels = vec![chatgpt_panel("a", AgentStatus::Idle)];
        let recent = vec![
            local_session("a", "Thread A"),
            local_session("b", "Thread B"),
        ];
        sync_chatgpt_panels(&mut panels, &[], &recent, 1);
        assert_eq!(panels.len(), 2);
        assert_eq!(panels[0].label, "Thread A");
        assert_eq!(panels[0].cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(panels[1].id, "chatgpt:b");
        assert_eq!(panels[1].status, AgentStatus::Unknown);
        assert_eq!(panels[1].slot, 2);
        // Both the refreshed panel and the new placeholder carry the thread's
        // own content time: a launch-time discovery has no real age of its own.
        assert_eq!(panels[0].last_activity_ms, 4_000);
        assert_eq!(panels[1].last_activity_ms, 4_000);
        // K1-K3 activate these by thread id, so both the refreshed panel and the
        // new placeholder have to carry it.
        assert!(matches!(
            &panels[0].transport,
            PanelTransport::CodexThread { thread_id } if thread_id == "a"
        ));
        assert!(matches!(
            &panels[1].transport,
            PanelTransport::CodexThread { thread_id } if thread_id == "b"
        ));
    }

    #[test]
    fn a_status_change_is_not_backdated_by_the_threads_content_time() {
        let mut panel = chatgpt_panel("a", AgentStatus::Working);
        // A hook reported this change after the thread's stored 4_000 timestamp.
        panel.status_since_ms = 9_000;
        let mut panels = vec![panel];
        sync_chatgpt_panels(&mut panels, &[], &[local_session("a", "Thread A")], 12_000);
        assert_eq!(panels[0].status_since_ms, 9_000);
    }

    #[test]
    fn source_content_updates_do_not_move_a_waiting_status_time() {
        let mut panel = chatgpt_panel("a", AgentStatus::NeedsInput);
        panel.status_since_ms = 9_000;
        let mut panels = vec![panel];
        let mut session = local_session("a", "Thread A");
        session.last_activity_ms = Some(20_000);
        sync_chatgpt_panels(&mut panels, &[], &[session], 30_000);
        assert_eq!(panels[0].status_since_ms, 9_000);
        assert_eq!(panels[0].last_activity_ms, 20_000);
    }

    #[test]
    fn live_hooks_stamp_transitions_out_of_unknown_but_discovery_does_not() {
        let mut agents = vec![];
        let mut panel = chatgpt_panel("a", AgentStatus::Unknown);
        report_panel_status(&mut agents, None, &mut panel, AgentStatus::NeedsInput);
        assert!(panel.status_since_ms > 0);
        report_discovered_panel_status(&mut agents, None, &mut panel, AgentStatus::Unknown);
        assert_eq!(panel.status_since_ms, 0);
        report_panel_status(&mut agents, None, &mut panel, AgentStatus::Working);
        assert!(panel.status_since_ms > 0);
    }

    #[test]
    fn content_age_is_explicit_and_unread_completion_uses_its_own_time() {
        let mut panel = herdr_row("pane", AgentStatus::Idle);
        panel.status_since_ms = 0;
        panel.last_activity_ms = 4_000;
        let mut snapshot = projection_snapshot(vec![panel]);
        let row = desktop_sessions_for(&snapshot).remove(0);
        assert!(row.event_is_activity);
        assert_eq!(row.event_at_ms, 4_000);
        snapshot.active_panels[0].unread = true;
        snapshot.active_panels[0].unread_since_ms = 5_000;
        let row = desktop_sessions_for(&snapshot).remove(0);
        assert!(!row.event_is_activity);
        assert_eq!(row.event_at_ms, 5_000);
    }

    #[test]
    fn acknowledged_errors_do_not_silence_other_completions_or_waits() {
        let mut error = herdr_row("error", AgentStatus::Error);
        error.status_since_ms = 100;
        let mut done = herdr_row("done", AgentStatus::Idle);
        done.unread = true;
        done.unread_since_ms = 200;
        let mut snapshot = projection_snapshot(vec![error.clone(), done]);
        let owner = error.agent_id.clone();
        assert!(acknowledge_session_unread(
            &mut snapshot,
            "herdr:error",
            &error
        ));
        assert!(agent_error_acknowledged(&snapshot, &owner));
        assert!(snapshot.active_panels[1].unread);
        assert!(desktop_sessions_for(&snapshot)
            .iter()
            .any(|r| r.attention == Some(AttentionClass::Done)));
        snapshot.active_panels[1].status = AgentStatus::NeedsInput;
        assert!(!agent_error_acknowledged(&snapshot, &owner));
        assert!(desktop_sessions_for(&snapshot)
            .iter()
            .any(|r| r.attention == Some(AttentionClass::NeedsInput)));
    }

    #[test]
    fn opening_an_error_acknowledges_only_that_version_and_revokes_its_card() {
        let mut first = herdr_row("first-error", AgentStatus::Error);
        first.status_since_ms = 100;
        let mut second = herdr_row("second-error", AgentStatus::Error);
        second.status_since_ms = 200;
        let mut snapshot = projection_snapshot(vec![first.clone(), second]);
        let mut coordinator = crate::notifications::NotificationCoordinator::ready();
        coordinator.observe_with_channel(&[], true, 1, false);
        let rows = desktop_sessions_for(&snapshot);
        assert_eq!(
            coordinator
                .observe_with_channel(&rows, true, 2, false)
                .len(),
            2
        );
        assert!(acknowledge_session_unread(
            &mut snapshot,
            "herdr:first-error",
            &first
        ));
        let rows = desktop_sessions_for(&snapshot);
        let acknowledged = rows
            .iter()
            .find(|r| r.identity.as_deref() == Some("herdr:first-error"))
            .unwrap();
        assert_eq!(acknowledged.status, AgentStatus::Error);
        assert!(acknowledged.error_acknowledged);
        assert_eq!(acknowledged.attention, None);
        assert_eq!(
            coordinator
                .observe_with_channel(&rows, true, 3, false)
                .len(),
            1
        );
        let panel = &mut snapshot.active_panels[0];
        report_panel_status(&mut snapshot.agents, None, panel, AgentStatus::Working);
        report_panel_status(&mut snapshot.agents, None, panel, AgentStatus::Error);
        assert!(!panel_error_acknowledged(
            &snapshot,
            &snapshot.active_panels[0]
        ));
        assert!(!acknowledge_session_unread(
            &mut snapshot,
            "herdr:first-error",
            &first
        ));
        assert_eq!(
            desktop_sessions_for(&snapshot)
                .iter()
                .filter(|r| r.attention == Some(AttentionClass::Error))
                .count(),
            2
        );
    }

    #[test]
    fn stale_ui_read_version_cannot_clear_a_new_completion() {
        let mut panel = herdr_row("pane", AgentStatus::Idle);
        panel.unread = true;
        panel.unread_since_ms = 20;
        let mut snapshot = projection_snapshot(vec![panel]);
        assert!(!acknowledge_session_version(
            &mut snapshot,
            "herdr:pane",
            10
        ));
        assert!(snapshot.active_panels[0].unread);
        assert!(acknowledge_session_version(&mut snapshot, "herdr:pane", 20));
    }

    #[test]
    fn sync_retires_stale_panels_but_keeps_live_hook_sessions() {
        let mut panels = vec![
            chatgpt_panel("gone", AgentStatus::Idle),
            chatgpt_panel("busy", AgentStatus::Working),
        ];
        let job = AgentJob {
            id: "chatgpt:busy:main".into(),
            agent_id: "chatgpt".into(),
            session_id: "busy".into(),
            status: JobStatus::Running,
            cwd: None,
            last_event: "PreToolUse".into(),
            updated_at_ms: 0,
            agent_type: None,
        };
        sync_chatgpt_panels(&mut panels, &[job], &[], 1);
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].session_id, "busy");
        assert_eq!(panels[0].status, AgentStatus::Working);
    }

    #[test]
    fn sync_keeps_running_desktop_threads_without_hooks() {
        let mut panels = vec![chatgpt_panel("running", AgentStatus::Working)];
        sync_chatgpt_panels(&mut panels, &[], &[], 1);
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].status, AgentStatus::Working);
    }

    #[test]
    fn sync_keeps_completed_chatgpt_thread_until_it_is_opened() {
        let mut done = chatgpt_panel("done", AgentStatus::Idle);
        done.unread = true;
        let mut panels = vec![done];
        sync_chatgpt_panels(&mut panels, &[], &[], 1);
        assert_eq!(panels.len(), 1);
        panels[0].unread = false;
        sync_chatgpt_panels(&mut panels, &[], &[], 2);
        assert!(panels.is_empty());
    }

    #[test]
    fn sync_leaves_other_agents_panels_untouched() {
        let mut other = chatgpt_panel("x", AgentStatus::Idle);
        other.agent_id = "codex".into();
        other.id = "codex:x".into();
        let mut panels = vec![other];
        sync_chatgpt_panels(&mut panels, &[], &[], 1);
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].agent_id, "codex");
    }

    fn projection_snapshot(panels: Vec<ActivePanel>) -> AppSnapshot {
        let mut snapshot = AppRuntime::new().read();
        // The projection reads the host connection like the floating panel
        // would; unit fixtures start from a connected Herdr unless told so.
        snapshot.herdr.connected = true;
        snapshot.herdr.agents = panels
            .iter()
            .filter_map(|panel| {
                let PanelTransport::Herdr { pane_id, .. } = &panel.transport else {
                    return None;
                };
                Some(crate::herdr::HerdrAgentInfo {
                    native_session_id: None,
                    pane_id: pane_id.clone(),
                    workspace_id: String::new(),
                    tab_id: String::new(),
                    agent: panel.agent_id.clone(),
                    display_name: panel.agent_id.clone(),
                    status: "idle".into(),
                    title: None,
                    cwd: None,
                    focused: false,
                    attention: false,
                    state_change_seq: 0,
                    model: None,
                    summary: None,
                })
            })
            .collect();
        snapshot.active_panels = panels;
        snapshot
    }

    #[test]
    fn missing_herdr_target_cannot_be_resurrected_by_hook_status_or_unread() {
        for status in [
            AgentStatus::Working,
            AgentStatus::NeedsInput,
            AgentStatus::Idle,
            AgentStatus::Offline,
        ] {
            let mut panel = herdr_row("gone", status);
            panel.unread = true;
            let mut snapshot = projection_snapshot(vec![panel]);
            assert!(!desktop_sessions_for(&snapshot).is_empty());
            snapshot.herdr.agents.clear();
            assert!(desktop_sessions_for(&snapshot).is_empty());
            assert!(lookup_panel_by_identity(&snapshot, "herdr:gone").is_none());
        }
    }

    #[test]
    fn missing_herdr_check_does_not_use_disconnected_or_other_server_snapshot() {
        let mut snapshot = projection_snapshot(vec![herdr_row("pane", AgentStatus::Working)]);
        snapshot.herdr.agents.clear();
        snapshot.herdr.connected = false;
        assert!(!herdr_target_missing(
            &snapshot,
            &snapshot.active_panels[0].transport
        ));
        #[cfg(target_os = "macos")]
        {
            snapshot.herdr.connected = true;
            snapshot.active_panels[0].transport = PanelTransport::Herdr {
                socket_path: Some("/tmp/another-herdr-server.sock".into()),
                pane_id: "pane".into(),
            };
            assert!(!herdr_target_missing(
                &snapshot,
                &snapshot.active_panels[0].transport
            ));
        }
    }

    fn herdr_row(pane_id: &str, status: AgentStatus) -> ActivePanel {
        let mut pane = herdr_pane_panel(&format!("qodercli:{pane_id}"), status);
        pane.transport = PanelTransport::Herdr {
            socket_path: None,
            pane_id: pane_id.into(),
        };
        pane
    }

    #[test]
    fn desktop_projection_keeps_stable_identity_across_hook_rewrites() {
        let mut hook = herdr_pane_panel("qodercli:dir-name", AgentStatus::Working);
        hook.transport = PanelTransport::Herdr {
            socket_path: Some("/tmp/herdr.sock".into()),
            pane_id: "wK:pV".into(),
        };
        let mut snapshot = projection_snapshot(vec![hook]);
        let first = desktop_sessions_for(&snapshot);
        assert_eq!(
            first[0].identity,
            panel_focus_key(&snapshot.active_panels[0])
        );

        // The hook rewrites the row's id and session, as `panel_match` may do.
        let mut rewritten = snapshot.active_panels[0].clone();
        rewritten.id = "qodercli:session-uuid".into();
        rewritten.session_id = "session-uuid".into();
        snapshot.active_panels = vec![rewritten];
        let second = desktop_sessions_for(&snapshot);
        assert_eq!(second[0].identity, first[0].identity);
    }

    #[test]
    fn desktop_projection_lists_desktop_and_herdr_rows_without_a_selection() {
        let thread = ActivePanel {
            id: "chatgpt:01a0d10c".into(),
            agent_id: "chatgpt".into(),
            session_id: "01a0d10c".into(),
            slot: 1,
            label: "Thread A".into(),
            status: AgentStatus::Working,
            cwd: None,
            jobs_working: 1,
            jobs_waiting: 0,
            jobs_error: 0,
            updated_at_ms: 5,
            status_since_ms: 5,
            last_activity_ms: 0,
            first_seen_ms: 0,
            transport: PanelTransport::CodexThread {
                thread_id: "01a0d10c".into(),
            },
            unread: false,
            unread_since_ms: 0,
        };
        let mut pane = herdr_pane_panel("qodercli:s1", AgentStatus::NeedsInput);
        pane.agent_id = "qodercli".into();
        let snapshot = projection_snapshot(vec![pane, thread]);
        assert_eq!(snapshot.active_agent, None);

        let rows = desktop_sessions_for(&snapshot);
        assert_eq!(rows.len(), 2);
        // Waiting sorts ahead of working, regardless of agent selection.
        assert_eq!(rows[0].agent_id, "qodercli");
        assert_eq!(rows[0].host, "Herdr");
        assert_eq!(rows[1].agent_id, "chatgpt");
        assert_eq!(rows[1].host, "Codex Desktop");
        assert_eq!(rows[1].identity.as_deref(), Some("thread:01a0d10c"));

        let summary = desktop_summary_for(&rows);
        assert_eq!(summary.working, 1);
        assert_eq!(summary.waiting, 1);
    }

    #[test]
    fn desktop_projection_marks_herdr_rows_stale_when_the_host_is_gone() {
        let mut snapshot =
            projection_snapshot(vec![herdr_pane_panel("qodercli:s1", AgentStatus::Idle)]);
        snapshot.herdr.connected = false;
        let rows = desktop_sessions_for(&snapshot);
        assert_eq!(rows.len(), 1, "an idle row stays listed but stamped");
        assert!(rows[0].stale);
        assert_eq!(rows[0].host, "Herdr (失联)");
    }

    #[test]
    fn desktop_projection_keeps_unread_offline_rows_and_drops_the_rest() {
        let retired = herdr_row("wK:gone", AgentStatus::Offline);
        let mut latched = herdr_row("wK:done", AgentStatus::Offline);
        latched.unread = true;
        let rows = desktop_sessions_for(&projection_snapshot(vec![retired, latched]));
        let ids = rows
            .iter()
            .map(|row| row.identity.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![None]);
    }

    #[test]
    fn desktop_projection_survives_more_than_three_rows() {
        let panels = (0..5)
            .map(|index| {
                let mut pane =
                    herdr_pane_panel(&format!("qodercli:s{index}"), AgentStatus::Working);
                pane.transport = PanelTransport::Herdr {
                    socket_path: None,
                    pane_id: format!("wK:p{index}"),
                };
                // Ordinary rows follow evidenced I/O, even when status changed
                // in the opposite order and poll timestamps are identical.
                pane.updated_at_ms = 7;
                pane.status_since_ms = 100 - index as u128;
                pane.last_activity_ms = index as u128;
                pane
            })
            .collect::<Vec<_>>();
        let rows = desktop_sessions_for(&projection_snapshot(panels));
        assert_eq!(rows.len(), 5);
        let identities = rows
            .iter()
            .map(|row| row.identity.as_deref().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            identities,
            // Same priority, so recency wins; the key keeps ties deterministic.
            vec![
                "herdr:wK:p4",
                "herdr:wK:p3",
                "herdr:wK:p2",
                "herdr:wK:p1",
                "herdr:wK:p0",
            ]
        );
        assert_eq!(desktop_summary_for(&rows).working, 5);
    }

    #[test]
    fn opening_one_session_acknowledges_only_that_row() {
        let mut first = herdr_row("wK:p1", AgentStatus::Done);
        first.unread = true;
        first.unread_since_ms = 123;
        let mut second = herdr_row("wK:p2", AgentStatus::Done);
        second.unread = true;
        let first_observed = first.clone();
        let second_observed = second.clone();
        let mut snapshot = projection_snapshot(vec![first, second]);

        assert!(acknowledge_session_unread(
            &mut snapshot,
            "herdr:wK:p1",
            &first_observed
        ));
        assert!(!snapshot.active_panels[0].unread);
        assert_eq!(snapshot.active_panels[0].unread_since_ms, 0);
        assert!(
            snapshot.active_panels[1].unread,
            "the other unread row must stay lit"
        );
        assert!(
            snapshot.agents.iter().any(|agent| agent.unread),
            "the remaining row keeps its agent light on"
        );

        assert!(acknowledge_session_unread(
            &mut snapshot,
            "herdr:wK:p2",
            &second_observed
        ));
        assert!(!snapshot.agents.iter().any(|agent| agent.unread));
    }

    #[test]
    fn acknowledging_an_unknown_identity_changes_nothing() {
        let mut row = herdr_row("wK:p1", AgentStatus::Done);
        row.unread = true;
        let observed = row.clone();
        let mut snapshot = projection_snapshot(vec![row]);
        assert!(!acknowledge_session_unread(
            &mut snapshot,
            "herdr:wK:gone",
            &observed
        ));
        assert!(snapshot.active_panels[0].unread);
    }

    #[test]
    fn identity_lookup_finds_the_panel_after_hooks_rewrite_its_id() {
        let mut row = herdr_row("wK:pV", AgentStatus::Working);
        row.id = "qodercli:session-uuid".into();
        row.session_id = "session-uuid".into();
        let snapshot = projection_snapshot(vec![row]);

        let found = lookup_panel_by_identity(&snapshot, "herdr:wK:pV")
            .expect("identity survives the hook rewriting panel id");
        assert_eq!(found.session_id, "session-uuid");

        assert!(lookup_panel_by_identity(&snapshot, "thread:wK:pV").is_none());
        assert!(lookup_panel_by_identity(&snapshot, "").is_none());
    }
    #[test]
    fn projection_does_not_drop_rows_or_counts_after_twelve_sessions() {
        let panels = (0..20)
            .map(|i| herdr_row(&format!("pane-{i}"), AgentStatus::Working))
            .collect();
        let rows = desktop_sessions_for(&projection_snapshot(panels));
        assert_eq!(rows.len(), 20);
        assert_eq!(desktop_summary_for(&rows).working, 20);
    }

    #[test]
    fn ambiguous_and_unresolved_rows_have_unique_keys_and_cannot_open() {
        let first = herdr_row("same-pane", AgentStatus::Working);
        let mut second = first.clone();
        second.id = "other-agent:same-pane".into();
        let mut unknown = chatgpt_panel("unknown-a", AgentStatus::Idle);
        unknown.label = "same project".into();
        let mut other_unknown = unknown.clone();
        other_unknown.id = "unknown-b".into();
        let snapshot = projection_snapshot(vec![first, second, unknown, other_unknown]);
        let rows = desktop_sessions_for(&snapshot);
        assert_eq!(
            rows.iter()
                .map(|row| &row.row_key)
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
        assert!(rows.iter().all(|row| row.identity.is_none()));
        assert!(lookup_panel_by_identity(&snapshot, "herdr:same-pane").is_none());
    }

    #[test]
    fn retired_rows_do_not_shadow_a_live_target_or_clear_new_news() {
        let mut retired = herdr_row("pane", AgentStatus::Offline);
        retired.unread = true;
        let mut live = herdr_row("pane", AgentStatus::Idle);
        live.session_id = "live-session".into();
        live.unread = true;
        live.unread_since_ms = 10;
        let observed = live.clone();
        let mut snapshot = projection_snapshot(vec![retired, live]);
        assert_eq!(
            lookup_panel_by_identity(&snapshot, "herdr:pane")
                .unwrap()
                .session_id,
            "live-session"
        );
        snapshot.active_panels[1].unread_since_ms = 11;
        assert!(!acknowledge_session_unread(
            &mut snapshot,
            "herdr:pane",
            &observed
        ));
        assert!(snapshot.active_panels.iter().all(|p| p.unread));
        let observed = snapshot.active_panels[1].clone();
        assert!(acknowledge_session_unread(
            &mut snapshot,
            "herdr:pane",
            &observed
        ));
        assert!(snapshot.active_panels[0].unread);
    }

    #[test]
    fn stale_hosts_are_not_counted_as_live_work() {
        let mut snapshot = projection_snapshot(vec![herdr_row("pane", AgentStatus::Working)]);
        snapshot.herdr.connected = false;
        assert_eq!(
            desktop_summary_for(&desktop_sessions_for(&snapshot)).working,
            0
        );
    }

    #[test]
    fn terminal_pid_is_not_advertised_as_exact_desktop_navigation() {
        let mut panel = chatgpt_panel("cli", AgentStatus::Working);
        panel.transport = PanelTransport::Terminal { pid: 42 };
        let rows = desktop_sessions_for(&projection_snapshot(vec![panel]));
        assert!(rows[0].identity.is_none());
    }

    /// A DSH row has a stable session id and switches through the page bridge's
    /// acknowledgement, so it must reach the floating panel, the attention
    /// queue and the six-slot navigation instead of being filtered out as an
    /// unlocatable row.
    #[test]
    fn a_dsh_session_is_an_exact_navigation_target() {
        let mut panel = chatgpt_panel("session-1", AgentStatus::Idle);
        panel.agent_id = "dsh".into();
        panel.transport = PanelTransport::DshWeb {
            session_id: "session-1".into(),
        };
        let rows = desktop_sessions_for(&projection_snapshot(vec![panel]));
        assert_eq!(rows[0].identity.as_deref(), Some("dsh:session-1"));
        assert!(
            crate::session_navigation::display_order(&rows, true)
                .iter()
                .any(|row| row.agent_id == "dsh"),
            "the navigation projection drops identity-less rows"
        );
    }
    #[test]
    fn different_herdr_sockets_have_distinct_focus_keys() {
        let first = herdr_row("pane", AgentStatus::Working);
        let mut second = first.clone();
        second.transport = PanelTransport::Herdr {
            socket_path: Some("/tmp/review-other-herdr.sock".into()),
            pane_id: "pane".into(),
        };
        assert_ne!(panel_focus_key(&first), panel_focus_key(&second));
        let rows = desktop_sessions_for(&projection_snapshot(vec![first, second]));
        assert!(rows.iter().all(|row| row.identity.is_some()));
    }

    #[test]
    fn attention_queue_ranks_waiting_over_error_over_unread_done() {
        let mut waiting = herdr_row("p1", AgentStatus::NeedsInput);
        let mut fresh_waiting = herdr_row("p5", AgentStatus::NeedsInput);
        // The poll stamps run against the event times on purpose: the queue has
        // to rank by when the status changed, not when it was last seen.
        waiting.updated_at_ms = 900;
        waiting.status_since_ms = 100;
        fresh_waiting.updated_at_ms = 50;
        fresh_waiting.status_since_ms = 200;
        let broken = herdr_row("p2", AgentStatus::Error);
        let mut done = herdr_row("p3", AgentStatus::Idle);
        done.unread = true;
        done.unread_since_ms = 5;
        let working = herdr_row("p4", AgentStatus::Working);
        let snapshot = projection_snapshot(vec![broken, done, working, waiting, fresh_waiting]);
        let rows = desktop_sessions_for(&snapshot);
        let attention = rows
            .iter()
            .filter_map(|row| row.attention)
            .collect::<Vec<_>>();
        assert_eq!(
            attention,
            vec![
                AttentionClass::NeedsInput,
                AttentionClass::NeedsInput,
                AttentionClass::Error,
                AttentionClass::Done
            ]
        );
        // Same level sorts by real event time, newest first.
        assert!(rows[0].identity.as_deref().unwrap().contains("p5"));
        assert_eq!(rows[0].event_at_ms, 200);
        // Background states stay out of the queue but remain projected.
        assert!(rows
            .iter()
            .find(|row| row.status == AgentStatus::Working)
            .is_some_and(|row| row.attention.is_none()));
    }

    #[test]
    fn stale_and_offline_rows_never_claim_attention() {
        let mut snapshot = projection_snapshot(vec![herdr_row("gone", AgentStatus::NeedsInput)]);
        snapshot.herdr.connected = false;
        let rows = desktop_sessions_for(&snapshot);
        assert_eq!(rows[0].attention, None);
        assert!(rows[0].stale);

        let mut exited = herdr_row("exit", AgentStatus::Offline);
        exited.unread = true;
        exited.unread_since_ms = 7;
        let rows = desktop_sessions_for(&projection_snapshot(vec![exited]));
        assert_eq!(
            rows.len(),
            1,
            "offline unread rows still show in the full list"
        );
        assert_eq!(rows[0].attention, None);
    }

    #[test]
    fn project_identity_normalizes_paths_and_never_guesses() {
        let companion = desktop_project_for(Some("/work/companion/"));
        assert_eq!(companion.0.as_deref(), Some("/work/companion"));
        assert_eq!(companion.1.as_deref(), Some("companion"));
        assert_eq!(companion.2.as_deref(), Some("work"));
        let other_root = desktop_project_for(Some("/home/x/companion"));
        assert_ne!(
            companion.0, other_root.0,
            "same name, different roots stay apart"
        );
        assert_eq!(
            desktop_project_for(None),
            (None, None, None),
            "no cwd is 未识别项目, not a guessed group"
        );
        assert_eq!(desktop_project_for(Some("relative/path")).0, None);
        assert_eq!(desktop_project_for(Some("/")).0, None);
    }

    #[test]
    fn rows_carry_short_agent_and_source_labels() {
        let mut desktop = chatgpt_panel("t", AgentStatus::Working);
        desktop.transport = PanelTransport::CodexThread {
            thread_id: "uuid-t".into(),
        };
        let cli = herdr_row("pane", AgentStatus::Working);
        let rows = desktop_sessions_for(&projection_snapshot(vec![desktop, cli]));
        let desktop = rows.iter().find(|row| row.agent_id == "chatgpt").unwrap();
        assert_eq!(desktop.agent_short, "Codex");
        assert_eq!(desktop.source, "Desktop");
        let cli = rows.iter().find(|row| row.agent_id == "qodercli").unwrap();
        assert_eq!(cli.source, "Herdr");
    }
}

pub fn plugin_action_catalog(
    plugins: &plugin::PluginRegistry,
    id: &str,
    surface: AgentSurface,
) -> Vec<ActionCatalogItem> {
    let mut catalog = plugins.catalog(id, surface);
    catalog.extend(keymap::action_catalog(None, AgentSurface::Unknown));
    catalog
}
