//! DeepSeek Harness (`dsh`) adapter helpers.
//!
//! `dsh` is a Node.js CLI whose `web` profile serves the browser UI on
//! `127.0.0.1:3080` by default, and whose `headless` profile runs one-shot
//! jobs in a terminal. Unlike the hook-based agents, DSH already persists its
//! own state: the session projection cache
//! (`$DSH_HOME/storages/session_projcache.json`) carries per-session status
//! (open step, pending tool calls, goal phase, title, cwd). This module reads
//! that cache to normalize DSH into the shared status model and resolves the
//! web UI URL for window activation.
//!
//! Additionally, this module calls DSH's authenticated Connection unary RPC
//! for session listing. Browser session selection remains client-side state,
//! so the page-side bridge plugin calls DSH's own client-side session
//! navigation service. K1-K3 need neither mouse simulation nor Chrome
//! scripting permission.

use std::{
    env,
    path::{Path, PathBuf},
};

#[cfg(target_os = "macos")]
use std::process::Command;

use serde::Serialize;
use serde_json::Value;
use sysinfo::System;

use crate::AgentStatus;

/// DSH's `web` profile listens here unless it was started with another `--port`.
const DEFAULT_DSH_PORT: u16 = 3080;

/// Normalized snapshot of the DeepSeek Harness current task.
#[derive(Debug, Clone)]
pub struct DshStatus {
    pub status: AgentStatus,
    pub task: Option<String>,
    pub workspace: Option<String>,
}

/// Per-session info from the DSH projection cache.
#[derive(Debug, Clone)]
pub struct DshSessionInfo {
    pub session_id: String,
    pub title: Option<String>,
    pub status: AgentStatus,
    pub workspace: Option<String>,
    pub last_prompt_at: u64,
}

/// Resolve the DeepSeek Harness home: `$DSH_HOME` when set, otherwise `~/.dsh`.
pub fn home_dir() -> PathBuf {
    if let Ok(home) = env::var("DSH_HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    dirs::home_dir()
        .map(|home| home.join(".dsh"))
        .unwrap_or_else(|| PathBuf::from(".dsh"))
}

fn projection_cache_path() -> PathBuf {
    home_dir().join("storages").join("session_projcache.json")
}

fn argument_is_dsh(argument: &str) -> bool {
    if argument.contains("://") {
        return false;
    }
    let path = Path::new(argument);
    path.file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("dsh"))
        || (path.file_name().is_some_and(|name| name == "bin.js")
            && argument.contains("/@deepseek-ai/dsh/"))
}

/// Command-line arguments of a running DSH web process, if any.
///
/// `dsh` boots as `node …/node_modules/.bin/dsh …`, so the process name is
/// normally `node` and the CLI identity lives in argv.
fn dsh_web_process_args(system: &System) -> Vec<String> {
    for process in system.processes().values() {
        let is_dsh = process.name().to_string_lossy().eq_ignore_ascii_case("dsh")
            || process
                .cmd()
                .iter()
                .any(|argument| argument_is_dsh(&argument.to_string_lossy()));
        if is_dsh {
            let args = process
                .cmd()
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let is_web = args.iter().any(|argument| argument == "web")
                || args
                    .windows(2)
                    .any(|pair| pair[0] == "--profile" && pair[1] == "web");
            if is_web {
                return args;
            }
        }
    }
    Vec::new()
}

/// The `--profile <name>` of the running `web` profile, when it was passed
/// explicitly. Used to decide which DSH profile the companion should offer to
/// install a plugin into.
pub fn web_profile(system: &System) -> Option<String> {
    dsh_web_process_args(system)
        .windows(2)
        .find(|pair| pair[0] == "--profile")
        .map(|pair| pair[1].clone())
}

/// The browser URL of the running `web` profile, honoring `--port` (default
/// `3080`). Returns `None` when the web profile is not running.
pub fn web_url(system: &System) -> Option<String> {
    let args = dsh_web_process_args(system);
    if args.is_empty() {
        return None;
    }
    let port = args
        .windows(2)
        .find(|pair| pair[0] == "--port")
        .and_then(|pair| pair[1].parse::<u16>().ok())
        .unwrap_or(3080);
    Some(format!("http://127.0.0.1:{port}"))
}

/// Switch the focused DSH browser page to Chat and focus its composer.
///
/// Chrome does not always materialize the page's accessibility subtree until
/// System Events performs a hit test. The two short right-to-left probes cover
/// DSH's supported resizable sidebar range and its collapsed rail; once the
/// subtree exists, the actual switch is semantic (`Chat` + `AXPress`). No page
/// JavaScript or Chrome "JavaScript from Apple Events" permission is used.
#[cfg(target_os = "macos")]
const FOCUS_CHAT_COMPOSER_SCRIPT: &str = r#"
on findRole(itemRef, wantedRole, itemDepth)
    tell application "System Events"
        try
            if role of itemRef is wantedRole then return itemRef
        end try
        if itemDepth < 24 then
            repeat with childRef in UI elements of itemRef
                set foundValue to my findRole(contents of childRef, wantedRole, itemDepth + 1)
                if foundValue is not missing value then return foundValue
            end repeat
        end if
    end tell
    return missing value
end findRole

on findChat(itemRef, itemDepth)
    tell application "System Events"
        try
            if role of itemRef is "AXRadioButton" and name of itemRef is "Chat" then return itemRef
        end try
        if itemDepth < 24 then
            repeat with childRef in UI elements of itemRef
                set foundValue to my findChat(contents of childRef, itemDepth + 1)
                if foundValue is not missing value then return foundValue
            end repeat
        end if
    end tell
    return missing value
end findChat

on chromeWebArea(rootWindow)
    tell application "System Events"
        try
            -- Fast path for current Chrome. The role check keeps a Chrome UI
            -- reshuffle from turning this into an unsafe blind reference.
            set candidate to UI element 2 of UI element 1 of UI element 1 of UI element 2 of UI element 1 of UI element 1 of UI element 1 of UI element 1 of rootWindow
            if role of candidate is "AXWebArea" then return candidate
        end try
    end tell
    return my findRole(rootWindow, "AXWebArea", 0)
end chromeWebArea

on chatButton(webArea)
    tell application "System Events"
        try
            set candidate to radio button "Chat" of tab group 1 of group 2 of group 1 of group 1 of webArea
            if role of candidate is "AXRadioButton" then return candidate
        end try
    end tell
    return my findChat(webArea, 0)
end chatButton

