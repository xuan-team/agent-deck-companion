//! Activity evidence is independent of status, focus and discovery timestamps.
//! Only timestamps/identities persist; this worker never reads terminal contents.
use crate::state::{now_ms, panel_focus_key, ActivePanel, AppRuntime, PanelTransport};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Activity {
    pub last_output_at: Option<u128>,
    pub last_keyboard_at: Option<u128>,
    pub last_activity_at: Option<u128>,
    pub last_prompt_submitted_at: Option<u128>,
    pub last_completed_at: Option<u128>,
    pub activity_source: Option<String>,
    pub coverage: String,
}
impl Default for Activity {
    fn default() -> Self {
        Self {
            last_output_at: None,
            last_keyboard_at: None,
            last_activity_at: None,
            last_prompt_submitted_at: None,
            last_completed_at: None,
            activity_source: None,
            coverage: "unknown".into(),
        }
    }
}

fn discard_unsafe_terminal_sample(record: &mut Activity) -> bool {
    if record.coverage != "terminal_inferred"
        && record.activity_source.as_deref() != Some("herdr:terminal_observed")
    {
        return false;
    }
    record.last_output_at = None;
    record.last_activity_at = record.last_keyboard_at;
    record.activity_source = record
        .last_keyboard_at
        .map(|_| "agentdeck:delivered_keys".into());
    record.coverage = if record.last_keyboard_at.is_some() {
        "input_only"
    } else {
        "unknown"
    }
    .into();
    true
}
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Output,
    Keyboard,
    Submitted,
    Completed,
}
#[derive(Default)]
struct Data {
    records: HashMap<String, Activity>,
    seen: HashSet<String>,
    order: VecDeque<String>,
    pending_ids: Vec<String>,
    dirty: bool,
}
#[derive(Default)]
pub struct Runtime {
    data: Mutex<Data>,
}

