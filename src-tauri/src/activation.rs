use std::{
    process::Command,
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};

use serde::Serialize;
use sysinfo::System;
use tauri::AppHandle;

use crate::agent_actions::{self, AgentSurface};
use crate::agent_detection;
use crate::dsh;
use crate::keymap;
use crate::plugin;
use crate::profile;
use crate::state::{
    acknowledge_session_unread, current_hint_preferences, lookup_panel_by_identity, now_ms,
    panel_belongs_to_agent, plugin_effective_keys, show_function_hint, ActivePanel, AgentStatus,
    AppRuntime, PanelTransport,
};
use crate::tray;

#[cfg(target_os = "macos")]
pub fn terminal_host_bundle_for_pid(pid: u32) -> Option<&'static str> {
    agent_detection::terminal_host_bundle_for_pid(pid)
}

#[cfg(target_os = "macos")]
pub fn is_chatgpt_desktop_pid(pid: u32) -> bool {
    let system = System::new_all();
    system
        .process(sysinfo::Pid::from_u32(pid))
        .is_some_and(|process| {
            process.name().eq_ignore_ascii_case("ChatGPT")
                && process.exe().is_some_and(|executable| {
                    executable
                        .to_string_lossy()
                        .contains("/ChatGPT.app/Contents/MacOS/ChatGPT")
                })
        })
}

/// Bring the browser carrying DSH to the front and show its tab. The plugin
/// reports which browser holds a connected page, so a normal raise only switches
/// that browser to DSH's tab. A URL is opened when no connected page exists.
#[cfg(target_os = "macos")]
fn raise_dsh_page(system: &System, url: &str) -> bool {
    let report = dsh::deck_tab_report(system).filter(|report| report.tabs > 0);
    let preferred = report
        .as_ref()
        .and_then(|report| browser_behind(&report.browsers));
    for browser in preferred.into_iter().chain(["Google Chrome", "Safari"]) {
        if tab_focus_script_for(browser).is_some_and(|script| dsh::focus_browser_tab(&script, url))
        {
            return true;
        }
    }
    // A connected page may be inaccessible to Apple Events. Raise its browser
    // without opening another copy in that case.
    if let Some(report) = report {
        return browser_behind(&report.browsers).is_some_and(focus_browser_app);
    }
    Command::new("open")
        .arg(url)
        .status()
        .is_ok_and(|status| status.success())
}

/// The browser the deck bridge reports as holding DSH's tabs. An engine we do
/// not know has no tab vocabulary to script, so raising it would only show an
/// unrelated tab — worse than not raising it at all.
#[cfg(target_os = "macos")]
fn browser_behind(browsers: &[String]) -> Option<&'static str> {
    browsers.iter().find_map(|browser| {
        Some(match browser.as_str() {
            "chrome" => "Google Chrome",
            "safari" => "Safari",
            _ => return None,
        })
    })
}

/// The tab-raise script for the browser the bridge reports. Chrome and Safari
/// need different AppleScript, and an unknown engine gets no script at all.
#[cfg(target_os = "macos")]
fn tab_focus_script_for(app: &str) -> Option<String> {
    match app {
        "Google Chrome" => Some(dsh::chrome_tab_focus_script()),
        "Safari" => Some(dsh::safari_tab_focus_script()),
        _ => None,
    }
}

/// Fallback raise: bring a browser to the front without a URL, which is the one
/// `open` form that cannot create a tab. Used when no tab matches DSH.
#[cfg(target_os = "macos")]
fn focus_browser_app(name: &str) -> bool {
    Command::new("open")
        .args(["-a", name])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "macos")]
pub fn resolved_surface(
    agent_id: &str,
    pid: Option<u32>,
    has_herdr_target: bool,
    plugins: &plugin::PluginRegistry,
) -> AgentSurface {
    if let Some(preferred) = agent_actions::preferred_surface(agent_id) {
        preferred
    } else if agent_id == "dsh" {
        // DSH 优先检查 Web 模式，即使它运行在 Herdr pane 中
        if dsh::web_url(&System::new_all()).is_some() {
            AgentSurface::Web
        } else {
            AgentSurface::Terminal
        }
    } else if has_herdr_target {
        AgentSurface::HerdrPane
    } else if agent_id == "chatgpt" && pid.is_some_and(is_chatgpt_desktop_pid) {
        AgentSurface::Desktop
    } else if agent_id == "chatgpt" && pid.is_none() {
        AgentSurface::Desktop
    } else if let Some(surface) = plugins.preferred_surface(agent_id) {
        surface
    } else if matches!(agent_id, "qwen" | "codex" | "claude" | "herdr") {
        AgentSurface::Terminal
    } else {
        AgentSurface::Unknown
    }
}

/// The surface to label the deck with before window discovery settles. Only
/// declared defaults: the hardware reader thread must not pay for a process
/// list scan or a focus probe to draw a key row.
pub fn assumed_surface(agent_id: &str, plugins: &plugin::PluginRegistry) -> AgentSurface {
    agent_actions::preferred_surface(agent_id)
        .or_else(|| plugins.preferred_surface(agent_id))
        .unwrap_or(AgentSurface::Unknown)
}

/// Re-label the deck's bottom row for one agent's surface, and tell the deck
/// worker when that row actually changed.
pub fn set_active_surface(state: &Arc<AppRuntime>, agent_id: &str, surface: AgentSurface) {
    let config = state.config.lock().expect("config poisoned").clone();
    let surface_changed = {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        if snapshot.active_agent.as_deref() != Some(agent_id) {
            return;
        }
        let surface_changed = snapshot.active_surface != Some(surface);
        snapshot.active_surface = Some(surface);
        let mut function_keys = if state.plugins.get(agent_id).is_some() {
            plugin_effective_keys(state, &config, agent_id, surface)
        } else {
            keymap::effective_keys(&config, Some(agent_id), surface)
        };
        #[cfg(target_os = "macos")]
        if agent_id == "herdr" {
            let prefix = crate::herdr::configured_prefix();
            for key in &mut function_keys {
                key.hint = key.hint.replacen("prefix+", &format!("{prefix} → "), 1);
            }
        }
        snapshot.function_keys = function_keys;
        snapshot.action_catalog = if state.plugins.get(agent_id).is_some() {
            crate::state::plugin_action_catalog(&state.plugins, agent_id, surface)
        } else {
            keymap::action_catalog(Some(agent_id), surface)
        };
        snapshot.updated_at_ms = now_ms();
        surface_changed
    };
    if surface_changed {
        crate::vendor_hid::request_deck_push(state);
    }
}