on focusChat(processName)
    tell application "System Events"
        tell process processName
            set rootWindow to front window
            if processName is "Google Chrome" then
                set webArea to my chromeWebArea(rootWindow)
            else
                set webArea to my findRole(rootWindow, "AXWebArea", 0)
            end if
            if webArea is missing value then return false

            set chatControl to my chatButton(webArea)
            if chatControl is missing value then
                -- Accessibility bootstrap for DSH's expanded sidebar (264–420 px).
                set {webX, webY} to position of webArea
                repeat with xOffset from 520 to 300 by -20
                    click at {webX + xOffset, webY + 56}
                end repeat
                set chatControl to my chatButton(webArea)
            end if
            if chatControl is missing value then
                -- Responsive/collapsed sidebar rail (56 px).
                repeat with xOffset from 190 to 80 by -10
                    click at {webX + xOffset, webY + 56}
                end repeat
                set chatControl to my chatButton(webArea)
            end if
            if chatControl is not missing value then
                perform action "AXPress" of chatControl
                delay 0.15
            end if

            set {webX, webY} to position of webArea
            set {webWidth, webHeight} to size of webArea
            set hitElement to click at {webX + (webWidth div 2), webY + webHeight - 105}
            try
                if role of hitElement is "AXTextArea" then return true
            end try

            -- Blank/new sessions use a centered hero composer instead of the
            -- bottom dock, so fall back to its semantic text area.
            set composer to my findRole(webArea, "AXTextArea", 0)
            if composer is missing value then return false
            set focused of composer to true
            return true
        end tell
    end tell
end focusChat

tell application "System Events"
    if exists process "Google Chrome" then
        tell process "Google Chrome"
            if frontmost then return my focusChat("Google Chrome")
        end tell
    end if
    if exists process "Safari" then
        tell process "Safari"
            if frontmost then return my focusChat("Safari")
        end tell
    end if
end tell
return false
"#;

#[cfg(target_os = "macos")]
pub fn focus_chat_composer() -> bool {
    Command::new("osascript")
        .args(["-e", FOCUS_CHAT_COMPOSER_SCRIPT])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
}

/// Show the browser tab already holding the DSH page, so raising the browser
/// does not leave an unrelated tab in front. `open -a` only activates the
/// application, which is not enough when DSH shares a window with other tabs.
///
/// AppleScript only accepts Chrome/Safari's tab vocabulary inside a literal
/// `tell application "…"` block, so each browser needs its own script.
#[cfg(target_os = "macos")]
pub fn focus_browser_tab(script: &str, url_prefix: &str) -> bool {
    let outcome = Command::new("osascript")
        .args(["-e", script, url_prefix])
        .output();
    let ok = match &outcome {
        Ok(output) => {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        }
        Err(_) => false,
    };
    if !ok {
        let detail = match outcome {
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                if stderr.is_empty() {
                    "no DSH tab found".to_owned()
                } else {
                    stderr
                }
            }
            Err(error) => error.to_string(),
        };
        eprintln!("[dsh] browser tab raise skipped: {detail}");
    }
    ok
}

#[cfg(target_os = "macos")]
fn tab_focus_script(app_name: &str, select_tab: &str) -> String {
    format!(
        r##"on run argv
    if (count of argv) is 0 then return false
    set wantedPrefix to item 1 of argv
    set alternatePrefix to wantedPrefix
    if wantedPrefix starts with "http://127.0.0.1:" then
        set alternatePrefix to "http://localhost:" & text 18 thru -1 of wantedPrefix
    end if
    set hitWindow to 0
    set hitTab to 0
    tell application "{app_name}"
        if not running then return false
        set windowCount to count of windows
        repeat with outer from 1 to windowCount
            set targetWindow to item outer of windows
            try
                set tabUrls to URL of tabs of targetWindow
            on error
                set tabUrls to {{}}
            end try
            repeat with inner from 1 to (count of tabUrls)
                set tabUrl to item inner of tabUrls
                if tabUrl is wantedPrefix or tabUrl starts with (wantedPrefix & "/") or tabUrl starts with (wantedPrefix & "?") or tabUrl starts with (wantedPrefix & "#") or tabUrl is alternatePrefix or tabUrl starts with (alternatePrefix & "/") or tabUrl starts with (alternatePrefix & "?") or tabUrl starts with (alternatePrefix & "#") then
                    set hitWindow to outer
                    set hitTab to inner
                end if
            end repeat
            if hitWindow > 0 then exit repeat
        end repeat
        if hitWindow is 0 then return false
{select_tab}        activate
        return true
    end tell
end run"##
    )
}

/// Chrome and Safari expose different verbs for "show this tab", and each
/// browser's vocabulary only compiles inside its own literal `tell` block.
/// Chrome's window index also sends its window forward; Safari has no such
/// property and would abort on that assignment.
#[cfg(target_os = "macos")]
pub fn chrome_tab_focus_script() -> String {
    tab_focus_script(
        "Google Chrome",
        concat!(
            "        set active tab index of item hitWindow of windows to hitTab\n",
            "        set index of item hitWindow of windows to 1\n",
        ),
    )
}

#[cfg(target_os = "macos")]
pub fn safari_tab_focus_script() -> String {
    tab_focus_script(
        "Safari",
        concat!(
            "        set current tab of item hitWindow of windows to ",
            "tab hitTab of item hitWindow of windows\n",
        ),
    )
}

/// A DSH session id travels into the bridge request body, so it is held to the
/// shape DSH actually mints and to a bounded length. The plugin re-checks it.
pub(crate) fn is_valid_session_id(session_id: &str) -> bool {
    session_id.starts_with("session-")
        && session_id.len() <= 80
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Lowercase hex request nonce, unique enough to distinguish repeated presses
/// when the bridge echoes it back as an acknowledgement.
fn new_handoff_nonce() -> String {
    format!(
        "{:08x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_micros())
            .unwrap_or(0)
    )
}

/// A `deck/select` response only counts when a tab acknowledged the switch;
/// `no-client`, `client-rejected` and `ack-timeout` mean no page confirmed it.
fn select_reported_acked(body: &Value) -> bool {
    body.get("acked").and_then(Value::as_bool) == Some(true)
}

/// Which browser tabs the deck bridge currently sees through polls or streams.
#[derive(Debug, PartialEq, Eq)]
pub struct DeckTabs {
    pub tabs: usize,
    /// `"chrome"`, `"safari"` or `"other"`, classified by the bridge from each
    /// connection's user agent.
    pub browsers: Vec<String>,
}

/// A `deck/tabs` response. A missing or malformed `tabs` field is treated as no
/// answer at all, because guessing would decide whether a tab gets opened.
fn deck_reported_tabs(body: &Value) -> Option<DeckTabs> {
    let tabs = body.get("tabs").and_then(Value::as_u64)?;
    let browsers = body
        .get("browsers")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(DeckTabs {
        tabs: tabs as usize,
        browsers,
    })
}

/// Port of the DSH web server that the `deck/*` routes are registered on.
fn deck_port(system: &System) -> u16 {
    web_url(system)
        .as_deref()
        .and_then(|url| url.strip_prefix("http://127.0.0.1:"))
        .and_then(|authority| authority.trim_end_matches('/').parse::<u16>().ok())
        .or_else(detect_dsh_port)
        .unwrap_or(DEFAULT_DSH_PORT)
}

/// Drive one async deck request from a synchronous caller. A deck key can also
/// arrive through the loopback HTTP API, whose handlers already run on Tauri's
/// async runtime, where `block_on` panics — so every await gets its own thread.
fn deck_await<T, Fut>(request: impl FnOnce() -> Fut + Send + 'static) -> Result<T, String>
where
    Fut: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || tauri::async_runtime::block_on(request()))
        .join()
        .unwrap_or_else(|_| Err("deck request worker panicked".to_owned()))
}