pub fn session_key(panel: &ActivePanel) -> String {
    let discovered_native = panel.id.starts_with("herdr:")
        && matches!(&panel.transport, PanelTransport::Herdr {pane_id,..} if pane_id != &panel.session_id);
    if uuid::Uuid::parse_str(&panel.session_id).is_ok()
        || (panel.agent_id == "opencode" && panel.session_id.starts_with("ses_"))
        || discovered_native
    {
        return serde_json::to_string(&(panel.agent_id.as_str(), panel.session_id.as_str()))
            .unwrap();
    }
    // Discovery-only panes lack a process/native generation. Keep their
    // history local to this observation rather than inheriting a prior CLI.
    let generation = if panel.id.starts_with("herdr:") {
        panel.first_seen_ms
    } else {
        0
    };
    serde_json::to_string(&(
        panel.agent_id.as_str(),
        panel.session_id.as_str(),
        panel_focus_key(panel),
        generation,
    ))
    .unwrap()
}
impl Runtime {
    pub fn activity(&self, panel: &ActivePanel) -> Activity {
        let mut record = self
            .data
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .records
            .get(&session_key(panel))
            .cloned()
            .unwrap_or_default();
        if record.coverage.is_empty() {
            record.coverage = "unknown".into();
        }
        record
    }
    pub fn record(
        &self,
        panel: &ActivePanel,
        id: &str,
        kind: Kind,
        at: u128,
        source: &str,
        coverage: &str,
    ) -> bool {
        if matches!(kind, Kind::Output) && source == "herdr:terminal_observed" {
            return false;
        }
        if at == 0 || at > now_ms() || id.len() > 512 {
            return false;
        }
        let key = session_key(panel);
        let event_key = serde_json::to_string(&(key.as_str(), id)).unwrap();
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        if !data.seen.insert(event_key.clone()) {
            return false;
        }
        data.order.push_back(event_key.clone());
        data.pending_ids.push(event_key);
        while data.order.len() > 10_000 {
            if let Some(old) = data.order.pop_front() {
                data.seen.remove(&old);
            }
        }
        data.dirty = true;
        let record = data.records.entry(key).or_default();
        let target = match kind {
            Kind::Output => &mut record.last_output_at,
            Kind::Keyboard => &mut record.last_keyboard_at,
            Kind::Submitted => &mut record.last_prompt_submitted_at,
            Kind::Completed => &mut record.last_completed_at,
        };
        let previous = *target;
        if previous.is_some_and(|old| old >= at) {
            return false;
        }
        *target = Some(previous.unwrap_or(0).max(at));
        if matches!(kind, Kind::Output | Kind::Keyboard) {
            if record.last_activity_at.is_none_or(|old| at > old) {
                record.activity_source = Some(source.into());
            }
            record.last_activity_at = record
                .last_output_at
                .into_iter()
                .chain(record.last_keyboard_at)
                .max();
            // Native output remains the preferred coverage over terminal inference.
            if record.coverage != "native_output_keyboard_partial"
                && !(matches!(kind, Kind::Keyboard) && record.last_output_at.is_some())
            {
                record.coverage = coverage.into();
            }
        }
        data.dirty = true;
        previous != Some(at) && previous.is_none_or(|old| at > old)
    }
    pub fn keyboard(&self, panel: &ActivePanel) {
        let at = now_ms();
        self.record(
            panel,
            &uuid::Uuid::new_v4().to_string(),
            Kind::Keyboard,
            at,
            "agentdeck:delivered_keys",
            "input_only",
        );
    }
    fn load(&self, path: &std::path::Path) {
        eprintln!("[activity] loading cache {}", path.display());
        let Ok(output) = Command::new("sqlite3")
            .args(["-json", "-readonly"])
            .arg(path)
            .arg("SELECT session_key, facts FROM activity_sessions;")
            .output()
        else {
            eprintln!("[activity] cannot start SQLite cache reader");
            return;
        };
        if !output.status.success() {
            eprintln!(
                "[activity] cache reader failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return;
        }
        let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
            eprintln!("[activity] cache query did not return a JSON row array");
            return;
        };
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        for row in rows {
            if let (Some(key), Some(json)) = (row["session_key"].as_str(), row["facts"].as_str()) {
                if let Ok(mut record) = serde_json::from_str::<Activity>(json) {
                    data.dirty |= discard_unsafe_terminal_sample(&mut record);
                    data.records.insert(key.into(), record);
                }
            }
        }
        eprintln!(
            "[activity] cache records={} cleanup_pending={}",
            data.records.len(),
            data.dirty
        );
        if let Ok(output) = Command::new("sqlite3")
            .args(["-json", "-readonly"])
            .arg(path)
            .arg("SELECT event_key FROM activity_event_ids ORDER BY rowid DESC LIMIT 10000;")
            .output()
        {
            if let Ok(rows) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) {
                for row in rows.iter().rev() {
                    if let Some(id) = row["event_key"].as_str() {
                        data.seen.insert(id.into());
                        data.order.push_back(id.into());
                    }
                }
            }
        }
    }
    fn flush(&self, path: &std::path::Path) -> Result<(), String> {
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        if !data.dirty {
            return Ok(());
        }
        let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
        let mut sql = "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS activity_sessions(session_key TEXT PRIMARY KEY,facts TEXT NOT NULL); CREATE TABLE IF NOT EXISTS activity_event_ids(event_key TEXT PRIMARY KEY); BEGIN IMMEDIATE;".to_owned();
        for id in &data.pending_ids {
            sql.push_str(&format!(
                "INSERT OR IGNORE INTO activity_event_ids VALUES ({});",
                quote(id)
            ));
        }
        for (key, record) in &data.records {
            sql.push_str(&format!("INSERT INTO activity_sessions VALUES ({},{}) ON CONFLICT(session_key) DO UPDATE SET facts=excluded.facts;",quote(key),quote(&serde_json::to_string(record).unwrap())));
        }
        sql.push_str("DELETE FROM activity_event_ids WHERE rowid NOT IN (SELECT rowid FROM activity_event_ids ORDER BY rowid DESC LIMIT 10000); COMMIT;");
        let mut child = Command::new("sqlite3")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        child
            .stdin
            .take()
            .ok_or("Activity SQLite stdin unavailable")?
            .write_all(sql.as_bytes())
            .map_err(|e| e.to_string())?;
        if !child.wait().map_err(|e| e.to_string())?.success() {
            return Err("Activity SQLite write failed".into());
        }
        data.dirty = false;
        data.pending_ids.clear();
        // Recent timestamps are monotonic even after bounded ID eviction.
        Ok(())
    }
}

