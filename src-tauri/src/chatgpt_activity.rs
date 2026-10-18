//! UI-independent execution sampling. Quota and thread-list schedules stay separate.
use crate::{
    chatgpt_state::{self, ActivityRecord},
    state::{self, AppRuntime},
};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Default, Debug, Clone, Serialize)]
pub struct Metrics {
    pub sampled_at_ms: u128,
    pub scan_ms: u128,
    pub sample_count: usize,
    pub detection_p50_ms: Option<u128>,
    pub detection_p95_ms: Option<u128>,
    pub healthy: bool,
    pub pushed_at_ms: u128,
}
#[derive(Default)]
struct Data {
    records: HashMap<String, ActivityRecord>,
    samples: VecDeque<u128>,
    metrics: Metrics,
}
#[derive(Default)]
pub struct Runtime {
    data: Mutex<Data>,
}
impl Runtime {
    pub fn records(&self) -> HashMap<String, ActivityRecord> {
        self.data.lock().unwrap().records.clone()
    }
    pub fn metrics(&self) -> Metrics {
        self.data.lock().unwrap().metrics.clone()
    }
    fn update(&self, next: Option<HashMap<String, ActivityRecord>>, now: u128, cost: u128) -> bool {
        let mut data = self.data.lock().unwrap();
        data.metrics.sampled_at_ms = now;
        data.metrics.scan_ms = cost;
        data.metrics.healthy = next.is_some();
        let Some(next) = next else {
            return false;
        }; // A failed disk read never resets a live status.
        let changed = data.records != next;
        for (id, record) in &next {
            // Measure only status transitions after first discovery, exclude old
            // historical logs and duplicate samples. This is detection, not paint.
            if data
                .records
                .get(id)
                .is_some_and(|old| old.status != record.status)
                && record.event_at_ms > 0
                && record.event_at_ms <= now
                && now - record.event_at_ms <= 60_000
            {
                data.samples.push_back(now - record.event_at_ms);
                if data.samples.len() > 100 {
                    data.samples.pop_front();
                }
            }
        }
        data.records = next;
        let mut samples: Vec<_> = data.samples.iter().copied().collect();
        samples.sort_unstable();
        data.metrics.sample_count = samples.len();
        data.metrics.detection_p50_ms = percentile(&samples, 50);
        data.metrics.detection_p95_ms = percentile(&samples, 95);
        if changed {
            data.metrics.pushed_at_ms = now;
        }
        changed
    }
}
fn percentile(sorted: &[u128], percent: usize) -> Option<u128> {
    if sorted.is_empty() {
        None
    } else {
        Some(sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)])
    }
}
pub fn start(app: tauri::AppHandle, state: Arc<AppRuntime>) {
    thread::spawn(move || loop {
        let started = Instant::now();
        let next = chatgpt_state::session_activity();
        let changed =
            state
                .chatgpt_activity
                .update(next, state::now_ms(), started.elapsed().as_millis());
        let mut io_changed = false;
        {
            let records = state.chatgpt_activity.records();
            let panels = state
                .snapshot
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .active_panels
                .clone();
            for panel in panels.iter().filter(|p| p.agent_id == "chatgpt") {
                if let Some(record) = records.get(&panel.session_id) {
                    for (kind, at, label) in [
                        (
                            crate::session_activity::Kind::Output,
                            record.output_at_ms,
                            "output",
                        ),
                        (
                            crate::session_activity::Kind::Submitted,
                            record.submitted_at_ms,
                            "submitted",
                        ),
                        (
                            crate::session_activity::Kind::Completed,
                            record.completed_at_ms,
                            "completed",
                        ),
                    ] {
                        io_changed |= state.session_activity.record(
                            panel,
                            &format!("rollout:{label}:{at}"),
                            kind,
                            at,
                            "codex:semantic_log",
                            "native_output_keyboard_partial",
                        );
                    }
                }
            }
        }
        if changed || io_changed {
            state::emit_state(&app, &state); // Also attaches cached evidence to newly discovered threads.
        }
        thread::sleep(Duration::from_millis(500).saturating_sub(started.elapsed()));
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentStatus;
    fn event(status: AgentStatus, at: u128) -> HashMap<String, ActivityRecord> {
        [(
            "test".into(),
            ActivityRecord {
                status,
                event_at_ms: at,
                output_at_ms: 0,
                submitted_at_ms: 0,
                completed_at_ms: 0,
            },
        )]
        .into_iter()
        .collect()
    }
    #[test]
    fn failed_reads_and_duplicate_polls_preserve_status_without_new_samples() {
        let r = Runtime::default();
        assert!(r.update(Some(event(AgentStatus::Working, 100)), 200, 2));
        assert!(!r.update(None, 210, 3));
        assert_eq!(r.records()["test"].status, AgentStatus::Working);
        assert!(!r.update(Some(event(AgentStatus::Working, 100)), 250, 2));
        assert!(r.update(Some(event(AgentStatus::NeedsInput, 300)), 450, 2));
        assert!(!r.update(Some(event(AgentStatus::NeedsInput, 300)), 500, 2));
        assert_eq!(r.metrics().sample_count, 1);
        assert_eq!(r.metrics().detection_p95_ms, Some(150));
    }
    #[test]
    fn history_and_clock_skew_are_not_detection_samples() {
        let r = Runtime::default();
        r.update(Some(event(AgentStatus::Working, 1)), 100000, 0);
        r.update(Some(event(AgentStatus::Idle, 10)), 100001, 0);
        r.update(Some(event(AgentStatus::Error, 200000)), 100002, 0);
        assert_eq!(r.metrics().sample_count, 0);
    }
}
