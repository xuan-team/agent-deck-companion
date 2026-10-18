use std::sync::Arc;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::state::{
    now_ms, report_discovered_panel_status, AgentStatus, AppRuntime, PanelTransport,
};

pub fn reconcile_processes(state: &Arc<AppRuntime>, system: &System) -> bool {
    let overrides = state.overrides.lock().expect("overrides poisoned").clone();
    let hook_statuses = state
        .hook_statuses
        .lock()
        .expect("hook statuses poisoned")
        .clone();
    let active = state
        .snapshot
        .read()
        .unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        })
        .active_agent
        .clone();
    let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
        eprintln!("[state] snapshot write lock poisoned, recovering");
        poisoned.into_inner()
    });
    // Retire a switched-off agent's rows before the vectors below are derived
    // from them, so nothing downstream can relight it from a stale panel.
    crate::state::prune_disabled_agents(&mut snapshot);
    let previous = snapshot.agents.clone();
    // An agent that owns a deck slot and currently has a live hook-backed
    // panel notifies through its own light, so it must not also light the
    // Herdr slot. Discovery-only `herdr:` panels stay in the Herdr aggregate
    // for agents without such a channel.
    let assigned_agents = snapshot
        .agent_slots
        .iter()
        .map(|slot| slot.agent_id.clone())
        .collect::<Vec<_>>();
    let channel_agents = snapshot
        .active_panels
        .iter()
        .filter(|panel| {
            panel.status != AgentStatus::Offline
                && !panel.id.starts_with("herdr:")
                && panel.agent_id != "herdr"
                && assigned_agents.contains(&panel.agent_id)
        })
        .map(|panel| panel.agent_id.clone())
        .collect::<Vec<_>>();
    let live_panels = snapshot
        .active_panels
        .iter()
        .filter(|panel| panel.status != AgentStatus::Offline)
        .map(|panel| {
            let is_herdr = matches!(panel.transport, PanelTransport::Herdr { .. })
                && !channel_agents.contains(&panel.agent_id);
            (panel.agent_id.clone(), panel.status, is_herdr)
        })
        .collect::<Vec<_>>();

    // A cached thread cannot prove the Desktop app is still running. Check
    // the real host before considering its panels/hooks, otherwise a first
    // discovery makes it immortal and leaves the app-server worker alive.
    let desktop_pid = system
        .processes()
        .iter()
        .find(|(_, process)| {
            process.exe().is_some_and(|path| {
                let path = path.to_string_lossy();
                path.ends_with("/Codex.app/Contents/MacOS/Codex")
                    || path.ends_with("/ChatGPT.app/Contents/MacOS/ChatGPT")
            })
        })
        .map(|(pid, _)| pid.as_u32());
    if desktop_pid.is_none() {
        for panel in &mut snapshot.active_panels {
            if panel.agent_id == "chatgpt"
                && matches!(panel.transport, PanelTransport::CodexThread { .. })
            {
                panel.status = AgentStatus::Offline;
            }
        }
    }

    for agent in &mut snapshot.agents {
        if agent.id == "companion" {
            agent.status = AgentStatus::Idle;
            agent.running = false;
            agent.pid = None;
            continue;
        }
        // Monitoring is off for this agent: do not scan the process table or
        // read its storage, which is the whole point of the switch.
        if !agent.enabled {
            agent.status = AgentStatus::Offline;
            agent.running = false;
            agent.pid = None;
            agent.task = None;
            agent.workspace = None;
            continue;
        }
        if agent.id == "chatgpt" {
            agent.pid = desktop_pid;
            agent.running = desktop_pid.is_some();
            agent.status = if agent.running {
                live_panels
                    .iter()
                    .filter(|(id, _, _)| id == "chatgpt")
                    .map(|(_, status, _)| *status)
                    .max_by_key(|status| status.priority())
                    .unwrap_or(AgentStatus::Unknown)
            } else {
                AgentStatus::Offline
            };
            continue;
        }
        let live_panel_status = live_panels
            .iter()
            .filter(|(id, _, is_herdr)| id == &agent.id || (agent.id == "herdr" && *is_herdr))
            .map(|(_, status, _)| *status)
            .max_by_key(|status| status.priority());
        let has_live_panel = live_panel_status.is_some();
        let has_recent_hook = hook_statuses
            .get(&agent.id)
            .is_some_and(|(_, updated_at)| now_ms().saturating_sub(*updated_at) < 60_000);

        if has_live_panel || has_recent_hook {
            if has_live_panel {
                agent.running = true;
            }
            if let Some(status) = live_panel_status {
                agent.status = status;
            } else if let Some((status, _)) = hook_statuses.get(&agent.id) {
                agent.status = *status;
            }
            continue;
        }

        #[cfg(target_os = "macos")]
        let desktop_chatgpt = (agent.id == "chatgpt").then(|| {
            system.processes().iter().find(|(_, process)| {
                process.name().eq_ignore_ascii_case("ChatGPT")
                    && process.cmd().first().is_some_and(|executable| {
                        executable
                            .to_string_lossy()
                            .ends_with("/ChatGPT.app/Contents/MacOS/ChatGPT")
                    })
            })
        });

        #[cfg(not(target_os = "macos"))]
        let desktop_chatgpt = None;

        let command_line_agent = system.processes().iter().find(|(_, process)| {
            crate::agent_detection::process_matches(process, &agent.id, &agent.executable)
                || state.plugins.process_matches(
                    &agent.id,
                    &process.name().to_string_lossy(),
                    &process
                        .cmd()
                        .iter()
                        .map(|part| part.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                )
        });

        let detected = desktop_chatgpt.flatten().or(command_line_agent);

        agent.pid = detected.map(|(pid, _)| pid.as_u32());
        agent.running = detected.is_some();
        agent.status = if let Some(status) = overrides.get(&agent.id) {
            agent.running = *status != AgentStatus::Offline;
            *status
        } else if let Some((status, updated_at)) = hook_statuses.get(&agent.id) {
            if now_ms().saturating_sub(*updated_at) < 6 * 60 * 60 * 1000 {
                agent.running = true;
                *status
            } else if !agent.running {
                AgentStatus::Offline
            } else {
                AgentStatus::Idle
            }
        } else if !agent.running {
            AgentStatus::Offline
        } else if active.as_deref() == Some(agent.id.as_str())
            && crate::agent_actions::preferred_surface(&agent.id)
                != Some(crate::agent_actions::AgentSurface::Desktop)
        {
            // A running desktop app (e.g. ChatGPT Desktop) gives no "busy"
            // signal; only hooks/panels may claim Working.
            AgentStatus::Working
        } else {
            AgentStatus::Idle
        };

        if agent.id == "dsh" && agent.running && !overrides.contains_key("dsh") {
            if let Some(dsh_status) = crate::dsh::read_status() {
                agent.status = match dsh_status.status {
                    AgentStatus::Offline => AgentStatus::Idle,
                    other => other,
                };
                agent.task = dsh_status.task;
                agent.workspace = dsh_status.workspace;
            }
        }
    }

    // Sync DSH sessions into active_panels when DSH is running.
    let dsh_running = snapshot.agents.iter().any(|a| a.id == "dsh" && a.running);
    if dsh_running {
        sync_dsh_panels(&mut snapshot);
    }

    snapshot.updated_at_ms = now_ms();
    snapshot.agents != previous
}

