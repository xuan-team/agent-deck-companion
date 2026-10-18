//! Authoritative ChatGPT/Codex quota and thread list, read from the desktop
//! app-server.
//!
//! The locally persisted `rate_limits` snapshots only record the moment Codex
//! last reported them, and the 5-hour window is anchored server-side at the
//! first request inside it, which never reaches disk. Reading
//! `account/rateLimits/read` from the app server is the only way to get the
//! same numbers the desktop usage panel shows.
//!
//! `thread/list` answers the same call for session identity: its `name` is the
//! title the desktop actually shows, whereas the disk fallback has to rebuild a
//! title from a stored preview.

use crate::chatgpt_state::LocalSession;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const QUOTA_INTERVAL: Duration = Duration::from_secs(30);
const SESSION_INTERVAL: Duration = Duration::from_secs(5);
const SESSION_LIMIT: usize = crate::state::DESKTOP_SESSION_LIMIT;
const SNAPSHOT_TTL_MS: u128 = 120_000;
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const FIVE_HOURS_MINUTES: u64 = 300;
const WEEKLY_MINUTES: u64 = 10_080;

#[derive(Debug, Clone, Copy)]
pub struct QuotaWindow {
    pub remaining_percent: u8,
    pub resets_at_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    pub fetched_at_ms: u128,
    pub five_h: Option<QuotaWindow>,
    pub weekly: Option<QuotaWindow>,
}

impl QuotaSnapshot {
    /// Live values stop being trustworthy once their window has rolled over or
    /// the app server stopped answering; the caller then falls back to disk.
    pub fn usable(&self, now_ms: u128) -> bool {
        now_ms.saturating_sub(self.fetched_at_ms) < SNAPSHOT_TTL_MS
            && self
                .five_h
                .as_ref()
                .and_then(|window| window.resets_at_ms)
                .is_none_or(|reset| reset > now_ms as u64)
    }
}

/// One window of the app server's `rateLimits` report. Durations are camelCase
/// here, unlike the snake_case rollout records handled by `chatgpt_state`.
fn parse_window(limit: &Value) -> Option<(u64, QuotaWindow)> {
    let duration = limit.get("windowDurationMins")?.as_u64()?;
    let used = limit.get("usedPercent")?.as_f64()?;
    Some((
        duration,
        QuotaWindow {
            remaining_percent: (100.0 - used).clamp(0.0, 100.0).round() as u8,
            resets_at_ms: limit
                .get("resetsAt")
                .and_then(|value| value.as_u64())
                .map(|seconds| seconds * 1_000),
        },
    ))
}

/// Classifies windows by duration rather than by the `primary`/`secondary`
/// slot, which changes meaning between plans. The shortest window is the
/// session limit, the longest is the weekly one.
pub fn parse_rate_limits(result: &Value, now_ms: u128) -> Option<QuotaSnapshot> {
    let limits = result.get("rateLimits")?;
    let mut found = [
        limits.get("primary").and_then(parse_window),
        limits.get("secondary").and_then(parse_window),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if found.is_empty() {
        return None;
    }
    found.sort_by_key(|(duration, _)| *duration);
    let five_index = found
        .iter()
        .position(|(duration, _)| *duration == FIVE_HOURS_MINUTES)
        .unwrap_or(0);
    let weekly_index = found
        .iter()
        .position(|(duration, _)| *duration == WEEKLY_MINUTES)
        .filter(|index| *index != five_index);
    Some(QuotaSnapshot {
        fetched_at_ms: now_ms,
        five_h: found.get(five_index).map(|(_, window)| window.clone()),
        weekly: weekly_index
            .and_then(|index| found.get(index))
            .map(|(_, window)| window.clone()),
    })
}

/// A page of the app server's thread list, reduced to what a deck panel needs.
#[derive(Debug, Clone)]
pub struct ThreadSnapshot {
    pub fetched_at_ms: u128,
    pub sessions: Vec<LocalSession>,
}

impl ThreadSnapshot {
    pub fn usable(&self, now_ms: u128) -> bool {
        now_ms.saturating_sub(self.fetched_at_ms) < SNAPSHOT_TTL_MS
    }
}

/// `thread/list` has exposed both snake_case and camelCase, and both seconds
/// and milliseconds, across desktop builds. Normalize to epoch milliseconds and
/// let an unreadable field mean "no timestamp" rather than a dropped thread.
fn row_time_ms(row: &Value, keys: &[&str]) -> Option<u128> {
    let value = keys
        .iter()
        .filter_map(|key| row.get(*key))
        .find_map(|value| match value {
            Value::Number(number) => number.as_u64().map(u128::from),
            Value::String(text) => text.trim().parse::<u128>().ok(),
            _ => None,
        })?;
    // Anything below ~1e11 ms is 1973-1976, so it is a seconds stamp instead.
    Some(if value < 100_000_000_000 {
        value * 1000
    } else {
        value
    })
}

/// `name` is the title the desktop shows; `preview` is the stored first
/// message, used only when a thread was never titled.
fn thread_session(row: &Value) -> Option<LocalSession> {
    let id = row.get("id")?.as_str()?.to_owned();
    let model = row.get("model").and_then(Value::as_str).unwrap_or_default();
    if !model.starts_with("gpt-") {
        return None;
    }
    let title = ["name", "preview"]
        .into_iter()
        .filter_map(|key| row.get(key).and_then(Value::as_str))
        .find(|text| !text.trim().is_empty())?
        .to_owned();
    Some(LocalSession {
        id,
        title,
        cwd: row
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        last_activity_ms: row_time_ms(
            row,
            &["updated_at_ms", "updatedAtMs", "updated_at", "updatedAt"],
        ),
    })
}

pub fn parse_threads(result: &Value, now_ms: u128, limit: usize) -> ThreadSnapshot {
    let sessions = result
        .get("data")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(thread_session).take(limit).collect())
        .unwrap_or_default();
    ThreadSnapshot {
        fetched_at_ms: now_ms,
        sessions,
    }
}