#[cfg(target_os = "macos")]
fn spawn_agent_focus(
    agent_id: &str,
    state: &Arc<AppRuntime>,
    pid: Option<u32>,
    executable: String,
    preferred_cwd: Option<String>,
    status: AgentStatus,
) {
    let agent_id = agent_id.to_string();
    let focus_generation = state.focus_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let focus_state = state.clone();
    let herdr_target = if agent_actions::preferred_surface(&agent_id) == Some(AgentSurface::Desktop)
    {
        None
    } else {
        crate::herdr::find_target(
            &agent_id,
            &executable,
            preferred_cwd.as_deref(),
            Some(&state.plugins),
            None,
        )
    };
    let tmux_target = herdr_target
        .is_none()
        .then(|| pid.and_then(|p| crate::tmux::find_target(p)))
        .flatten();
    let surface = resolved_surface(&agent_id, pid, herdr_target.is_some(), &state.plugins);
    set_active_surface(state, &agent_id, surface);
    let terminal_pid = herdr_target
        .as_ref()
        .map(|target| target.pid)
        .or(tmux_target.as_ref().map(|target| target.client_pid))
        .or(pid);
    let terminal_bundle = terminal_pid.and_then(terminal_host_bundle_for_pid);
    let dsh_url = if agent_id == "dsh" {
        dsh::web_url(&System::new_all())
    } else {
        None
    };
    eprintln!(
        "[activate] agent={} pid={:?} executable={} surface={:?} \
         herdr_target={:?} tmux_target={:?} terminal_pid={:?} terminal_bundle={:?}",
        agent_id,
        pid,
        executable,
        surface,
        herdr_target.as_ref().map(|t| (&t.pane_id, t.pid)),
        tmux_target.as_ref().map(|t| (&t.pane_id, t.client_pid)),
        terminal_pid,
        terminal_bundle,
    );
    thread::spawn(move || {
        let _focus = focus_state
            .focus_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if focus_state.focus_generation.load(Ordering::SeqCst) != focus_generation {
            eprintln!("[activate] stale focus request for agent={agent_id}, skipping");
            return;
        }
        // DSH Web wins even when the dsh server process itself runs inside a
        // Herdr pane; resolved_surface applies the same priority.
        if let Some(url) = dsh_url {
            raise_dsh_page(&System::new_all(), &url);
        } else if let Some(target) = herdr_target {
            eprintln!(
                "[activate] → herdr focus: pane={} pid={}",
                target.pane_id, target.pid
            );
            let _ = crate::herdr::focus_target(&agent_id, &target, status);
            if focus_state.focus_generation.load(Ordering::SeqCst) != focus_generation {
                return;
            }
            // Focus the host terminal window for any herdr-backed agent, not just "herdr" itself.
            // Qwen/Codex/Claude running in herdr panes also need the terminal window raised.
            #[cfg(target_os = "macos")]
            let host_window_focused = crate::herdr::focus_host_window();
            #[cfg(not(target_os = "macos"))]
            let host_window_focused = false;
            eprintln!("[activate] herdr host_window_focused={host_window_focused}");
            if !host_window_focused {
                if let Some(bundle) = terminal_bundle {
                    eprintln!("[activate] → open -b {}", bundle);
                    let _ = Command::new("open").args(["-b", bundle]).status();
                }
            }
        } else if let Some(target) = tmux_target {
            eprintln!(
                "[activate] → tmux focus: pane={} client_pid={}",
                target.pane_id, target.client_pid
            );
            let _ = crate::tmux::focus_target(&target);
            #[cfg(target_os = "macos")]
            let host_window_focused = crate::tmux::focus_host_window(&target);
            if focus_state.focus_generation.load(Ordering::SeqCst) != focus_generation {
                return;
            }
            if let Some(bundle) = terminal_bundle {
                if !host_window_focused {
                    eprintln!("[activate] → open -b {}", bundle);
                    let _ = Command::new("open").args(["-b", bundle]).status();
                }
            }
        } else if agent_id == "chatgpt" {
            eprintln!("[activate] → ChatGPT desktop: open -b com.openai.codex");
            let _ = Command::new("open")
                .args(["-b", "com.openai.codex"])
                .status();
        } else if let Some(bundle) = terminal_bundle {
            if focus_state.focus_generation.load(Ordering::SeqCst) != focus_generation {
                return;
            }
            let plugin_hints = focus_state
                .plugins
                .get(&agent_id)
                .map(|plugin| plugin.terminal_title_hints.as_slice())
                .unwrap_or(&[]);
            let host_window_focused =
                agent_detection::focus_terminal_window_with_hints(&agent_id, bundle, plugin_hints);
            eprintln!("[activate] terminal host_window_focused={host_window_focused}");
            if !host_window_focused {
                eprintln!("[activate] → terminal bundle: open -b {}", bundle);
                let _ = Command::new("open").args(["-b", bundle]).status();
            }
        } else if let Some(pid) = terminal_pid {
            eprintln!("[activate] → terminal pid fallback: osascript pid={}", pid);
            let script = format!(
                "tell application \"System Events\" to set frontmost of first process whose unix id is {pid} to true"
            );
            let _ = Command::new("osascript").args(["-e", &script]).output();
        } else {
            eprintln!("[activate] → NO target found, skipping focus");
        }
    });
}

/// Re-run window focus for an agent that is already active, without mutating
/// activation state. Pressing the same deck key should raise the agent's
/// window even when the companion already considers it active.
#[cfg(target_os = "macos")]
pub fn refocus_agent(agent_id: &str, state: &Arc<AppRuntime>) {
    let (pid, executable, preferred_cwd, status) = {
        let snapshot = state.snapshot.read().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        });
        let agent_info = snapshot.agents.iter().find(|agent| agent.id == agent_id);
        let pid = agent_info.and_then(|agent| agent.pid);
        let executable = agent_info
            .map(|agent| agent.executable.clone())
            .unwrap_or_default();
        let status = agent_info
            .map(|agent| agent.status)
            .unwrap_or(AgentStatus::Offline);
        let preferred_cwd = snapshot
            .jobs
            .iter()
            .filter(|job| agent_id == job.agent_id)
            .max_by_key(|job| job.updated_at_ms)
            .and_then(|job| job.cwd.clone());
        (pid, executable, preferred_cwd, status)
    };
    spawn_agent_focus(agent_id, state, pid, executable, preferred_cwd, status);
}

/// Publish the hardware-selected agent before any window discovery or focus
/// work. The queued event still goes through `process_event` for side effects.
pub fn prime_agent_selection(agent_id: &str, state: &Arc<AppRuntime>) {
    if agent_id == "companion" {
        return;
    }
    let _ = select_agent_state(agent_id, state);
    let assumed = assumed_surface(agent_id, &state.plugins);
    set_active_surface(state, agent_id, assumed);
}

fn select_agent_state(
    agent_id: &str,
    state: &Arc<AppRuntime>,
) -> (Option<u32>, String, Option<String>, AgentStatus) {
    {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.active_agent = Some(agent_id.to_string());
        // Selecting an agent is not evidence of work in progress; only hooks,
        // discovered panes and the monitor may report Working.
        for agent in &mut snapshot.agents {
            if agent.id != agent_id && agent.status == AgentStatus::Working {
                agent.status = if agent.running {
                    AgentStatus::Idle
                } else {
                    AgentStatus::Offline
                };
            }
            agent.active = agent.id == agent_id;
        }
        snapshot.updated_at_ms = now_ms();
        let agent_info = snapshot.agents.iter().find(|agent| agent.id == agent_id);
        let pid = agent_info.and_then(|agent| agent.pid);
        let executable = agent_info
            .map(|agent| agent.executable.clone())
            .unwrap_or_default();
        let status = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .map(|agent| agent.status)
            .unwrap_or(AgentStatus::Offline);
        let preferred_cwd = snapshot
            .jobs
            .iter()
            .filter(|job| agent_id == job.agent_id)
            .max_by_key(|job| job.updated_at_ms)
            .and_then(|job| job.cwd.clone());
        (pid, executable, preferred_cwd, status)
    }
}

