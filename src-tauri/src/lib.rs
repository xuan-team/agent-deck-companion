use std::{
    process::Command,
    sync::{atomic::Ordering, Arc},
    thread,
    time::{Duration, Instant},
};

use sysinfo::System;
use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

mod activation;
mod agent_actions;
mod agent_detection;
mod agent_preferences;
mod chatgpt_activity;
mod chatgpt_quota;
mod chatgpt_state;
mod commands;
mod desktop_notifications;
mod dsh;
mod dsh_plugin;
mod function_hint;
pub mod glyph_font;
pub mod glyph_txn;
#[cfg(target_os = "macos")]
mod hardware_hid;
mod hardware_mode;
#[cfg(target_os = "macos")]
mod herdr;
mod hook_integration;
mod hooks;
mod http_api;
mod keymap;
#[cfg(target_os = "macos")]
mod macos_input;
mod monitor;
mod notification_audio;
mod notifications;
mod permissions;
mod plugin;
mod profile;
mod session_activity;
mod session_navigation;
mod session_tabs;
mod session_window;
mod state;
#[cfg(target_os = "macos")]
mod tmux;
mod tray;
pub mod vendor_hid;

use state::{emit_state, now_ms, AgentStatus, AppRuntime, DeviceEvent};

pub fn process_event(
    event: DeviceEvent,
    app: &AppHandle,
    state: &Arc<AppRuntime>,
) -> state::AppSnapshot {
    let encoded = serde_json::to_string(&event).unwrap_or_else(|_| "invalid event".into());
    {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.device.last_event = Some(encoded);
        snapshot.updated_at_ms = now_ms();
    }

    if session_navigation::handle(&event, app, state) {
        emit_state(app, state);
        return state.read();
    }

    if event.kind == "encoder" {
        if let Some(delta) = event.delta {
            adjust_reasoning_level(delta, app, state);
        }
    } else if matches!(
        event.kind.as_str(),
        "key_down" | "agent_key" | "agent_key_primed"
    ) {
        if let Some(key) = event.key.as_deref() {
            let normalized = key.to_ascii_uppercase();
            let agent_id = if profile::AGENT_SLOTS.contains(&normalized.as_str()) {
                state
                    .config
                    .lock()
                    .expect("config poisoned")
                    .assigned_agent(&normalized)
                    .map(str::to_owned)
            } else {
                None
            };
            if let Some(agent_id) = agent_id.filter(|id| state.agent_enabled(id)) {
                let already_active = {
                    let snapshot = state.snapshot.read().unwrap_or_else(|poisoned| {
                        eprintln!("[state] snapshot read lock poisoned, recovering");
                        poisoned.into_inner()
                    });
                    snapshot.active_agent.as_deref() == Some(agent_id.as_str())
                };
                if event.kind == "agent_key_primed" && !already_active {
                    // A newer device selection superseded this queued focus job.
                } else if !already_active || event.kind == "agent_key_primed" {
                    activation::activate_agent(&agent_id, app, state);
                } else {
                    #[cfg(target_os = "macos")]
                    activation::refocus_agent(&agent_id, state);
                }
            } else {
                #[cfg(target_os = "macos")]
                if key.to_ascii_uppercase().starts_with('K') {
                    activation::execute_function_key(key, state);
                }
            }
        }
    }
    emit_state(app, state);
    state.read()
}

fn adjust_reasoning_level(delta: i32, app: &AppHandle, state: &Arc<AppRuntime>) {
    if delta == 0 {
        return;
    }
    let agent_id = {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        let next = (i32::from(snapshot.device.knob_position) + delta).clamp(0, 3) as u8;
        snapshot.device.knob_position = next;
        snapshot.device.reasoning_level = chatgpt_state::level_name(next).into();
        snapshot.device.reasoning_sync = "syncing".into();
        snapshot.updated_at_ms = now_ms();
        snapshot.active_agent.clone()
    };

    #[cfg(target_os = "macos")]
    if agent_id.as_deref() == Some("chatgpt") {
        let app = app.clone();
        let state = state.clone();
        thread::spawn(move || sync_chatgpt_reasoning(&app, &state));
    } else {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.device.reasoning_sync = "idle".into();
    }
    emit_state(app, state);
}