pub fn start(app: tauri::AppHandle, state: Arc<AppRuntime>) {
    let path = dirs::config_dir().map(|p| p.join("Agent Deck Companion/activity.sqlite"));
    if let Some(path) = &path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        state.session_activity.load(path);
    }
    thread::spawn(move || {
        let mut persistence_error_reported = false;
        loop {
            let started = Instant::now();
            // Do not read/render Herdr panes here: terminal sampling can disturb a live TUI.
            let dirty = state
                .session_activity
                .data
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .dirty;
            if dirty {
                crate::state::emit_state(&app, &state);
            }
            if let Some(path) = &path {
                match state.session_activity.flush(path) {
                    Err(error) if !persistence_error_reported => {
                        eprintln!("[activity] {error}");
                        persistence_error_reported = true;
                    }
                    Ok(()) => persistence_error_reported = false,
                    _ => {}
                }
            }
            thread::sleep(Duration::from_secs(1).saturating_sub(started.elapsed()));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsafe_terminal_output_is_rejected_without_making_a_session_active() {
        let r = Runtime::default();
        let p = panel("no-terminal-reads");
        assert!(!r.record(
            &p,
            "sample",
            Kind::Output,
            100,
            "herdr:terminal_observed",
            "terminal_inferred"
        ));
        assert_eq!(r.activity(&p).last_activity_at, None);
        assert_eq!(r.activity(&p).coverage, "unknown");
    }

    #[test]
    fn loading_legacy_samples_clears_only_unsafe_output_and_keeps_confirmed_keys() {
        let root =
            std::env::temp_dir().join(format!("deck-legacy-activity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("activity.sqlite");
        let r = Runtime::default();
        let p = panel("legacy");
        {
            let mut data = r.data.lock().unwrap();
            data.records.insert(
                session_key(&p),
                Activity {
                    last_output_at: Some(200),
                    last_keyboard_at: Some(100),
                    last_activity_at: Some(200),
                    last_completed_at: Some(300),
                    activity_source: Some("herdr:terminal_observed".into()),
                    coverage: "terminal_inferred".into(),
                    ..Default::default()
                },
            );
            let mut only_output = p.clone();
            only_output.session_id = "legacy-output-only".into();
            data.records.insert(
                session_key(&only_output),
                Activity {
                    last_output_at: Some(200),
                    last_activity_at: Some(200),
                    activity_source: Some("herdr:terminal_observed".into()),
                    coverage: "terminal_inferred".into(),
                    ..Default::default()
                },
            );
            data.dirty = true;
        }
        r.flush(&path).unwrap();
        let restored = Runtime::default();
        restored.load(&path);
        let record = restored.activity(&p);
        assert_eq!(record.last_output_at, None);
        assert_eq!(record.last_keyboard_at, Some(100));
        assert_eq!(record.last_activity_at, Some(100));
        assert_eq!(record.last_completed_at, Some(300));
        assert_eq!(record.coverage, "input_only");
        let mut only_output = p.clone();
        only_output.session_id = "legacy-output-only".into();
        assert_eq!(restored.activity(&only_output).last_activity_at, None);
        assert_eq!(restored.activity(&only_output).coverage, "unknown");
        restored.flush(&path).unwrap();
        let again = Runtime::default();
        again.load(&path);
        assert_eq!(again.activity(&p), record);
        let _ = std::fs::remove_dir_all(root);
    }
    pub fn panel(id: &str) -> ActivePanel {
        ActivePanel {
            id: id.into(),
            agent_id: "qwen".into(),
            session_id: id.into(),
            slot: 1,
            label: id.into(),
            status: crate::state::AgentStatus::Working,
            cwd: Some("/same/project".into()),
            jobs_working: 1,
            jobs_waiting: 0,
            jobs_error: 0,
            updated_at_ms: 1,
            status_since_ms: 1,
            last_activity_ms: 0,
            first_seen_ms: 1,
            transport: PanelTransport::Herdr {
                socket_path: None,
                pane_id: id.into(),
            },
            unread: false,
            unread_since_ms: 0,
        }
    }
    #[test]
    fn only_output_and_delivered_keys_change_activity_not_lifecycle_or_replay() {
        let r = Runtime::default();
        let p = panel("one");
        r.record(&p, "submit", Kind::Submitted, 100, "hook", "unknown");
        r.record(&p, "complete", Kind::Completed, 200, "hook", "unknown");
        assert_eq!(r.activity(&p).last_activity_at, None);
        assert!(r.record(&p, "out", Kind::Output, 300, "herdr", "terminal_inferred"));
        assert!(!r.record(&p, "out", Kind::Output, 400, "herdr", "terminal_inferred"));
        assert!(!r.record(&p, "old", Kind::Output, 250, "herdr", "terminal_inferred"));
        assert!(r.record(&p, "key", Kind::Keyboard, 350, "agentdeck", "input_only"));
        assert_eq!(r.activity(&p).last_activity_at, Some(350));
        assert_eq!(r.activity(&p).coverage, "terminal_inferred");
        assert_eq!(r.activity(&panel("two")).last_activity_at, None);
        assert!(!r.record(
            &p,
            "future",
            Kind::Output,
            now_ms() + 10_000,
            "herdr",
            "terminal_inferred"
        ));
    }
    #[test]
    fn sqlite_restores_evidenced_time_without_turning_restart_into_activity() {
        let root = std::env::temp_dir().join(format!("deck-activity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("activity.sqlite");
        let r = Runtime::default();
        let p = panel("quote's-session");
        r.record(
            &p,
            "one",
            Kind::Output,
            123,
            "qwen:native_log",
            "native_output_keyboard_partial",
        );
        r.flush(&path).unwrap();
        let restored = Runtime::default();
        restored.load(&path);
        assert_eq!(restored.activity(&p).last_activity_at, Some(123));
        assert!(!restored.record(&p, "one", Kind::Output, 500, "herdr", "terminal_inferred"));
        assert!(!restored.record(
            &p,
            "replay",
            Kind::Output,
            123,
            "herdr",
            "terminal_inferred"
        ));
        let mut replaced = p.clone();
        replaced.session_id = "replacement".into();
        assert_eq!(restored.activity(&replaced).last_activity_at, None);
        let _ = std::fs::remove_dir_all(root);
    }
}