/// A `deck/tabs` answer plus the reason when there is none, for callers that
/// must report *why* the bridge is unreachable rather than just "no answer".
pub fn bridge_probe(system: &System) -> Result<DeckTabs, String> {
    let mut client = DshApiClient::new(deck_port(system));
    client.init_auth()?;
    deck_await(move || async move { client.deck_tabs().await })
}

/// Ask the deck bridge whether a DSH page is already open.
///
/// `None` means unknown: no DSH server reachable, or a bridge too old to
/// register the deck routes. Callers then have to fall back to the behaviour
/// that predates the bridge.
pub fn deck_tab_report(system: &System) -> Option<DeckTabs> {
    match bridge_probe(system) {
        Ok(report) => {
            eprintln!("[dsh] deck/tabs {report:?}");
            Some(report)
        }
        Err(error) => {
            eprintln!("[dsh] deck/tabs failed: {error}");
            None
        }
    }
}

/// Session list through DSH's authenticated Connection RPC. Unlike the storage
/// read, this proves the credentials and the running server agree.
pub fn api_sessions(system: &System) -> Result<Vec<DshApiSession>, String> {
    let mut client = DshApiClient::new(deck_port(system));
    client.init_auth()?;
    deck_await(move || async move { client.list_sessions().await })
}

/// Whether any DSH process is alive, web or headless. The web URL alone is not
/// enough: a headless run has sessions but no browser page.
fn any_dsh_process(system: &System) -> bool {
    system.processes().values().any(|process| {
        process.name().to_string_lossy().eq_ignore_ascii_case("dsh")
            || process
                .cmd()
                .iter()
                .any(|argument| argument_is_dsh(&argument.to_string_lossy()))
    })
}

/// One-shot picture of the DSH integration for the Settings page.
///
/// Everything here is best-effort: a missing server, credentials or bridge is
/// reported as a field rather than an error, because that is exactly the state
/// the user needs to see.
#[derive(Debug, Clone, Serialize)]
pub struct DshDiagnostics {
    /// DSH home the companion reads (`$DSH_HOME` or `~/.dsh`).
    pub home: String,
    pub credentials: bool,
    pub process_running: bool,
    pub web_url: Option<String>,
    /// Non-blank sessions in DSH's own storage, as the monitor reads them.
    pub storage_sessions: usize,
    /// Sessions answered by the Connection RPC.
    pub api_sessions: Vec<DshApiSession>,
    pub api_error: Option<String>,
    /// Tabs currently connected to the `dsh-deck-bridge` host plugin.
    pub bridge_tabs: Option<usize>,
    pub bridge_browsers: Vec<String>,
    pub bridge_error: Option<String>,
    /// Pushes accepted since launch. Non-zero means the plugin channel is
    /// working and the 5-second poll is only a fallback.
    pub pushes: u64,
    /// Host time of the last accepted push, 0 when none arrived.
    pub last_push_at_ms: u128,
}

pub fn diagnostics(pushes: u64, last_push_at_ms: u128) -> DshDiagnostics {
    let system = System::new_all();
    let home = home_dir();
    let running_url = web_url(&system);
    let mut diagnostics = DshDiagnostics {
        home: home.display().to_string(),
        credentials: home.join(".credentials.yaml").is_file(),
        process_running: any_dsh_process(&system),
        web_url: running_url.clone(),
        storage_sessions: read_sessions().len(),
        api_sessions: Vec::new(),
        api_error: None,
        bridge_tabs: None,
        bridge_browsers: Vec::new(),
        bridge_error: None,
        pushes,
        last_push_at_ms,
    };
    // Without a web server there is nothing to probe; saying so beats a
    // two-second timeout that reports the same thing less clearly.
    if running_url.is_none() {
        let detail = "dsh web is not running".to_owned();
        diagnostics.api_error = Some(detail.clone());
        diagnostics.bridge_error = Some(detail);
        return diagnostics;
    }
    match api_sessions(&system) {
        Ok(sessions) => diagnostics.api_sessions = sessions,
        Err(error) => diagnostics.api_error = Some(error),
    }
    match bridge_probe(&system) {
        Ok(tabs) => {
            diagnostics.bridge_tabs = Some(tabs.tabs);
            diagnostics.bridge_browsers = tabs.browsers;
        }
        Err(error) => diagnostics.bridge_error = Some(error),
    }
    diagnostics
}

/// Ask the running DSH server to make its open tabs display `session_id`.
///
/// This is the `dsh-deck-bridge` HTTP path: the request lands on a route the
/// plugin registers on DSH's own authenticated `/api` channel, the plugin
/// delivers it to every connected tab through a long poll (or the legacy SSE
/// route), and the call returns once a tab acknowledges the switch. It needs
/// no Apple Events, browser scripting, or accessibility permission.
pub fn request_session_selection(system: &System, session_id: &str) -> bool {
    if !is_valid_session_id(session_id) {
        eprintln!("[dsh] http-select rejected session id {session_id:?}");
        return false;
    }
    let nonce = new_handoff_nonce();
    let mut client = DshApiClient::new(deck_port(system));
    if let Err(error) = client.init_auth() {
        eprintln!("[dsh] http-select session={session_id} auth failed: {error}");
        return false;
    }
    let session = session_id.to_string();
    match deck_await(move || async move { client.select_session(&session, &nonce).await }) {
        Ok(acked) => {
            eprintln!("[dsh] http-select session={session_id} acked={acked}");
            acked
        }
        Err(error) => {
            eprintln!("[dsh] http-select session={session_id} failed: {error}");
            false
        }
    }
}

/// The current durable goal behind a `goal` row value.
///
/// Two shapes have to be tolerated. Current DSH writes the projection state
/// `{current: {goal: {...}} | null, seenGoalIds, failure}`; the older
/// aggregated cache nested the goal directly as `{goal: {...}}`. Reading only
/// the older path silently lost every `blocked` goal.
fn current_goal(goal: &Value) -> Option<&Value> {
    goal.get("current")
        .and_then(|current| current.get("goal"))
        .or_else(|| goal.get("goal"))
}

