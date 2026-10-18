use std::sync::Arc;

use axum::http::HeaderMap;
use tauri::AppHandle;

use crate::state::{
    now_ms, report_panel_status, ActivePanel, AgentJob, AgentStatus, AppRuntime, JobStatus,
    PanelTransport,
};

pub fn payload_string(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload.get(key).and_then(|value| match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

#[derive(Default)]
pub struct HookTransport {
    pub pid: Option<u32>,
    pub herdr_socket: Option<String>,
    pub herdr_pane: Option<String>,
    pub tmux_socket: Option<String>,
    pub tmux_pane: Option<String>,
}

fn hook_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

pub fn hook_transport(headers: &HeaderMap) -> HookTransport {
    HookTransport {
        pid: hook_header(headers, "x-agent-deck-pid").and_then(|v| v.parse().ok()),
        herdr_socket: hook_header(headers, "x-agent-deck-herdr-socket"),
        herdr_pane: hook_header(headers, "x-agent-deck-herdr-pane"),
        tmux_socket: hook_header(headers, "x-agent-deck-tmux")
            .and_then(|value| value.split(',').next().map(str::to_owned)),
        tmux_pane: hook_header(headers, "x-agent-deck-tmux-pane"),
    }
}

pub fn reduced_status<'a>(
    jobs: impl Iterator<Item = &'a AgentJob>,
    _running: AgentStatus,
) -> AgentStatus {
    let statuses = jobs.map(|job| job.status).collect::<Vec<_>>();
    if statuses.iter().any(|status| *status == JobStatus::Failed) {
        AgentStatus::Error
    } else if statuses.iter().any(|status| *status == JobStatus::Waiting) {
        AgentStatus::NeedsInput
    } else if statuses.iter().any(|status| *status == JobStatus::Running) {
        AgentStatus::Working
    } else {
        AgentStatus::Idle
    }
}

fn reset_previous_turn(jobs: &mut [AgentJob], agent_id: &str, session_id: &str) {
    for job in jobs
        .iter_mut()
        .filter(|job| job.agent_id == agent_id && job.session_id == session_id)
    {
        if matches!(job.status, JobStatus::Failed | JobStatus::Waiting) {
            job.status = JobStatus::Interrupted;
        }
    }
}

fn panel_transport(value: HookTransport, current: Option<&PanelTransport>) -> PanelTransport {
    if let Some(pane_id) = value.herdr_pane {
        PanelTransport::Herdr {
            socket_path: value.herdr_socket,
            pane_id,
        }
    } else if let (Some(socket_path), Some(pane_id)) = (value.tmux_socket, value.tmux_pane) {
        PanelTransport::Tmux {
            socket_path,
            pane_id,
            pid: value.pid,
        }
    } else if matches!(
        current,
        Some(
            PanelTransport::CodexThread { .. }
                | PanelTransport::Herdr { .. }
                | PanelTransport::Tmux { .. }
        )
    ) {
        // Hook helper PIDs are not a Desktop thread's navigation target.
        // Losing this transport also loses attribution on the next event.
        current.cloned().unwrap()
    } else if let Some(pid) = value.pid {
        PanelTransport::Terminal { pid }
    } else {
        current.cloned().unwrap_or(PanelTransport::Unknown)
    }
}

fn same_panel_transport(existing: &PanelTransport, incoming: &PanelTransport) -> bool {
    match (existing, incoming) {
        (
            PanelTransport::Tmux {
                socket_path: existing_socket,
                pane_id: existing_pane,
                ..
            },
            PanelTransport::Tmux {
                socket_path,
                pane_id,
                ..
            },
        ) => existing_socket == socket_path && existing_pane == pane_id,
        (
            PanelTransport::Herdr {
                socket_path: existing_socket,
                pane_id: existing_pane,
            },
            PanelTransport::Herdr {
                socket_path,
                pane_id,
            },
        ) => {
            existing_pane == pane_id
                && crate::herdr::socket_paths_match(
                    existing_socket.as_deref(),
                    socket_path.as_deref(),
                )
        }
        _ => false,
    }
}

fn same_agent_panel_transport(
    panel: &ActivePanel,
    agent_id: &str,
    incoming: &PanelTransport,
) -> bool {
    panel.agent_id == agent_id && same_panel_transport(&panel.transport, incoming)
}

/// A `codex` hook only reports ChatGPT Desktop activity when it names a desktop
/// thread the deck already lists *as a desktop thread* (materialized from the
/// desktop's own data, so it carries the thread transport). The CLI shares
/// `~/.codex` and its hooks, so without this it would be counted as the desktop
/// app's own status. Selection is irrelevant here: attribution follows the
/// session's source, not whichever agent the deck is showing.
fn owns_desktop_thread(
    panels: &[ActivePanel],
    transport: &HookTransport,
    session_id: &str,
) -> bool {
    transport.herdr_pane.is_none()
        && transport.tmux_pane.is_none()
        && panels.iter().any(|panel| {
            panel.agent_id == "chatgpt"
                && panel.session_id == session_id
                && matches!(&panel.transport, PanelTransport::CodexThread { .. })
        })
}

/// Which deck agent a hook event belongs to. The Codex CLI shares `~/.codex`
/// and its hooks with the desktop app, and the Qoder CLI is filed under its
/// binary name by Herdr, so neither can keep the id it posted with.
fn hook_agent_owner<'a>(
    panels: &[ActivePanel],
    transport: &HookTransport,
    agent_id: &'a str,
    session_id: &str,
) -> &'a str {
    if agent_id == "codex" && owns_desktop_thread(panels, transport, session_id) {
        "chatgpt"
    } else if agent_id == "qoder" {
        // One id with the pane row Herdr already discovered is what stops the
        // same session from appearing twice.
        "qodercli"
    } else {
        agent_id
    }
}