fn codex_binary_candidates(override_path: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut candidates = override_path.into_iter().collect::<Vec<_>>();
    let mut app_roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = home.as_ref() {
        app_roots.push(home.join("Applications"));
    }
    for root in app_roots {
        for app in ["ChatGPT.app", "Codex.app"] {
            let resources = root.join(app).join("Contents/Resources");
            // Current desktop layout: use the bundled executable directly,
            // without depending on PATH or the shell launcher in codex-cli/bin.
            candidates.push(resources.join("codex-cli/CodexCLI.app/Contents/MacOS/codex"));
            candidates.push(resources.join("codex-cli/bin/codex"));
            candidates.push(resources.join("codex"));
        }
    }
    if let Some(home) = home {
        candidates.push(home.join(".local/bin/codex"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
    candidates.push(PathBuf::from("/usr/local/bin/codex"));
    candidates
}

fn resolve_codex_binary() -> Option<PathBuf> {
    codex_binary_candidates(
        std::env::var_os("CODEX_BIN").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
    .into_iter()
    .find(|path| path.is_file())
}

/// Owned child plus the id-correlated line reader for its stdio. Dropping this
/// kills `codex app-server`, so a deactivating agent never leaks a process.
struct Server {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start() -> Result<Self, String> {
        let binary = resolve_codex_binary().ok_or_else(|| "codex binary not found".to_string())?;
        eprintln!("[chatgpt_quota] using {}", binary.display());
        // `mcp_servers={}` keeps the desktop MCP configuration from starting
        // servers we would never talk to.
        let mut child = Command::new(&binary)
            .args(["app-server", "-c", "mcp_servers={}"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("spawn {}: {error}", binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "app-server stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "app-server stdout unavailable".to_string())?;
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => return,
                    Ok(_) => {
                        if sender.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        let mut server = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        server.handshake()?;
        Ok(server)
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        writeln!(self.stdin, "{message}")
            .and_then(|()| self.stdin.flush())
            .map_err(|error| error.to_string())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let line = match self.lines.recv_timeout(REQUEST_TIMEOUT) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => return Err(format!("{method} timed out")),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("app-server stdout closed".to_string())
                }
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("method").is_some() {
                // Server-initiated traffic; refuse requests so the server does
                // not wait on a reply it will never get.
                if let Some(request_id) = message.get("id") {
                    let _ = self.send(&json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": { "code": -32601, "message": "unsupported" }
                    }));
                }
                continue;
            }
            if message.get("id").and_then(|value| value.as_u64()) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!("{method}: {error}"));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| format!("{method} response missing result"));
            }
        }
    }

    fn handshake(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({ "clientInfo": { "name": "agent-deck-companion", "version": "1.0.0" } }),
        )?;
        self.send(&json!({ "jsonrpc": "2.0", "method": "initialized" }))
    }
}

