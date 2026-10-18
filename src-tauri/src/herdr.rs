use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use sysinfo::System;

use crate::agent_actions::{ActionPlan, KeyModifiers};
use crate::agent_detection;
use crate::plugin::PluginRegistry;
use crate::AgentStatus;
use serde::Serialize;
use serde_json::Value;

const REPORT_SOURCE: &str = "agent-deck-companion";
const DEFAULT_PREFIX: &str = "ctrl+b";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HerdrAgentInfo {
    pub native_session_id: Option<String>,
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub agent: String,
    pub display_name: String,
    pub status: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub focused: bool,
    pub attention: bool,
    pub state_change_seq: u64,
    pub model: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct HerdrSnapshot {
    pub connected: bool,
    pub focused_pane_id: Option<String>,
    pub agents: Vec<HerdrAgentInfo>,
}

#[derive(Debug, Clone)]
pub struct HerdrTarget {
    pub pane_id: String,
    pub tab_id: Option<String>,
    pub workspace_id: Option<String>,
    pub socket_path: Option<String>,
    pub pid: u32,
}

#[derive(Debug, Clone)]
struct Candidate {
    pane_id: String,
    tab_id: Option<String>,
    workspace_id: Option<String>,
    socket_path: Option<String>,
    pid: u32,
    cwd: Option<String>,
    start_time: u64,
}

fn choose_candidate(
    candidates: impl IntoIterator<Item = Candidate>,
    preferred_cwd: Option<&str>,
    focused_pane_id: Option<&str>,
) -> Option<Candidate> {
    let mut by_pane = HashMap::<String, Candidate>::new();
    for candidate in candidates {
        let current = by_pane
            .entry(candidate.pane_id.clone())
            .or_insert_with(|| candidate.clone());
        if candidate.start_time > current.start_time {
            *current = candidate;
        }
    }
    by_pane.into_values().max_by_key(|candidate| {
        let is_focused = focused_pane_id.is_some_and(|focused| focused == candidate.pane_id);
        let cwd_match = preferred_cwd
            .is_some_and(|preferred| candidate.cwd.as_deref().is_some_and(|cwd| cwd == preferred));
        (is_focused, cwd_match, candidate.start_time)
    })
}

pub fn find_target(
    agent_id: &str,
    executable: &str,
    preferred_cwd: Option<&str>,
    plugins: Option<&PluginRegistry>,
    focused_pane_id: Option<&str>,
) -> Option<HerdrTarget> {
    let system = System::new_all();

    // First, look for pane-specific targets (processes with HERDR_PANE_ID).
    let candidates = system.processes().iter().filter_map(|(pid, process)| {
        let plugin_match = plugins.is_some_and(|registry| {
            registry.process_matches(
                agent_id,
                &process.name().to_string_lossy(),
                &process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            )
        });
        if !agent_detection::process_matches(process, agent_id, executable) && !plugin_match {
            return None;
        }
        let pane_id = agent_detection::environment_value(process, "HERDR_PANE_ID")?;
        Some(Candidate {
            pane_id,
            tab_id: agent_detection::environment_value(process, "HERDR_TAB_ID"),
            workspace_id: agent_detection::environment_value(process, "HERDR_WORKSPACE_ID"),
            socket_path: agent_detection::environment_value(process, "HERDR_SOCKET_PATH"),
            pid: pid.as_u32(),
            cwd: process.cwd().map(|cwd| cwd.to_string_lossy().into_owned()),
            start_time: process.start_time(),
        })
    });
    if let Some(candidate) = choose_candidate(candidates, preferred_cwd, focused_pane_id) {
        return Some(HerdrTarget {
            pane_id: candidate.pane_id,
            tab_id: candidate.tab_id,
            workspace_id: candidate.workspace_id,
            socket_path: candidate.socket_path,
            pid: candidate.pid,
        });
    }

    // For the "herdr" agent itself, the herdr client process is a valid
    // target even without HERDR_PANE_ID — it's the multiplexer controller.
    if agent_id == "herdr" {
        let client = system.processes().iter().find(|(_, process)| {
            agent_detection::process_matches(process, "herdr", executable)
                && agent_detection::environment_value(process, "HERDR_PANE_ID").is_none()
        });
        if let Some((pid, _process)) = client {
            return Some(HerdrTarget {
                pane_id: format!("client:{}", pid.as_u32()),
                tab_id: None,
                workspace_id: None,
                socket_path: None,
                pid: pid.as_u32(),
            });
        }
    }

    None
}

fn prefix_from_config(contents: &str) -> Option<String> {
    let mut in_keys = false;
    for raw_line in contents.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_keys = line == "[keys]";
            continue;
        }
        if !in_keys {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != "prefix" {
            continue;
        }
        let value = value.trim();
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            return Some(value[1..value.len() - 1].to_string());
        }
    }
    None
}