pub fn refresh_process_table(system: &mut System) {
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::OnlyIfNotSet),
    );
}

pub fn reconcile_processes_once(state: &Arc<AppRuntime>) -> bool {
    let mut system = System::new();
    refresh_process_table(&mut system);
    reconcile_processes(state, &system)
}

pub fn poll_dsh_status(state: &Arc<AppRuntime>) -> bool {
    // A switched-off DSH must not be probed at all: no storage read, no file
    // watch, no panel sync.
    if !state.agent_enabled("dsh") {
        return false;
    }
    if state
        .overrides
        .lock()
        .is_ok_and(|overrides| overrides.contains_key("dsh"))
    {
        return false;
    }
    let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
        eprintln!("[state] snapshot write lock poisoned, recovering");
        poisoned.into_inner()
    });
    let Some(agent_index) = snapshot.agents.iter().position(|agent| agent.id == "dsh") else {
        return false;
    };
    if !snapshot.agents[agent_index].running {
        return false;
    }
    let mut changed = false;
    // The per-session projection files are the live source; `read_status` also
    // falls back to the aggregated cache for builds that only write that file.
    // A DSH with no sessions yet is not an error: the panel sync below still
    // retires rows for sessions that were deleted.
    if let Some(dsh_status) = crate::dsh::read_status() {
        let status = match dsh_status.status {
            AgentStatus::Offline => AgentStatus::Idle,
            other => other,
        };
        let agent = &mut snapshot.agents[agent_index];
        if agent.status != status
            || agent.task != dsh_status.task
            || agent.workspace != dsh_status.workspace
        {
            agent.status = status;
            agent.task = dsh_status.task;
            agent.workspace = dsh_status.workspace;
            changed = true;
        }
    }

    // Sync DSH sessions into active_panels on every tick, independently of the
    // agent-level status: otherwise deleting the last session left its row on
    // screen until the next minute-long process reconcile.
    let panels_before = snapshot
        .active_panels
        .iter()
        .filter(|p| p.agent_id == "dsh" && p.status != AgentStatus::Offline)
        .map(|p| (p.id.clone(), p.slot, p.status, p.label.clone()))
        .collect::<Vec<_>>();
    sync_dsh_panels(&mut snapshot);
    let panels_after = snapshot
        .active_panels
        .iter()
        .filter(|p| p.agent_id == "dsh" && p.status != AgentStatus::Offline)
        .map(|p| (p.id.clone(), p.slot, p.status, p.label.clone()))
        .collect::<Vec<_>>();
    if panels_before != panels_after {
        changed = true;
    }

    if changed {
        snapshot.updated_at_ms = now_ms();
    }
    changed
}

/// Sync DSH sessions from the projection cache into active_panels.
///
/// Creates/updates panels for the top 3 most recent non-blank sessions.
/// Marks stale DSH panels as Offline when their session no longer exists.
fn sync_dsh_panels(snapshot: &mut crate::state::AppSnapshot) {
    let sessions = crate::dsh::read_sessions();
    sync_dsh_panels_from(snapshot, &sessions);
}