#[cfg(target_os = "macos")]
fn sync_chatgpt_reasoning(app: &AppHandle, state: &Arc<AppRuntime>) {
    let Ok(_guard) = state.reasoning_sync.lock() else {
        return;
    };
    let native_encoder = chatgpt_state::native_encoder_available();
    let encoder_mode = chatgpt_state::encoder_mode();
    let serial_input = {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.device.native_encoder = native_encoder;
        snapshot.device.encoder_mode = encoder_mode.clone();
        snapshot.device.mode == "hid_vendor"
    };
    let use_native_encoder = native_encoder && serial_input;

    if use_native_encoder && encoder_mode.as_deref() != Some("reasoning") {
        set_reasoning_sync_state(state, "wrong_mode");
        emit_state(app, state);
        return;
    }
    if !use_native_encoder {
        if !macos_input::has_or_request_access() {
            set_reasoning_sync_state(state, "permission_denied");
            emit_state(app, state);
            return;
        }
        let _ = Command::new("open")
            .args(["-b", "com.openai.codex"])
            .status();
        thread::sleep(Duration::from_millis(180));
    }

    for _ in 0..12 {
        let desired = state
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| {
                eprintln!("[state] snapshot read lock poisoned, recovering");
                poisoned.into_inner()
            })
            .device
            .knob_position;
        let actual = chatgpt_state::current_reasoning_level();
        {
            let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
                eprintln!("[state] snapshot write lock poisoned, recovering");
                poisoned.into_inner()
            });
            snapshot.device.reasoning_actual =
                actual.map(|level| chatgpt_state::level_name(level).into());
        }
        emit_state(app, state);

        let Some(actual) = actual else {
            set_reasoning_sync_state(state, "unavailable");
            emit_state(app, state);
            return;
        };
        if actual == desired {
            set_reasoning_sync_state(state, "confirmed");
            emit_state(app, state);
            return;
        }

        if use_native_encoder {
            thread::sleep(Duration::from_millis(160));
        } else {
            macos_input::set_chatgpt_reasoning_step(desired > actual);
            thread::sleep(Duration::from_millis(520));
        }
    }
    set_reasoning_sync_state(state, "timeout");
    emit_state(app, state);
}

#[cfg(target_os = "macos")]
fn set_reasoning_sync_state(state: &Arc<AppRuntime>, value: &str) {
    let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
        eprintln!("[state] snapshot write lock poisoned, recovering");
        poisoned.into_inner()
    });
    snapshot.device.reasoning_sync = value.into();
}

const COMPANION_WINDOW_LABEL: &str = "main";

/// Shows the companion window, recreating it from `tauri.conf.json` if it was
/// destroyed. The tray entry, the Dock icon and a second app launch all funnel
/// through here so none of them can end up inert.
fn show_companion_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(COMPANION_WINDOW_LABEL) {
        // A hidden window keeps its UI state; a minimized one has to be
        // restored before `set_focus` sticks.
        if window.is_minimized().unwrap_or(false) {
            let _ = window.unminimize();
        }
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let Some(config) = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == COMPANION_WINDOW_LABEL)
        .cloned()
    else {
        eprintln!("[companion] no window config for {COMPANION_WINDOW_LABEL}");
        return;
    };
    match WebviewWindowBuilder::from_config(app, &config).and_then(|builder| builder.build()) {
        Ok(window) => {
            let _ = window.show();
            let _ = window.set_focus();
        }
        Err(error) => eprintln!("[companion] cannot recreate window: {error}"),
    }
}

/// Closing the configuration window only hides it: the core keeps monitoring
/// agents and the tray entry reopens the same window, with its state intact.
fn hide_on_close(window: &WebviewWindow) {
    let handle = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = handle.hide();
        }
    });
}

