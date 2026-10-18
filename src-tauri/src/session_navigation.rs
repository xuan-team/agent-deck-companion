//! Companion is a control context, never a monitored process or session owner.
use crate::session_tabs::{Tab, DEFAULT_ORDER};
use crate::{
    session_window::{self, SessionPanelPreferences},
    state::{AgentStatus, AppRuntime, AttentionClass, DesktopSession},
};
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};

pub const ID: &str = "companion";
pub const PAGE_SIZE: usize = 6;
pub const SESSION_KEYS: [&str; 6] = ["K1", "K2", "K3", "K6", "K7", "K8"];
fn slot_for_key(key: &str) -> Option<usize> {
    SESSION_KEYS.iter().position(|candidate| *candidate == key)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderMode {
    #[default]
    Tabs,
    Pages,
}
#[derive(Debug, Clone, Serialize)]
pub struct NavigationView {
    pub encoder_mode: EncoderMode,
    pub hardware_ready: bool,
    pub active: bool,
    pub visible: bool,
    pub show_all: bool,
    pub tab: Tab,
    pub tab_order: [Tab; 3],
    pub visible_tabs: Vec<Tab>,
    pub page: usize,
    pub pages: usize,
    pub revision: u64,
    pub slots: Vec<DesktopSession>,
    pub slot_keys: [&'static str; PAGE_SIZE],
    pub feedback: Option<String>,
}
impl Default for NavigationView {
    fn default() -> Self {
        Self {
            encoder_mode: EncoderMode::Tabs,
            hardware_ready: false,
            active: false,
            visible: false,
            show_all: false,
            tab: Tab::Attention,
            tab_order: DEFAULT_ORDER,
            visible_tabs: DEFAULT_ORDER.to_vec(),
            page: 0,
            pages: 1,
            revision: 0,
            slots: Vec::new(),
            slot_keys: SESSION_KEYS,
            feedback: None,
        }
    }
}
impl NavigationView {
    fn page_key_target(&self, key: &str) -> Option<usize> {
        if !self.active || !self.visible {
            return None;
        }
        let page = match key {
            "K4" => self.page.saturating_sub(1),
            "K5" => self
                .page
                .saturating_add(1)
                .min(self.pages.saturating_sub(1)),
            _ => return None,
        };
        (page != self.page).then_some(page)
    }
    fn encoder_target(&self, delta: i32) -> Option<(Tab, usize)> {
        if !self.active || !self.visible || delta == 0 {
            return None;
        }
        match self.encoder_mode {
            EncoderMode::Tabs => {
                let index = self
                    .visible_tabs
                    .iter()
                    .position(|tab| *tab == self.tab)
                    .unwrap_or(0);
                let next = (index as i64 + i64::from(delta))
                    .clamp(0, self.visible_tabs.len().saturating_sub(1) as i64)
                    as usize;
                (next != index).then(|| (self.visible_tabs[next], 0))
            }
            EncoderMode::Pages => {
                let steps = delta.unsigned_abs() as usize;
                let page = if delta > 0 {
                    self.page.saturating_add(steps)
                } else {
                    self.page.saturating_sub(steps)
                }
                .min(self.pages.saturating_sub(1));
                (page != self.page).then_some((self.tab, page))
            }
        }
    }
}
#[derive(Default)]
pub struct Navigator {
    pub view: NavigationView,
    saved: Option<SessionPanelPreferences>,
    previous_agent: Option<String>,
    previous_surface: Option<crate::agent_actions::AgentSurface>,
    last_press: Option<(String, u128, String)>,
    pending: Option<(u64, usize)>,
}

/// Flatten the actual UI grouping (first group occurrence, then its rows).
pub fn display_order(rows: &[DesktopSession], all: bool) -> Vec<DesktopSession> {
    let mut groups: Vec<(String, Vec<DesktopSession>)> = Vec::new();
    for row in rows.iter().filter(|r| {
        r.identity.is_some() && r.status != AgentStatus::Offline && (all || r.attention.is_some())
    }) {
        let key = if all {
            format!("agent:{}", row.agent_short)
        } else {
            row.project_id.clone().unwrap_or_default()
        };
        if let Some((_, entries)) = groups.iter_mut().find(|(k, _)| *k == key) {
            entries.push(row.clone());
        } else {
            groups.push((key, vec![row.clone()]));
        }
    }
    groups.into_iter().flat_map(|(_, rows)| rows).collect()
}
impl Navigator {
    pub fn reconcile_tabs(&mut self, rows: &[DesktopSession]) {
        self.view.visible_tabs = crate::session_tabs::visible_order(&self.view.tab_order, rows);
        // Do not invalidate a confirmed open before its window restoration.
        if self.view.active
            && self.pending.is_none()
            && !self.view.visible_tabs.contains(&self.view.tab)
        {
            self.populate(
                rows,
                crate::session_tabs::fallback_tab(&self.view.tab_order, rows),
                0,
            );
        }
    }
    fn reorder(&mut self, order: [Tab; 3]) {
        self.view.visible_tabs = order
            .iter()
            .copied()
            .filter(|tab| self.view.visible_tabs.contains(tab))
            .collect();
        self.view.tab_order = order;
        self.view.revision += 1;
        self.pending = None;
        self.last_press = None;
    }
    fn toggle_encoder_mode(&mut self) -> bool {
        if !self.view.active || !self.view.visible {
            return false;
        }
        self.view.encoder_mode = match self.view.encoder_mode {
            EncoderMode::Tabs => EncoderMode::Pages,
            EncoderMode::Pages => EncoderMode::Tabs,
        };
        self.view.revision += 1;
        self.pending = None;
        self.last_press = None;
        self.view.feedback = None;
        true
    }
    fn press(&mut self, key: &str, source: &str, now: u128) -> Option<bool> {
        if self
            .last_press
            .as_ref()
            .is_some_and(|(old, at, old_source)| {
                old == key && old_source != source && now.saturating_sub(*at) < 80
            })
        {
            return None;
        }
        let double = self
            .last_press
            .as_ref()
            .is_some_and(|(old, at, _)| old == key && now.saturating_sub(*at) <= 300);
        self.last_press = if double {
            None
        } else {
            Some((key.into(), now, source.into()))
        };
        Some(double)
    }
    fn take_restore(
        &mut self,
        expected: Option<u64>,
    ) -> Option<(
        Option<SessionPanelPreferences>,
        Option<String>,
        Option<crate::agent_actions::AgentSurface>,
    )> {
        if !self.view.active || expected.is_some_and(|revision| revision != self.view.revision) {
            return None;
        }
        self.pending = None;
        self.view.active = false;
        self.view.revision += 1;
        self.last_press = None;
        Some((
            self.saved.take(),
            self.previous_agent.take(),
            self.previous_surface.take(),
        ))
    }
    pub fn project(&self, rows: &[DesktopSession]) -> NavigationView {
        let mut view = self.view.clone();
        for slot in &mut view.slots {
            if let Some(live) = rows.iter().find(|r| {
                r.identity == slot.identity && r.status != AgentStatus::Offline && !r.stale
            }) {
                // Keep the captured event version and identity; metadata can refresh.
                slot.label = live.label.clone();
                slot.status = live.status;
                slot.activity = live.activity.clone();
                slot.recent_activity_ms = live.recent_activity_ms;
                slot.project_name = live.project_name.clone();
                slot.project_hint = live.project_hint.clone();
                slot.stale = slot.attention.is_some_and(|kind| {
                    let version = if kind == AttentionClass::Done {
                        slot.unread_since_ms
                    } else {
                        slot.event_at_ms
                    };
                    version > 0
                        && !crate::notifications::matches_event(
                            rows,
                            slot.identity.as_deref().unwrap_or_default(),
                            kind,
                            version,
                        )
                });
            } else {
                slot.stale = true;
            }
        }
        view
    }
    fn populate(&mut self, rows: &[DesktopSession], tab: Tab, page: usize) {
        self.view.visible_tabs = crate::session_tabs::visible_order(&self.view.tab_order, rows);
        let tab = if self.view.visible_tabs.contains(&tab) {
            tab
        } else {
            crate::session_tabs::fallback_tab(&self.view.tab_order, rows)
        };
        let filtered = rows
            .iter()
            .filter(|row| tab.matches(row))
            .cloned()
            .collect::<Vec<_>>();
        let rows = display_order(&filtered, tab != Tab::Attention);
        self.view.slot_keys = SESSION_KEYS;
        self.view.pages = rows.len().div_ceil(PAGE_SIZE).max(1);
        self.view.page = page.min(self.view.pages - 1);
        self.view.slots = rows
            .into_iter()
            .skip(self.view.page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .collect();
        self.view.tab = tab;
        self.view.show_all = tab != Tab::Attention;
        self.view.revision += 1;
        self.view.feedback = None;
        self.pending = None;
    }
}
pub fn active(state: &AppRuntime) -> bool {
    state
        .navigation
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .view
        .active
}
fn show(app: &AppHandle, view: &NavigationView) {
    session_window::apply_temporary(app, view.visible, false, view.tab);
}
pub fn enter(app: &AppHandle, state: &Arc<AppRuntime>, key: &str, source: &str) {
    let now = crate::state::now_ms();
    let snapshot = state.read();
    let (view, double) = {
        let mut nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
        let Some(double) = nav.press(key, source, now) else {
            return;
        };
        crate::activation::reserve_focus_request(state);
        if !nav.view.active {
            let preferences = session_window::load_preferences();
            nav.view.tab_order = preferences.tab_order;
            nav.saved = Some(preferences);
            nav.previous_agent = snapshot.active_agent.clone();
            nav.previous_surface = snapshot.active_surface;
            nav.view.active = true;
            nav.view.encoder_mode = EncoderMode::Tabs;
            let first = nav.view.tab_order[0];
            nav.populate(&snapshot.desktop_sessions, first, 0);
        }
        nav.view.visible = true;
        if double {
            nav.view.encoder_mode = EncoderMode::Tabs;
            nav.populate(&snapshot.desktop_sessions, Tab::Attention, 0);
        }
        (nav.view.clone(), double)
    };
    // No generic activation: presenting a control surface must not raise an Agent.
    {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|p| p.into_inner());
        snapshot.active_agent = Some(ID.into());
        snapshot.active_surface = Some(crate::agent_actions::AgentSurface::Desktop);
        for agent in &mut snapshot.agents {
            agent.active = agent.id == ID;
        }
    }
    show(app, &view);
    if double && view.tab == Tab::Attention {
        open_slot(app, state, 0, view.revision);
    } else if double {
        feedback(app, state, "暂无需处理会话");
    }
}
pub fn navigate(app: &AppHandle, state: &Arc<AppRuntime>, tab: Tab, page: usize) {
    crate::activation::reserve_focus_request(state);
    let rows = state.read().desktop_sessions;
    let view = {
        let mut nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
        if !nav.view.active || !nav.view.visible {
            return;
        }
        nav.last_press = None;
        nav.populate(&rows, tab, page);
        nav.view.clone()
    };
    show(app, &view);
}
pub fn toggle(app: &AppHandle, state: &Arc<AppRuntime>) {
    crate::activation::reserve_focus_request(state);
    let view = {
        let mut nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
        if !nav.view.active {
            None
        } else {
            nav.last_press = None;
            nav.pending = None;
            nav.view.revision += 1;
            nav.view.visible = !nav.view.visible;
            Some(nav.view.clone())
        }
    };
    if let Some(view) = view {
        show(app, &view);
    } else {
        session_window::toggle(app);
    }
}
pub fn exit(app: &AppHandle, state: &Arc<AppRuntime>, restore_agent: bool) {
    exit_matching(app, state, restore_agent, None);
}
pub fn revision(state: &AppRuntime) -> Option<u64> {
    let nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
    nav.view.active.then_some(nav.view.revision)
}
pub fn finish_open(app: &AppHandle, state: &Arc<AppRuntime>, revision: Option<u64>) {
    if let Some(revision) = revision {
        exit_matching(app, state, false, Some(revision));
    }
}
fn exit_matching(
    app: &AppHandle,
    state: &Arc<AppRuntime>,
    restore_agent: bool,
    expected: Option<u64>,
) {
    let (saved, agent, surface) = {
        let mut nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
        let Some(restore) = nav.take_restore(expected) else {
            return;
        };
        restore
    };
    if restore_agent {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|p| p.into_inner());
        snapshot.active_agent = agent.clone();
        snapshot.active_surface = surface;
        for item in &mut snapshot.agents {
            item.active = agent.as_deref() == Some(&item.id);
        }
    }
    if let Some(saved) = saved {
        session_window::apply_temporary(app, saved.visible, saved.collapsed, saved.tab);
    }
}
#[cfg(target_os = "macos")]
pub fn open_slot(app: &AppHandle, state: &Arc<AppRuntime>, index: usize, revision: u64) {
    let rows = state.read().desktop_sessions;
    let selection = {
        let mut nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
        nav.last_press = None;
        if nav.pending == Some((revision, index)) {
            return;
        }
        if !nav.view.active || !nav.view.visible || nav.view.revision != revision {
            return;
        }
        let row = nav.project(&rows).slots.get(index).cloned();
        if row.as_ref().is_some_and(|row| row.stale) {
            nav.view.feedback = Some("会话状态已变化，请切换标签刷新".into());
            drop(nav);
            crate::state::emit_state(app, state);
            return;
        }
        if row.is_some() {
            nav.pending = Some((revision, index));
        }
        row
    };
    let Some(row) = selection else {
        feedback(app, state, "该位置暂无可打开的会话");
        return;
    };
    let Some(identity) = row.identity else {
        return;
    };
    let generation = crate::activation::reserve_focus_request(state);
    let state = state.clone();
    let app = app.clone();
    std::thread::spawn(move || {
        let expected = row
            .attention
            .map(|kind| {
                (
                    kind,
                    if kind == AttentionClass::Done {
                        row.unread_since_ms
                    } else {
                        row.event_at_ms
                    },
                )
            })
            .filter(|(_, version)| *version > 0);
        let outcome = if let Some(expected) = expected {
            crate::activation::open_notification_request(&state, &identity, generation, expected)
        } else {
            crate::activation::open_session_request(&state, &identity, generation)
        };
        // Do not restore a newer navigation transaction's window.
        let same = {
            let nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
            nav.view.active
                && nav.view.revision == revision
                && nav.pending == Some((revision, index))
        };
        if same {
            state
                .navigation
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pending = None;
            if outcome == crate::activation::OpenOutcome::Opened {
                finish_open(&app, &state, Some(revision));
            } else {
                feedback(&app, &state, &format!("会话打开未完成：{outcome:?}"));
            }
        }
        crate::state::emit_state(&app, &state);
    });
}
#[cfg(not(target_os = "macos"))]
pub fn open_slot(app: &AppHandle, state: &Arc<AppRuntime>, _: usize, _: u64) {
    feedback(app, state, "当前平台不支持会话定位");
}
fn feedback(app: &AppHandle, state: &AppRuntime, message: &str) {
    state
        .navigation
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .view
        .feedback = Some(message.into());
    let _ = app.emit("companion_navigation_feedback", message);
}

pub fn function_keys(view: &NavigationView) -> Vec<crate::agent_actions::FunctionKeyState> {
    (1..=10)
        .map(|i| {
            let key = format!("K{i}");
            let index = slot_for_key(&key);
            let row = index.and_then(|index| view.slots.get(index));
            let (label, hint, supported, action) = match i {
                4 => (
                    "PREV PAGE".into(),
                    "上一页".into(),
                    view.visible && view.page > 0,
                    "companion.page.previous".into(),
                ),
                5 => (
                    "NEXT PAGE".into(),
                    "下一页".into(),
                    view.visible && view.page + 1 < view.pages,
                    "companion.page.next".into(),
                ),
                9 => (
                    "CANCEL".into(),
                    "退出选择并恢复原浮窗状态".into(),
                    true,
                    "companion.cancel".into(),
                ),
                10 => (
                    "PANEL".into(),
                    "隐藏 / 展示会话浮窗".into(),
                    true,
                    "session_panel.toggle".into(),
                ),
                _ => {
                    let index = index.unwrap();
                    (
                        format!("SESSION {}", index + 1),
                        row.map(|r| r.label.clone())
                            .unwrap_or_else(|| "暂无可打开的会话".into()),
                        view.visible && row.is_some_and(|r| !r.stale),
                        format!("companion.slot.{}", index + 1),
                    )
                }
            };
            crate::agent_actions::FunctionKeyState {
                key,
                label,
                hint,
                supported,
                action_id: Some(action),
                source: "companion".into(),
                customized: false,
            }
        })
        .collect()
}

/// Return true only when the event belongs to the control context.
pub fn handle(event: &crate::state::DeviceEvent, app: &AppHandle, state: &Arc<AppRuntime>) -> bool {
    if event.kind == "encoder" && active(state) {
        let view = state.read().companion_navigation;
        if let Some((tab, page)) = event.delta.and_then(|delta| view.encoder_target(delta)) {
            navigate(app, state, tab, page);
        }
        return true;
    }
    if !matches!(
        event.kind.as_str(),
        "key_down" | "agent_key" | "agent_key_primed"
    ) {
        return false;
    }
    let Some(key) = event.key.as_deref().map(str::to_ascii_uppercase) else {
        return false;
    };
    let assigned = state
        .config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .assigned_agent(&key)
        .map(str::to_owned);
    if assigned.as_deref() == Some(ID) {
        enter(app, state, &key, &event.kind);
        return true;
    }
    if active(state) {
        if assigned.is_some() {
            crate::activation::reserve_focus_request(state);
            exit(app, state, true);
            return false;
        }
        if key == "ENCODER_PRESS" {
            let changed = state
                .navigation
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .toggle_encoder_mode();
            if changed {
                crate::activation::reserve_focus_request(state);
            }
            return true;
        }
        if key == "K10" {
            toggle(app, state);
            return true;
        }
        let view = state.read().companion_navigation;
        if key == "K9" {
            crate::activation::reserve_focus_request(state);
            exit(app, state, true);
        } else if key == "K4" || key == "K5" {
            if let Some(page) = view.page_key_target(&key) {
                navigate(app, state, view.tab, page);
            }
        } else if let Some(index) = slot_for_key(&key) {
            open_slot(app, state, index, view.revision);
        }
        // Consume all function keys; old Approve/Reject must never run here.
        return key.starts_with('K');
    }
    let config = state
        .config
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let snapshot = state.snapshot.read().unwrap_or_else(|p| p.into_inner());
    let action = config
        .configured_action(
            snapshot.active_agent.as_deref().unwrap_or("__no_agent__"),
            crate::keymap::surface_key(
                snapshot
                    .active_surface
                    .unwrap_or(crate::agent_actions::AgentSurface::Unknown),
            ),
            &key,
        )
        .cloned();
    drop(snapshot);
    if matches!(action, Some(crate::profile::ConfiguredAction::SystemAction { action_id }) if action_id == "session_panel.toggle")
    {
        toggle(app, state);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_attention_is_skipped_and_pending_opens_keep_their_revision() {
        let mut plain = row("qwen", "p", "qwen");
        plain.attention = None;
        let mut nav = Navigator::default();
        nav.view.active = true;
        nav.view.visible = true;
        nav.populate(&[plain.clone()], Tab::Attention, 0);
        assert_eq!(nav.view.tab, Tab::Others);
        assert_eq!(nav.view.visible_tabs, vec![Tab::Codex, Tab::Others]);
        assert_eq!(nav.view.encoder_target(-1), Some((Tab::Codex, 0)));
        let urgent = row("qwen", "p", "qwen");
        nav.populate(&[urgent], Tab::Attention, 0);
        let revision = nav.view.revision;
        nav.pending = Some((revision, 0));
        nav.reconcile_tabs(&[plain.clone()]);
        assert_eq!(nav.view.revision, revision);
        nav.pending = None;
        nav.reconcile_tabs(&[plain]);
        assert_eq!(nav.view.tab, Tab::Others);
        assert!(nav.view.revision > revision);
        assert_eq!(nav.view.tab_order, DEFAULT_ORDER);
    }

    #[test]
    fn three_tabs_split_codex_and_others_but_attention_aggregates_both() {
        let mut idle = row("idle", "p", "chatgpt");
        idle.attention = None;
        idle.status = AgentStatus::Idle;
        let rows = vec![
            row("desktop", "p", "chatgpt"),
            row("cli", "q", "codex"),
            row("qwen", "r", "qwen"),
            idle,
        ];
        let mut nav = Navigator::default();
        nav.populate(&rows, Tab::Attention, 0);
        assert_eq!(nav.view.slots.len(), 3);
        nav.populate(&rows, Tab::Codex, 0);
        assert_eq!(nav.view.slots.len(), 3);
        assert!(nav.view.slots.iter().all(crate::session_tabs::is_codex));
        nav.populate(&rows, Tab::Others, 0);
        assert_eq!(nav.view.slots.len(), 1);
        assert_eq!(nav.view.slots[0].row_key, "qwen");
    }
    #[test]
    fn custom_order_drives_encoder_and_invalidates_pending_without_replacing_slots() {
        let mut nav = Navigator::default();
        nav.view.active = true;
        nav.view.visible = true;
        nav.populate(&[row("a", "p", "chatgpt")], Tab::Codex, 0);
        let revision = nav.view.revision;
        nav.pending = Some((revision, 0));
        nav.reorder([Tab::Attention, Tab::Others, Tab::Codex]);
        assert!(nav.pending.is_none());
        assert!(nav.view.revision > revision);
        assert_eq!(nav.view.slots[0].row_key, "a");
        assert_eq!(nav.view.tab, Tab::Codex);
        assert_eq!(nav.view.encoder_target(-1), Some((Tab::Others, 0)));
        assert_eq!(nav.view.encoder_target(1), None);
        nav.populate(&[], Tab::Attention, 0);
        assert_eq!(nav.view.tab, Tab::Others);
        assert_eq!(nav.view.visible_tabs, vec![Tab::Others, Tab::Codex]);
        assert_eq!(nav.view.encoder_target(1), Some((Tab::Codex, 0)));
        assert_eq!(nav.view.encoder_target(i32::MAX), Some((Tab::Codex, 0)));
        assert_eq!(nav.view.encoder_target(i32::MIN), None);
    }
    #[test]
    fn project_names_refresh_while_frozen_group_identity_is_preserved() {
        let mut nav = Navigator::default();
        let original = row("a", "old", "chatgpt");
        nav.populate(&[original.clone()], Tab::Codex, 0);
        let mut live = original;
        live.project_id = Some("new".into());
        live.project_name = Some("New project".into());
        let projected = nav.project(&[live]);
        assert_eq!(projected.slots[0].project_id.as_deref(), Some("old"));
        assert_eq!(
            projected.slots[0].project_name.as_deref(),
            Some("New project")
        );
    }

    #[test]
    fn six_slots_and_page_keys_cover_fifteen_rows_without_overlap() {
        let mut nav = Navigator::default();
        nav.view.active = true;
        nav.view.visible = true;
        let rows = (0..15)
            .map(|i| row(&i.to_string(), "x", "qwen"))
            .collect::<Vec<_>>();
        for (page, start, len) in [(0, 0, 6), (1, 6, 6), (2, 12, 3)] {
            nav.populate(&rows, Tab::Others, page);
            assert_eq!(nav.view.pages, 3);
            assert_eq!(nav.view.slots.len(), len);
            let keys = function_keys(&nav.view);
            for (key, index) in [
                ("K1", 0),
                ("K2", 1),
                ("K3", 2),
                ("K6", 3),
                ("K7", 4),
                ("K8", 5),
            ] {
                assert_eq!(slot_for_key(key), Some(index));
                let state = keys.iter().find(|s| s.key == key).unwrap();
                assert_eq!(state.label, format!("SESSION {}", index + 1));
                assert_eq!(state.supported, index < len);
                if index < len {
                    assert_eq!(state.hint, (start + index).to_string());
                }
            }
            assert_eq!(keys[3].supported, page > 0);
            assert_eq!(keys[4].supported, page < 2);
            assert_eq!(slot_for_key("K4"), None);
            assert_eq!(slot_for_key("K5"), None);
            assert!(keys[8].supported);
            assert!(keys[9].supported);
        }
    }

    use super::*;
    fn row(id: &str, project: &str, agent: &str) -> DesktopSession {
        DesktopSession {
            error_acknowledged: false,
            activity: Default::default(),
            row_key: id.into(),
            identity: Some(id.into()),
            agent_id: agent.into(),
            display_name: agent.into(),
            agent_short: agent.into(),
            source: "Herdr".into(),
            label: id.into(),
            status: AgentStatus::NeedsInput,
            unread: false,
            attention: Some(AttentionClass::NeedsInput),
            event_at_ms: 10,
            observed_since_ms: 1,
            event_is_activity: false,
            unread_since_ms: 0,
            project_id: Some(project.into()),
            project_name: Some(project.into()),
            project_hint: None,
            host: "Herdr".into(),
            stale: false,
            updated_at_ms: 10,
            recent_activity_ms: 10,
        }
    }
    #[test]
    fn encoder_pages_preserve_tab_and_stop_at_boundaries() {
        let mut nav = Navigator::default();
        nav.view.active = true;
        nav.view.visible = true;
        let rows = (0..12)
            .map(|i| row(&i.to_string(), "x", "qwen"))
            .collect::<Vec<_>>();
        nav.populate(&rows, Tab::Others, 0);
        assert_eq!(nav.view.encoder_target(-1), Some((Tab::Codex, 0)));
        let slots = nav.view.slots.clone();
        let revision = nav.view.revision;
        nav.pending = Some((revision, 0));
        assert!(nav.toggle_encoder_mode());
        assert!(nav.pending.is_none());
        assert!(nav.view.revision > revision);
        assert_eq!(nav.view.slots[0].identity, slots[0].identity);
        assert_eq!(nav.view.encoder_target(1), Some((Tab::Others, 1)));
        nav.populate(&rows, Tab::Others, 1);
        assert_eq!(nav.view.slots[0].row_key, "6");
        assert_eq!(nav.view.encoder_target(-1), Some((Tab::Others, 0)));
        assert_eq!(nav.view.encoder_target(i32::MAX), None);
        nav.populate(&rows, Tab::Others, 1);
        assert_eq!(nav.view.encoder_target(1), None);
        assert_eq!(nav.view.encoder_target(i32::MIN), Some((Tab::Others, 0)));
        assert!(nav.toggle_encoder_mode());
        assert_eq!(nav.view.encoder_target(-1), Some((Tab::Codex, 0)));
        assert_eq!(nav.view.encoder_target(1), None);
    }
    #[test]
    fn encoder_hidden_and_single_page_inputs_do_not_change_slots() {
        let mut nav = Navigator::default();
        nav.view.active = true;
        nav.view.visible = true;
        nav.populate(&[row("a", "x", "qwen")], Tab::Attention, 0);
        assert!(nav.toggle_encoder_mode());
        assert_eq!(nav.view.encoder_target(1), None);
        assert_eq!(nav.view.encoder_target(-1), None);
        assert_eq!(nav.view.encoder_target(0), None);
        let revision = nav.view.revision;
        nav.view.visible = false;
        assert!(!nav.toggle_encoder_mode());
        assert_eq!(nav.view.revision, revision);
        assert_eq!(nav.view.encoder_target(1), None);
    }

    #[test]
    fn grouping_is_flattened_before_slots_are_numbered() {
        let rows = vec![
            row("a", "x", "qwen"),
            row("b", "y", "codex"),
            row("c", "x", "claude"),
        ];
        assert_eq!(
            display_order(&rows, false)
                .iter()
                .map(|r| r.row_key.as_str())
                .collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        assert_eq!(
            display_order(&rows, true)
                .iter()
                .map(|r| r.row_key.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }
    #[test]
    fn refresh_does_not_replace_removed_slots_or_insert_new_rows() {
        let mut nav = Navigator::default();
        nav.populate(
            &[row("a", "x", "qwen"), row("b", "x", "qwen")],
            Tab::Attention,
            0,
        );
        let view = nav.project(&[row("new", "x", "qwen"), row("b", "x", "qwen")]);
        assert_eq!(view.slots[0].row_key, "a");
        assert!(view.slots[0].stale);
        assert_eq!(view.slots[1].row_key, "b");
        assert!(!view.slots[1].stale);
        assert_eq!(view.revision, nav.view.revision);
    }
    #[test]
    fn newer_attention_versions_invalidate_the_captured_slot() {
        let mut nav = Navigator::default();
        let first = row("a", "x", "qwen");
        nav.populate(&[first.clone()], Tab::Attention, 0);
        let mut next = first;
        next.event_at_ms = 11;
        let view = nav.project(&[next]);
        assert!(view.slots[0].stale);
        assert!(!function_keys(&view)[0].supported);
    }
    #[test]
    fn paging_and_empty_lists_never_fall_back_to_old_slots() {
        let rows = (0..7)
            .map(|i| row(&i.to_string(), "x", "qwen"))
            .collect::<Vec<_>>();
        let mut nav = Navigator::default();
        nav.populate(&rows, Tab::Others, 0);
        assert_eq!(nav.view.slots.len(), 6);
        assert_eq!(nav.view.pages, 2);
        let revision = nav.view.revision;
        nav.populate(&rows, Tab::Others, 99);
        assert_eq!(nav.view.page, 1);
        assert_eq!(nav.view.slots[0].row_key, "6");
        assert_eq!(nav.view.slots.len(), 1);
        assert!(nav.view.revision > revision);
        nav.populate(&[], Tab::Attention, 0);
        assert!(nav.view.slots.is_empty());
        assert_eq!(nav.view.pages, 1);
    }
    #[test]
    fn duplicate_event_sources_are_not_a_double_click() {
        let mut nav = Navigator::default();
        assert_eq!(nav.press("A5", "agent_key", 1000), Some(false));
        assert_eq!(nav.press("A5", "key_down", 1020), None);
        assert_eq!(nav.press("A5", "agent_key", 1150), Some(true));
        assert_eq!(nav.press("A5", "agent_key", 1200), Some(false));
        assert_eq!(nav.press("A4", "agent_key", 1250), Some(false));
    }
    #[test]
    fn stale_open_callbacks_cannot_restore_a_new_navigation_transaction() {
        let mut nav = Navigator::default();
        let original = SessionPanelPreferences {
            visible: false,
            collapsed: true,
            show_all: true,
            ..Default::default()
        };
        nav.saved = Some(original);
        nav.previous_agent = Some("qwen".into());
        nav.view.active = true;
        nav.view.revision = 3;
        assert!(nav.take_restore(Some(2)).is_none());
        assert!(nav.view.active);
        assert_eq!(nav.saved, Some(original));
        let restore = nav.take_restore(Some(3)).unwrap();
        assert_eq!(restore.0, Some(original));
        assert_eq!(restore.1.as_deref(), Some("qwen"));
        assert!(!nav.view.active);
        assert!(nav.take_restore(None).is_none());
    }

    #[test]
    fn hidden_navigation_disables_session_keys_but_keeps_panel_toggle() {
        let mut nav = Navigator::default();
        nav.populate(&[row("a", "x", "qwen")], Tab::Attention, 0);
        nav.view.visible = false;
        let keys = function_keys(&nav.view);
        assert!(keys[..8].iter().all(|key| !key.supported));
        assert!(keys[8].supported);
        assert!(keys[9].supported);
    }
}

pub fn reveal_attention(app: &AppHandle, state: &Arc<AppRuntime>) {
    let hidden = {
        let nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
        !nav.view.visible
    };
    if hidden {
        toggle(app, state);
    }
    navigate(app, state, Tab::Attention, 0);
}

pub fn set_tab_order(app: &AppHandle, state: &Arc<AppRuntime>, order: [Tab; 3]) {
    let view = {
        let mut nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
        nav.reorder(order);
        nav.view.clone()
    };
    if view.active {
        show(app, &view);
    } else {
        let _ = app.emit(
            "session_panel_preferences_changed",
            session_window::load_preferences(),
        );
    }
}