pub fn activate_agent(agent_id: &str, app: &AppHandle, state: &Arc<AppRuntime>) {
    #[cfg(target_os = "macos")]
    if agent_id != "chatgpt" {
        crate::macos_input::stop_chatgpt_voice_hold();
    }
    let (pid, executable, preferred_cwd, status) = select_agent_state(agent_id, state);

    #[cfg(target_os = "macos")]
    spawn_agent_focus(agent_id, state, pid, executable, preferred_cwd, status);

    tray::refresh_tray_menu(app, state);
    if current_hint_preferences(state).auto_show {
        show_function_hint(app, state, true);
    }
}

#[cfg(target_os = "macos")]
fn activate_panel_while_current(panel: &ActivePanel, current: &dyn Fn() -> bool) -> FocusReport {
    if !current() {
        return FocusReport::Failed;
    }
    match &panel.transport {
        PanelTransport::Herdr {
            socket_path,
            pane_id,
        } => {
            // Raising a Herdr client can publish that client's previous pane as focused.
            // Let that activation settle, then make pane focus the final operation.
            let host_focused = crate::herdr::focus_host_window();
            if host_focused {
                thread::sleep(Duration::from_millis(180));
            }
            if !current() {
                return FocusReport::Failed;
            }
            let mut pane_focused = crate::herdr::focus_panel(
                socket_path.as_deref(),
                pane_id,
                &panel.agent_id,
                panel.status,
            );
            thread::sleep(Duration::from_millis(80));
            let mut observed_pane = crate::herdr::focused_pane_id(socket_path.as_deref());
            if observed_pane.as_deref() != Some(pane_id) {
                eprintln!(
                    "[focus_session] Herdr focus not settled: requested={} observed={:?}; retrying",
                    pane_id, observed_pane
                );
                thread::sleep(Duration::from_millis(100));
                if !current() {
                    return FocusReport::Failed;
                }
                pane_focused |= crate::herdr::focus_panel(
                    socket_path.as_deref(),
                    pane_id,
                    &panel.agent_id,
                    panel.status,
                );
                thread::sleep(Duration::from_millis(80));
                observed_pane = crate::herdr::focused_pane_id(socket_path.as_deref());
            }
            let pane_observed = observed_pane.as_deref() == Some(pane_id);
            eprintln!(
                "[focus_session] Herdr result: requested={} observed={:?} host_focused={} request_succeeded={} matched={}",
                pane_id, observed_pane, host_focused, pane_focused, pane_observed
            );
            if !pane_focused || !pane_observed {
                return FocusReport::Failed;
            }
            if host_focused {
                FocusReport::Raised
            } else {
                FocusReport::Switched
            }
        }
        PanelTransport::Tmux {
            socket_path,
            pane_id,
            pid,
        } => {
            let pane_focused = crate::tmux::focus_pane(socket_path, pane_id);
            if !current() {
                return FocusReport::Failed;
            }
            let host_focused = pid
                .and_then(|pid| terminal_host_bundle_for_pid(pid))
                .is_some_and(|bundle| {
                    agent_detection::focus_terminal_window_with_hints(
                        &panel.agent_id,
                        bundle,
                        std::slice::from_ref(&panel.label),
                    ) || Command::new("open")
                        .args(["-b", bundle])
                        .status()
                        .is_ok_and(|s| s.success())
                });
            if !pane_focused {
                return FocusReport::Failed;
            }
            if host_focused {
                FocusReport::Raised
            } else {
                FocusReport::Switched
            }
        }
        PanelTransport::Terminal { pid } => {
            // Raising the host window is the only observable effect here, so it
            // stands for the switch itself.
            terminal_host_bundle_for_pid(*pid)
                .is_some_and(|bundle| {
                    agent_detection::focus_terminal_window_with_hints(
                        &panel.agent_id,
                        bundle,
                        std::slice::from_ref(&panel.label),
                    ) || Command::new("open")
                        .args(["-b", bundle])
                        .status()
                        .is_ok_and(|s| s.success())
                })
                .into()
        }
        PanelTransport::DshWeb { session_id } => {
            let system = sysinfo::System::new_all();
            eprintln!(
                "[focus_session] DSH web: session_id={} title=\"{}\"",
                session_id, panel.label
            );
            // Session navigation belongs to the page bridge and needs no OS
            // input or browser automation permission. Raising its tab is best
            // effort after the page confirms the selection.
            let selected = crate::dsh::request_session_selection(&system, session_id);
            if selected {
                if !current() {
                    return FocusReport::Failed;
                }
                if let Some(url) = dsh::web_url(&system) {
                    let _ = raise_dsh_page(&system, &url);
                }
            }
            selected.into()
        }
        PanelTransport::CodexThread { thread_id } => {
            let Some(url) = codex_thread_url(thread_id) else {
                eprintln!("[focus_session] rejected ChatGPT thread id {thread_id:?}");
                return FocusReport::Failed;
            };
            eprintln!("[focus_session] → ChatGPT deep link {url}");
            Command::new("open")
                .args(["-b", "com.openai.codex", url.as_str()])
                .status()
                .is_ok_and(|status| status.success())
                .into()
        }
        PanelTransport::Unknown => FocusReport::Failed,
    }
}

#[cfg(target_os = "macos")]
impl From<bool> for FocusReport {
    /// A transport with a single observable effect cannot separate the session
    /// switch from raising its window, so success reports both.
    fn from(succeeded: bool) -> Self {
        if succeeded {
            FocusReport::Raised
        } else {
            FocusReport::Failed
        }
    }
}

/// The desktop's own thread list is external input by the time it reaches a
/// process launch, so only a canonical UUID is allowed into the URL.
#[cfg(target_os = "macos")]
fn codex_thread_url(thread_id: &str) -> Option<String> {
    uuid::Uuid::parse_str(thread_id)
        .ok()
        .map(|id| format!("codex://threads/{id}"))
}

#[cfg(target_os = "macos")]
pub fn focus_session(session_index: usize, state: &Arc<AppRuntime>) {
    let panel = {
        let snapshot = state.read();
        let Some(agent_id) = snapshot.active_agent.as_deref() else {
            eprintln!("[focus_session] no active agent");
            return;
        };
        let mut panels = snapshot
            .active_panels
            .iter()
            .filter(|panel| {
                panel_belongs_to_agent(panel, agent_id)
                    && (panel.status != AgentStatus::Offline || panel.unread)
            })
            .collect::<Vec<_>>();
        let ordered =
            crate::session_navigation::display_order(&state.desktop_rows(&snapshot), true);
        panels.retain(|panel| {
            ordered
                .iter()
                .any(|row| row.identity == crate::state::panel_focus_key(panel))
        });
        panels.sort_by_key(|panel| {
            ordered
                .iter()
                .position(|row| row.identity == crate::state::panel_focus_key(panel))
                .unwrap_or(usize::MAX)
        });
        panels
            .get(session_index)
            .map(|panel| ((*panel).clone(), agent_id.to_owned()))
    };

    let Some((panel, owner)) = panel else {
        eprintln!("[focus_session] no active panel for K{}", session_index + 1);
        return;
    };

    let Some(identity) = crate::state::panel_focus_key(&panel) else {
        return;
    };
    let generation = reserve_focus_request(state);
    let focus_state = state.clone();
    thread::spawn(move || {
        let _ = open_agent_session_request(&focus_state, &identity, generation, &owner);
        crate::vendor_hid::request_deck_push(&focus_state);
    });
}