pub fn spawn_worker(
    quota: Arc<Mutex<Option<QuotaSnapshot>>>,
    sessions: Arc<Mutex<Option<ThreadSnapshot>>>,
    wanted: Arc<AtomicBool>,
) {
    thread::spawn(move || worker(quota, sessions, wanted));
}

fn refresh(
    server: &mut Server,
    due_sessions: bool,
    due_quota: bool,
    quota: &Arc<Mutex<Option<QuotaSnapshot>>>,
    sessions: &Arc<Mutex<Option<ThreadSnapshot>>>,
) -> Result<(), String> {
    if due_sessions {
        let result = server.request(
            "thread/list",
            json!({
                "limit": SESSION_LIMIT,
                "sortKey": "recency_at",
                "archived": false,
                // Skip the JSONL rollout sweep the default path runs to repair
                // thread metadata; the deck only reads what the state DB holds.
                "useStateDbOnly": true,
            }),
        )?;
        let snapshot = parse_threads(&result, crate::state::now_ms(), SESSION_LIMIT);
        *sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot);
    }
    if due_quota {
        let result = server.request("account/rateLimits/read", json!({}))?;
        let snapshot = parse_rate_limits(&result, crate::state::now_ms())
            .ok_or_else(|| "rateLimits response had no windows".to_string())?;
        *quota
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot);
    }
    Ok(())
}