fn setup_tray(app: &AppHandle, state: &Arc<AppRuntime>) -> tauri::Result<()> {
    let menu = tray::build_tray_menu(app, state)?;
    let menu_state = state.clone();
    let builder = TrayIconBuilder::with_id("agent-deck")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .title("DECK")
        .tooltip("Agent Deck Companion")
        .icon(function_hint::menu_bar_icon())
        .icon_as_template(true)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "show_function_hint" => state::show_function_hint(app, &menu_state, false),
            "auto_function_hint" => {
                let current = state::current_hint_preferences(&menu_state);
                state::update_hint_preferences_state(
                    &menu_state,
                    !current.auto_show,
                    current.pinned,
                );
                tray::refresh_tray_menu(app, &menu_state);
                emit_state(app, &menu_state);
            }
            "pin_function_hint" => {
                let current = state::current_hint_preferences(&menu_state);
                let pinned = !current.pinned;
                state::update_hint_preferences_state(&menu_state, current.auto_show, pinned);
                tray::refresh_tray_menu(app, &menu_state);
                if pinned {
                    state::show_function_hint(app, &menu_state, false);
                } else {
                    state::show_function_hint(app, &menu_state, true);
                }
                emit_state(app, &menu_state);
            }
            "open_companion" => show_companion_window(app),
            "toggle_session_panel" => crate::session_window::toggle(app),
            "esp32_hardware" => {
                let enabled = !menu_state.hardware_enabled.load(Ordering::Relaxed);
                hardware_mode::set_enabled(enabled);
                menu_state
                    .hardware_enabled
                    .store(enabled, Ordering::Relaxed);
                if enabled {
                    start_hardware_listeners(app, &menu_state);
                }
                tray::refresh_tray_menu(app, &menu_state);
                emit_state(app, &menu_state);
            }
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                crate::session_window::toggle(tray.app_handle());
            }
        });
    builder.build(app)?;
    Ok(())
}

/// Spawns the ESP32 F-key tap and the vendor-HID workers, at most once per
/// process. The tray toggle re-invokes this when the user turns hardware on;
/// neither listener can be revoked in-process afterwards.
fn start_hardware_listeners(app: &AppHandle, state: &Arc<AppRuntime>) {
    if state
        .hardware_listeners_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    #[cfg(target_os = "macos")]
    hardware_hid::start(app.clone(), state.clone());
    vendor_hid::start(app.clone(), state.clone());
}