/// The result of opening one floating-panel row, so the UI can tell the user
/// why a click did not light up the target window.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenOutcome {
    /// The open succeeded; only news observed before this raise is acknowledged.
    Opened,
    /// The session itself came up, but its host window could not be raised.
    /// The open still counts, because the transport confirmed the switch.
    SwitchedNotRaised,
    /// No live panel carries this identity anymore; the row is stale.
    TargetGone,
    /// A newer open took over before this one reached the target.
    Superseded,
    /// The raise was attempted but the target did not confirm; unread is kept.
    FocusFailed,
}

/// What one transport actually achieved, so a confirmed session switch is not
/// reported as a failure just because the OS refused to raise its window.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusReport {
    /// The target switched and its window is in front.
    Raised,
    /// The target switched, but the host window could not be raised.
    Switched,
    /// The target never confirmed the switch.
    Failed,
}

#[cfg(target_os = "macos")]
impl FocusReport {
    /// Whether the session itself landed, which is what acknowledges its news.
    fn switched(self) -> bool {
        matches!(self, FocusReport::Raised | FocusReport::Switched)
    }
}

#[cfg(target_os = "macos")]
impl From<FocusReport> for OpenOutcome {
    fn from(report: FocusReport) -> Self {
        match report {
            FocusReport::Raised => OpenOutcome::Opened,
            FocusReport::Switched => OpenOutcome::SwitchedNotRaised,
            FocusReport::Failed => OpenOutcome::FocusFailed,
        }
    }
}

/// Open exactly the session a row points at, resolved by its stable identity
/// instead of a row position or the selected agent. Runs on a blocking thread
/// because each transport settles focus with short sleeps.
pub fn reserve_focus_request(state: &Arc<AppRuntime>) -> u64 {
    state.focus_generation.fetch_add(1, Ordering::SeqCst) + 1
}

#[cfg(target_os = "macos")]
pub fn open_session_request(
    state: &Arc<AppRuntime>,
    identity: &str,
    generation: u64,
) -> OpenOutcome {
    open_session_with(state, identity, generation, activate_panel_while_current)
}

#[cfg(target_os = "macos")]
fn open_session_with(
    state: &Arc<AppRuntime>,
    identity: &str,
    generation: u64,
    activate: impl FnOnce(&ActivePanel, &dyn Fn() -> bool) -> FocusReport,
) -> OpenOutcome {
    open_session_with_expected(state, identity, generation, None, activate)
}

#[cfg(target_os = "macos")]
pub fn open_notification_request(
    state: &Arc<AppRuntime>,
    identity: &str,
    generation: u64,
    expected: (crate::state::AttentionClass, u128),
) -> OpenOutcome {
    open_session_with_expected(
        state,
        identity,
        generation,
        Some(expected),
        activate_panel_while_current,
    )
}

#[cfg(target_os = "macos")]
fn open_session_with_expected(
    state: &Arc<AppRuntime>,
    identity: &str,
    generation: u64,
    expected: Option<(crate::state::AttentionClass, u128)>,
    activate: impl FnOnce(&ActivePanel, &dyn Fn() -> bool) -> FocusReport,
) -> OpenOutcome {
    open_session_scoped(state, identity, generation, expected, None, activate)
}

#[cfg(target_os = "macos")]
fn open_agent_session_request(
    state: &Arc<AppRuntime>,
    identity: &str,
    generation: u64,
    owner: &str,
) -> OpenOutcome {
    open_session_scoped(
        state,
        identity,
        generation,
        None,
        Some(owner),
        activate_panel_while_current,
    )
}

#[cfg(target_os = "macos")]
fn open_session_scoped(
    state: &Arc<AppRuntime>,
    identity: &str,
    generation: u64,
    expected: Option<(crate::state::AttentionClass, u128)>,
    owner: Option<&str>,
    activate: impl FnOnce(&ActivePanel, &dyn Fn() -> bool) -> FocusReport,
) -> OpenOutcome {
    thread::sleep(Duration::from_millis(100));
    let _focus = state.focus_lock.lock().unwrap_or_else(|p| p.into_inner());
    let current = || state.focus_generation.load(Ordering::SeqCst) == generation;
    if !current() {
        return OpenOutcome::Superseded;
    }
    let snapshot = state.snapshot.read().unwrap_or_else(|p| p.into_inner());
    if owner.is_some_and(|id| snapshot.active_agent.as_deref() != Some(id)) {
        return OpenOutcome::Superseded;
    }
    if expected.is_some_and(|(kind, version)| {
        !crate::notifications::matches_event(
            &state.desktop_rows(&snapshot),
            identity,
            kind,
            version,
        )
    }) {
        return OpenOutcome::Superseded;
    }
    let Some(panel) = lookup_panel_by_identity(&snapshot, identity) else {
        return OpenOutcome::TargetGone;
    };
    if owner.is_some_and(|id| !panel_belongs_to_agent(&panel, id)) {
        return OpenOutcome::TargetGone;
    }
    drop(snapshot);
    let report = activate(&panel, &current);
    if !current() {
        return OpenOutcome::Superseded;
    }
    if !report.switched() {
        return OpenOutcome::FocusFailed;
    }
    let mut snapshot = state.snapshot.write().unwrap_or_else(|p| p.into_inner());
    if !current() {
        return OpenOutcome::Superseded;
    }
    if owner.is_some_and(|id| snapshot.active_agent.as_deref() != Some(id)) {
        return OpenOutcome::Superseded;
    }
    if expected.is_some_and(|(kind, version)| {
        !crate::notifications::matches_event(
            &state.desktop_rows(&snapshot),
            identity,
            kind,
            version,
        )
    }) {
        return OpenOutcome::Superseded;
    }
    let Some(live) = lookup_panel_by_identity(&snapshot, identity) else {
        return OpenOutcome::TargetGone;
    };
    if live.session_id != panel.session_id {
        return OpenOutcome::TargetGone;
    }
    if let Ok(mut focused_panels) = state.focused_panels.lock() {
        focused_panels.insert(panel.agent_id.clone(), identity.to_owned());
    }
    acknowledge_session_unread(&mut snapshot, identity, &panel);
    if report == FocusReport::Raised && owner.is_none() {
        let (id, surface) = session_control_target(&panel, &state.plugins);
        snapshot.active_agent = Some(id.clone());
        snapshot.active_surface = Some(surface);
        for agent in &mut snapshot.agents {
            agent.active = agent.id == id;
        }
        if let Ok(mut focused) = state.focused_panels.lock() {
            focused.insert(id, identity.into());
        }
    }
    snapshot.updated_at_ms = now_ms();
    drop(snapshot);
    crate::vendor_hid::request_deck_push(state);
    report.into()
}