fn worker(
    quota: Arc<Mutex<Option<QuotaSnapshot>>>,
    sessions: Arc<Mutex<Option<ThreadSnapshot>>>,
    wanted: Arc<AtomicBool>,
) {
    let mut server: Option<Server> = None;
    let mut backoff = MIN_BACKOFF;
    let mut next_quota = Instant::now();
    let mut next_sessions = Instant::now();
    loop {
        if !wanted.load(Ordering::Relaxed) {
            // Releasing the ChatGPT agent must not leave a child process behind.
            drop(server.take());
            backoff = MIN_BACKOFF;
            next_quota = Instant::now();
            next_sessions = Instant::now();
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        if server.is_none() {
            match Server::start() {
                Ok(connected) => {
                    server = Some(connected);
                    backoff = MIN_BACKOFF;
                }
                Err(error) => {
                    eprintln!("ChatGPT quota unavailable: {error}");
                    let wait = backoff;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    thread::sleep(wait);
                    continue;
                }
            }
        }
        let started = Instant::now();
        let due_sessions = started >= next_sessions;
        let due_quota = started >= next_quota;
        let outcome = refresh(
            server.as_mut().expect("server started above"),
            due_sessions,
            due_quota,
            &quota,
            &sessions,
        );
        if let Err(error) = outcome {
            eprintln!("ChatGPT app-server read dropped: {error}");
            drop(server.take());
            let wait = backoff;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            thread::sleep(wait);
            continue;
        }
        backoff = MIN_BACKOFF;
        let now = Instant::now();
        if due_sessions {
            next_sessions = now + SESSION_INTERVAL;
        }
        if due_quota {
            next_quota = now + QUOTA_INTERVAL;
        }
        thread::sleep(
            next_sessions
                .min(next_quota)
                .saturating_duration_since(now)
                .max(Duration::from_millis(200)),
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolves_current_desktop_layout_without_home_and_keeps_override_first() {
        let override_path = std::path::PathBuf::from("/custom/codex");
        let candidates = super::codex_binary_candidates(Some(override_path.clone()), None);
        assert_eq!(candidates.first(), Some(&override_path));
        assert_eq!(candidates[1], std::path::PathBuf::from(
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex"
        ));
        assert!(candidates.contains(&std::path::PathBuf::from(
            "/Applications/ChatGPT.app/Contents/Resources/codex"
        )));
        let home = std::path::PathBuf::from("/test/home");
        let candidates = super::codex_binary_candidates(None, Some(home));
        assert!(candidates.contains(&std::path::PathBuf::from(
            "/test/home/Applications/Codex.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex"
        )));
        assert!(candidates.contains(&std::path::PathBuf::from("/test/home/.local/bin/codex")));
    }

    use super::*;

    fn fixture() -> Value {
        json!({
          "ordinaryUsageAllowed": true,
          "rateLimits": {
            "limitId": "codex",
            "primary": { "usedPercent": 0, "windowDurationMins": 300, "resetsAt": 1790062934 },
            "secondary": { "usedPercent": 5, "windowDurationMins": 10080, "resetsAt": 1790338308 },
            "credits": { "hasCredits": false, "unlimited": false, "balance": "0" },
            "planType": "plus"
          }
        })
    }

    #[test]
    fn reads_both_windows_from_the_live_report() {
        let snapshot = parse_rate_limits(&fixture(), 1_790_055_000_000).unwrap();
        let five = snapshot.five_h.unwrap();
        assert_eq!(five.remaining_percent, 100);
        assert_eq!(five.resets_at_ms, Some(1_790_062_934_000));
        let weekly = snapshot.weekly.unwrap();
        assert_eq!(weekly.remaining_percent, 95);
        assert!(snapshot.usable(1_790_055_000_000 + 60_000));
    }

    #[test]
    fn classifies_by_duration_not_by_slot() {
        let mut reversed = fixture();
        let limits = reversed.get_mut("rateLimits").unwrap();
        limits["primary"] =
            json!({ "usedPercent": 5.0, "windowDurationMins": 10080, "resetsAt": 1790338308 });
        limits["secondary"] =
            json!({ "usedPercent": 7.5, "windowDurationMins": 300, "resetsAt": 1790062934 });
        let snapshot = parse_rate_limits(&reversed, 1_790_055_000_000).unwrap();
        assert_eq!(snapshot.five_h.unwrap().remaining_percent, 93);
        assert_eq!(snapshot.weekly.unwrap().remaining_percent, 95);
    }

    #[test]
    fn tolerates_missing_or_null_windows() {
        let only_primary = json!({
            "rateLimits": {
                "primary": { "usedPercent": 40, "windowDurationMins": 300 },
                "secondary": null
            }
        });
        let snapshot = parse_rate_limits(&only_primary, 1).unwrap();
        assert_eq!(snapshot.five_h.unwrap().remaining_percent, 60);
        assert_eq!(snapshot.five_h.unwrap().resets_at_ms, None);
        assert!(snapshot.weekly.is_none());
        assert!(parse_rate_limits(&json!({ "rateLimits": null }), 1).is_none());
    }

    #[test]
    fn titles_come_from_the_desktop_thread_name() {
        let page = json!({
            "data": [
                { "id": "t1", "name": "Review and fix two commits (2)",
                  "preview": "需要你review两个项目的修改", "model": "gpt-5.6-luna",
                  "cwd": "/Users/test/agent_deck_companion" },
                { "id": "t2", "name": "  ", "preview": "untitled thread",
                  "model": "gpt-5.6-sol", "cwd": "/tmp" },
                { "id": "t3", "name": "foreign provider", "preview": "x",
                  "model": "claude-opus", "cwd": "/tmp" },
                { "id": "t4", "name": null, "preview": "   ",
                  "model": "gpt-5.6-luna", "cwd": "/tmp" },
                { "id": "t5", "preview": "fifth thread", "model": "gpt-5.6-luna" },
            ],
            "nextCursor": "more"
        });
        let snapshot = parse_threads(&page, 1_790_055_000_000, 3);
        assert_eq!(snapshot.fetched_at_ms, 1_790_055_000_000);
        let titles = snapshot
            .sessions
            .iter()
            .map(|session| session.title.as_str())
            .collect::<Vec<_>>();
        // Named first, preview only when untitled, non-Codex models and blank
        // titles dropped, just like the disk query.
        assert_eq!(
            titles,
            vec![
                "Review and fix two commits (2)",
                "untitled thread",
                "fifth thread"
            ]
        );
        assert_eq!(snapshot.sessions[2].cwd, "");
        assert!(snapshot.usable(1_790_055_000_000 + 60_000));
        assert!(!snapshot.usable(1_790_055_000_000 + 120_000));
    }

    #[test]
    fn missing_or_empty_pages_are_not_an_error() {
        assert!(parse_threads(&json!({}), 1, 3).sessions.is_empty());
        assert!(parse_threads(&json!({ "data": [] }), 1, 3)
            .sessions
            .is_empty());
    }

    #[test]
    fn expired_or_stale_snapshots_fall_back_to_disk() {
        let snapshot = QuotaSnapshot {
            fetched_at_ms: 1_000_000,
            five_h: Some(QuotaWindow {
                remaining_percent: 100,
                resets_at_ms: Some(2_000_000),
            }),
            weekly: None,
        };
        assert!(snapshot.usable(1_060_000));
        assert!(!snapshot.usable(1_200_000));
        assert!(!snapshot.usable(2_050_000));
    }
}