pub fn run() {
    let state = Arc::new(AppRuntime::new());
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_companion_window(app);
        }))
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            permissions::get_macos_permissions,
            permissions::request_macos_permission,
            commands::get_state,
            commands::get_desktop_notifications,
            commands::set_desktop_notifications,
            commands::request_desktop_notification_permission,
            commands::test_desktop_notification,
            commands::get_notification_audio,
            commands::set_notification_audio,
            commands::play_notification_audio,
            commands::retry_notification_audio,
            commands::herdr_agent_action,
            commands::open_desktop_session,
            commands::assign_agent_slot,
            commands::set_key_binding,
            commands::reset_key_binding,
            commands::reset_agent_bindings,
            commands::list_hook_integrations,
            commands::install_agent_hooks,
            commands::uninstall_agent_hooks,
            commands::set_hint_preferences,
            commands::show_function_key_hint,
            commands::hide_function_key_hint,
            commands::get_session_panel_preferences,
            commands::show_session_panel,
            commands::set_session_panel_collapsed,
            commands::hide_session_panel,
            commands::mark_session_read,
            commands::dismiss_session_notification,
            commands::set_session_notification_overlay,
            commands::set_session_panel_view,
            commands::set_session_panel_tab,
            commands::set_session_panel_tab_order,
            commands::set_ui_theme,
            commands::set_ui_locale,
            commands::set_companion_page,
            commands::open_companion_slot,
            commands::send_hardware_event,
            commands::set_agent_enabled,
            commands::dsh_diagnostics,
            commands::install_dsh_plugin,
            commands::uninstall_dsh_plugin
        ])
        .setup(move |app| {
            let app_handle = app.handle().clone();
            if let Err(error) = hook_integration::refresh_bridge_if_present() {
                eprintln!("hook bridge refresh failed: {error}");
            }
            if let Some(window) = app_handle.get_webview_window(COMPANION_WINDOW_LABEL) {
                hide_on_close(&window);
            }
            if let Err(error) = function_hint::create_window(&app_handle) {
                eprintln!("function hint window failed to create: {error}");
            }
            // A platform window failure must degrade to "no panel", never
            // take the whole companion down with it.
            if let Err(error) = session_window::create_window(&app_handle) {
                eprintln!("session panel window failed to create: {error}");
            }
            state.desktop_notifications.set_panel_visible(
                app_handle
                    .get_webview_window(session_window::WINDOW_LABEL)
                    .is_some_and(|window| window.is_visible().unwrap_or(false)),
            );
            desktop_notifications::start(app_handle.clone(), state.clone());
            setup_tray(&app_handle, &state)?;
            if state.hardware_enabled.load(Ordering::Relaxed)
                && state::current_hint_preferences(&state).pinned
            {
                state::show_function_hint(&app_handle, &state, false);
            }
            session_activity::start(app_handle.clone(), state.clone());
            http_api::start_http_api(app_handle.clone(), state.clone());
            chatgpt_activity::start(app_handle.clone(), state.clone());
            if state.hardware_enabled.load(Ordering::Relaxed) {
                start_hardware_listeners(&app_handle, &state);
            }
            let monitor_state = state.clone();
            let monitor_app = app_handle.clone();
            thread::spawn(move || {
                let mut system = System::new();
                // Initial process reconciliation
                monitor::refresh_process_table(&mut system);
                if monitor::reconcile_processes(&monitor_state, &system) {
                    emit_state(&monitor_app, &monitor_state);
                }

                // Software mode discovers and displays only; an agent becomes
                // active when the user clicks a row or presses a deck key.
                loop {
                    monitor::refresh_process_table(&mut system);
                    if monitor::reconcile_processes(&monitor_state, &system) {
                        emit_state(&monitor_app, &monitor_state);
                    }
                    thread::sleep(Duration::from_secs(60));
                }
            });
            let dsh_state = state.clone();
            let dsh_app = app_handle.clone();
            thread::spawn(move || loop {
                if monitor::poll_dsh_status(&dsh_state) {
                    emit_state(&dsh_app, &dsh_state);
                }
                // The deck plugin pushes session changes over loopback. While
                // that channel is demonstrably alive this poll only has to
                // catch a missed push, and it returns to 5s the moment the
                // pushes stop.
                let interval = if dsh_state.dsh_push_is_fresh(Duration::from_secs(30)) {
                    Duration::from_secs(30)
                } else {
                    Duration::from_secs(5)
                };
                thread::sleep(interval);
            });
            #[cfg(target_os = "macos")]
            {
                let herdr_state = state.clone();
                let herdr_app = app_handle.clone();
                thread::spawn(move || {
                    let _ = monitor::refresh_herdr_state(&herdr_app, &herdr_state);
                    let mut retry = Duration::from_secs(1);
                    loop {
                        let connected_at = Instant::now();
                        let result = herdr::subscribe_events(|| {
                            let _ = monitor::refresh_herdr_state(&herdr_app, &herdr_state);
                        });
                        if let Err(error) = result {
                            eprintln!("Herdr event subscription disconnected: {error}");
                        }
                        if connected_at.elapsed() >= Duration::from_secs(30) {
                            retry = Duration::from_secs(1);
                        }
                        thread::sleep(retry);
                        retry = (retry * 2).min(Duration::from_secs(30));
                    }
                });

                let fallback_state = state.clone();
                let fallback_app = app_handle.clone();
                thread::spawn(move || loop {
                    thread::sleep(Duration::from_secs(30));
                    let _ = monitor::refresh_herdr_state(&fallback_app, &fallback_state);
                });

                chatgpt_quota::spawn_worker(
                    state.chatgpt_quota.clone(),
                    state.chatgpt_threads.clone(),
                    state.chatgpt_quota_wanted.clone(),
                );
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Agent Deck Companion")
        .run(|_app, _event| {
            // Clicking the Dock icon reopens the companion. Only the always-
            // on-top panel may be visible at that point, so ignore
            // `has_visible_windows` and always surface the companion.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = _event {
                show_companion_window(_app);
            }
        });
}

#[cfg(test)]
mod active_panel_tests {
    use crate::hooks::reduced_status;
    use crate::state::{AgentJob, AgentStatus, JobStatus};

    fn job(id: &str, status: JobStatus) -> AgentJob {
        AgentJob {
            id: id.into(),
            agent_id: "claude".into(),
            session_id: "session-1".into(),
            status,
            cwd: None,
            last_event: "test".into(),
            updated_at_ms: 0,
            agent_type: None,
        }
    }

    #[test]
    fn completed_subagent_does_not_hide_running_worker() {
        let jobs = [
            job("main", JobStatus::Completed),
            job("worker-1", JobStatus::Completed),
            job("worker-2", JobStatus::Running),
        ];
        assert_eq!(
            reduced_status(jobs.iter(), AgentStatus::Idle),
            AgentStatus::Working
        );
    }

    #[test]
    fn blocked_worker_has_priority_over_running_worker() {
        let jobs = [
            job("worker-1", JobStatus::Running),
            job("worker-2", JobStatus::Waiting),
        ];
        assert_eq!(
            reduced_status(jobs.iter(), AgentStatus::Working),
            AgentStatus::NeedsInput
        );
    }
}