pub fn configured_prefix() -> String {
    let config_path = std::env::var_os("HERDR_CONFIG_PATH")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config/herdr/config.toml")));
    config_path
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| prefix_from_config(&contents))
        .unwrap_or_else(|| DEFAULT_PREFIX.into())
}

fn default_socket_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
        return Some(PathBuf::from(path));
    }
    let root = std::env::var_os("HERDR_CONFIG_PATH")
        .map(PathBuf::from)
        .and_then(|path| path.parent().map(ToOwned::to_owned))
        .or_else(|| dirs::home_dir().map(|home| home.join(".config/herdr")))?;
    Some(
        match std::env::var("HERDR_SESSION")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(session) => root.join("sessions").join(session).join("herdr.sock"),
            None => root.join("herdr.sock"),
        },
    )
}

pub(crate) fn socket_identity(socket_path: Option<&str>) -> Option<PathBuf> {
    let path = socket_path
        .map(PathBuf::from)
        .or_else(default_socket_path)?;
    Some(path.canonicalize().unwrap_or(path))
}

pub(crate) fn socket_paths_match(left: Option<&str>, right: Option<&str>) -> bool {
    socket_identity(left) == socket_identity(right)
}

fn api_request(socket_path: Option<&str>, method: &str, params: Value) -> Result<Value, String> {
    let path = socket_path
        .map(PathBuf::from)
        .or_else(default_socket_path)
        .ok_or_else(|| "Herdr socket path unavailable".to_string())?;
    let mut stream = UnixStream::connect(&path)
        .map_err(|error| format!("connect {}: {error}", path.display()))?;
    let timeout = Some(Duration::from_millis(800));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|e| e.to_string())?;
    let request = serde_json::json!({
        "id": "agent-deck-companion", "method": method, "params": params
    });
    serde_json::to_writer(&mut stream, &request).map_err(|e| e.to_string())?;
    stream.write_all(b"\n").map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let response: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if let Some(error) = response.get("error") {
        return Err(error.to_string());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| "Herdr response missing result".into())
}

/// Subscribe to Herdr lifecycle changes on one persistent socket connection.
/// The callback is debounced so a burst of related pane events causes a single
/// companion refresh. Returning means the socket disconnected and should be
/// reconnected by the caller.
pub fn subscribe_events(mut on_change: impl FnMut()) -> Result<(), String> {
    let path = default_socket_path().ok_or_else(|| "Herdr socket path unavailable".to_string())?;
    subscribe_events_at(&path, &mut on_change)
}