fn sync_dsh_panels_from(
    snapshot: &mut crate::state::AppSnapshot,
    sessions: &[crate::dsh::DshSessionInfo],
) {
    use crate::state::{ActivePanel, PanelTransport};

    // The three hardware slots are a fresh projection of the current recency
    // order. Keeping old DSH panels live made stale slots win after sorting,
    // so the deck could continue showing an older agent's/session's labels.
    // Panels that are being re-listed keep their previous status, because that
    // is what lets the update below notice a working -> idle completion.
    let listed = sessions
        .iter()
        .take(3)
        .map(|session| session.session_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    for panel in &mut snapshot.active_panels {
        if let PanelTransport::DshWeb { session_id } = &panel.transport {
            if !listed.contains(session_id.as_str()) {
                panel.status = AgentStatus::Offline;
            }
        }
    }

    // Create or update panels for the top 3 sessions.
    for (index, session) in sessions.iter().take(3).enumerate() {
        let panel_id = format!("dsh:{}", session.session_id);
        let label = session
            .title
            .clone()
            .or_else(|| {
                session
                    .workspace
                    .as_ref()
                    .and_then(|path| std::path::Path::new(path).file_name())
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| {
                format!(
                    "Session {}",
                    &session.session_id[..8.min(session.session_id.len())]
                )
            });

        // Find existing panel for this session.
        let existing = snapshot.active_panels.iter_mut().find(|p| p.id == panel_id);

        if let Some(panel) = existing {
            // Update existing panel.
            panel.agent_id = "dsh".into();
            panel.session_id = session.session_id.clone();
            panel.slot = (index + 1) as u8;
            panel.label = label;
            panel.cwd = session.workspace.clone();
            panel.jobs_working = u16::from(session.status == AgentStatus::Working);
            panel.jobs_waiting = u16::from(matches!(
                session.status,
                AgentStatus::Idle | AgentStatus::NeedsInput
            ));
            panel.jobs_error = u16::from(session.status == AgentStatus::Error);
            panel.updated_at_ms = now_ms();
            panel.last_activity_ms = panel
                .last_activity_ms
                .max(u128::from(session.last_prompt_at));
            panel.transport = PanelTransport::DshWeb {
                session_id: session.session_id.clone(),
            };
            report_discovered_panel_status(
                &mut snapshot.agents,
                snapshot.active_agent.as_deref(),
                panel,
                session.status,
            );
        } else {
            snapshot.active_panels.push(ActivePanel {
                id: panel_id,
                agent_id: "dsh".into(),
                session_id: session.session_id.clone(),
                slot: (index + 1) as u8,
                label,
                status: session.status,
                cwd: session.workspace.clone(),
                jobs_working: u16::from(session.status == AgentStatus::Working),
                jobs_waiting: u16::from(matches!(
                    session.status,
                    AgentStatus::Idle | AgentStatus::NeedsInput
                )),
                jobs_error: u16::from(session.status == AgentStatus::Error),
                updated_at_ms: now_ms(),
                // DSH reports when the session last took a prompt; that is the
                // row's real age, not the moment this poll first saw it.
                status_since_ms: 0,
                last_activity_ms: u128::from(session.last_prompt_at),
                first_seen_ms: now_ms(),
                transport: PanelTransport::DshWeb {
                    session_id: session.session_id.clone(),
                },
                unread: false,
                unread_since_ms: 0,
            });
        }
    }
}

#[cfg(target_os = "macos")]
pub fn sync_herdr_panels(
    snapshot: &mut crate::state::AppSnapshot,
    herdr_snapshot: &crate::herdr::HerdrSnapshot,
) {
    use crate::state::{ActivePanel, AgentStatus, PanelTransport};

    // Herdr hosts other agents, so a disabled agent must not have its rows
    // recreated here just because its pane is still alive in the multiplexer.
    let disabled = snapshot
        .agents
        .iter()
        .filter(|agent| !agent.enabled)
        .map(|agent| agent.id.clone())
        .collect::<Vec<_>>();

    if !herdr_snapshot.connected {
        // A missing snapshot is not evidence that every pane has exited.
        // Keep rows available with an unknown, stale status until reconnect.
        for panel in &mut snapshot.active_panels {
            if panel.status != AgentStatus::Offline
                && matches!(&panel.transport, PanelTransport::Herdr { socket_path, .. }
                    if crate::herdr::socket_paths_match(socket_path.as_deref(), None))
            {
                panel.status = AgentStatus::Unknown;
            }
        }
        return;
    }

    let live_pane_ids = herdr_snapshot
        .agents
        .iter()
        .map(|agent| agent.pane_id.as_str())
        .collect::<Vec<_>>();
    for panel in &mut snapshot.active_panels {
        if let PanelTransport::Herdr {
            socket_path,
            pane_id,
        } = &panel.transport
        {
            if !crate::herdr::socket_paths_match(socket_path.as_deref(), None) {
                continue;
            }
            if !live_pane_ids.contains(&pane_id.as_str()) {
                panel.status = AgentStatus::Offline;
            }
        }
    }
    for agent in &herdr_snapshot.agents {
        if disabled.contains(&agent.agent) {
            continue;
        }
        let id = format!("herdr:{}", agent.pane_id);
        let status = match agent.status.as_str() {
            "working" => AgentStatus::Working,
            "blocked" => AgentStatus::NeedsInput,
            "idle" | "done" => AgentStatus::Idle,
            "unknown" => AgentStatus::Unknown,
            _ => AgentStatus::Error,
        };
        // The shared OpenCode server may report a native session before Herdr
        // discovers its pane. Adopt that row only by the exact native identity.
        if let Some(native_id) = agent.native_session_id.as_deref() {
            for panel in snapshot.active_panels.iter_mut().filter(|p| {
                p.agent_id == agent.agent
                    && p.session_id == native_id
                    && matches!(p.transport, PanelTransport::Unknown)
            }) {
                panel.transport = PanelTransport::Herdr {
                    socket_path: None,
                    pane_id: agent.pane_id.clone(),
                };
            }
        }
        let matching_indices = snapshot
            .active_panels
            .iter()
            .enumerate()
            .filter_map(|(index, panel)| match &panel.transport {
                PanelTransport::Herdr {
                    socket_path,
                    pane_id,
                } if pane_id == &agent.pane_id
                    && crate::herdr::socket_paths_match(socket_path.as_deref(), None) =>
                {
                    Some(index)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if !matching_indices.is_empty() {
            // Prefer the hook-backed panel because it carries the real agent session ID.
            let canonical_index = matching_indices
                .iter()
                .copied()
                .find(|index| !snapshot.active_panels[*index].id.starts_with("herdr:"))
                .unwrap_or(matching_indices[0]);
            let slot = matching_indices
                .iter()
                .map(|index| snapshot.active_panels[*index].slot)
                .min()
                .expect("matching pane has at least one panel");
            for index in matching_indices {
                if index != canonical_index {
                    snapshot.active_panels[index].status = AgentStatus::Offline;
                }
            }
            let panel = &mut snapshot.active_panels[canonical_index];
            panel.slot = slot;
            // A hook-backed panel has already resolved its logical owner. This is
            // especially important for ChatGPT Desktop, whose Codex runtime can
            // appear as `codex` in Herdr discovery. Only discovery-only panels
            // should inherit the process-level agent classification.
            let discovery_only = panel.id.starts_with("herdr:");
            let same_logical_owner = panel.agent_id == agent.agent;
            if discovery_only {
                let session = agent.native_session_id.as_deref().unwrap_or(&agent.pane_id);
                if panel.session_id != session || !same_logical_owner {
                    panel.session_id = session.into();
                    panel.status = AgentStatus::Unknown;
                    panel.status_since_ms = 0;
                    panel.last_activity_ms = 0;
                    panel.first_seen_ms = now_ms();
                    panel.unread = false;
                    panel.unread_since_ms = 0;
                }
                panel.agent_id = agent.agent.clone();
            }
            // Only update status from Herdr if the panel hasn't been updated by a hook recently.
            // Hook-based status is more accurate and real-time.
            let native_jobs = snapshot
                .jobs
                .iter()
                .filter(|job| {
                    !discovery_only
                        && job.agent_id == panel.agent_id
                        && job.session_id == panel.session_id
                })
                .collect::<Vec<_>>();
            let native_status =
                crate::hooks::reduced_status(native_jobs.iter().copied(), AgentStatus::Idle);
            // Native failure / permission events remain authoritative until a
            // new native lifecycle event clears them. Herdr's idle is only a
            // process observation, not evidence that an API failure recovered.
            let native_authoritative = !native_jobs.is_empty()
                && (panel.agent_id == "opencode"
                    || matches!(native_status, AgentStatus::Error | AgentStatus::NeedsInput));
            if native_authoritative && panel.status != native_status {
                crate::state::report_panel_status(
                    &mut snapshot.agents,
                    snapshot.active_agent.as_deref(),
                    panel,
                    native_status,
                );
            }
            let hook_recent = native_authoritative
                || native_jobs
                    .iter()
                    .any(|job| now_ms().saturating_sub(job.updated_at_ms) < 10_000);
            // Unknown is absence of a Herdr observation, not evidence that a
            // hook-backed agent lost its last known state after ten seconds.
            if !hook_recent && (discovery_only || status != AgentStatus::Unknown) {
                report_discovered_panel_status(
                    &mut snapshot.agents,
                    snapshot.active_agent.as_deref(),
                    panel,
                    status,
                );
            }
            if discovery_only
                || same_logical_owner
                || (panel.agent_id == "qodercli" && agent.agent == "qoder")
            {
                panel.label = agent
                    .summary
                    .clone()
                    .or_else(|| agent.title.clone())
                    .unwrap_or_else(|| agent.display_name.clone());
                panel.cwd = agent.cwd.clone();
            }
            if native_authoritative && panel.agent_id != "opencode" {
                panel.jobs_working = native_jobs
                    .iter()
                    .filter(|j| j.status == crate::state::JobStatus::Running)
                    .count() as u16;
                panel.jobs_waiting = native_jobs
                    .iter()
                    .filter(|j| j.status == crate::state::JobStatus::Waiting)
                    .count() as u16;
                panel.jobs_error = native_jobs
                    .iter()
                    .filter(|j| j.status == crate::state::JobStatus::Failed)
                    .count() as u16;
            } else if !native_authoritative {
                panel.jobs_working = u16::from(agent.status == "working");
                panel.jobs_waiting =
                    u16::from(matches!(agent.status.as_str(), "blocked" | "idle" | "done"));
                panel.jobs_error = 0;
            }
            panel.updated_at_ms = now_ms();
        } else {
            let used = snapshot
                .active_panels
                .iter()
                .filter(|panel| {
                    panel.agent_id == agent.agent.as_str() && panel.status != AgentStatus::Offline
                })
                .map(|panel| panel.slot)
                .collect::<Vec<_>>();
            let slot = (1..=u8::MAX)
                .find(|slot| !used.contains(slot))
                .unwrap_or(u8::MAX);
            snapshot.active_panels.push(ActivePanel {
                id,
                agent_id: agent.agent.clone(),
                session_id: agent
                    .native_session_id
                    .clone()
                    .unwrap_or_else(|| agent.pane_id.clone()),
                slot,
                label: agent
                    .summary
                    .clone()
                    .or_else(|| agent.title.clone())
                    .unwrap_or_else(|| agent.display_name.clone()),
                status,
                cwd: agent.cwd.clone(),
                jobs_working: u16::from(agent.status == "working"),
                jobs_waiting: u16::from(matches!(
                    agent.status.as_str(),
                    "blocked" | "idle" | "done"
                )),
                jobs_error: 0,
                updated_at_ms: now_ms(),
                // Herdr's pane payload has no activity timestamp at all, so this
                // row's age stays unknown until a real status change lands.
                status_since_ms: 0,
                last_activity_ms: 0,
                first_seen_ms: now_ms(),
                transport: PanelTransport::Herdr {
                    socket_path: None,
                    pane_id: agent.pane_id.clone(),
                },
                unread: false,
                unread_since_ms: 0,
            });
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::{
        herdr::{HerdrAgentInfo, HerdrSnapshot},
        state::{ActivePanel, AppRuntime, PanelTransport},
    };

    fn herdr_agent(pane_id: &str, cwd: &str) -> HerdrAgentInfo {
        HerdrAgentInfo {
            native_session_id: None,
            pane_id: pane_id.into(),
            workspace_id: "workspace".into(),
            tab_id: "tab".into(),
            agent: "qwen".into(),
            display_name: "qwen".into(),
            status: "idle".into(),
            title: Some(format!("Qwen - {cwd}")),
            cwd: Some(format!("/work/{cwd}")),
            focused: false,
            attention: false,
            state_change_seq: 1,
            model: None,
            summary: None,
        }
    }

    fn panel(id: &str, session_id: &str, slot: u8, pane_id: &str) -> ActivePanel {
        ActivePanel {
            id: id.into(),
            agent_id: "qwen".into(),
            session_id: session_id.into(),
            slot,
            label: session_id.into(),
            status: AgentStatus::Idle,
            cwd: None,
            jobs_working: 0,
            jobs_waiting: 1,
            jobs_error: 0,
            updated_at_ms: 1,
            status_since_ms: 1,
            last_activity_ms: 0,
            first_seen_ms: 0,
            transport: PanelTransport::Herdr {
                socket_path: None,
                pane_id: pane_id.into(),
            },
            unread: false,
            unread_since_ms: 0,
        }
    }

    fn dsh_session(id: &str, title: &str, status: AgentStatus) -> crate::dsh::DshSessionInfo {
        crate::dsh::DshSessionInfo {
            session_id: id.into(),
            title: Some(title.into()),
            status,
            workspace: Some(format!("/work/{id}")),
            last_prompt_at: 1,
        }
    }

    fn dsh_panel(id: &str, slot: u8) -> ActivePanel {
        ActivePanel {
            id: format!("dsh:{id}"),
            agent_id: "dsh".into(),
            session_id: id.into(),
            slot,
            label: format!("old-{id}"),
            status: AgentStatus::Idle,
            cwd: None,
            jobs_working: 0,
            jobs_waiting: 1,
            jobs_error: 0,
            updated_at_ms: 1,
            status_since_ms: 1,
            last_activity_ms: 0,
            first_seen_ms: 0,
            transport: PanelTransport::DshWeb {
                session_id: id.into(),
            },
            unread: false,
            unread_since_ms: 0,
        }
    }

    #[test]
    fn qwen_expired_token_survives_idle_discovery_and_acknowledgement() {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.snapshot.read().unwrap().clone();
        snapshot.active_panels = vec![panel("qwen:live", "live", 1, "wK:pV")];
        snapshot.jobs = vec![crate::state::AgentJob {
            id: "qwen:live:main".into(),
            agent_id: "qwen".into(),
            session_id: "live".into(),
            status: crate::state::JobStatus::Failed,
            cwd: None,
            last_event: "StopFailure".into(),
            updated_at_ms: 1,
            agent_type: None,
        }];
        let herdr = HerdrSnapshot {
            connected: true,
            focused_pane_id: Some("wK:pV".into()),
            agents: vec![herdr_agent("wK:pV", "demo")],
        };
        sync_herdr_panels(&mut snapshot, &herdr);
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::Error);
        let version = snapshot.active_panels[0].status_since_ms;
        let key = crate::session_activity::session_key(&snapshot.active_panels[0]);
        snapshot.acknowledged_errors.insert(key, version);
        sync_herdr_panels(&mut snapshot, &herdr);
        assert_eq!(snapshot.active_panels[0].status_since_ms, version);
        assert!(crate::state::agent_error_acknowledged(&snapshot, "qwen"));
        snapshot.jobs[0].status = crate::state::JobStatus::Ready;
        sync_herdr_panels(&mut snapshot, &herdr);
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::Idle);
    }

    #[test]
    fn herdr_unknown_does_not_erase_hook_backed_status() {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.snapshot.write().unwrap();
        snapshot.active_panels = vec![ActivePanel {
            status: AgentStatus::NeedsInput,
            updated_at_ms: 1,
            ..panel("qwen:session-1", "session-1", 1, "wK:pV")
        }];
        sync_herdr_panels(
            &mut snapshot,
            &HerdrSnapshot {
                connected: true,
                focused_pane_id: None,
                agents: vec![HerdrAgentInfo {
                    status: "unknown".into(),
                    ..herdr_agent("wK:pV", "demo")
                }],
            },
        );
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::NeedsInput);
    }

    #[test]
    fn opencode_native_state_survives_discovery_and_late_pane_association() {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.snapshot.read().unwrap().clone();
        let mut native = panel("opencode:ses_root", "ses_root", 1, "native-pane");
        native.agent_id = "opencode".into();
        native.status = AgentStatus::NeedsInput;
        native.jobs_waiting = 2;
        native.transport = PanelTransport::Unknown;
        let key = crate::session_activity::session_key(&native);
        snapshot.active_panels = vec![native];
        snapshot.jobs = vec![crate::state::AgentJob {
            id: "opencode:ses_root:main".into(),
            agent_id: "opencode".into(),
            session_id: "ses_root".into(),
            status: crate::state::JobStatus::Waiting,
            cwd: None,
            last_event: "PermissionRequest".into(),
            updated_at_ms: 1,
            agent_type: None,
        }];
        let mut agent = herdr_agent("native-pane", "demo");
        agent.agent = "opencode".into();
        agent.native_session_id = Some("ses_root".into());
        let herdr = HerdrSnapshot {
            connected: true,
            focused_pane_id: None,
            agents: vec![agent],
        };
        sync_herdr_panels(&mut snapshot, &herdr);
        assert_eq!(snapshot.active_panels.len(), 1);
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::NeedsInput);
        assert_eq!(snapshot.active_panels[0].jobs_waiting, 2);
        assert_eq!(
            crate::session_activity::session_key(&snapshot.active_panels[0]),
            key
        );
        assert!(matches!(
            snapshot.active_panels[0].transport,
            PanelTransport::Herdr { .. }
        ));
        sync_herdr_panels(
            &mut snapshot,
            &HerdrSnapshot {
                connected: true,
                focused_pane_id: None,
                agents: vec![],
            },
        );
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::Offline);
    }

    #[test]
    fn discovered_native_session_identity_survives_restart_and_resets_for_replacement() {
        let mut snapshot = AppRuntime::new().read();
        snapshot.active_panels.clear();
        let mut agent = herdr_agent("native-pane", "same-project");
        agent.native_session_id = Some("11111111-1111-4111-8111-111111111111".into());
        let mut herdr = HerdrSnapshot {
            connected: true,
            focused_pane_id: None,
            agents: vec![agent],
        };
        sync_herdr_panels(&mut snapshot, &herdr);
        let key = crate::session_activity::session_key(&snapshot.active_panels[0]);
        let mut restarted = snapshot.active_panels[0].clone();
        restarted.first_seen_ms += 100;
        assert_eq!(crate::session_activity::session_key(&restarted), key);
        snapshot.active_panels[0].unread = true;
        snapshot.active_panels[0].unread_since_ms = 10;
        herdr.agents[0].native_session_id = Some("22222222-2222-4222-8222-222222222222".into());
        sync_herdr_panels(&mut snapshot, &herdr);
        assert_ne!(
            crate::session_activity::session_key(&snapshot.active_panels[0]),
            key
        );
        assert!(!snapshot.active_panels[0].unread);
        assert_eq!(snapshot.active_panels[0].status_since_ms, 0);
    }

    #[test]
    fn herdr_first_observation_is_stable_and_does_not_invent_activity_time() {
        let mut snapshot = AppRuntime::new().read();
        snapshot.active_panels.clear();
        let herdr = HerdrSnapshot {
            connected: true,
            agents: vec![herdr_agent("review-pane", "repo")],
            ..Default::default()
        };
        sync_herdr_panels(&mut snapshot, &herdr);
        assert!(snapshot.active_panels[0].first_seen_ms > 0);
        snapshot.active_panels[0].first_seen_ms = 123;
        sync_herdr_panels(&mut snapshot, &herdr);
        let rows = crate::state::desktop_sessions_for(&snapshot);
        assert_eq!(rows[0].observed_since_ms, 123);
        assert_eq!(rows[0].event_at_ms, 0);
        assert_eq!(snapshot.active_panels[0].last_activity_ms, 0);
    }

    #[test]
    fn dsh_sync_reassigns_top_three_slots_and_retires_stale_panels() {
        let mut snapshot = AppRuntime::new().read();
        snapshot.active_panels = vec![
            dsh_panel("old", 1),
            dsh_panel("second", 2),
            dsh_panel("first", 4),
            dsh_panel("fourth", 3),
        ];
        let sessions = vec![
            dsh_session("first", "First", AgentStatus::Working),
            dsh_session("second", "Second", AgentStatus::Idle),
            dsh_session("third", "Third", AgentStatus::NeedsInput),
            dsh_session("fourth", "Fourth", AgentStatus::Idle),
        ];

        sync_dsh_panels_from(&mut snapshot, &sessions);

        let mut live = snapshot
            .active_panels
            .iter()
            .filter(|panel| panel.agent_id == "dsh" && panel.status != AgentStatus::Offline)
            .collect::<Vec<_>>();
        live.sort_by_key(|panel| panel.slot);
        assert_eq!(
            live.iter()
                .map(|panel| panel.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        assert_eq!(live[0].jobs_working, 1);
        assert_eq!(live[2].jobs_waiting, 1);
        assert!(snapshot
            .active_panels
            .iter()
            .find(|panel| panel.session_id == "old")
            .is_some_and(|panel| panel.status == AgentStatus::Offline));
        assert!(snapshot
            .active_panels
            .iter()
            .find(|panel| panel.session_id == "fourth")
            .is_some_and(|panel| panel.status == AgentStatus::Offline));
    }

    #[test]
    fn herdr_sync_coalesces_hook_and_discovery_panels_by_pane() {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.active_panels = vec![
            panel("herdr:wK:pV", "wK:pV", 1, "wK:pV"),
            panel("qwen:session-1", "session-1", 2, "wK:pV"),
        ];

        sync_herdr_panels(
            &mut snapshot,
            &HerdrSnapshot {
                connected: true,
                focused_pane_id: None,
                agents: vec![herdr_agent("wK:pV", "nema-lamp")],
            },
        );

        let live = snapshot
            .active_panels
            .iter()
            .filter(|panel| panel.status != AgentStatus::Offline)
            .collect::<Vec<_>>();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, "qwen:session-1");
        assert_eq!(live[0].slot, 1);
    }

    #[test]
    fn a_pane_that_finished_while_another_agent_was_selected_is_reported() {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.active_agent = Some("chatgpt".into());
        snapshot.active_panels = vec![ActivePanel {
            status: AgentStatus::Working,
            ..panel("qwen:session-1", "session-1", 1, "wK:pV")
        }];

        sync_herdr_panels(
            &mut snapshot,
            &HerdrSnapshot {
                connected: true,
                focused_pane_id: None,
                agents: vec![HerdrAgentInfo {
                    status: "idle".into(),
                    ..herdr_agent("wK:pV", "nema-lamp")
                }],
            },
        );

        assert!(
            snapshot.active_panels[0].unread,
            "the row has to remember that its task completed"
        );
        assert!(
            snapshot
                .agents
                .iter()
                .any(|agent| agent.id == "qwen" && agent.unread),
            "the qwen key has to carry the news the user was not looking at"
        );
    }

    #[test]
    fn herdr_sync_marks_hook_panel_offline_when_pane_exits() {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        snapshot.active_panels = vec![panel("qwen:session-1", "session-1", 1, "wK:p1")];

        sync_herdr_panels(
            &mut snapshot,
            &HerdrSnapshot {
                connected: true,
                ..Default::default()
            },
        );

        assert_eq!(snapshot.active_panels[0].status, AgentStatus::Offline);
    }

    #[test]
    fn herdr_sync_preserves_chatgpt_owner_of_hook_backed_panel() {
        let mut snapshot = AppRuntime::new().read();
        let mut chatgpt_panel = panel("chatgpt:session-1", "session-1", 1, "wK:p1");
        chatgpt_panel.agent_id = "chatgpt".into();
        snapshot.active_panels = vec![chatgpt_panel];
        let mut discovered = herdr_agent("wK:p1", "repo");
        discovered.agent = "codex".into();
        let herdr = HerdrSnapshot {
            connected: true,
            focused_pane_id: Some("wK:p1".into()),
            agents: vec![discovered],
        };

        sync_herdr_panels(&mut snapshot, &herdr);

        assert_eq!(snapshot.active_panels.len(), 1);
        assert_eq!(snapshot.active_panels[0].agent_id, "chatgpt");
        assert_eq!(snapshot.active_panels[0].session_id, "session-1");
        assert_eq!(snapshot.active_panels[0].label, "session-1");
    }

    fn slots_config(agent_ids: [&str; 5]) -> crate::profile::AppConfig {
        use crate::profile::{AgentSlotConfig, AppConfig, Profile};
        let profile = Profile {
            name: "test".into(),
            agent_slots: crate::profile::AGENT_SLOTS
                .iter()
                .zip(agent_ids)
                .map(|(slot, agent_id)| {
                    (
                        (*slot).into(),
                        AgentSlotConfig {
                            agent_id: agent_id.into(),
                        },
                    )
                })
                .collect(),
            global_keys: Default::default(),
            agent_keys: Default::default(),
        };
        AppConfig {
            version: 3,
            active_profile: "default".into(),
            profiles: [("default".to_string(), profile)].into_iter().collect(),
        }
    }

    fn runtime_with_slots(agent_ids: [&str; 5]) -> Arc<AppRuntime> {
        let runtime = Arc::new(AppRuntime::new());
        crate::state::replace_config(&runtime, slots_config(agent_ids));
        runtime
    }

    #[test]
    fn herdr_light_excludes_agents_assigned_to_a_slot_with_a_live_channel() {
        let runtime = runtime_with_slots(["qwen", "herdr", "chatgpt", "claude", "dsh"]);
        {
            let mut snapshot = runtime
                .snapshot
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // A hook-backed qwen panel coalesced onto its Herdr pane is the
            // notification channel; the discovery panel hosts a plain shell.
            let mut qwen = panel("qwen:session-1", "session-1", 2, "wK:pq");
            qwen.status = AgentStatus::NeedsInput;
            let mut shell = panel("herdr:wK:ps", "wK:ps", 3, "wK:ps");
            shell.agent_id = "herdr".into();
            snapshot.active_panels = vec![qwen, shell];
        }

        reconcile_processes(&runtime, &System::new());

        let snapshot = runtime.snapshot.read().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        });
        let herdr = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == "herdr")
            .expect("herdr agent");
        assert!(herdr.running);
        assert_eq!(
            herdr.status,
            AgentStatus::Idle,
            "a blocked qwen inside herdr must not light the herdr slot while qwen owns a slot"
        );
        let qwen = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == "qwen")
            .expect("qwen agent");
        assert_eq!(qwen.status, AgentStatus::NeedsInput);
    }

    #[test]
    fn herdr_light_still_counts_unslotted_agents() {
        // No slot is assigned to codex, so its pane inside herdr must still
        // notify through the herdr light.
        let runtime = runtime_with_slots(["qwen", "herdr", "chatgpt", "claude", "dsh"]);
        {
            let mut snapshot = runtime
                .snapshot
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut codex = panel("herdr:wK:pc", "wK:pc", 3, "wK:pc");
            codex.agent_id = "codex".into();
            codex.status = AgentStatus::NeedsInput;
            snapshot.active_panels = vec![codex];
        }

        reconcile_processes(&runtime, &System::new());

        let snapshot = runtime.snapshot.read().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        });
        let herdr = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == "herdr")
            .expect("herdr agent");
        assert_eq!(herdr.status, AgentStatus::NeedsInput);
    }

    #[test]
    fn herdr_light_excludes_slot_assigned_agent_with_off_pane_channel() {
        let runtime = runtime_with_slots(["qwen", "herdr", "chatgpt", "claude", "dsh"]);
        {
            let mut snapshot = runtime
                .snapshot
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // The qwen hook panel lost its Herdr pane headers, so discovery
            // surfaced a second live panel for the same agent on another pane.
            let mut qwen = panel("qwen:session-1", "session-1", 2, "wK:pq");
            qwen.transport = PanelTransport::Terminal { pid: 4321 };
            let mut delegated = panel("herdr:wK:px", "wK:px", 3, "wK:px");
            delegated.status = AgentStatus::NeedsInput;
            snapshot.active_panels = vec![qwen, delegated];
        }

        reconcile_processes(&runtime, &System::new());

        let snapshot = runtime.snapshot.read().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        });
        let herdr = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == "herdr")
            .expect("herdr agent");
        assert_ne!(
            herdr.status,
            AgentStatus::NeedsInput,
            "a qwen pane inside herdr must not notify through the herdr slot"
        );
        // The panel itself stays live so herdr keeps showing and switching to it.
        assert!(snapshot
            .active_panels
            .iter()
            .any(|panel| panel.id == "herdr:wK:px" && panel.status == AgentStatus::NeedsInput));
    }

    #[test]
    fn process_reconcile_uses_live_panel_status_without_a_recent_hook() {
        let runtime = Arc::new(AppRuntime::new());
        runtime
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .active_panels = vec![panel("herdr:wK:pV", "wK:pV", 1, "wK:pV")];

        reconcile_processes(&runtime, &System::new());

        let snapshot = runtime.snapshot.read().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot read lock poisoned, recovering");
            poisoned.into_inner()
        });
        let qwen = snapshot
            .agents
            .iter()
            .find(|agent| agent.id == "qwen")
            .expect("qwen agent");
        assert!(qwen.running);
        assert_eq!(qwen.status, AgentStatus::Idle);
    }
    #[test]
    fn cached_desktop_panels_do_not_keep_an_exited_app_alive() {
        let runtime = std::sync::Arc::new(AppRuntime::new());
        {
            let mut snapshot = runtime.snapshot.write().unwrap();
            let mut thread = panel("chatgpt:thread", "thread", 1, "unused");
            thread.agent_id = "chatgpt".into();
            thread.transport = PanelTransport::CodexThread {
                thread_id: "thread".into(),
            };
            thread.status = AgentStatus::Working;
            snapshot.active_panels.push(thread);
            let desktop = snapshot
                .agents
                .iter_mut()
                .find(|a| a.id == "chatgpt")
                .unwrap();
            desktop.running = true;
        }
        reconcile_processes(&runtime, &System::new());
        let snapshot = runtime.snapshot.read().unwrap();
        assert!(
            !snapshot
                .agents
                .iter()
                .find(|a| a.id == "chatgpt")
                .unwrap()
                .running
        );
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::Offline);
    }

    #[test]
    fn discovery_refresh_does_not_extend_the_age_of_a_hook() {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.snapshot.write().unwrap();
        let mut row = panel("qwen:session", "session", 1, "pane");
        row.status = AgentStatus::Working;
        row.updated_at_ms = now_ms();
        snapshot.active_panels.push(row);
        sync_herdr_panels(
            &mut snapshot,
            &HerdrSnapshot {
                connected: true,
                agents: vec![herdr_agent("pane", "project")],
                ..Default::default()
            },
        );
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::Idle);
        assert!(snapshot.active_panels[0].unread);
    }

    #[test]
    fn a_lost_herdr_snapshot_preserves_rows_without_claiming_live_work() {
        let runtime = AppRuntime::new();
        let mut snapshot = runtime.snapshot.write().unwrap();
        let mut row = panel("qwen:session", "session", 1, "pane");
        row.status = AgentStatus::Working;
        snapshot.active_panels.push(row);
        sync_herdr_panels(&mut snapshot, &HerdrSnapshot::default());
        assert_eq!(snapshot.active_panels.len(), 1);
        assert_eq!(snapshot.active_panels[0].status, AgentStatus::Unknown);
        assert!(!snapshot.active_panels[0].unread);
    }
}

#[cfg(target_os = "macos")]
pub fn refresh_herdr_state(app: &tauri::AppHandle, state: &Arc<AppRuntime>) -> bool {
    let herdr_snapshot = crate::herdr::session_snapshot();
    if state
        .snapshot
        .read()
        .is_ok_and(|snapshot| snapshot.herdr == herdr_snapshot)
    {
        return false;
    }
    {
        let mut snapshot = state.snapshot.write().unwrap_or_else(|poisoned| {
            eprintln!("[state] snapshot write lock poisoned, recovering");
            poisoned.into_inner()
        });
        crate::state::prune_disabled_agents(&mut snapshot);
        sync_herdr_panels(&mut snapshot, &herdr_snapshot);
        snapshot.herdr = herdr_snapshot;
        crate::state::refresh_live_agent_statuses(&mut snapshot);
        snapshot.updated_at_ms = now_ms();
    }
    crate::state::emit_state(app, state);
    true
}
