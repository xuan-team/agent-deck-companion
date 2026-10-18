use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};

use crate::agent_actions;
use crate::function_hint;
use crate::hook_integration;
use crate::keymap;
use crate::monitor;
use crate::profile;
use crate::state::{
    emit_state, now_ms, parse_surface, replace_config, save_profile_for_state, show_function_hint,
    update_hint_preferences_state, validate_agent_id, AppRuntime, DeviceEvent,
};

#[tauri::command]
pub fn get_state(state: tauri::State<'_, Arc<AppRuntime>>) -> crate::state::AppSnapshot {
    state.read()
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn herdr_agent_action(
    pane_id: String,
    action: String,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::state::AppSnapshot, String> {
    if !crate::herdr::agent_action(&pane_id, &action) {
        return Err(format!("Herdr action failed: {action}"));
    }
    Ok(state.read())
}

/// Open one desktop-session row by its stable identity. The raise happens on
/// a blocking thread; the UI gets the final outcome back so a failed open can
/// keep its unread light.
#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn open_desktop_session(
    identity: String,
    expected_kind: Option<crate::state::AttentionClass>,
    expected_version: Option<u128>,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::activation::OpenOutcome, String> {
    let runtime = state.inner().clone();
    let expected = match (expected_kind, expected_version) {
        (Some(kind), Some(version)) => Some((kind, version)),
        (None, None) => None,
        _ => return Err("notification event version is incomplete".into()),
    };
    // Reserve in command arrival order, before blocking-pool scheduling.
    let navigation_revision = crate::session_navigation::revision(&runtime);
    let generation = crate::activation::reserve_focus_request(&runtime);
    let blocking_runtime = runtime.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || match expected {
        Some(event) => crate::activation::open_notification_request(
            &blocking_runtime,
            &identity,
            generation,
            event,
        ),
        None => crate::activation::open_session_request(&blocking_runtime, &identity, generation),
    })
    .await
    .map_err(|error| error.to_string())?;
    if outcome == crate::activation::OpenOutcome::Opened {
        crate::session_navigation::finish_open(&app, &runtime, navigation_revision);
    }
    emit_state(&app, &runtime);
    Ok(outcome)
}

#[tauri::command]
pub fn assign_agent_slot(
    slot: String,
    agent_id: String,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::state::AppSnapshot, String> {
    validate_agent_id(state.inner(), &agent_id)?;
    let mut next = state.config.lock().map_err(|_| "config poisoned")?.clone();
    next.assign_agent(&slot, &agent_id)?;
    save_profile_for_state(state.inner(), &next)?;
    replace_config(state.inner(), next);
    emit_state(&app, state.inner());
    Ok(state.read())
}

#[tauri::command]
pub fn set_key_binding(
    agent_id: String,
    surface: String,
    key: String,
    action_id: String,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::state::AppSnapshot, String> {
    validate_agent_id(state.inner(), &agent_id)?;
    let surface_value =
        parse_surface(&surface).ok_or_else(|| format!("unknown Agent surface: {surface}"))?;
    let surface_name = keymap::surface_key(surface_value);
    let action = if action_id == "disabled" {
        profile::ConfiguredAction::Disabled
    } else if matches!(
        action_id.as_str(),
        "voice_input.toggle" | "session_panel.toggle"
    ) {
        profile::ConfiguredAction::SystemAction { action_id }
    } else {
        let available =
            agent_actions::binding_for_action(Some(&agent_id), surface_value, &action_id).is_some()
                || state
                    .plugins
                    .catalog(&agent_id, surface_value)
                    .iter()
                    .any(|item| item.action_id == action_id);
        if !available {
            return Err(format!(
                "action {action_id} is unavailable for {agent_id}/{surface_name}"
            ));
        }
        profile::ConfiguredAction::AgentAction { action_id }
    };
    let mut next = state.config.lock().map_err(|_| "config poisoned")?.clone();
    next.set_agent_action(&agent_id, surface_name, &key, action)?;
    save_profile_for_state(state.inner(), &next)?;
    replace_config(state.inner(), next);
    emit_state(&app, state.inner());
    Ok(state.read())
}

#[tauri::command]
pub fn reset_key_binding(
    agent_id: String,
    surface: String,
    key: String,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::state::AppSnapshot, String> {
    validate_agent_id(state.inner(), &agent_id)?;
    let surface_value =
        parse_surface(&surface).ok_or_else(|| format!("unknown Agent surface: {surface}"))?;
    let mut next = state.config.lock().map_err(|_| "config poisoned")?.clone();
    next.reset_agent_action(&agent_id, keymap::surface_key(surface_value), &key);
    save_profile_for_state(state.inner(), &next)?;
    replace_config(state.inner(), next);
    emit_state(&app, state.inner());
    Ok(state.read())
}

#[tauri::command]
pub fn reset_agent_bindings(
    agent_id: String,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::state::AppSnapshot, String> {
    validate_agent_id(state.inner(), &agent_id)?;
    let mut next = state.config.lock().map_err(|_| "config poisoned")?.clone();
    next.active_mut().agent_keys.remove(&agent_id);
    save_profile_for_state(state.inner(), &next)?;
    replace_config(state.inner(), next);
    emit_state(&app, state.inner());
    Ok(state.read())
}

#[tauri::command]
pub fn set_hint_preferences(
    auto_show: bool,
    pinned: bool,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> crate::state::AppSnapshot {
    update_hint_preferences_state(state.inner(), auto_show, pinned);
    crate::tray::refresh_tray_menu(&app, state.inner());
    if pinned {
        show_function_hint(&app, state.inner(), false);
    } else if app
        .get_webview_window(function_hint::WINDOW_LABEL)
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
    {
        show_function_hint(&app, state.inner(), true);
    }
    emit_state(&app, state.inner());
    state.read()
}

#[tauri::command]
pub fn show_function_key_hint(app: AppHandle, state: tauri::State<'_, Arc<AppRuntime>>) {
    show_function_hint(&app, state.inner(), false);
}

#[tauri::command]
pub fn hide_function_key_hint(app: AppHandle, state: tauri::State<'_, Arc<AppRuntime>>) {
    use std::sync::atomic::Ordering;
    state.hint_generation.fetch_add(1, Ordering::SeqCst);
    function_hint::hide(&app);
}

#[tauri::command]
pub fn show_session_panel(app: AppHandle) {
    crate::session_window::set_visible(&app, true);
}

#[tauri::command]
pub fn get_session_panel_preferences(
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> crate::session_window::SessionPanelPreferences {
    panel_preferences(state.inner())
}

pub fn panel_preferences(
    state: &Arc<AppRuntime>,
) -> crate::session_window::SessionPanelPreferences {
    let mut preferences = crate::session_window::load_preferences();
    let nav = state.navigation.lock().unwrap_or_else(|p| p.into_inner());
    if nav.view.active {
        preferences.visible = nav.view.visible;
        preferences.collapsed = false;
        preferences.show_all = nav.view.show_all;
        preferences.tab = nav.view.tab;
        preferences.tab_order = nav.view.tab_order;
    }
    preferences
}

#[tauri::command]
pub fn set_session_panel_collapsed(collapsed: bool, app: AppHandle) -> Result<(), String> {
    crate::session_window::set_collapsed(&app, collapsed)
}

/// Temporarily grow the collapsed bar for one notification card. Never changes
/// the user's collapse or visibility preference.
#[tauri::command]
pub fn set_session_notification_overlay(active: bool, app: AppHandle) -> Result<(), String> {
    crate::session_window::set_notification_overlay(&app, active)
}

#[tauri::command]
pub fn hide_session_panel(app: AppHandle, state: tauri::State<'_, Arc<AppRuntime>>) {
    if crate::session_navigation::active(&state) {
        crate::session_navigation::toggle(&app, &state);
        emit_state(&app, &state);
        return;
    }
    crate::session_window::set_visible(&app, false);
}

/// Acknowledge one session's unread result without opening it. Same guard as
/// the open path: only the exact completion that was observed is cleared, so
/// a Done that arrived during the click survives.
#[tauri::command]
pub fn mark_session_read(
    identity: String,
    unread_since_ms: u128,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<bool, String> {
    let runtime = state.inner().clone();
    let observed = {
        let snapshot = runtime.snapshot.read().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        });
        crate::state::lookup_panel_by_identity(&snapshot, &identity)
    };
    let Some(observed) = observed else {
        return Ok(false);
    };
    if !observed.unread || observed.unread_since_ms != unread_since_ms {
        return Ok(false);
    }
    let cleared = {
        let mut snapshot = runtime.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        crate::state::acknowledge_session_version(&mut snapshot, &identity, unread_since_ms)
    };
    emit_state(&app, &runtime);
    Ok(cleared)
}

/// Hide one notification card. Purely visual: the event stays in the coordinator
/// log so the same round is not re-reported, and it cannot clear `unread` or
/// resolve a wait, which only `mark_session_read` and the source may do.
#[tauri::command]
pub fn dismiss_session_notification(
    event_id: String,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<bool, String> {
    let dismissed = state.dismiss_session_notification(&event_id);
    if dismissed {
        emit_state(&app, state.inner());
    }
    Ok(dismissed)
}

/// Remember which list view (attention queue vs all sessions) the panel was
/// left in, so the next launch reopens where the user stopped.
#[tauri::command]
pub fn set_session_panel_view(
    show_all: bool,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) {
    if crate::session_navigation::active(&state) {
        crate::activation::reserve_focus_request(&state);
        crate::session_navigation::navigate(
            &app,
            &state,
            if show_all {
                crate::session_tabs::Tab::Others
            } else {
                crate::session_tabs::Tab::Attention
            },
            0,
        );
        emit_state(&app, &state);
        return;
    }
    crate::session_window::set_show_all(show_all);
}

#[tauri::command]
pub fn list_hook_integrations(
    app: AppHandle,
) -> Result<Vec<hook_integration::HookIntegrationState>, String> {
    hook_integration::integration_states_with_dsh(resource_dir(&app).as_deref())
}

/// Install the `dsh-deck-bridge` plugin into the running DSH profile.
///
/// This shells out to DSH's own `dsh plugin` command through `npx`, which may
/// resolve and download the DSH CLI on a cold cache, so it runs on a blocking
/// worker instead of the main thread and the UI shows a busy state meanwhile.
#[tauri::command]
pub async fn install_dsh_plugin(
    app: AppHandle,
) -> Result<Vec<hook_integration::HookIntegrationState>, String> {
    apply_dsh_plugin(app, true).await
}

#[tauri::command]
pub async fn uninstall_dsh_plugin(
    app: AppHandle,
) -> Result<Vec<hook_integration::HookIntegrationState>, String> {
    apply_dsh_plugin(app, false).await
}

async fn apply_dsh_plugin(
    app: AppHandle,
    install: bool,
) -> Result<Vec<hook_integration::HookIntegrationState>, String> {
    let resource_dir = resource_dir(&app);
    let dsh_home = crate::dsh::home_dir();
    let bridge = crate::dsh_plugin::bridge_dir(resource_dir.as_deref());
    tauri::async_runtime::spawn_blocking(move || {
        crate::dsh_plugin::apply(&dsh_home, bridge, install)?;
        hook_integration::integration_states_with_dsh(resource_dir.as_deref())
    })
    .await
    .unwrap_or_else(|error| Err(format!("plugin installer task failed: {error}")))
}

/// Where a packaged app keeps bundled resources; `None` in a development build,
/// where `dsh_plugin::bridge_dir` falls back to the repository copy.
pub(crate) fn resource_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().resource_dir().ok()
}

#[tauri::command]
pub fn install_agent_hooks(
    agent_id: String,
) -> Result<Vec<hook_integration::HookIntegrationState>, String> {
    hook_integration::install(&agent_id)
}

#[tauri::command]
pub fn uninstall_agent_hooks(
    agent_id: String,
) -> Result<Vec<hook_integration::HookIntegrationState>, String> {
    hook_integration::uninstall(&agent_id)
}

#[tauri::command]
pub fn send_hardware_event(
    event: DeviceEvent,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> crate::state::AppSnapshot {
    crate::process_event(event, &app, state.inner())
}

#[tauri::command]
pub fn dsh_diagnostics(state: tauri::State<'_, Arc<AppRuntime>>) -> crate::dsh::DshDiagnostics {
    let (pushes, last_push_at_ms) = state.dsh_push_stats();
    crate::dsh::diagnostics(pushes, last_push_at_ms)
}

/// Turn an agent's monitoring on or off.
///
/// The switch is persisted and applied to the live snapshot immediately: a
/// disabled agent is retired from the session lists, jobs and deck lights, and
/// stops being detected and polled. Its A1–A5 slot binding is left alone, so
/// turning the agent back on restores the previous layout.
#[tauri::command]
pub fn set_agent_enabled(
    agent_id: String,
    enabled: bool,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::state::AppSnapshot, String> {
    validate_agent_id(state.inner(), &agent_id)?;
    if !enabled && agent_id == "companion" {
        return Err("the Companion agent is always monitored".into());
    }
    crate::agent_preferences::set_disabled(&agent_id, !enabled)?;
    {
        let mut snapshot = state
            .snapshot
            .write()
            .map_err(|_| "snapshot poisoned".to_owned())?;
        let agent = snapshot
            .agents
            .iter_mut()
            .find(|agent| agent.id == agent_id)
            .ok_or_else(|| format!("unknown Agent: {agent_id}"))?;
        agent.enabled = enabled;
        if !enabled && snapshot.active_agent.as_deref() == Some(agent_id.as_str()) {
            snapshot.active_agent = None;
            snapshot.active_surface = None;
            snapshot.sessions = Vec::new();
        }
        crate::state::prune_disabled_agents(&mut snapshot);
        snapshot.updated_at_ms = now_ms();
    }
    // Detection normally runs on a minute-long cycle; re-detecting now makes a
    // re-enabled agent appear without waiting for the next sweep.
    monitor::reconcile_processes_once(state.inner());
    emit_state(&app, state.inner());
    Ok(state.read())
}

#[tauri::command]
pub fn get_notification_audio(
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> crate::notification_audio::AudioView {
    state.notification_audio.view()
}

#[tauri::command]
pub fn set_notification_audio(
    slot: u8,
    name: String,
    wav: Option<Vec<u8>>,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::notification_audio::AudioView, String> {
    state.notification_audio.set(slot, name, wav)?;
    Ok(state.notification_audio.view())
}

#[tauri::command]
pub fn play_notification_audio(
    slot: u8,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<(), String> {
    if slot > 2 {
        return Err("unknown notification slot".into());
    }
    if !state
        .notification_audio
        .connected
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err("请先连接键盘再试听".into());
    }
    // Preview works before the clip is cached: the firmware plays its own
    // built-in SPIFFS sound for slots that have no uploaded PCM yet.
    *state.notification_audio.play.lock().unwrap() = Some(slot);
    Ok(())
}

#[tauri::command]
pub fn retry_notification_audio(state: tauri::State<'_, Arc<AppRuntime>>) {
    state
        .notification_audio
        .generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[tauri::command]
pub fn get_desktop_notifications(
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> crate::desktop_notifications::NotificationView {
    state.desktop_notifications.refresh_permission();
    state.desktop_notifications.view()
}

#[tauri::command]
pub fn set_desktop_notifications(
    enabled: bool,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<crate::desktop_notifications::NotificationView, String> {
    let view = state.desktop_notifications.set_enabled(enabled)?;
    if enabled && view.available && view.permission == "not_requested" {
        state.desktop_notifications.request_permission()?;
    }
    emit_state(&app, &state);
    Ok(state.desktop_notifications.view())
}

#[tauri::command]
pub fn request_desktop_notification_permission(
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<(), String> {
    state.desktop_notifications.request_permission()
}

#[tauri::command]
pub fn test_desktop_notification(state: tauri::State<'_, Arc<AppRuntime>>) -> Result<(), String> {
    state.desktop_notifications.test_notification()
}

#[tauri::command]
pub fn set_companion_page(page: usize, app: AppHandle, state: tauri::State<'_, Arc<AppRuntime>>) {
    let tab = state
        .navigation
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .view
        .tab;
    crate::activation::reserve_focus_request(&state);
    crate::session_navigation::navigate(&app, &state, tab, page);
    emit_state(&app, &state);
}
#[tauri::command]
pub fn open_companion_slot(
    index: usize,
    revision: u64,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) {
    crate::session_navigation::open_slot(&app, &state, index, revision);
}

pub fn update_panel_tab(tab: crate::session_tabs::Tab, app: &AppHandle, state: &Arc<AppRuntime>) {
    if crate::session_navigation::active(state) {
        crate::session_navigation::navigate(app, state, tab, 0);
    } else {
        crate::session_window::set_tab(tab);
        let _ = app.emit(
            "session_panel_preferences_changed",
            crate::session_window::load_preferences(),
        );
    }
    emit_state(app, state);
}
#[tauri::command]
pub fn set_session_panel_tab(
    tab: crate::session_tabs::Tab,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) {
    update_panel_tab(tab, &app, &state);
}
pub fn update_panel_tab_order(
    order: Vec<crate::session_tabs::Tab>,
    app: &AppHandle,
    state: &Arc<AppRuntime>,
) -> Result<(), String> {
    let order = crate::session_tabs::validate_order(&order)?;
    crate::activation::reserve_focus_request(state);
    crate::session_window::set_tab_order(order);
    if crate::session_window::load_preferences().tab_order != order {
        return Err("Unable to persist tab order".into());
    }
    crate::session_navigation::set_tab_order(app, state, order);
    emit_state(app, state);
    Ok(())
}

pub fn update_ui_theme(
    theme: crate::state::UiTheme,
    app: &AppHandle,
    state: &Arc<AppRuntime>,
) -> Result<(), String> {
    crate::state::update_ui_theme_state(state, theme);
    // Broadcast through the normal snapshot so every window — companion, hint
    // and session panel — repaints from the one selection.
    emit_state(app, state);
    Ok(())
}

#[tauri::command]
pub fn set_ui_theme(
    theme: crate::state::UiTheme,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<(), String> {
    update_ui_theme(theme, &app, &state)
}

pub fn update_ui_locale(
    locale: crate::state::UiLocale,
    app: &AppHandle,
    state: &Arc<AppRuntime>,
) -> Result<(), String> {
    crate::state::update_ui_locale_state(state, locale);
    // Menu labels are native strings, so they must be rebuilt in the new
    // language; the broadcast keeps every WebView in step with the picker.
    crate::tray::refresh_tray_menu(app, state);
    emit_state(app, state);
    Ok(())
}

#[tauri::command]
pub fn set_ui_locale(
    locale: crate::state::UiLocale,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<(), String> {
    update_ui_locale(locale, &app, &state)
}
#[tauri::command]
pub fn set_session_panel_tab_order(
    order: Vec<crate::session_tabs::Tab>,
    app: AppHandle,
    state: tauri::State<'_, Arc<AppRuntime>>,
) -> Result<(), String> {
    update_panel_tab_order(order, &app, &state)
}