/// Derive a normalized status from the parsed projection cache.
fn derive_from(root: &Value) -> Option<DshStatus> {
    let sessions = root.get("tables")?.get("sessions")?.as_object()?;

    // The current task is the most recently prompted non-blank session.
    let mut best_key: u128 = 0;
    let mut best_session: Option<&Value> = None;
    for session in sessions.values() {
        let Some(rows) = session.get("rows") else {
            continue;
        };
        let Some(meta) = rows.get("sessionListMetadata").and_then(|m| m.get("val")) else {
            continue;
        };
        let blank = meta.get("blank").and_then(Value::as_bool).unwrap_or(false);
        if blank {
            continue;
        }
        let last_prompt = meta
            .get("lastPromptAt")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u128;
        let Some(created) = session
            .get("identity")
            .and_then(|identity| identity.get("createdAt"))
            .and_then(Value::as_u64)
        else {
            continue;
        };
        let key = last_prompt.max(created as u128);
        if best_session.is_none() || key > best_key {
            best_key = key;
            best_session = Some(session);
        }
    }
    let session = best_session?;
    let Some(rows) = session.get("rows") else {
        return None;
    };

    let stats = rows.get("sessionStats").and_then(|stats| stats.get("val"));
    let open_step = stats.and_then(|stats| stats.get("openStep"));
    let pending_calls = stats.and_then(|stats| stats.get("pendingCalls"));
    let goal = rows.get("goal").and_then(|goal| goal.get("val"));
    let title = rows.get("title").and_then(|title| title.get("val"));
    let workspace = session
        .get("identity")
        .and_then(|identity| identity.get("cwd"));

    let _pending_count = pending_calls
        .and_then(Value::as_object)
        .map(|calls| calls.len())
        .unwrap_or(0);
    let goal_phase = goal
        .and_then(current_goal)
        .and_then(|goal| goal.get("phase"))
        .and_then(Value::as_str);

    // "Working" means work is happening right now: pending tool calls, or an
    // open generation step. An armed-but-idle or completed goal is not working,
    // so it falls through to Idle rather than reporting a stale Working.
    let status = if goal_phase == Some("blocked") {
        AgentStatus::NeedsInput
    } else if open_step.is_some_and(|step| step.is_object()) {
        AgentStatus::Working
    } else {
        AgentStatus::Idle
    };

    let task = title
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            goal.and_then(current_goal)
                .and_then(|goal| goal.get("objective"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let workspace = workspace.and_then(Value::as_str).map(str::to_owned);

    Some(DshStatus {
        status,
        task,
        workspace,
    })
}

/// Read the current DSH task status.
///
/// The per-session projection files are the live source: newer DSH builds do
/// not write the aggregated `session_projcache.json` at all, and keying on that
/// file left the whole status/panel poll dead. The aggregate is still read as a
/// fallback for builds that only keep it.
pub fn read_status() -> Option<DshStatus> {
    if let Some(session) = read_sessions().into_iter().next() {
        return Some(DshStatus {
            status: session.status,
            task: session.title,
            workspace: session.workspace,
        });
    }
    let bytes = std::fs::read(projection_cache_path()).ok()?;
    let root: Value = serde_json::from_slice(&bytes).ok()?;
    derive_from(&root)
}

/// Derive status for one session entry in the projection cache.
///
/// Accepts either a session-files `record` or an aggregated-cache session
/// entry: both carry the same `rows` shape.
fn derive_session_status(session: &Value) -> Option<(AgentStatus, Option<String>)> {
    let rows = session.get("rows")?;
    let stats = rows.get("sessionStats").and_then(|s| s.get("val"));
    let open_step = stats.and_then(|s| s.get("openStep"));
    let goal = rows.get("goal").and_then(|g| g.get("val"));
    let title = rows.get("title").and_then(|t| t.get("val"));

    let goal_phase = goal
        .and_then(current_goal)
        .and_then(|g| g.get("phase"))
        .and_then(Value::as_str);

    let status = if goal_phase == Some("blocked") {
        AgentStatus::NeedsInput
    } else if open_step.is_some_and(|step| step.is_object()) {
        AgentStatus::Working
    } else {
        AgentStatus::Idle
    };

    let task = title
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            goal.and_then(current_goal)
                .and_then(|g| g.get("objective"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });

    Some((status, task))
}

/// Read all non-blank DSH sessions from both the projection cache and individual session files.
///
/// DSH uses a two-tier storage:
/// 1. `session_projcache.json` - aggregated cache (may not contain all sessions)
/// 2. `session_projcache/sessions/*.json` - individual session files (contains all sessions)
///
/// Returns sessions sorted by `lastPromptAt` descending (most recent first).
/// Used to populate K1-K3 session display for the DSH agent.
pub fn read_sessions() -> Vec<DshSessionInfo> {
    let mut result: Vec<DshSessionInfo> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    // First, read from individual session files (more complete)
    let sessions_dir = home_dir()
        .join("storages")
        .join("session_projcache")
        .join("sessions");

    if sessions_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json") {
                    // Extract session ID from filename (e.g., "session-xxx.json" -> "session-xxx")
                    let session_id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string());

                    if let Some(session_id) = session_id {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Ok(root) = serde_json::from_str::<Value>(&content) {
                                if let Some(session) = parse_session_file(&root, &session_id) {
                                    if !seen_ids.contains(&session.session_id) {
                                        seen_ids.insert(session.session_id.clone());
                                        result.push(session);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Then, read from the aggregated cache (for any sessions not in individual files)
    if let Ok(bytes) = std::fs::read(projection_cache_path()) {
        if let Ok(root) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(sessions) = root
                .get("tables")
                .and_then(|t| t.get("sessions"))
                .and_then(|s| s.as_object())
            {
                for (session_id, session) in sessions {
                    if seen_ids.contains(session_id) {
                        continue; // Already loaded from individual file
                    }

                    let Some(rows) = session.get("rows") else {
                        continue;
                    };
                    let Some(meta) = rows.get("sessionListMetadata").and_then(|m| m.get("val"))
                    else {
                        continue;
                    };
                    let blank = meta.get("blank").and_then(Value::as_bool).unwrap_or(false);
                    if blank {
                        continue;
                    }

                    let last_prompt_at = meta
                        .get("lastPromptAt")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let created = session
                        .get("identity")
                        .and_then(|i| i.get("createdAt"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let workspace = session
                        .get("identity")
                        .and_then(|i| i.get("cwd"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);

                    let Some((status, title)) = derive_session_status(session) else {
                        continue;
                    };

                    let sort_key = last_prompt_at.max(created);

                    result.push(DshSessionInfo {
                        session_id: session_id.clone(),
                        title,
                        status,
                        workspace,
                        last_prompt_at: sort_key,
                    });
                }
            }
        }
    }

    // Sort by most recent first
    result.sort_by(|a, b| b.last_prompt_at.cmp(&a.last_prompt_at));
    result
}

/// Parse a single session file and extract session info.
fn parse_session_file(root: &Value, session_id: &str) -> Option<DshSessionInfo> {
    let record = root.get("record")?;
    let identity = record.get("identity")?;
    let rows = record.get("rows")?;

    // Check if session is blank (should be filtered out)
    let is_blank = rows
        .get("sessionListMetadata")
        .and_then(|m| m.get("val"))
        .and_then(|m| m.get("blank"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if is_blank {
        return None;
    }

    let title = rows
        .get("title")
        .and_then(|t| t.get("val"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let workspace = identity
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let created = identity
        .get("createdAt")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    // Determine status from session data. `record` and an aggregated-cache
    // session entry share the same `rows` shape, so the two readers must not
    // drift — this used to be a second, differently-broken copy of the goal
    // path.
    let (status, _) = derive_session_status(root.get("record")?)?;

    // Use createdAt as sort key (individual files don't have lastPromptAt in metadata)
    let sort_key = rows
        .get("sessionListMetadata")
        .and_then(|m| m.get("val"))
        .and_then(|m| m.get("lastPromptAt"))
        .and_then(Value::as_u64)
        .unwrap_or(created);

    Some(DshSessionInfo {
        session_id: session_id.to_string(),
        title,
        status,
        workspace,
        last_prompt_at: sort_key,
    })
}

// ============================================================================
// DSH Connection unary RPC client
// ============================================================================

/// DSH session summary returned by the Connection API.
#[derive(Debug, Clone, Serialize)]
pub struct DshApiSession {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
    pub running: bool,
    pub blank: bool,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

/// DSH API client for authenticated unary RPCs on the running DSH instance.
pub struct DshApiClient {
    port: u16,
    auth_cookie: Option<String>,
}

fn parse_api_session(value: &Value) -> Option<DshApiSession> {
    Some(DshApiSession {
        session_id: value.get("sessionId")?.as_str()?.to_owned(),
        updated_at: value.get("updatedAt")?.as_u64()?,
        running: value.get("running")?.as_bool()?,
        blank: value.get("blank")?.as_bool()?,
        cwd: value.get("cwd").and_then(Value::as_str).map(str::to_owned),
        title: value
            .get("projections")
            .and_then(|projections| projections.get("values"))
            .and_then(|values| values.get("title"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

impl DshApiClient {
    /// Create a new API client for the given DSH port.
    pub fn new(port: u16) -> Self {
        Self {
            port,
            auth_cookie: None,
        }
    }

    /// Read DSH credentials and forge an auth cookie.
    pub fn init_auth(&mut self) -> Result<(), String> {
        let credentials_path = home_dir().join(".credentials.yaml");
        let content = std::fs::read_to_string(&credentials_path)
            .map_err(|e| format!("Failed to read credentials: {}", e))?;

        // Parse YAML to extract the secret
        // Format:
        // version: 1
        // records:
        //   client-connection/browser-session:
        //     kind: grant
        //     payload:
        //       version: 1
        //       secret: <base64url-encoded-secret>

        let secret = parse_credential_secret(&content)
            .ok_or_else(|| "Failed to parse DSH credentials".to_string())?;

        // Forge the auth cookie
        self.auth_cookie = Some(forge_auth_cookie(&secret, self.port)?);
        Ok(())
    }

    /// List all sessions through DSH's authenticated Connection unary RPC.
    pub async fn list_sessions(&self) -> Result<Vec<DshApiSession>, String> {
        let cookie = self
            .auth_cookie
            .as_ref()
            .ok_or_else(|| "Auth not initialized. Call init_auth() first.".to_string())?;

        // session/list is a Connection unary RPC. /api/remote.mux is reserved
        // for streaming endpoints such as session/follow and uses a different
        // multiplexed frame protocol.
        let rpc_id = uuid::Uuid::new_v4().to_string();
        let request = serde_json::json!({
            "type": "client-request",
            "rpcId": rpc_id,
            "method": "session/list",
            "payload": { "args": { "_request": {} } }
        });
        let url = format!("http://127.0.0.1:{}/api/session/list", self.port);
        let response = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| format!("Failed to create DSH HTTP client: {e}"))?
            .post(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(
                reqwest::header::ORIGIN,
                format!("http://127.0.0.1:{}", self.port),
            )
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Failed to call DSH session/list: {e}"))?;
        let status = response.status();
        let response: Value = response.json().await.map_err(|e| {
            format!("DSH session/list returned HTTP {status} with invalid JSON: {e}")
        })?;
        if !status.is_success() {
            return Err(format!(
                "DSH session/list returned HTTP {status}: {response}"
            ));
        }
        if response.get("type").and_then(Value::as_str) != Some("server-response")
            || response.get("rpcId").and_then(Value::as_str) != Some(rpc_id.as_str())
        {
            return Err("DSH session/list returned an invalid response envelope".into());
        }
        let result = response
            .get("result")
            .and_then(Value::as_object)
            .ok_or_else(|| "DSH session/list response has no result".to_string())?;
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(format!(
                "DSH session/list failed: {}",
                result.get("error").unwrap_or(&Value::Null)
            ));
        }
        let items = result
            .get("value")
            .and_then(|value| value.get("items"))
            .and_then(|v| v.as_array())
            .ok_or_else(|| "DSH session/list result has no items".to_string())?;

        let mut sessions = Vec::new();
        for item in items {
            if let Some(session) = parse_api_session(item) {
                if !session.blank {
                    sessions.push(session);
                }
            }
        }

        // Sort by updated_at descending
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        Ok(sessions)
    }

    /// Ask the `dsh-deck-bridge` host plugin to switch every open tab to
    /// `session_id`. The plugin holds the request until a tab acknowledges the
    /// page-side switch, so `false` means nobody moved rather than "sent".
    pub async fn select_session(&self, session_id: &str, nonce: &str) -> Result<bool, String> {
        let cookie = self
            .auth_cookie
            .as_ref()
            .ok_or_else(|| "Auth not initialized. Call init_auth() first.".to_string())?;

        let url = format!("http://127.0.0.1:{}/api/deck/select", self.port);
        let response = reqwest::Client::builder()
            // Longer than the plugin's own 5s ack window, so a slow tab is
            // reported by the plugin rather than by a client-side timeout.
            .timeout(std::time::Duration::from_secs(7))
            .build()
            .map_err(|e| format!("Failed to create DSH HTTP client: {e}"))?
            .post(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(
                reqwest::header::ORIGIN,
                format!("http://127.0.0.1:{}", self.port),
            )
            .json(&serde_json::json!({ "sessionId": session_id, "nonce": nonce }))
            .send()
            .await
            .map_err(|e| format!("Failed to call DSH deck/select: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            // 404 is the usual shape when the profile still runs an older
            // bridge that registered no deck routes.
            return Err(format!("DSH deck/select returned HTTP {status}"));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("DSH deck/select returned invalid JSON: {e}"))?;
        let acked = select_reported_acked(&body);
        if !acked {
            eprintln!(
                "[dsh] deck/select unacked: reason={} tabs={} rejected={} detail={}",
                body.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                body.get("tabs").and_then(Value::as_u64).unwrap_or(0),
                body.get("rejected").and_then(Value::as_u64).unwrap_or(0),
                body.get("detail").and_then(Value::as_str).unwrap_or("none")
            );
        }
        Ok(acked)
    }

    /// Count the tabs connected to the deck bridge, so a synchronous caller can
    /// tell "a DSH page is open somewhere" from "nothing to raise".
    pub async fn deck_tabs(&self) -> Result<DeckTabs, String> {
        let cookie = self
            .auth_cookie
            .as_ref()
            .ok_or_else(|| "Auth not initialized. Call init_auth() first.".to_string())?;

        let url = format!("http://127.0.0.1:{}/api/deck/tabs", self.port);
        let response = reqwest::Client::builder()
            // This route only reads a map, so it answers immediately; a short
            // timeout keeps it off the critical path when DSH is gone.
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .map_err(|e| format!("Failed to create DSH HTTP client: {e}"))?
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(
                reqwest::header::ORIGIN,
                format!("http://127.0.0.1:{}", self.port),
            )
            .send()
            .await
            .map_err(|e| format!("Failed to call DSH deck/tabs: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("DSH deck/tabs returned HTTP {status}"));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("DSH deck/tabs returned invalid JSON: {e}"))?;
        deck_reported_tabs(&body).ok_or_else(|| "DSH deck/tabs returned no tab count".to_owned())
    }
}

/// Parse the credential secret from DSH's credentials.yaml content.
fn parse_credential_secret(content: &str) -> Option<String> {
    // Simple YAML parsing for the specific structure we expect
    let mut in_browser_session = false;
    let mut in_payload = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("client-connection/browser-session:") {
            in_browser_session = true;
            continue;
        }

        if in_browser_session {
            if trimmed.starts_with("payload:") {
                in_payload = true;
                continue;
            }

            if in_payload && trimmed.starts_with("secret:") {
                return trimmed.strip_prefix("secret:")?.trim().to_string().into();
            }

            // If we hit another top-level key, stop
            if !line.starts_with(' ') && !line.starts_with('\t') && trimmed.contains(':') {
                break;
            }
        }
    }

    None
}

/// Forge a DSH auth cookie using the HMAC-SHA256 signing secret.
fn forge_auth_cookie(secret_base64url: &str, port: u16) -> Result<String, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("Failed to get time: {e}"))?
        .as_millis();
    let now_ms = u64::try_from(now_ms).map_err(|_| "Current time exceeds u64".to_string())?;
    forge_auth_cookie_at(secret_base64url, port, now_ms)
}

fn forge_auth_cookie_at(
    secret_base64url: &str,
    port: u16,
    issued_at_ms: u64,
) -> Result<String, String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    // Decode the secret
    let secret = URL_SAFE_NO_PAD
        .decode(secret_base64url)
        .map_err(|e| format!("Failed to decode secret: {}", e))?;

    // Create cookie payload
    let authority = format!("127.0.0.1:{}", port);
    let payload = serde_json::json!({
        "version": 1,
        "authority": authority,
        "issuedAt": issued_at_ms,
        // The Host accepts cookies no longer-lived than its configured maximum.
        // Five minutes is sufficient for a local RPC and valid for every allowed
        // Host configuration (whose minimum is one day).
        "expiresAt": issued_at_ms + 5 * 60 * 1000,
    });

    let payload_json = serde_json::to_string(&payload)
        .map_err(|e| format!("Failed to serialize payload: {}", e))?;

    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());

    // Sign the payload
    let mut mac =
        HmacSha256::new_from_slice(&secret).map_err(|e| format!("Failed to create HMAC: {}", e))?;
    mac.update(payload_b64.as_bytes());
    let signature = mac.finalize().into_bytes();
    let signature_b64 = URL_SAFE_NO_PAD.encode(signature);

    // Format: v1.<payload>.<signature>
    let cookie_value = format!("v1.{}.{}", payload_b64, signature_b64);

    // Cookie name and payload keys mirror client-connection/browser-auth.
    use sha2::{Digest, Sha256 as Sha256Hash};
    let mut hasher = Sha256Hash::new();
    hasher.update(authority.as_bytes());
    let hash = URL_SAFE_NO_PAD.encode(hasher.finalize());
    let cookie_name = format!("dsh-auth-{}", hash);

    Ok(format!("{}={}", cookie_name, cookie_value))
}

/// Detect the DSH web UI port from the running process.
fn detect_dsh_port() -> Option<u16> {
    let system = System::new_all();
    for process in system.processes().values() {
        let cmd: Vec<String> = process
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        let cmd_str = cmd.join(" ");

        if cmd_str.contains("dsh") && cmd_str.contains("web") {
            // Look for --port flag
            for (i, arg) in cmd.iter().enumerate() {
                if arg == "--port" && i + 1 < cmd.len() {
                    if let Ok(port) = cmd[i + 1].parse::<u16>() {
                        return Some(port);
                    }
                }
            }
            // Default port
            return Some(3080);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(
        title: Value,
        open_step: Value,
        pending_calls: Value,
        goal: Value,
        created: u64,
        last_prompt: u64,
    ) -> Value {
        json!({
            "identity": { "createdAt": created, "cwd": "/work/demo" },
            "rows": {
                "sessionStats": { "val": { "openStep": open_step, "pendingCalls": pending_calls } },
                "title": { "val": title },
                "goal": { "val": goal },
                "sessionListMetadata": { "val": { "blank": false, "lastPromptAt": last_prompt } }
            }
        })
    }

    fn cache_with(sessions: Value) -> Value {
        json!({ "tables": { "sessions": sessions } })
    }

    #[test]
    fn detects_cli_shim_and_direct_package_entrypoint() {
        assert!(argument_is_dsh("/usr/local/bin/dsh"));
        assert!(argument_is_dsh(
            "/tmp/node_modules/@deepseek-ai/dsh/lib/bin.js"
        ));
        assert!(!argument_is_dsh("/work/project/doc/dsh.md"));
        assert!(!argument_is_dsh("http://example.test/dsh"));
    }

    #[test]
    fn open_step_without_tools_reads_as_thinking() {
        let root = cache_with(json!({
            "a": session(
                json!("implement retry"),
                json!({ "turn": 1, "step": 2, "startTime": 1, "firstTokenTime": 2 }),
                json!({}),
                Value::Null,
                10,
                10
            )
        }));
        let status = derive_from(&root).unwrap();
        assert_eq!(status.status, AgentStatus::Working);
        assert_eq!(status.task.as_deref(), Some("implement retry"));
        assert_eq!(status.workspace.as_deref(), Some("/work/demo"));
    }

    #[test]
    fn pending_tool_calls_read_as_working() {
        let root = cache_with(json!({
            "a": session(
                Value::Null,
                json!({ "turn": 1, "step": 2, "startTime": 1 }),
                json!({ "bash-1": {} }),
                Value::Null,
                10,
                10
            )
        }));
        assert_eq!(derive_from(&root).unwrap().status, AgentStatus::Working);
    }

    #[test]
    fn blocked_goal_reads_as_needs_input() {
        let root = cache_with(json!({
            "a": session(
                Value::Null,
                Value::Null,
                json!({}),
                json!({ "goal": { "objective": "ship it", "phase": "blocked" } }),
                10,
                10
            )
        }));
        let status = derive_from(&root).unwrap();
        assert_eq!(status.status, AgentStatus::NeedsInput);
        assert_eq!(status.task.as_deref(), Some("ship it"));
    }

    /// Current DSH writes the goal projection state
    /// `{current: {goal: {...}} | null, seenGoalIds, failure}`. Reading only the
    /// older nested shape lost every blocked goal, so both are asserted here.
    #[test]
    fn the_current_goal_projection_shape_is_read_too() {
        let root = cache_with(json!({
            "a": session(
                Value::Null,
                Value::Null,
                json!({}),
                json!({
                    "current": {
                        "goal": {
                            "id": "goal-1",
                            "revision": 1,
                            "objective": "ship it",
                            "phase": "blocked",
                            "maxGoalRounds": 3
                        },
                        "roundsStarted": 0,
                        "createdAt": 1,
                        "updatedAt": 2
                    },
                    "seenGoalIds": ["goal-1"],
                    "failure": Value::Null
                }),
                10,
                10
            )
        }));
        let status = derive_from(&root).unwrap();
        assert_eq!(status.status, AgentStatus::NeedsInput);
        assert_eq!(status.task.as_deref(), Some("ship it"));

        // A cleared goal (`current: null`) must not be mistaken for a blocked one.
        let root = cache_with(json!({
            "a": session(
                Value::Null,
                Value::Null,
                json!({}),
                json!({ "current": Value::Null, "seenGoalIds": [], "failure": Value::Null }),
                10,
                10
            )
        }));
        assert_eq!(derive_from(&root).unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn idle_session_reads_as_idle_and_prefers_most_recent() {
        let root = cache_with(json!({
            "old": session(json!("old task"), Value::Null, json!({}), Value::Null, 10, 10),
            "new": session(json!("new task"), Value::Null, json!({}), Value::Null, 20, 20)
        }));
        let status = derive_from(&root).unwrap();
        assert_eq!(status.status, AgentStatus::Idle);
        assert_eq!(status.task.as_deref(), Some("new task"));
    }

    #[test]
    fn completed_or_armed_goal_without_open_step_reads_as_idle() {
        // A completed goal and an armed-but-idle goal must both read as Idle:
        // no open step means no work is happening right now.
        for phase in ["active", "complete", "paused"] {
            let root = cache_with(json!({
                "a": session(
                    Value::Null,
                    Value::Null,
                    json!({}),
                    json!({ "goal": { "objective": "ship it", "phase": phase } }),
                    10,
                    10
                )
            }));
            assert_eq!(
                derive_from(&root).unwrap().status,
                AgentStatus::Idle,
                "goal phase {phase}"
            );
        }
    }

    #[test]
    fn blank_sessions_are_ignored() {
        let root = cache_with(json!({
            "blank": {
                "identity": { "createdAt": 99, "cwd": "/work/demo" },
                "rows": {
                    "sessionStats": { "val": { "openStep": Value::Null, "pendingCalls": json!({}) } },
                    "title": { "val": Value::Null },
                    "goal": { "val": Value::Null },
                    "sessionListMetadata": { "val": { "blank": true, "lastPromptAt": Value::Null } }
                }
            }
        }));
        assert!(derive_from(&root).is_none());
    }

    #[test]
    fn empty_or_invalid_cache_is_tolerated() {
        assert!(derive_from(&json!({})).is_none());
        assert!(derive_from(&json!({ "tables": {} })).is_none());
    }

    #[test]
    fn session_ids_are_held_to_the_shape_dsh_mints() {
        assert!(is_valid_session_id(
            "session-a43b534e-2c67-48d7-9d3a-523b17079bf0"
        ));
        assert!(is_valid_session_id("session-"));
        // The bridge re-checks the same bound, so the two must not drift.
        assert!(is_valid_session_id(&format!("session-{}", "a".repeat(72))));
        assert!(!is_valid_session_id(&format!("session-{}", "a".repeat(73))));
        assert!(!is_valid_session_id("a43b534e-2c67-48d7-9d3a-523b17079bf0"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("session-../../etc/passwd"));
        assert!(!is_valid_session_id("session- spaces"));
    }

    /// AppleScript's tab vocabulary only compiles inside a literal
    /// `tell application "…"` block, so the generated scripts are handed to
    /// `osascript` for a real parse. Without a needle the handler returns
    /// before its `tell` block, so no browser is read, raised or launched.
    #[cfg(target_os = "macos")]
    #[test]
    fn tab_focus_scripts_compile_as_applescript() {
        for script in [chrome_tab_focus_script(), safari_tab_focus_script()] {
            let parsed = std::process::Command::new("osascript")
                .arg("-e")
                .arg(&script)
                .output()
                .expect("osascript");
            assert!(
                parsed.status.success(),
                "script rejected: {}",
                String::from_utf8_lossy(&parsed.stderr)
            );
        }
    }

    #[test]
    fn handoff_nonces_stay_lowercase_hex() {
        let nonce = new_handoff_nonce();
        assert_eq!(nonce.len(), 8);
        assert!(nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    }

    #[test]
    fn deck_select_only_succeeds_when_a_tab_acknowledged() {
        assert!(select_reported_acked(
            &json!({ "ok": true, "acked": true, "tabs": 1 })
        ));
        for unanswered in [
            // No tab is listening on /api/deck/stream.
            json!({ "ok": false, "tabs": 0, "acked": false, "reason": "no-stream" }),
            // A tab is connected but never acked the page-side switch.
            json!({ "ok": false, "tabs": 1, "acked": false, "reason": "ack-timeout" }),
            json!({}),
            json!({ "acked": "true" }),
        ] {
            assert!(!select_reported_acked(&unanswered), "{unanswered}");
        }
    }

    #[test]
    fn deck_tabs_are_read_only_when_the_bridge_counts_them() {
        assert_eq!(
            deck_reported_tabs(&json!({ "ok": true, "tabs": 2, "browsers": ["chrome"] })),
            Some(DeckTabs {
                tabs: 2,
                browsers: vec!["chrome".to_owned()]
            })
        );
        // A bridge that never reports a browser still answers the only question
        // the caller needs: is a page open?
        assert_eq!(
            deck_reported_tabs(&json!({ "tabs": 0 })),
            Some(DeckTabs {
                tabs: 0,
                browsers: vec![]
            })
        );
        for unusable in [
            json!({}),
            json!({ "tabs": null }),
            json!({ "tabs": "1" }),
            json!({ "tabs": -1 }),
        ] {
            assert_eq!(deck_reported_tabs(&unusable), None, "{unusable}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn chat_focus_uses_accessibility_instead_of_page_javascript() {
        let script = FOCUS_CHAT_COMPOSER_SCRIPT.to_ascii_lowercase();
        assert!(!script.contains("execute javascript"));
        assert!(script.contains("axpress"));
        assert!(script.contains("axtextarea"));
    }

    #[test]
    fn api_session_reads_title_from_projection_hints() {
        let session = parse_api_session(&json!({
            "sessionId": "session-1",
            "updatedAt": 123,
            "running": true,
            "blank": false,
            "cwd": "/work/demo",
            "projections": { "values": { "title": "Demo Session" } }
        }))
        .expect("valid session");
        assert_eq!(session.session_id, "session-1");
        assert_eq!(session.title.as_deref(), Some("Demo Session"));
    }

    #[test]
    fn forged_cookie_matches_dsh_browser_auth_shape() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

        let secret = URL_SAFE_NO_PAD.encode([7_u8; 32]);
        let cookie = forge_auth_cookie_at(&secret, 3080, 1_000).expect("cookie");
        let (name, value) = cookie.split_once('=').expect("cookie pair");
        assert!(name.starts_with("dsh-auth-"));
        let parts = value.split('.').collect::<Vec<_>>();
        assert_eq!(parts[0], "v1");
        let payload: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("payload base64"))
                .expect("payload json");
        assert_eq!(payload["version"], 1);
        assert_eq!(payload["authority"], "127.0.0.1:3080");
        assert_eq!(payload["issuedAt"], 1_000);
        assert_eq!(payload["expiresAt"], 301_000);
        assert!(payload.get("v").is_none());
        assert!(payload.get("iat").is_none());
    }

    #[test]
    fn read_sessions_returns_sorted_non_blank_sessions() {
        let root = cache_with(json!({
            "session-aaa": session(
                json!("first task"),
                Value::Null,
                json!({}),
                Value::Null,
                100,
                100
            ),
            "session-bbb": session(
                json!("second task"),
                json!({ "turn": 1, "step": 1, "startTime": 1 }),
                json!({}),
                Value::Null,
                200,
                200
            ),
            "session-ccc": session(
                json!("third task"),
                Value::Null,
                json!({}),
                Value::Null,
                150,
                150
            ),
            "session-blank": {
                "identity": { "createdAt": 999, "cwd": "/work/demo" },
                "rows": {
                    "sessionStats": { "val": { "openStep": Value::Null, "pendingCalls": json!({}) } },
                    "title": { "val": Value::Null },
                    "goal": { "val": Value::Null },
                    "sessionListMetadata": { "val": { "blank": true, "lastPromptAt": Value::Null } }
                }
            }
        }));
        // Write to a temp file and test the parsing logic directly.
        let sessions = {
            let tables = root.get("tables").unwrap();
            let sessions_map = tables.get("sessions").unwrap().as_object().unwrap();
            let mut result: Vec<DshSessionInfo> = Vec::new();
            for (session_id, session) in sessions_map {
                let rows = session.get("rows").unwrap();
                let meta = rows
                    .get("sessionListMetadata")
                    .and_then(|m| m.get("val"))
                    .unwrap();
                let blank = meta.get("blank").and_then(Value::as_bool).unwrap_or(false);
                if blank {
                    continue;
                }
                let last_prompt_at = meta
                    .get("lastPromptAt")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let (status, title) = derive_session_status(session).unwrap();
                result.push(DshSessionInfo {
                    session_id: session_id.clone(),
                    title,
                    status,
                    workspace: Some("/work/demo".to_string()),
                    last_prompt_at,
                });
            }
            result.sort_by(|a, b| b.last_prompt_at.cmp(&a.last_prompt_at));
            result
        };

        assert_eq!(sessions.len(), 3, "blank session excluded");
        assert_eq!(sessions[0].session_id, "session-bbb");
        assert_eq!(sessions[0].title.as_deref(), Some("second task"));
        assert_eq!(sessions[0].status, AgentStatus::Working);
        assert_eq!(sessions[1].session_id, "session-ccc");
        assert_eq!(sessions[2].session_id, "session-aaa");
    }

    #[test]
    fn parse_session_file_extracts_title_from_record() {
        // Test that parse_session_file correctly extracts data from individual session files
        let session_file = json!({
            "version": 7,
            "record": {
                "identity": {
                    "formatVersion": 3,
                    "createdAt": 1789547716094_i64,
                    "cwd": "/Users/test/agent_deck_companion",
                    "isSeeded": false,
                    "inheritedEventCount": 0
                },
                "rows": {
                    "title": {
                        "ver": 1,
                        "seq": 26,
                        "val": "demo"
                    },
                    "sessionStats": {
                        "val": {
                            "openStep": Value::Null,
                            "pendingCalls": json!({})
                        }
                    },
                    "goal": {
                        "val": Value::Null
                    },
                    "sessionListMetadata": {
                        "val": {
                            "blank": false,
                            "lastPromptAt": 1789547800000_i64
                        }
                    }
                }
            }
        });

        let session = parse_session_file(
            &session_file,
            "session-8724ab6b-7a55-44fa-a813-753cd913bcef",
        )
        .unwrap();
        assert_eq!(
            session.session_id,
            "session-8724ab6b-7a55-44fa-a813-753cd913bcef"
        );
        assert_eq!(session.title.as_deref(), Some("demo"));
        assert_eq!(
            session.workspace.as_deref(),
            Some("/Users/test/agent_deck_companion")
        );
        assert_eq!(session.status, AgentStatus::Idle);
        assert_eq!(session.last_prompt_at, 1789547800000);
    }

    /// Smoke test over whatever DSH data this machine happens to have. It must
    /// not hard-code one developer's session titles: the assertion is that real
    /// files parse into usable rows, and it is skipped when DSH never ran.
    #[test]
    fn read_sessions_reads_whatever_real_files_exist() {
        let sessions_dir = home_dir()
            .join("storages")
            .join("session_projcache")
            .join("sessions");
        if !sessions_dir.exists() {
            eprintln!("skipping: {} does not exist", sessions_dir.display());
            return;
        }

        let sessions = read_sessions();
        for (index, session) in sessions.iter().enumerate() {
            eprintln!(
                "Session {index}: id={}, title={:?}",
                session.session_id, session.title
            );
        }
        for session in &sessions {
            assert!(
                session.session_id.starts_with("session-"),
                "an unparsable row leaked out: {session:?}"
            );
        }
        // Every parsed row must be non-blank and carry a recency key, which is
        // what the three-slot projection orders on.
        assert!(
            sessions.iter().all(|session| session.last_prompt_at > 0),
            "rows without a sort key would sort arbitrarily"
        );
    }
}