#[cfg(target_os = "macos")]
fn session_control_target(
    panel: &ActivePanel,
    plugins: &plugin::PluginRegistry,
) -> (String, AgentSurface) {
    match &panel.transport {
        PanelTransport::Herdr { .. } => ("herdr".into(), AgentSurface::HerdrPane),
        PanelTransport::CodexThread { .. } => ("chatgpt".into(), AgentSurface::Desktop),
        PanelTransport::DshWeb { .. } => ("dsh".into(), AgentSurface::Web),
        _ => (
            panel.agent_id.clone(),
            assumed_surface(&panel.agent_id, plugins),
        ),
    }
}

#[cfg(target_os = "macos")]
pub fn execute_function_key(key: &str, state: &Arc<AppRuntime>) {
    let key_upper = key.to_ascii_uppercase();

    if key_upper.starts_with('K') {
        if let Some(digit) = key_upper
            .strip_prefix('K')
            .and_then(|s| s.parse::<usize>().ok())
        {
            if digit >= 1 && digit <= 3 {
                focus_session(digit - 1, state);
                return;
            }
        }
    }

    let (agent_id, stored_surface, pid, executable, preferred_cwd, status) = {
        let snapshot = state.snapshot.read().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        });
        let Some(agent_id) = snapshot.active_agent.clone() else {
            return;
        };
        let agent_info = snapshot.agents.iter().find(|agent| agent.id == agent_id);
        let pid = agent_info.and_then(|agent| agent.pid);
        let executable = agent_info
            .map(|agent| agent.executable.clone())
            .unwrap_or_default();
        let status = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .map(|agent| agent.status)
            .unwrap_or(AgentStatus::Offline);
        let preferred_cwd = snapshot
            .jobs
            .iter()
            .filter(|job| job.agent_id == agent_id)
            .max_by_key(|job| job.updated_at_ms)
            .and_then(|job| job.cwd.clone());
        (
            agent_id,
            snapshot.active_surface.unwrap_or(AgentSurface::Unknown),
            pid,
            executable,
            preferred_cwd,
            status,
        )
    };

    let key = key.to_ascii_uppercase();
    let state = state.clone();
    thread::spawn(move || {
        if agent_id == "chatgpt" && key != "K10" {
            crate::macos_input::stop_chatgpt_voice_hold();
        }
        let herdr_target = {
            if agent_actions::preferred_surface(&agent_id) == Some(AgentSurface::Desktop) {
                None
            } else {
                let focused = crate::herdr::focused_pane_id(None);
                crate::herdr::find_target(
                    &agent_id,
                    &executable,
                    preferred_cwd.as_deref(),
                    Some(&state.plugins),
                    focused.as_deref(),
                )
            }
        };
        let tmux_target = herdr_target
            .is_none()
            .then(|| pid.and_then(|p| crate::tmux::find_target(p)))
            .flatten();
        let surface = resolved_surface(&agent_id, pid, herdr_target.is_some(), &state.plugins);
        // A DSH web server may physically live in a Herdr/tmux pane, but its
        // function keys belong to the browser. Sending a plan to the pane TTY
        // types "/compact" into the server terminal instead of the page.
        let (herdr_target, tmux_target) = if agent_id == "dsh" && surface == AgentSurface::Web {
            (None, None)
        } else {
            (herdr_target, tmux_target)
        };
        eprintln!(
            "[fn_key] key={} agent={} pid={:?} surface={:?} herdr_target={:?} tmux_target={:?}",
            key,
            agent_id,
            pid,
            surface,
            herdr_target.as_ref().map(|t| (&t.pane_id, t.pid)),
            tmux_target.as_ref().map(|t| (&t.pane_id, t.client_pid)),
        );
        if surface != stored_surface {
            set_active_surface(&state, &agent_id, surface);
        }
        let execution = {
            let config = state.config.lock().expect("config poisoned");
            let configured =
                config.configured_action(&agent_id, keymap::surface_key(surface), &key);
            if let Some(profile::ConfiguredAction::AgentAction { action_id }) = configured {
                state
                    .plugins
                    .plan_by_action(&agent_id, surface, action_id)
                    .map(keymap::EffectiveExecution::Agent)
                    .or_else(|| keymap::effective_key(&config, &agent_id, surface, &key).1)
            } else if configured.is_some() {
                keymap::effective_key(&config, &agent_id, surface, &key).1
            } else {
                state
                    .plugins
                    .plan(&agent_id, surface, &key)
                    .map(|(_, plan)| keymap::EffectiveExecution::Agent(plan))
                    .or_else(|| keymap::effective_key(&config, &agent_id, surface, &key).1)
            }
        };
        let Some(execution) = execution else {
            eprintln!("[fn_key] no execution for key={}", key);
            return;
        };
        eprintln!("[fn_key] execution={:?}", execution);

        if let keymap::EffectiveExecution::Agent(agent_actions::ActionPlan::ActivePanel { slot }) =
            execution
        {
            let panel = state.snapshot.read().ok().and_then(|snapshot| {
                snapshot
                    .active_panels
                    .iter()
                    .find(|panel| {
                        panel.agent_id == agent_id
                            && panel.slot == slot
                            && panel.status != AgentStatus::Offline
                    })
                    .cloned()
            });
            if let Some(identity) = panel.as_ref().and_then(crate::state::panel_focus_key) {
                let generation = reserve_focus_request(&state);
                let _ = open_agent_session_request(&state, &identity, generation, &agent_id);
                crate::vendor_hid::request_deck_push(&state);
            }
            return;
        }

        if let Some(target) = herdr_target.as_ref() {
            if let keymap::EffectiveExecution::Agent(plan) = execution {
                if crate::herdr::send_plan(target, plan) {
                    let panel = state.snapshot.read().ok().and_then(|snapshot| {
                        snapshot.active_panels.iter().find(|panel| {
                            panel.status != AgentStatus::Offline && panel_belongs_to_agent(panel,&agent_id)
                                && matches!(&panel.transport, PanelTransport::Herdr {socket_path,pane_id} if pane_id==&target.pane_id && crate::herdr::socket_paths_match(socket_path.as_deref(),target.socket_path.as_deref()))
                        }).cloned()
                    });
                    if let Some(panel) = panel {
                        state.session_activity.keyboard(&panel);
                    }
                    eprintln!(
                        "[fn_key] herdr direct send succeeded: pane={}",
                        target.pane_id
                    );
                    return;
                }
                eprintln!("[fn_key] herdr direct send unavailable; falling back to CGEvent");
            }
        }

        if let Some(target) = tmux_target.as_ref() {
            if let keymap::EffectiveExecution::Agent(plan) = execution {
                if crate::tmux::send_plan(target, plan) {
                    let panel = state.snapshot.read().ok().and_then(|snapshot| {
                        snapshot.active_panels.iter().find(|panel| {
                            panel.status != AgentStatus::Offline && panel_belongs_to_agent(panel,&agent_id)
                                && matches!(&panel.transport, PanelTransport::Tmux {socket_path,pane_id,..} if pane_id==&target.pane_id && socket_path==&target.socket_path)
                        }).cloned()
                    });
                    if let Some(panel) = panel {
                        state.session_activity.keyboard(&panel);
                    }
                    eprintln!(
                        "[fn_key] tmux direct send succeeded: pane={}",
                        target.pane_id
                    );
                    return;
                }
                eprintln!("[fn_key] tmux direct send unavailable; falling back to CGEvent");
            }
        }

        if !crate::macos_input::has_or_request_access() {
            eprintln!("[fn_key] no accessibility access");
            return;
        }

        let terminal_pid = herdr_target
            .as_ref()
            .map(|target| target.pid)
            .or(tmux_target.as_ref().map(|target| target.client_pid))
            .or(pid);
        let terminal_bundle = terminal_pid.and_then(terminal_host_bundle_for_pid);
        let dsh_url = if agent_id == "dsh" {
            dsh::web_url(&System::new_all())
        } else {
            None
        };
        let focused = if let Some(target) = herdr_target {
            let pane_focused = crate::herdr::focus_target(&agent_id, &target, status);
            let mut host_focused = false;
            #[cfg(target_os = "macos")]
            if agent_id == "herdr" {
                host_focused = crate::herdr::focus_host_window();
            }
            if !host_focused {
                host_focused = terminal_bundle.is_some_and(|bundle| {
                    Command::new("open")
                        .args(["-b", bundle])
                        .status()
                        .is_ok_and(|status| status.success())
                });
            }
            eprintln!(
                "[fn_key] focused: pane_focused={} host_focused={} terminal_bundle={:?}",
                pane_focused, host_focused, terminal_bundle,
            );
            pane_focused && host_focused
        } else if let Some(target) = tmux_target {
            let pane_focused = crate::tmux::focus_target(&target);
            #[cfg(target_os = "macos")]
            let host_window_focused = crate::tmux::focus_host_window(&target);
            let host_focused = host_window_focused
                || terminal_bundle.is_some_and(|bundle| {
                    Command::new("open")
                        .args(["-b", bundle])
                        .status()
                        .is_ok_and(|status| status.success())
                });
            pane_focused && host_focused
        } else if surface == AgentSurface::Desktop && agent_id == "chatgpt" {
            Command::new("open")
                .args(["-b", "com.openai.codex"])
                .status()
                .is_ok_and(|status| status.success())
        } else if let Some(url) = dsh_url {
            raise_dsh_page(&System::new_all(), &url)
        } else if let Some(bundle) = terminal_bundle {
            let plugin_hints = state
                .plugins
                .get(&agent_id)
                .map(|plugin| plugin.terminal_title_hints.as_slice())
                .unwrap_or(&[]);
            let host_window_focused =
                agent_detection::focus_terminal_window_with_hints(&agent_id, bundle, plugin_hints);
            eprintln!("[fn_key] terminal host_window_focused={host_window_focused}");
            host_window_focused
                || Command::new("open")
                    .args(["-b", bundle])
                    .status()
                    .is_ok_and(|status| status.success())
        } else {
            false
        };

        if !focused {
            eprintln!("[fn_key] focused=false, skipping key send");
            return;
        }
        if agent_id == "dsh" && surface == AgentSurface::Web {
            let _ = dsh::focus_chat_composer();
        } else {
            thread::sleep(Duration::from_millis(180));
        }
        let herdr_prefix = (agent_id == "herdr" || surface == AgentSurface::HerdrPane)
            .then(crate::herdr::configured_prefix);
        eprintln!(
            "[fn_key] herdr_prefix={:?} surface={:?}",
            herdr_prefix, surface,
        );
        match execution {
            keymap::EffectiveExecution::Agent(plan) => {
                if matches!(plan, agent_actions::ActionPlan::ChatgptFork) {
                    let completed = crate::macos_input::chatgpt_fork();
                    eprintln!("[fn_key] ChatGPT fork completed={completed}");
                } else if matches!(plan, agent_actions::ActionPlan::ChatgptVoiceHold) {
                    let recording = crate::macos_input::toggle_chatgpt_voice_hold();
                    eprintln!("[fn_key] ChatGPT voice recording={recording}");
                } else if matches!(plan, agent_actions::ActionPlan::ChatgptApprove) {
                    eprintln!("[fn_key] ChatGPT approve requires native HID (ACT07) - not yet implemented");
                } else if matches!(plan, agent_actions::ActionPlan::ChatgptReject) {
                    eprintln!(
                        "[fn_key] ChatGPT reject requires native HID (ACT08) - not yet implemented"
                    );
                } else if matches!(plan, agent_actions::ActionPlan::ChatgptFast) {
                    eprintln!("[fn_key] ChatGPT FAST (ACT06) is BLE-only — no USB equivalent");
                } else {
                    eprintln!("[fn_key] → post plan={:?}", plan);
                    crate::macos_input::post(plan, herdr_prefix.as_deref());
                }
            }
            keymap::EffectiveExecution::System(keymap::SystemAction::SessionPanelToggle) => {
                // AppHandle-owned system action is consumed by process_event.
            }
            keymap::EffectiveExecution::System(keymap::SystemAction::VoiceInputToggle) => {
                if agent_id == "chatgpt" && surface == AgentSurface::Desktop {
                    crate::macos_input::start_chatgpt_dictation();
                } else {
                    crate::macos_input::toggle_dictation();
                }
            }
        }
    });
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{codex_thread_url, select_agent_state};
    use crate::state::{AgentStatus, AppRuntime};
    use crate::{agent_actions::AgentSurface, state::PanelTransport};
    use std::sync::Arc;

    fn runtime_with_running_idle(agent_id: &str) -> Arc<AppRuntime> {
        let runtime = Arc::new(AppRuntime::new());
        let mut snapshot = runtime
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let agent = snapshot
            .agents
            .iter_mut()
            .find(|agent| agent.id == agent_id)
            .expect("agent present");
        agent.running = true;
        agent.status = AgentStatus::Idle;
        drop(snapshot);
        runtime
    }

    fn status_of(runtime: &AppRuntime, agent_id: &str) -> AgentStatus {
        let snapshot = runtime
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshot
            .agents
            .iter()
            .find(|a| a.id == agent_id)
            .map(|a| a.status)
            .expect("agent present")
    }

    #[test]
    fn selecting_an_agent_never_claims_work_in_progress() {
        for agent_id in ["chatgpt", "codex", "qwen", "herdr"] {
            let runtime = runtime_with_running_idle(agent_id);

            select_agent_state(agent_id, &runtime);

            assert_eq!(
                status_of(&runtime, agent_id),
                AgentStatus::Idle,
                "{agent_id} was stamped working just because it was selected"
            );
        }
    }

    #[test]
    fn selecting_an_agent_does_not_acknowledge_its_completion() {
        let runtime = runtime_with_running_idle("qwen");
        {
            let mut snapshot = runtime.snapshot.write().expect("snapshot writable");
            snapshot
                .agents
                .iter_mut()
                .find(|agent| agent.id == "qwen")
                .expect("qwen present")
                .unread = true;
        }
        select_agent_state("qwen", &runtime);
        assert!(
            runtime
                .snapshot
                .read()
                .expect("snapshot readable")
                .agents
                .iter()
                .find(|agent| agent.id == "qwen")
                .expect("qwen present")
                .unread
        );
    }

    #[test]
    fn open_outcomes_use_the_public_snake_case_codes() {
        for (outcome, code) in [
            (super::OpenOutcome::Opened, "\"opened\""),
            (
                super::OpenOutcome::SwitchedNotRaised,
                "\"switched_not_raised\"",
            ),
            (super::OpenOutcome::TargetGone, "\"target_gone\""),
            (super::OpenOutcome::Superseded, "\"superseded\""),
            (super::OpenOutcome::FocusFailed, "\"focus_failed\""),
        ] {
            assert_eq!(serde_json::to_string(&outcome).unwrap(), code);
        }
    }

    #[test]
    fn a_confirmed_switch_without_a_window_raise_is_not_reported_as_failure() {
        use super::{open_session_with, reserve_focus_request, FocusReport, OpenOutcome};
        for (report, outcome) in [
            (FocusReport::Raised, OpenOutcome::Opened),
            (FocusReport::Switched, OpenOutcome::SwitchedNotRaised),
            (FocusReport::Failed, OpenOutcome::FocusFailed),
        ] {
            let state = open_runtime();
            let generation = reserve_focus_request(&state);
            assert_eq!(
                open_session_with(&state, "thread:first", generation, |_, _| report),
                outcome
            );
            assert_eq!(
                state.snapshot.read().unwrap().active_agent.as_deref(),
                if report == FocusReport::Raised {
                    Some("chatgpt")
                } else {
                    None
                }
            );
            // Only a session that never landed keeps its news unread.
            assert_eq!(
                state.snapshot.read().unwrap().active_panels[0].unread,
                report == FocusReport::Failed
            );
        }
    }

    #[test]
    fn control_target_follows_the_selected_transport_not_other_running_hosts() {
        let runtime = open_runtime();
        let mut panel = runtime.snapshot.read().unwrap().active_panels[0].clone();
        panel.agent_id = "qwen".into();
        panel.transport = crate::state::PanelTransport::Herdr {
            socket_path: None,
            pane_id: "w:p1".into(),
        };
        assert_eq!(
            super::session_control_target(&panel, &runtime.plugins),
            (
                "herdr".into(),
                crate::agent_actions::AgentSurface::HerdrPane
            )
        );
        panel.transport = crate::state::PanelTransport::Terminal { pid: 1 };
        assert_eq!(
            super::session_control_target(&panel, &runtime.plugins).0,
            "qwen"
        );
    }

    #[test]
    fn thread_urls_only_carry_canonical_uuids() {
        assert_eq!(
            codex_thread_url("01a0c2e3-2d59-7170-8849-c12965e71661").as_deref(),
            Some("codex://threads/01a0c2e3-2d59-7170-8849-c12965e71661")
        );
    }

    #[test]
    fn thread_ids_that_are_not_uuids_are_rejected() {
        for value in [
            "",
            "chatgpt:01a0c2e3-2d59-7170-8849-c12965e71661",
            "codex://threads/01a0c2e3-2d59-7170-8849-c12965e71661",
            "01a0c2e3-2d59-7170-8849-c12965e71661 && open -b com.openai.codex",
            "~/../etc/passwd",
        ] {
            assert_eq!(codex_thread_url(value), None, "accepted {value:?}");
        }
    }

    /// The bridge's engine labels come from another repo, so a wrong guess here
    /// silently stops raising anything.
    #[cfg(target_os = "macos")]
    #[test]
    fn deck_browsers_map_only_known_engines_to_an_app() {
        assert_eq!(
            super::browser_behind(&["chrome".to_owned()]),
            Some("Google Chrome")
        );
        assert_eq!(
            super::browser_behind(&["safari".to_owned()]),
            Some("Safari")
        );
        for unknown in [
            Vec::new(),
            vec!["other".to_owned()],
            vec!["firefox".to_owned()],
            vec!["Chrome".to_owned()],
        ] {
            assert_eq!(super::browser_behind(&unknown), None, "raised {unknown:?}");
        }
    }
    fn open_runtime() -> Arc<AppRuntime> {
        use crate::state::{ActivePanel, PanelTransport};
        let state = Arc::new(AppRuntime::new());
        state.snapshot.write().unwrap().active_panels = ["first", "second"]
            .into_iter()
            .map(|id| ActivePanel {
                id: id.into(),
                agent_id: "chatgpt".into(),
                session_id: id.into(),
                slot: 1,
                label: id.into(),
                status: AgentStatus::Idle,
                cwd: None,
                jobs_working: 0,
                jobs_waiting: 0,
                jobs_error: 0,
                updated_at_ms: 1,
                status_since_ms: 1,
                last_activity_ms: 0,
                first_seen_ms: 0,
                transport: PanelTransport::CodexThread {
                    thread_id: id.into(),
                },
                unread: true,
                unread_since_ms: 1,
            })
            .collect();
        state
    }

    #[test]
    fn normal_agent_session_keys_preserve_agent_and_lcd_with_or_without_herdr_mapping() {
        for mapped in [true, false] {
            let state = open_runtime();
            {
                let mut snapshot = state.snapshot.write().unwrap();
                snapshot.active_agent = Some("qwen".into());
                snapshot.active_surface = Some(AgentSurface::HerdrPane);
                for panel in &mut snapshot.active_panels {
                    panel.agent_id = "qwen".into();
                }
                snapshot.active_panels[0].transport = PanelTransport::Herdr {
                    socket_path: Some("/tmp/regression-herdr.sock".into()),
                    pane_id: "qwen-one".into(),
                };
                let mut other = snapshot.active_panels[1].clone();
                other.id = "other".into();
                other.session_id = "other".into();
                other.agent_id = "opencode".into();
                other.transport = PanelTransport::CodexThread {
                    thread_id: "other".into(),
                };
                snapshot.active_panels.push(other);
                snapshot.agent_slots.retain(|slot| slot.agent_id != "herdr");
                if mapped {
                    snapshot.agent_slots.push(crate::state::AgentSlotState {
                        key: "A4".into(),
                        agent_id: "herdr".into(),
                    });
                }
            }
            let generation = super::reserve_focus_request(&state);
            let outcome = super::open_session_scoped(
                &state,
                "herdr:/tmp/regression-herdr.sock:qwen-one",
                generation,
                None,
                Some("qwen"),
                |_, _| super::FocusReport::Raised,
            );
            assert_eq!(outcome, super::OpenOutcome::Opened);
            let snapshot = state.read();
            assert_eq!(snapshot.active_agent.as_deref(), Some("qwen"));
            assert_eq!(snapshot.active_surface, Some(AgentSurface::HerdrPane));
            assert_eq!(
                snapshot
                    .sessions
                    .iter()
                    .filter(|s| !s.id.is_empty())
                    .count(),
                2
            );
            assert!(snapshot
                .sessions
                .iter()
                .filter(|s| !s.id.is_empty())
                .all(|s| s.id != "other"));
            assert_eq!(
                state
                    .focused_panels
                    .lock()
                    .unwrap()
                    .get("qwen")
                    .map(String::as_str),
                Some("herdr:/tmp/regression-herdr.sock:qwen-one")
            );
        }
    }

    #[test]
    fn stale_agent_key_does_not_focus_after_active_agent_changes() {
        let state = open_runtime();
        let generation = super::reserve_focus_request(&state);
        assert_eq!(
            super::open_session_scoped(
                &state,
                "thread:first",
                generation,
                None,
                Some("qwen"),
                |_, _| panic!("stale owner must not focus")
            ),
            super::OpenOutcome::Superseded
        );
    }

    #[test]
    fn removed_herdr_pane_never_invokes_activation() {
        let state = open_runtime();
        {
            let mut snapshot = state.snapshot.write().unwrap();
            snapshot.herdr.connected = true;
            snapshot.herdr.agents.clear();
            snapshot.active_panels[0].transport = crate::state::PanelTransport::Herdr {
                socket_path: None,
                pane_id: "gone".into(),
            };
        }
        let generation = super::reserve_focus_request(&state);
        let outcome =
            super::open_session_with_expected(&state, "herdr:gone", generation, None, |_, _| {
                panic!("removed pane must not activate")
            });
        assert_eq!(outcome, super::OpenOutcome::TargetGone);
        assert!(state.snapshot.read().unwrap().active_panels[0].unread);
    }

    #[test]
    fn stale_notification_never_invokes_transport_or_clears_new_completion() {
        let state = open_runtime();
        state.snapshot.write().unwrap().active_panels[0].unread_since_ms = 2;
        let generation = super::reserve_focus_request(&state);
        let outcome = super::open_session_with_expected(
            &state,
            "thread:first",
            generation,
            Some((crate::state::AttentionClass::Done, 1)),
            |_, _| panic!("stale card must not focus"),
        );
        assert_eq!(outcome, super::OpenOutcome::Superseded);
        assert!(state.snapshot.read().unwrap().active_panels[0].unread);
    }

    #[test]
    fn a_new_completion_during_notification_focus_is_not_acknowledged() {
        let state = open_runtime();
        let generation = super::reserve_focus_request(&state);
        let outcome = super::open_session_with_expected(
            &state,
            "thread:first",
            generation,
            Some((crate::state::AttentionClass::Done, 1)),
            |_, _| {
                state.snapshot.write().unwrap().active_panels[0].unread_since_ms = 2;
                super::FocusReport::Raised
            },
        );
        assert_eq!(outcome, super::OpenOutcome::Superseded);
        assert!(state.snapshot.read().unwrap().active_panels[0].unread);
    }

    #[test]
    fn a_new_open_wins_even_when_the_old_transport_is_already_settling() {
        use super::{open_session_with, reserve_focus_request, FocusReport, OpenOutcome};
        use std::sync::{mpsc, Mutex};
        let state = open_runtime();
        let effects = Arc::new(Mutex::new(Vec::new()));
        let (started, entered) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let old_state = state.clone();
        let old_effects = effects.clone();
        let old_generation = reserve_focus_request(&state);
        let old = std::thread::spawn(move || {
            open_session_with(&old_state, "thread:first", old_generation, |_, current| {
                old_effects.lock().unwrap().push("first");
                started.send(()).unwrap();
                resume
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                // Model Herdr's delayed retry: it must stop after supersession.
                if current() {
                    old_effects.lock().unwrap().push("old retry");
                }
                FocusReport::Raised
            })
        });
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let generation = reserve_focus_request(&state);
        release.send(()).unwrap();
        let outcome = open_session_with(&state, "thread:second", generation, |_, _| {
            effects.lock().unwrap().push("second");
            FocusReport::Raised
        });
        assert_eq!(old.join().unwrap(), OpenOutcome::Superseded);
        assert_eq!(outcome, OpenOutcome::Opened);
        assert_eq!(*effects.lock().unwrap(), vec!["first", "second"]);
        let snapshot = state.snapshot.read().unwrap();
        assert!(snapshot.active_panels[0].unread);
        assert!(!snapshot.active_panels[1].unread);
    }

    #[test]
    fn failed_and_racing_error_opens_keep_attention_success_acknowledges() {
        use super::{open_session_with, reserve_focus_request, FocusReport, OpenOutcome};
        let state = open_runtime();
        {
            let mut snapshot = state.snapshot.write().unwrap();
            let panel = &mut snapshot.active_panels[0];
            panel.status = AgentStatus::Error;
            panel.status_since_ms = 100;
            panel.unread = false;
            panel.unread_since_ms = 0;
        }
        let generation = reserve_focus_request(&state);
        assert_eq!(
            open_session_with(&state, "thread:first", generation, |_, _| {
                FocusReport::Failed
            }),
            OpenOutcome::FocusFailed
        );
        assert!(state
            .snapshot
            .read()
            .unwrap()
            .acknowledged_errors
            .is_empty());
        let generation = reserve_focus_request(&state);
        assert_eq!(
            open_session_with(&state, "thread:first", generation, |_, _| {
                state.snapshot.write().unwrap().active_panels[0].status_since_ms = 101;
                FocusReport::Raised
            }),
            OpenOutcome::Opened
        );
        assert!(state
            .snapshot
            .read()
            .unwrap()
            .acknowledged_errors
            .is_empty());
        let generation = reserve_focus_request(&state);
        assert_eq!(
            open_session_with(&state, "thread:first", generation, |_, _| {
                FocusReport::Raised
            }),
            OpenOutcome::Opened
        );
        let snapshot = state.snapshot.read().unwrap();
        assert!(crate::state::panel_error_acknowledged(
            &snapshot,
            &snapshot.active_panels[0]
        ));
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::Error);
    }

    #[test]
    fn failed_open_and_news_arriving_during_open_are_not_acknowledged() {
        use super::{open_session_with, reserve_focus_request, FocusReport, OpenOutcome};
        let state = open_runtime();
        let generation = reserve_focus_request(&state);
        assert_eq!(
            open_session_with(&state, "thread:first", generation, |_, _| {
                FocusReport::Failed
            }),
            OpenOutcome::FocusFailed
        );
        assert!(state.snapshot.read().unwrap().active_panels[0].unread);
        let generation = reserve_focus_request(&state);
        assert_eq!(
            open_session_with(&state, "thread:first", generation, |_, _| {
                state.snapshot.write().unwrap().active_panels[0].unread_since_ms = 2;
                FocusReport::Raised
            }),
            OpenOutcome::Opened
        );
        assert!(state.snapshot.read().unwrap().active_panels[0].unread);
    }

    #[test]
    fn an_offline_identity_never_runs_a_transport() {
        use super::{open_session_with, reserve_focus_request, OpenOutcome};
        let state = open_runtime();
        state.snapshot.write().unwrap().active_panels[0].status = AgentStatus::Offline;
        let generation = reserve_focus_request(&state);
        assert_eq!(
            open_session_with(&state, "thread:first", generation, |_, _| panic!(
                "stale target activated"
            )),
            OpenOutcome::TargetGone
        );
    }
}