fn normalize_desktop_transport(
    snapshot: &crate::state::AppSnapshot,
    agent_id: &str,
    session_id: &str,
    transport: &mut HookTransport,
) {
    if agent_id != "codex" || transport.tmux_pane.is_some() {
        return;
    }
    let Some(pane_id) = transport.herdr_pane.as_ref() else {
        return;
    };
    let target = PanelTransport::Herdr {
        socket_path: transport.herdr_socket.clone(),
        pane_id: pane_id.clone(),
    };
    // A Desktop launched from Herdr may inherit an obsolete pane variable.
    // Require both a confirmed missing target and an independently discovered
    // Desktop thread; a valid CLI pane keeps its original attribution.
    if crate::state::herdr_target_missing(snapshot, &target)
        && owns_desktop_thread(
            &snapshot.active_panels,
            &HookTransport::default(),
            session_id,
        )
    {
        transport.herdr_pane = None;
        transport.herdr_socket = None;
    }
}

pub fn process_hook_event(
    agent_id: &str,
    payload: serde_json::Value,
    mut transport: HookTransport,
    app: &AppHandle,
    state: &Arc<AppRuntime>,
) -> Result<crate::state::AppSnapshot, String> {
    if !matches!(agent_id, "codex" | "claude" | "qwen" | "qoder" | "opencode") {
        return Err(format!("unsupported hook agent: {agent_id}"));
    }
    let event = payload_string(&payload, "hook_event_name")
        .ok_or_else(|| "hook_event_name is required".to_string())?;
    let session_id = payload_string(&payload, "session_id").unwrap_or_else(|| "unknown".into());
    if agent_id == "opencode" {
        if payload.get("opencode_v2").and_then(|v| v.as_bool()) != Some(true)
            || !session_id.starts_with("ses_")
            || !matches!(
                event.as_str(),
                "SessionStart"
                    | "SessionBind"
                    | "UserPromptSubmit"
                    | "PreToolUse"
                    | "PermissionRequest"
                    | "PermissionResolved"
                    | "Stop"
                    | "StopFailure"
                    | "Interrupted"
                    | "SessionEnd"
                    | "Output"
                    | "Metadata"
            )
        {
            return Err("invalid OpenCode v2 event".into());
        }
        // v2 may use a shared server: inherited pane variables and its PID
        // belong to the server launcher, not necessarily this session.
        if event == "SessionBind" {
            let snapshot = state.snapshot.read().map_err(|_| "snapshot poisoned")?;
            let valid_pane =
                transport.herdr_pane.as_ref().is_some_and(|pane| {
                    snapshot.herdr.connected
                        && snapshot
                            .herdr
                            .agents
                            .iter()
                            .any(|p| &p.pane_id == pane && p.agent == "opencode")
                }) && crate::herdr::socket_paths_match(transport.herdr_socket.as_deref(), None);
            if !valid_pane {
                return Err("OpenCode TUI binding requires a live OpenCode pane".into());
            }
        } else {
            transport = HookTransport::default();
        }
    }
    if agent_id == "opencode" && matches!(event.as_str(), "Output" | "Metadata") {
        let changed = {
            let mut snapshot = state.snapshot.write().map_err(|_| "snapshot poisoned")?;
            if let Some(panel) = snapshot
                .active_panels
                .iter_mut()
                .find(|p| p.agent_id == "opencode" && p.session_id == session_id)
            {
                if event == "Output" {
                    state.session_activity.record(
                        panel,
                        &payload_string(&payload, "event_id").ok_or("event_id required")?,
                        crate::session_activity::Kind::Output,
                        payload
                            .get("event_at_ms")
                            .and_then(|v| v.as_u64())
                            .map(u128::from)
                            .unwrap_or_else(now_ms),
                        "opencode:v2_text",
                        "native_output_keyboard_partial",
                    )
                } else {
                    let label = payload_string(&payload, "session_title")
                        .unwrap_or_else(|| panel.label.clone());
                    let changed = panel.label != label;
                    panel.label = label;
                    panel.cwd = payload_string(&payload, "cwd").or_else(|| panel.cwd.clone());
                    changed
                }
            } else {
                false
            }
        };
        if changed {
            crate::state::emit_state(app, state);
        }
        return Ok(state.read());
    }
    let target_agent_id = {
        let snapshot = state.snapshot.read().map_err(|_| "snapshot poisoned")?;
        normalize_desktop_transport(&snapshot, agent_id, &session_id, &mut transport);
        hook_agent_owner(&snapshot.active_panels, &transport, agent_id, &session_id)
    };
    // A switched-off agent is not monitored: its lifecycle events must not
    // resurrect the rows the user removed. Re-enable it in Settings first.
    // The check uses the resolved owner, so a Codex hook that belongs to a
    // monitored ChatGPT Desktop thread is still applied.
    if !state.agent_enabled(&target_agent_id) {
        return Ok(state.read());
    }
    let child_id = payload_string(&payload, "task_id")
        .or_else(|| payload_string(&payload, "todo_id"))
        .or_else(|| payload_string(&payload, "agent_id"));
    let job_id = format!(
        "{target_agent_id}:{session_id}:{}",
        child_id.clone().unwrap_or_else(|| "main".into())
    );
    let (mut job_status, agent_status) = match event.as_str() {
        "SessionBind" => match payload_string(&payload, "native_status").as_deref() {
            Some("working") => (JobStatus::Running, AgentStatus::Working),
            Some("needs_input") => (JobStatus::Waiting, AgentStatus::NeedsInput),
            Some("error") => (JobStatus::Failed, AgentStatus::Error),
            Some("idle") => (JobStatus::Ready, AgentStatus::Idle),
            _ => return Err("invalid OpenCode native_status".into()),
        },
        "SessionStart" => (JobStatus::Ready, AgentStatus::Idle),
        "UserPromptSubmit" => (JobStatus::Running, AgentStatus::Working),
        "PreToolUse" => (JobStatus::Running, AgentStatus::Working),
        "PermissionRequest" => (JobStatus::Waiting, AgentStatus::NeedsInput),
        "PostToolUse" | "PostToolUseFailure" => (JobStatus::Running, AgentStatus::Working),
        "SubagentStart" | "TaskCreated" | "TodoCreated" => {
            (JobStatus::Running, AgentStatus::Working)
        }
        "SubagentStop" | "TaskCompleted" | "TodoCompleted" => {
            (JobStatus::Completed, AgentStatus::Done)
        }
        "Stop" => (JobStatus::Completed, AgentStatus::Idle),
        "StopFailure" => (JobStatus::Failed, AgentStatus::Error),
        "SessionEnd" => (JobStatus::Interrupted, AgentStatus::Idle),
        "Interrupted" | "PermissionResolved" => (JobStatus::Interrupted, AgentStatus::Idle),
        _ => (JobStatus::Running, AgentStatus::Working),
    };
    let updated_at_ms = now_ms();
    let hook_pid = transport.pid;

    {
        let mut snapshot = state
            .snapshot
            .write()
            .map_err(|_| "snapshot lock poisoned")?;
        // Reborrow so sibling fields can be borrowed at the same time, which a
        // guard's Deref would otherwise serialise.
        let snapshot = &mut *snapshot;
        if agent_id == "opencode" && event == "SessionBind" {
            if let Some(job) = snapshot
                .jobs
                .iter()
                .find(|j| j.id == job_id && j.last_event != "SessionBind")
            {
                job_status = job.status;
            }
        }
        if event == "UserPromptSubmit" {
            reset_previous_turn(&mut snapshot.jobs, target_agent_id, &session_id);
        }
        if agent_id == "opencode"
            && matches!(event.as_str(), "Stop" | "StopFailure" | "Interrupted")
        {
            for job in snapshot
                .jobs
                .iter_mut()
                .filter(|j| j.agent_id == "opencode" && j.session_id == session_id)
            {
                job.status = JobStatus::Interrupted;
            }
        }
        let mut updated_existing = false;
        if event == "SessionEnd" {
            for job in snapshot
                .jobs
                .iter_mut()
                .filter(|job| job.agent_id == target_agent_id && job.session_id == session_id)
            {
                updated_existing = true;
                if !matches!(job.status, JobStatus::Completed | JobStatus::Failed) {
                    job.status = JobStatus::Interrupted;
                }
                job.last_event = event.clone();
                job.updated_at_ms = updated_at_ms;
            }
        } else if let Some(job) = snapshot.jobs.iter_mut().find(|job| job.id == job_id) {
            updated_existing = true;
            job.status = job_status;
            if agent_id != "opencode" || event != "SessionBind" || job.last_event == "SessionBind" {
                job.last_event = event.clone();
            }
            job.cwd = payload_string(&payload, "cwd").or_else(|| job.cwd.clone());
            job.agent_type =
                payload_string(&payload, "agent_type").or_else(|| job.agent_type.clone());
            job.updated_at_ms = updated_at_ms;
        }
        if !updated_existing {
            snapshot.jobs.push(AgentJob {
                id: job_id,
                agent_id: target_agent_id.into(),
                session_id: session_id.clone(),
                status: job_status,
                cwd: payload_string(&payload, "cwd"),
                last_event: event.clone(),
                updated_at_ms,
                agent_type: payload_string(&payload, "agent_type"),
            });
        }

        let session_jobs = snapshot
            .jobs
            .iter()
            .filter(|job| job.agent_id == target_agent_id && job.session_id == session_id);
        let status = if event == "SessionEnd" {
            AgentStatus::Offline
        } else {
            reduced_status(session_jobs, agent_status)
        };
        let counts = snapshot
            .jobs
            .iter()
            .filter(|job| job.agent_id == target_agent_id && job.session_id == session_id)
            .fold((0_u16, 0_u16, 0_u16), |mut counts, job| {
                match job.status {
                    JobStatus::Running => counts.0 += 1,
                    JobStatus::Ready | JobStatus::Waiting => counts.1 += 1,
                    JobStatus::Failed => counts.2 += 1,
                    JobStatus::Completed | JobStatus::Interrupted => {}
                }
                counts
            });
        let panel_id = format!("{target_agent_id}:{session_id}");
        let existing_transport = snapshot
            .active_panels
            .iter()
            .find(|panel| {
                panel.id == panel_id
                    || (agent_id == "opencode"
                        && panel.agent_id == "opencode"
                        && panel.session_id == session_id)
            })
            .map(|panel| &panel.transport);
        let resolved_transport = panel_transport(transport, existing_transport);

        // When we have tmux/herdr pane info, deduplicate by pane instead of session_id.
        // Agents may send different session_ids for the same pane (e.g., directory
        // name vs UUID), which would otherwise create duplicate panels.
        let pane_match = snapshot
            .active_panels
            .iter_mut()
            .find(|panel| same_agent_panel_transport(panel, target_agent_id, &resolved_transport));

        if let Some(panel) = pane_match {
            // Update the panel already discovered for this pane (session_id may have changed).
            if panel.session_id != session_id {
                // The pane identity is stable, but a replacement logical
                // session must not reuse the previous session's event version.
                panel.status_since_ms = updated_at_ms.max(panel.status_since_ms.saturating_add(1));
                panel.first_seen_ms = updated_at_ms;
                panel.unread = false;
                panel.unread_since_ms = 0;
            }
            panel.session_id = session_id.clone();
            panel.id = format!("{target_agent_id}:{session_id}");
            panel.cwd = payload_string(&payload, "cwd").or_else(|| panel.cwd.clone());
            if !matches!(panel.transport, PanelTransport::Herdr { .. }) {
                panel.label = payload_string(&payload, "session_title")
                    .unwrap_or_else(|| panel.label.clone());
            }
            panel.jobs_working = counts.0;
            panel.jobs_waiting = counts.1;
            panel.jobs_error = counts.2;
            panel.updated_at_ms = updated_at_ms;
            panel.transport = resolved_transport;
            report_panel_status(
                &mut snapshot.agents,
                snapshot.active_agent.as_deref(),
                panel,
                status,
            );
        } else if let Some(panel) = snapshot
            .active_panels
            .iter_mut()
            .find(|panel| panel.id == panel_id)
        {
            panel.cwd = payload_string(&payload, "cwd").or_else(|| panel.cwd.clone());
            if !matches!(panel.transport, PanelTransport::Herdr { .. }) {
                panel.label = payload_string(&payload, "session_title")
                    .unwrap_or_else(|| panel.label.clone());
            }
            panel.jobs_working = counts.0;
            panel.jobs_waiting = counts.1;
            panel.jobs_error = counts.2;
            panel.updated_at_ms = updated_at_ms;
            panel.transport = resolved_transport;
            report_panel_status(
                &mut snapshot.agents,
                snapshot.active_agent.as_deref(),
                panel,
                status,
            );
        } else {
            let used = snapshot
                .active_panels
                .iter()
                .filter(|panel| {
                    panel.agent_id == target_agent_id && panel.status != AgentStatus::Offline
                })
                .map(|panel| panel.slot)
                .collect::<Vec<_>>();
            let slot = (1..=u8::MAX)
                .find(|slot| !used.contains(slot))
                .unwrap_or(u8::MAX);
            let cwd = payload_string(&payload, "cwd");
            let label = payload_string(&payload, "session_title")
                .or_else(|| {
                    cwd.as_deref()
                        .and_then(|path| std::path::Path::new(path).file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| {
                    format!("Session {}", session_id.chars().take(8).collect::<String>())
                });
            snapshot.active_panels.push(ActivePanel {
                id: panel_id,
                agent_id: target_agent_id.into(),
                session_id: session_id.clone(),
                slot,
                label,
                status,
                cwd,
                jobs_working: counts.0,
                jobs_waiting: counts.1,
                jobs_error: counts.2,
                updated_at_ms,
                status_since_ms: updated_at_ms,
                last_activity_ms: 0,
                first_seen_ms: updated_at_ms,
                transport: resolved_transport,
                unread: false,
                unread_since_ms: 0,
            });
        }

        // Retired session jobs are history, not the current agent state.
        let aggregate = snapshot
            .active_panels
            .iter()
            .filter(|panel| {
                panel.agent_id == target_agent_id && panel.status != AgentStatus::Offline
            })
            .map(|panel| panel.status)
            .max_by_key(|status| status.priority())
            .unwrap_or(AgentStatus::Offline);
        let any_panel_running = snapshot
            .active_panels
            .iter()
            .any(|panel| panel.agent_id == target_agent_id && panel.status != AgentStatus::Offline);
        let is_active = snapshot.active_agent.as_deref() == Some(target_agent_id);
        if let Some(agent) = snapshot
            .agents
            .iter_mut()
            .find(|agent| agent.id == target_agent_id)
        {
            if let Some(pid) = hook_pid {
                agent.pid = Some(pid);
            }
            agent.running = any_panel_running;
            agent.status = if agent.running {
                aggregate
            } else {
                AgentStatus::Offline
            };
            if !is_active
                && matches!(
                    agent.status,
                    AgentStatus::Done | AgentStatus::NeedsInput | AgentStatus::Error
                )
            {
                agent.unread = true;
            }
            // Falling back to Idle is what a completion looks like, so it must
            // not erase the news; only a new task or selecting the agent does.
            if agent.status == AgentStatus::Working {
                agent.unread = false;
            }
        }
        snapshot
            .jobs
            .sort_by_key(|job| std::cmp::Reverse(job.updated_at_ms));
        snapshot.jobs.truncate(50);
        snapshot.updated_at_ms = updated_at_ms;

        state
            .hook_statuses
            .lock()
            .map_err(|_| "hook statuses lock poisoned")?
            .insert(target_agent_id.into(), (aggregate, updated_at_ms));
    }
    if matches!(event.as_str(), "UserPromptSubmit" | "Stop") {
        let snapshot = state.snapshot.read().map_err(|_| "snapshot poisoned")?;
        if let Some(panel) = snapshot
            .active_panels
            .iter()
            .find(|p| p.agent_id == target_agent_id && p.session_id == session_id)
        {
            let kind = if event == "Stop" {
                crate::session_activity::Kind::Completed
            } else {
                crate::session_activity::Kind::Submitted
            };
            let event_id = payload_string(&payload, "event_id")
                .unwrap_or_else(|| format!("hook:{event}:{updated_at_ms}"));
            state.session_activity.record(
                panel,
                &event_id,
                kind,
                if agent_id == "opencode" {
                    payload
                        .get("event_at_ms")
                        .and_then(|v| v.as_u64())
                        .map(u128::from)
                        .unwrap_or(updated_at_ms)
                } else {
                    updated_at_ms
                },
                if agent_id == "opencode" {
                    "opencode:v2_lifecycle"
                } else {
                    "hook:lifecycle"
                },
                "unknown",
            );
        }
    }
    crate::state::emit_state(app, state);
    Ok(state.read())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_transport_matches_discovery_panel_without_socket() {
        let discovered = PanelTransport::Herdr {
            socket_path: None,
            pane_id: "wK:pV".into(),
        };
        let hooked = PanelTransport::Herdr {
            socket_path: crate::herdr::socket_identity(None)
                .map(|p| p.to_string_lossy().into_owned()),
            pane_id: "wK:pV".into(),
        };

        assert!(same_panel_transport(&discovered, &hooked));
    }

    #[test]
    fn herdr_transport_does_not_match_a_different_pane() {
        let first = PanelTransport::Herdr {
            socket_path: None,
            pane_id: "wK:pV".into(),
        };
        let second = PanelTransport::Herdr {
            socket_path: None,
            pane_id: "wK:p1".into(),
        };

        assert!(!same_panel_transport(&first, &second));
    }

    #[test]
    fn pane_match_does_not_reassign_a_different_agent() {
        let panel = ActivePanel {
            id: "qwen:session".into(),
            agent_id: "qwen".into(),
            session_id: "session".into(),
            slot: 1,
            label: "Qwen".into(),
            status: AgentStatus::Working,
            cwd: None,
            jobs_working: 1,
            jobs_waiting: 0,
            jobs_error: 0,
            updated_at_ms: 0,
            status_since_ms: 0,
            last_activity_ms: 0,
            first_seen_ms: 0,
            transport: PanelTransport::Herdr {
                socket_path: None,
                pane_id: "wK:pV".into(),
            },
            unread: false,
            unread_since_ms: 0,
        };
        let incoming = PanelTransport::Herdr {
            socket_path: crate::herdr::socket_identity(None)
                .map(|path| path.to_string_lossy().into_owned()),
            pane_id: "wK:pV".into(),
        };

        assert!(same_agent_panel_transport(&panel, "qwen", &incoming));
        assert!(!same_agent_panel_transport(&panel, "chatgpt", &incoming));
    }

    fn desktop_thread_panel(session_id: &str) -> ActivePanel {
        ActivePanel {
            id: format!("chatgpt:{session_id}"),
            agent_id: "chatgpt".into(),
            session_id: session_id.into(),
            slot: 1,
            label: "Show Shanghai 3-day weather".into(),
            status: AgentStatus::Idle,
            cwd: None,
            jobs_working: 0,
            jobs_waiting: 0,
            jobs_error: 0,
            updated_at_ms: 0,
            status_since_ms: 0,
            last_activity_ms: 0,
            first_seen_ms: 0,
            transport: PanelTransport::CodexThread {
                thread_id: session_id.into(),
            },
            unread: false,
            unread_since_ms: 0,
        }
    }

    fn cli_terminal_transport() -> HookTransport {
        HookTransport {
            pid: Some(4242),
            herdr_pane: Some("wK:p3J".into()),
            ..Default::default()
        }
    }

    #[test]
    fn obsolete_herdr_environment_does_not_steal_a_known_desktop_thread() {
        let mut snapshot = crate::state::AppRuntime::new().read();
        snapshot.herdr.connected = true;
        snapshot.active_panels = vec![desktop_thread_panel("desktop")];
        let mut transport = cli_terminal_transport();
        normalize_desktop_transport(&snapshot, "codex", "desktop", &mut transport);
        assert_eq!(
            hook_agent_owner(&snapshot.active_panels, &transport, "codex", "desktop"),
            "chatgpt"
        );
        let resolved = panel_transport(transport, Some(&snapshot.active_panels[0].transport));
        assert!(matches!(resolved, PanelTransport::CodexThread { .. }));

        let mut unlisted = cli_terminal_transport();
        normalize_desktop_transport(&snapshot, "codex", "cli", &mut unlisted);
        assert!(unlisted.herdr_pane.is_some());
        snapshot.herdr.connected = false;
        let mut unverified = cli_terminal_transport();
        normalize_desktop_transport(&snapshot, "codex", "desktop", &mut unverified);
        assert!(unverified.herdr_pane.is_some());
    }

    #[test]
    fn a_codex_cli_event_in_a_pane_stays_with_codex_even_for_a_desktop_thread() {
        let panels = [desktop_thread_panel("01a0d10c")];
        let tmux = HookTransport {
            pid: Some(4242),
            tmux_pane: Some("%7".into()),
            ..Default::default()
        };

        assert!(!owns_desktop_thread(
            &panels,
            &cli_terminal_transport(),
            "01a0d10c"
        ));
        assert!(!owns_desktop_thread(&panels, &tmux, "01a0d10c"));
    }

    #[test]
    fn a_pane_less_codex_event_for_a_listed_desktop_thread_counts_for_chatgpt() {
        let panels = [desktop_thread_panel("01a0d10c")];
        let desktop = HookTransport {
            pid: Some(18140),
            ..Default::default()
        };

        // Attribution follows the session's source, so it holds whether or not
        // the deck has ChatGPT selected.
        assert!(owns_desktop_thread(&panels, &desktop, "01a0d10c"));
    }

    #[test]
    fn a_pane_less_codex_event_for_an_unlisted_session_stays_with_codex() {
        let panels = [desktop_thread_panel("01a0d10c")];
        let desktop = HookTransport {
            pid: Some(18140),
            ..Default::default()
        };

        assert!(!owns_desktop_thread(&panels, &desktop, "01a0d17e"));
    }

    #[test]
    fn a_qoder_event_is_filed_under_the_name_herdr_uses() {
        // Herdr discovers the Qoder CLI pane as `qodercli`. The hook posts as
        // `qoder`, so both must land on one id or the session shows twice.
        let panels = [ActivePanel {
            id: "qodercli:session-uuid".into(),
            agent_id: "qodercli".into(),
            session_id: "session-uuid".into(),
            slot: 1,
            label: "Qoder in a pane".into(),
            status: AgentStatus::Working,
            cwd: None,
            jobs_working: 1,
            jobs_waiting: 0,
            jobs_error: 0,
            updated_at_ms: 0,
            status_since_ms: 0,
            last_activity_ms: 0,
            first_seen_ms: 0,
            transport: PanelTransport::Herdr {
                socket_path: Some("/tmp/herdr.sock".into()),
                pane_id: "wK:pV".into(),
            },
            unread: false,
            unread_since_ms: 0,
        }];
        let in_pane = HookTransport {
            herdr_socket: Some("/tmp/herdr.sock".into()),
            herdr_pane: Some("wK:pV".into()),
            ..Default::default()
        };

        assert_eq!(
            hook_agent_owner(&panels, &in_pane, "qoder", "session-uuid"),
            "qodercli"
        );
        // Other agents keep the id they posted with.
        assert_eq!(
            hook_agent_owner(&panels, &in_pane, "qwen", "session-uuid"),
            "qwen"
        );
        let desktop = [desktop_thread_panel("01a0d10c")];
        assert_eq!(
            hook_agent_owner(
                &desktop,
                &HookTransport {
                    pid: Some(18140),
                    ..Default::default()
                },
                "codex",
                "01a0d10c"
            ),
            "chatgpt"
        );
    }

    #[test]
    fn a_hook_discovered_pane_panel_never_claims_desktop_attribution() {
        // A row re-filed under chatgpt but resolved to a pane transport is not
        // desktop evidence; only rows materialized with the thread transport
        // re-file events to ChatGPT, whatever the deck has selected.
        let mut pane_backed = desktop_thread_panel("01a0d10c");
        pane_backed.transport = PanelTransport::Herdr {
            socket_path: None,
            pane_id: "wK:p3J".into(),
        };
        let desktop = HookTransport {
            pid: Some(18140),
            ..Default::default()
        };

        assert!(!owns_desktop_thread(&[pane_backed], &desktop, "01a0d10c"));
    }
    #[test]
    fn desktop_hook_pid_preserves_thread_navigation_and_next_event_attribution() {
        let mut panel = desktop_thread_panel("desktop-thread");
        for pid in [100, 101] {
            panel.transport = panel_transport(
                HookTransport {
                    pid: Some(pid),
                    ..Default::default()
                },
                Some(&panel.transport),
            );
            assert!(
                matches!(&panel.transport, PanelTransport::CodexThread { thread_id } if thread_id == "desktop-thread")
            );
            assert!(owns_desktop_thread(
                &[panel.clone()],
                &HookTransport {
                    pid: Some(pid),
                    ..Default::default()
                },
                "desktop-thread"
            ));
        }
        panel.transport = panel_transport(cli_terminal_transport(), Some(&panel.transport));
        assert!(matches!(panel.transport, PanelTransport::Herdr { .. }));
    }
    #[test]
    fn identical_pane_names_on_different_sockets_are_not_deduplicated() {
        let default = PanelTransport::Herdr {
            socket_path: None,
            pane_id: "pane".into(),
        };
        let other = PanelTransport::Herdr {
            socket_path: Some("/tmp/review-other-herdr.sock".into()),
            pane_id: "pane".into(),
        };
        assert!(!same_panel_transport(&default, &other));
    }
}
#[test]
fn hook_without_pane_environment_preserves_a_known_navigation_target() {
    let target = PanelTransport::Herdr {
        socket_path: None,
        pane_id: "pane".into(),
    };
    let resolved = panel_transport(
        HookTransport {
            pid: Some(42),
            ..Default::default()
        },
        Some(&target),
    );
    assert!(same_panel_transport(&target, &resolved));
}
#[test]
fn a_new_prompt_clears_old_child_errors_only_for_its_session() {
    let mut jobs = vec![
        AgentJob {
            id: "child".into(),
            agent_id: "qodercli".into(),
            session_id: "s1".into(),
            status: JobStatus::Failed,
            cwd: None,
            last_event: "StopFailure".into(),
            updated_at_ms: 1,
            agent_type: None,
        },
        AgentJob {
            id: "other".into(),
            agent_id: "qodercli".into(),
            session_id: "s2".into(),
            status: JobStatus::Waiting,
            cwd: None,
            last_event: "PermissionRequest".into(),
            updated_at_ms: 1,
            agent_type: None,
        },
    ];
    reset_previous_turn(&mut jobs, "qodercli", "s1");
    assert_eq!(jobs[0].status, JobStatus::Interrupted);
    assert_eq!(jobs[1].status, JobStatus::Waiting);
}