fn subscribe_events_at(path: &std::path::Path, on_change: &mut impl FnMut()) -> Result<(), String> {
    let mut stream = UnixStream::connect(&path)
        .map_err(|error| format!("connect {}: {error}", path.display()))?;
    stream
        .set_write_timeout(Some(Duration::from_millis(800)))
        .map_err(|error| error.to_string())?;
    let subscriptions = [
        "workspace.created",
        "workspace.updated",
        "workspace.closed",
        "workspace.focused",
        "tab.created",
        "tab.closed",
        "tab.focused",
        "tab.renamed",
        "tab.moved",
        "pane.created",
        "pane.closed",
        "pane.updated",
        "pane.focused",
        "pane.moved",
        "pane.exited",
        "pane.agent_detected",
    ]
    .into_iter()
    .map(|kind| serde_json::json!({"type": kind}))
    .collect::<Vec<_>>();
    serde_json::to_writer(
        &mut stream,
        &serde_json::json!({
            "id": "agent-deck-companion-events",
            "method": "events.subscribe",
            "params": {"subscriptions": subscriptions}
        }),
    )
    .map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut pending_since: Option<Instant> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                if pending_since.is_some() {
                    on_change();
                }
                return Err("Herdr event stream closed".into());
            }
            Ok(_) => {
                let message: Value = serde_json::from_str(&line)
                    .map_err(|error| format!("invalid Herdr event: {error}"))?;
                if message.get("error").is_some() {
                    return Err(format!("Herdr subscription failed: {}", message["error"]));
                }
                // The first line is the subscription acknowledgement. Event
                // envelopes carry an `event` member and are the only lines
                // that should trigger a state refresh.
                if message.get("event").is_some() {
                    if pending_since.is_none() {
                        pending_since = Some(Instant::now());
                        reader
                            .get_mut()
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .map_err(|error| error.to_string())?;
                    }
                    if pending_since
                        .is_some_and(|since| since.elapsed() >= Duration::from_millis(200))
                    {
                        on_change();
                        pending_since = None;
                        reader
                            .get_mut()
                            .set_read_timeout(None)
                            .map_err(|error| error.to_string())?;
                    }
                }
            }
            Err(error)
                if pending_since.is_some()
                    && matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                on_change();
                pending_since = None;
                reader
                    .get_mut()
                    .set_read_timeout(None)
                    .map_err(|error| error.to_string())?;
            }
            Err(error) => return Err(format!("read Herdr event stream: {error}")),
        }
    }
}

fn request_succeeded(target: &HerdrTarget, method: &str, params: Value) -> bool {
    api_request(target.socket_path.as_deref(), method, params).is_ok()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn snapshot_agent(
    item: &Value,
    focused_pane_id: Option<&str>,
    fallback: bool,
) -> Option<HerdrAgentInfo> {
    let pane_id = text(item, "pane_id")?;
    let agent = text(item, "agent").or_else(|| {
        if !fallback {
            return None;
        }
        let title =
            text(item, "terminal_title_stripped").or_else(|| text(item, "terminal_title"))?;
        let normalized = title.trim().to_ascii_lowercase();
        (normalized == "qwen" || normalized.starts_with("qwen - ")).then(|| "qwen".into())
    })?;
    let tokens = item.get("tokens");
    let status = text(item, "agent_status").unwrap_or_else(|| "unknown".into());
    Some(HerdrAgentInfo {
        native_session_id: item
            .get("agent_session")
            .filter(|session| session["kind"] == "id")
            .and_then(|session| text(session, "value")),
        pane_id: pane_id.clone(),
        workspace_id: text(item, "workspace_id").unwrap_or_default(),
        tab_id: text(item, "tab_id").unwrap_or_default(),
        display_name: text(item, "display_agent").unwrap_or_else(|| agent.clone()),
        agent,
        status: status.clone(),
        title: text(item, "title").or_else(|| text(item, "terminal_title_stripped")),
        cwd: text(item, "foreground_cwd").or_else(|| text(item, "cwd")),
        focused: item
            .get("focused")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || focused_pane_id == Some(&pane_id),
        attention: matches!(status.as_str(), "blocked" | "done"),
        state_change_seq: item
            .get("state_change_seq")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        model: tokens.and_then(|value| text(value, "model")),
        summary: tokens.and_then(|value| text(value, "summary")),
    })
}

/// Read Herdr's authoritative, one-shot session snapshot. The parser is
/// intentionally tolerant so companion releases do not pin a private protocol
/// shape; unknown fields are ignored and missing display metadata degrades.
pub fn session_snapshot() -> HerdrSnapshot {
    api_request(None, "session.snapshot", serde_json::json!({}))
        .map(|result| parse_session_snapshot(&result))
        .unwrap_or_default()
}

pub fn focused_pane_id(socket_path: Option<&str>) -> Option<String> {
    api_request(socket_path, "session.snapshot", serde_json::json!({}))
        .ok()
        .and_then(|result| parse_session_snapshot(&result).focused_pane_id)
}

fn pane_route(socket_path: Option<&str>, pane_id: &str) -> Option<(String, String)> {
    api_request(socket_path, "session.snapshot", serde_json::json!({}))
        .ok()
        .map(|result| parse_session_snapshot(&result))?
        .agents
        .into_iter()
        .find(|agent| agent.pane_id == pane_id)
        .map(|agent| (agent.workspace_id, agent.tab_id))
}

fn parse_session_snapshot(root: &Value) -> HerdrSnapshot {
    let result = root.get("result").unwrap_or(root);
    // Protocol versions may return the SessionSnapshot directly or under a
    // `snapshot` result member; accept both without depending on the CLI.
    let snapshot = result.get("snapshot").unwrap_or(result);
    let focused_pane_id = snapshot
        .get("focused_pane_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut agents = snapshot
        .get("agents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| snapshot_agent(item, focused_pane_id.as_deref(), false))
        .collect::<Vec<_>>();
    // Older/already-running Qwen panes can predate Herdr's agent detector and
    // therefore appear only in `panes`. The official Qwen terminal title is a
    // conservative recovery signal until Herdr promotes the pane to `agents`.
    if let Some(panes) = snapshot.get("panes").and_then(Value::as_array) {
        for item in panes {
            let Some(agent) = snapshot_agent(item, focused_pane_id.as_deref(), true) else {
                continue;
            };
            if !agents
                .iter()
                .any(|existing| existing.pane_id == agent.pane_id)
            {
                agents.push(agent);
            }
        }
    }
    HerdrSnapshot {
        connected: true,
        focused_pane_id,
        agents,
    }
}

pub fn agent_action(pane_id: &str, action: &str) -> bool {
    if pane_id.is_empty() || pane_id.starts_with('-') {
        return false;
    }
    let (method, params) = match action {
        "focus" => ("agent.focus", serde_json::json!({"target": pane_id})),
        "continue" => (
            "pane.send_keys",
            serde_json::json!({"pane_id": pane_id, "keys": ["enter"]}),
        ),
        "stop" => (
            "pane.send_keys",
            serde_json::json!({"pane_id": pane_id, "keys": ["esc"]}),
        ),
        _ => return false,
    };
    api_request(None, method, params).is_ok()
}

/// Send an action plan directly to a herdr pane via the pane.send_keys API.
/// This bypasses macOS global keyboard routing and works even when the host
/// terminal window is not focused (e.g., when using hardware keyboard).
pub fn send_plan(target: &HerdrTarget, plan: ActionPlan) -> bool {
    let Some(keys) = plan_keys(plan) else {
        eprintln!("[herdr::send_plan] unsupported plan: {:?}", plan);
        return false;
    };

    eprintln!(
        "[herdr::send_plan] pane={} socket={:?} keys={:?}",
        target.pane_id, target.socket_path, keys
    );
    let result = api_request(
        target.socket_path.as_deref(),
        "pane.send_keys",
        serde_json::json!({"pane_id": target.pane_id, "keys": keys}),
    );
    eprintln!("[herdr::send_plan] api_request result: {:?}", result);
    result.is_ok()
}

fn plan_keys(plan: ActionPlan) -> Option<Vec<String>> {
    Some(match plan {
        ActionPlan::TextInput { text, submit } => {
            let mut keys: Vec<String> = text.chars().map(|c| c.to_string()).collect();
            if submit {
                keys.push("enter".to_string());
            }
            keys
        }
        ActionPlan::Shortcut {
            key_code,
            modifiers,
        } => {
            vec![herdr_shortcut(key_code, modifiers)?]
        }
        ActionPlan::LeaderSequence {
            leader,
            key_code,
            modifiers,
        } => {
            // One API request preserves the order in the target pane, even
            // when its host window is not frontmost.
            let parts = leader.split('+').collect::<Vec<_>>();
            let (key, modifiers_parts) = parts.split_last()?;
            let modifiers_owned = modifiers_parts
                .iter()
                .map(|part| part.to_string())
                .collect::<Vec<_>>();
            let leader_modifiers = crate::plugin::parse_modifiers(&modifiers_owned)?;
            let (leader_code, implicit_modifiers) = crate::plugin::key_code(key)?;
            vec![
                herdr_shortcut(leader_code, leader_modifiers.union(implicit_modifiers))?,
                herdr_shortcut(key_code, modifiers)?,
            ]
        }
        _ => return None,
    })
}

fn herdr_shortcut(key_code: u16, modifiers: KeyModifiers) -> Option<String> {
    // Terminal input cannot represent Command; do not silently drop it.
    if modifiers.contains(KeyModifiers::COMMAND) {
        return None;
    }
    let mut parts = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    if modifiers.contains(KeyModifiers::OPTION) {
        parts.push("alt");
    }
    parts.push(herdr_key_name(key_code)?);
    Some(parts.join("+"))
}

fn herdr_key_name(key_code: u16) -> Option<&'static str> {
    Some(match key_code {
        0 => "a",
        11 => "b",
        2 => "d",
        14 => "e",
        3 => "f",
        5 => "g",
        4 => "h",
        34 => "i",
        38 => "j",
        40 => "k",
        46 => "m",
        45 => "n",
        35 => "p",
        12 => "q",
        1 => "s",
        17 => "t",
        32 => "u",
        9 => "v",
        13 => "w",
        16 => "y",
        6 => "z",
        7 => "x",
        37 => "l",
        8 => "c",
        15 => "r",
        31 => "o",
        44 => "?",
        48 => "Tab",
        53 => "Escape",
        36 => "Enter",
        123 => "Left",
        124 => "Right",
        125 => "Down",
        126 => "Up",
        _ => return None,
    })
}

pub fn focus_pane(socket_path: Option<&str>, pane_id: &str) -> bool {
    api_request(
        socket_path,
        "agent.focus",
        serde_json::json!({"target": pane_id}),
    )
    .is_ok()
}

pub fn focus_panel(
    socket_path: Option<&str>,
    pane_id: &str,
    agent_id: &str,
    status: AgentStatus,
) -> bool {
    let route = pane_route(socket_path, pane_id);
    let workspace_focused = route.as_ref().is_none_or(|(workspace_id, _)| {
        api_request(
            socket_path,
            "workspace.focus",
            serde_json::json!({"workspace_id": workspace_id}),
        )
        .is_ok()
    });
    let tab_focused = route.as_ref().is_none_or(|(_, tab_id)| {
        api_request(
            socket_path,
            "tab.focus",
            serde_json::json!({"tab_id": tab_id}),
        )
        .is_ok()
    });

    if focus_pane(socket_path, pane_id) {
        eprintln!(
            "[focus_session] Herdr route: pane={} workspace={:?} tab={:?} workspace_focused={} tab_focused={} agent_focused=true",
            pane_id,
            route.as_ref().map(|(workspace, _)| workspace),
            route.as_ref().map(|(_, tab)| tab),
            workspace_focused,
            tab_focused,
        );
        return workspace_focused && tab_focused;
    }
    if api_request(
        socket_path,
        "pane.report_agent",
        serde_json::json!({
            "pane_id": pane_id, "source": REPORT_SOURCE,
            "agent": agent_id, "state": herdr_state(status)
        }),
    )
    .is_err()
    {
        return false;
    }
    let focused = focus_pane(socket_path, pane_id);
    let _ = api_request(
        socket_path,
        "pane.release_agent",
        serde_json::json!({
            "pane_id": pane_id, "source": REPORT_SOURCE, "agent": agent_id
        }),
    );
    eprintln!(
        "[focus_session] Herdr route: pane={} workspace={:?} tab={:?} workspace_focused={} tab_focused={} agent_focused={}",
        pane_id,
        route.as_ref().map(|(workspace, _)| workspace),
        route.as_ref().map(|(_, tab)| tab),
        workspace_focused,
        tab_focused,
        focused,
    );
    workspace_focused && tab_focused && focused
}

fn herdr_state(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Working => "working",
        AgentStatus::NeedsInput => "blocked",
        AgentStatus::Idle | AgentStatus::Done => "idle",
        AgentStatus::Offline | AgentStatus::Error | AgentStatus::Unknown => "unknown",
    }
}

pub fn focus_target(agent_id: &str, target: &HerdrTarget, status: AgentStatus) -> bool {
    // Client-only target (no specific pane) — terminal activation is enough.
    if target.pane_id.starts_with("client:") {
        return true;
    }

    if request_succeeded(
        target,
        "agent.focus",
        serde_json::json!({"target": target.pane_id}),
    ) {
        return true;
    }

    // Qwen Code is not yet an official Herdr agent kind. Register it just long
    // enough to use Herdr's deterministic agent focus API, then release our
    // semantic authority so we cannot leave stale state behind.
    let reported = request_succeeded(
        target,
        "pane.report_agent",
        serde_json::json!({
            "pane_id": target.pane_id, "source": REPORT_SOURCE,
            "agent": agent_id, "state": herdr_state(status)
        }),
    );
    if !reported {
        let workspace_focused = target.workspace_id.as_deref().is_none_or(|workspace| {
            request_succeeded(
                target,
                "workspace.focus",
                serde_json::json!({"workspace_id": workspace}),
            )
        });
        let tab_focused = target.tab_id.as_deref().is_some_and(|tab| {
            request_succeeded(target, "tab.focus", serde_json::json!({"tab_id": tab}))
        });
        return workspace_focused && tab_focused;
    }
    let focused = request_succeeded(
        target,
        "agent.focus",
        serde_json::json!({"target": target.pane_id}),
    );
    let _ = request_succeeded(
        target,
        "pane.release_agent",
        serde_json::json!({
            "pane_id": target.pane_id, "source": REPORT_SOURCE, "agent": agent_id
        }),
    );
    focused
}

/// Raise the Alacritty window hosting the Herdr client. A Herdr client target
/// is represented as `client:<pid>` and has no pane title, so activating the
/// Alacritty bundle alone can leave a different Alacritty window (for example
/// Qwen's tmux window) in front. Herdr's terminal title is normally a
/// bracketed workspace form (e.g. `[topeM1] ~`), but some builds expose the
/// shell form `topeM1: ~`; handle both before falling back to bundle activation.
#[cfg(target_os = "macos")]
pub fn focus_host_window() -> bool {
    let script = r#"tell application "System Events"
    tell process "Alacritty"
        set frontmost to true
        repeat with w in windows
            set windowName to name of w
            if windowName starts with "[" then
                perform action "AXRaise" of w
                return true
            end if
        end repeat
        repeat with w in windows
            set windowName to name of w
            if windowName contains ":" and windowName does not contain "Qwen" then
                perform action "AXRaise" of w
                return true
            end if
        end repeat
    end tell
end tell
return false"#;
    let raised = Command::new("osascript")
        .args(["-e", script])
        .output()
        .map(|output| {
            let raised = String::from_utf8_lossy(&output.stdout).trim() == "true";
            if !output.status.success() || !raised {
                eprintln!(
                    "[focus_session] Herdr host raise exit={:?} stderr={}",
                    output
                        .status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".to_owned()),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            output.status.success() && raised
        })
        .unwrap_or_else(|error| {
            eprintln!("[focus_session] Herdr host raise could not run osascript: {error}");
            false
        });
    if raised {
        return true;
    }
    // Choosing a specific window needs Apple Events, which macOS refuses without
    // a consent the user may never have granted — and a rebuilt ad-hoc signed
    // bundle loses that consent. Activating the host app needs no such
    // permission, so use it to still bring Herdr's window forward. Guarded
    // because `open -b` would otherwise launch a fresh Alacritty window.
    if !alacritty_is_running() {
        eprintln!("[focus_session] Herdr host raise skipped: no Alacritty process");
        return false;
    }
    let activated = Command::new("open")
        .args(["-b", ALACRITTY_BUNDLE_ID])
        .status()
        .is_ok_and(|status| status.success());
    eprintln!(
        "[focus_session] Herdr host raise fell back to activating {ALACRITTY_BUNDLE_ID}: {activated}"
    );
    activated
}

#[cfg(target_os = "macos")]
const ALACRITTY_BUNDLE_ID: &str = "org.alacritty";

#[cfg(target_os = "macos")]
fn alacritty_is_running() -> bool {
    let system = System::new_all();
    system
        .processes()
        .values()
        // sysinfo reports macOS process names as the lowercased binary name.
        .any(|process| {
            process
                .name()
                .to_string_lossy()
                .eq_ignore_ascii_case("alacritty")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_history_sends_ordered_leader_sequence() {
        assert_eq!(
            plan_keys(ActionPlan::LeaderSequence {
                leader: "ctrl+x",
                key_code: 37,
                modifiers: KeyModifiers::NONE,
            }),
            Some(vec!["ctrl+x".into(), "l".into()])
        );
        assert_eq!(
            plan_keys(ActionPlan::LeaderSequence {
                leader: "invalid+x",
                key_code: 37,
                modifiers: KeyModifiers::NONE,
            }),
            None
        );
        assert_eq!(herdr_shortcut(37, KeyModifiers::COMMAND), None);
    }

    fn candidate(pane: &str, cwd: &str, start_time: u64) -> Candidate {
        Candidate {
            pane_id: pane.into(),
            tab_id: None,
            workspace_id: None,
            socket_path: None,
            pid: start_time as u32,
            cwd: Some(cwd.into()),
            start_time,
        }
    }

    #[test]
    fn preferred_cwd_wins_over_newer_process() {
        let chosen = choose_candidate(
            [
                candidate("p1", "/work/a", 10),
                candidate("p2", "/work/b", 20),
            ],
            Some("/work/a"),
            None,
        )
        .unwrap();
        assert_eq!(chosen.pane_id, "p1");
    }

    #[test]
    fn duplicate_processes_in_one_pane_are_collapsed() {
        let chosen = choose_candidate(
            [
                candidate("p1", "/work/a", 10),
                candidate("p1", "/work/a", 30),
            ],
            None,
            None,
        )
        .unwrap();
        assert_eq!(chosen.pid, 30);
    }

    #[test]
    fn prefix_is_read_only_from_keys_section() {
        let config = r#"
prefix = "ctrl+x"
[theme]
prefix = "ctrl+y"
[keys]
prefix = "ctrl+s" # local override
"#;
        assert_eq!(prefix_from_config(config).as_deref(), Some("ctrl+s"));
        assert_eq!(prefix_from_config("[keys]\nhelp = \"prefix+?\""), None);
    }

    #[test]
    fn parses_wrapped_snapshot_response() {
        let root = serde_json::json!({
            "id": "agent-deck-companion",
            "result": {
                "snapshot": {
                    "focused_pane_id": "wH:p2Q",
                    "agents": [{
                        "agent": "codex",
                        "agent_status": "blocked",
                        "cwd": "/work/agent_deck_companion",
                        "focused": true,
                        "pane_id": "wH:p2Q",
                        "revision": 8,
                        "state_change_seq": 11,
                        "tab_id": "wH:tE",
                        "terminal_id": "term_1",
                        "terminal_title_stripped": "agent_deck_companion",
                        "workspace_id": "wH"
                    }]
                },
                "type": "session_snapshot"
            }
        });
        let snapshot = parse_session_snapshot(&root);
        assert!(snapshot.connected);
        assert_eq!(snapshot.focused_pane_id.as_deref(), Some("wH:p2Q"));
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(snapshot.agents[0].status, "blocked");
        assert!(snapshot.agents[0].attention);
    }

    #[test]
    fn recovers_existing_qwen_from_snapshot_panes() {
        let root = serde_json::json!({
            "snapshot": {
                "focused_pane_id": "wH:p1",
                "agents": [],
                "panes": [{
                    "agent_status": "unknown",
                    "foreground_cwd": "/tmp/project",
                    "pane_id": "wH:p1",
                    "tab_id": "wH:t1",
                    "terminal_title_stripped": "Qwen - project",
                    "workspace_id": "wH"
                }]
            }
        });
        let snapshot = parse_session_snapshot(&root);
        assert_eq!(snapshot.agents.len(), 1);
        assert_eq!(snapshot.agents[0].agent, "qwen");
        assert_eq!(snapshot.agents[0].status, "unknown");
        assert!(snapshot.agents[0].focused);
    }

    #[test]
    fn socket_client_uses_newline_delimited_json() {
        use std::os::unix::net::UnixListener;

        let path = std::env::temp_dir().join(format!(
            "agent-deck-herdr-test-{}-{}.sock",
            std::process::id(),
            now_for_test()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "session.snapshot");
            assert_eq!(request["params"], serde_json::json!({}));
            reader
                .get_mut()
                .write_all(b"{\"id\":\"agent-deck-companion\",\"result\":{\"type\":\"session_snapshot\",\"snapshot\":{\"agents\":[]}}}\n")
                .unwrap();
        });
        let result = api_request(path.to_str(), "session.snapshot", serde_json::json!({})).unwrap();
        assert_eq!(result["type"], "session_snapshot");
        server.join().unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn event_subscription_batches_a_burst_before_refreshing() {
        use std::os::unix::net::UnixListener;

        let path = std::env::temp_dir().join(format!(
            "agent-deck-herdr-events-{}-{}.sock",
            std::process::id(),
            now_for_test()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "events.subscribe");
            assert!(request["params"]["subscriptions"]
                .as_array()
                .is_some_and(|items| !items.is_empty()));
            reader
                .get_mut()
                .write_all(
                    b"{\"id\":\"agent-deck-companion-events\",\"result\":{\"type\":\"subscribed\"}}\n\
                      {\"event\":\"pane.agent_status_changed\",\"data\":{\"pane_id\":\"w1:p1\",\"agent_status\":\"idle\"}}\n\
                      {\"event\":\"pane.focused\",\"data\":{\"pane_id\":\"w1:p1\"}}\n",
                )
                .unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });
        let mut refreshes = 0;
        assert!(subscribe_events_at(&path, &mut || refreshes += 1).is_err());
        assert_eq!(refreshes, 1);
        server.join().unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn non_native_agent_is_reported_before_focus() {
        use std::os::unix::net::UnixListener;

        let path = PathBuf::from("/tmp").join(format!(
            "adq-{}-{}.sock",
            std::process::id(),
            now_for_test()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let expected = [
                ("session.snapshot", true),
                ("agent.focus", false),
                ("pane.report_agent", true),
                ("agent.focus", true),
                ("pane.release_agent", true),
            ];
            for (method, succeeds) in expected {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], method);
                let response = if succeeds {
                    serde_json::json!({"id":"agent-deck-companion","result":{"type":"ok"}})
                } else {
                    serde_json::json!({"id":"agent-deck-companion","error":{"code":"agent_not_found"}})
                };
                serde_json::to_writer(reader.get_mut(), &response).unwrap();
                reader.get_mut().write_all(b"\n").unwrap();
            }
        });
        assert!(focus_panel(
            path.to_str(),
            "wH:p31",
            "qwen",
            AgentStatus::Idle
        ));
        server.join().unwrap();
        fs::remove_file(path).unwrap();
    }

    fn now_for_test() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
