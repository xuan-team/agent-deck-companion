//! tmux session detection and pane focusing.
//!
//! When an agent runs inside a tmux pane, the companion can use `tmux`
//! commands to focus the correct pane — the same idea as the herdr
//! integration but using tmux's own CLI.

use std::process::Command;

use sysinfo::System;

use crate::agent_actions::{ActionPlan, KeyModifiers};
use crate::agent_detection;

#[derive(Debug, Clone)]
pub struct TmuxTarget {
    /// Path to the tmux socket (from the TMUX env var).
    pub socket_path: String,
    /// The tmux pane ID that contains the agent (e.g. "%3").
    pub pane_id: String,
    /// The PID of the tmux client process (used to find the terminal host).
    pub client_pid: u32,
}

/// Send an agent action directly to a tmux pane. This bypasses macOS global
/// keyboard routing and is deterministic even when several terminal windows
/// share the same Alacritty process.
pub fn send_plan(target: &TmuxTarget, plan: ActionPlan) -> bool {
    let send = |args: &[&str]| {
        Command::new("tmux")
            .args([
                "-S",
                target.socket_path.as_str(),
                "send-keys",
                "-t",
                target.pane_id.as_str(),
            ])
            .args(args)
            .status()
            .is_ok_and(|status| status.success())
    };
    match plan {
        ActionPlan::TextInput { text, submit } => {
            let text_ok = send(&["-l", text]);
            let submit_ok = !submit || send(&["Enter"]);
            text_ok && submit_ok
        }
        ActionPlan::Shortcut {
            key_code,
            modifiers,
        } => {
            let Some(key) = tmux_key_name(key_code) else {
                return false;
            };
            let mut owned = String::new();
            if modifiers.contains(KeyModifiers::CONTROL) {
                owned.push_str("C-");
            }
            if modifiers.contains(KeyModifiers::SHIFT) {
                owned.push_str("S-");
            }
            if modifiers.contains(KeyModifiers::OPTION) {
                owned.push_str("M-");
            }
            owned.push_str(key);
            send(&[owned.as_str()])
        }
        ActionPlan::HerdrPrefixedShortcut { .. } => false,
        // Web-composer plans never target a tmux pane; DSH web keys are
        // routed to the browser before direct-send is considered.
        ActionPlan::SlashCommand { .. } => false,
        ActionPlan::LeaderSequence { .. } => false,
        ActionPlan::ChatgptFork => false,
        ActionPlan::ChatgptVoiceHold => false,
        ActionPlan::ChatgptApprove => false,
        ActionPlan::ChatgptReject => false,
        ActionPlan::ChatgptFast => false,
        ActionPlan::ActivePanel { .. } => false,
    }
}

fn tmux_key_name(key_code: u16) -> Option<&'static str> {
    Some(match key_code {
        8 => "c",
        31 => "o",
        44 => "?",
        48 => "Tab",
        53 => "Escape",
        123 => "Left",
        124 => "Right",
        125 => "Down",
        126 => "Up",
        36 => "Enter",
        _ => return None,
    })
}

/// Try to find a tmux target for the given agent process.
///
/// Walks up from `agent_pid` looking for a process with the `TMUX` env var
/// set. If found, enumerates tmux panes to find the one whose tty matches
/// the agent's controlling tty.
pub fn find_target(agent_pid: u32) -> Option<TmuxTarget> {
    let system = System::new_all();

    // Find the agent's controlling tty by walking up the process tree.
    let target_tty = find_controlling_tty(&system, agent_pid)?;

    // Find a process with the TMUX env var (a tmux client or a process
    // inside tmux).
    let tmux_info = find_tmux_env(&system, agent_pid)?;

    // Enumerate panes and find the one matching the agent's tty.
    let pane_id = find_pane_for_tty(&tmux_info.socket_path, &target_tty)?;

    // Find the tmux client PID (for terminal host resolution).
    let client_pid = find_tmux_client_pid(&system, &tmux_info.socket_path)?;

    Some(TmuxTarget {
        socket_path: tmux_info.socket_path,
        pane_id,
        client_pid,
    })
}

/// Focus a tmux pane by sending `tmux select-window` + `tmux select-pane`.
pub fn focus_target(target: &TmuxTarget) -> bool {
    // select-window also focuses the pane in most configurations.
    let window_ok = Command::new("tmux")
        .args([
            "-S",
            &target.socket_path,
            "select-window",
            "-t",
            &target.pane_id,
        ])
        .output()
        .is_ok_and(|o| o.status.success());

    let pane_ok = Command::new("tmux")
        .args([
            "-S",
            &target.socket_path,
            "select-pane",
            "-t",
            &target.pane_id,
        ])
        .output()
        .is_ok_and(|o| o.status.success());

    window_ok || pane_ok
}

pub fn focus_pane(socket_path: &str, pane_id: &str) -> bool {
    focus_target(&TmuxTarget {
        socket_path: socket_path.into(),
        pane_id: pane_id.into(),
        client_pid: 0,
    })
}

/// Raise the terminal window that owns this tmux client. Multiple Alacritty
/// windows share one application bundle, so `open -b org.alacritty` only
/// activates whichever window was already frontmost. The tmux pane title is
/// reflected in the terminal window title and lets us raise the right one.
#[cfg(target_os = "macos")]
pub fn focus_host_window(target: &TmuxTarget) -> bool {
    let output = Command::new("tmux")
        .args([
            "-S",
            &target.socket_path,
            "display-message",
            "-t",
            &target.pane_id,
            "-p",
            "#{pane_title}",
        ])
        .output();
    let Ok(output) = output else { return false };
    if !output.status.success() {
        return false;
    }
    let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if title.is_empty() {
        return false;
    }
    let escaped = title.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        r#"set targetTitle to "{}"
tell application "System Events"
    tell process "Alacritty"
        repeat with w in windows
            if (name of w) contains targetTitle then
                perform action "AXRaise" of w
                return true
            end if
        end repeat
    end tell
end tell
return false"#,
        escaped
    );
    Command::new("osascript")
        .args(["-e", &script])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
}

// ── internals ────────────────────────────────────────────────────────

struct TmuxEnv {
    socket_path: String,
}

/// Walk up from `pid` looking for a process with the `TMUX` env var.
/// The TMUX var has the format `<socket_path>,<pid>,<offset>`.
fn find_tmux_env(system: &System, pid: u32) -> Option<TmuxEnv> {
    let mut current = sysinfo::Pid::from_u32(pid);
    for _ in 0..12 {
        let process = system.process(current)?;
        if let Some(tmux_var) = agent_detection::environment_value(process, "TMUX") {
            // TMUX = "<socket>,<server_pid>,<offset>"
            let socket_path = tmux_var.split(',').next()?.to_string();
            return Some(TmuxEnv { socket_path });
        }
        current = process.parent()?;
    }
    None
}

/// Find the controlling tty for a process by walking up the tree.
/// Returns the tty name like "ttys000".
fn find_controlling_tty(system: &System, pid: u32) -> Option<String> {
    let mut current = sysinfo::Pid::from_u32(pid);
    for _ in 0..12 {
        let process = system.process(current)?;
        if let Some(tty) = get_tty_for_pid(process.pid().as_u32()) {
            return Some(tty);
        }
        current = process.parent()?;
    }
    None
}

/// Get the controlling tty for a PID using `ps`.
fn get_tty_for_pid(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tty = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if tty.is_empty() || tty == "??" || tty == "?" {
        return None;
    }
    Some(tty)
}

/// Enumerate all tmux panes and find the one whose tty matches `target_tty`.
fn find_pane_for_tty(socket_path: &str, target_tty: &str) -> Option<String> {
    // List all panes across all sessions with their tty and pane_id.
    let output = Command::new("tmux")
        .args([
            "-S",
            socket_path,
            "list-panes",
            "-a",
            "-F",
            "#{pane_tty} #{pane_id}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let mut parts = line.splitn(2, ' ');
        let pane_tty = parts.next()?.trim();
        let pane_id = parts.next()?.trim();

        // pane_tty is a full path like "/dev/ttys000".
        // target_tty from sysinfo might be "ttys000" or "/dev/ttys000".
        if pane_tty.ends_with(target_tty) || target_tty.ends_with(pane_tty) {
            return Some(pane_id.to_string());
        }
    }
    None
}

/// Find a tmux client process connected to the given socket.
/// A client is a tmux process whose parent is NOT tmux (it's a shell).
fn find_tmux_client_pid(system: &System, socket_path: &str) -> Option<u32> {
    // The tmux client process does not consistently inherit TMUX (notably
    // when the server/client was started from a standalone terminal). Ask
    // tmux itself for the client PID first; this is authoritative for the
    // socket and avoids losing otherwise valid standalone tmux targets.
    if let Ok(output) = Command::new("tmux")
        .args(["-S", socket_path, "list-clients", "-F", "#{client_pid}"])
        .output()
    {
        if output.status.success() {
            if let Some(pid) = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .next()
            {
                return Some(pid);
            }
        }
    }

    // First try: find a tmux process with TMUX env matching our socket.
    for (_, process) in system.processes() {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if name != "tmux" && name != "tmux:" {
            continue;
        }
        if let Some(tmux_var) = agent_detection::environment_value(process, "TMUX") {
            if tmux_var.starts_with(socket_path) {
                // This is a tmux client — its parent should be a shell, not tmux.
                if process.parent().is_some_and(|parent| {
                    let parent_name = system
                        .process(parent)
                        .map(|p| p.name().to_string_lossy().to_ascii_lowercase())
                        .unwrap_or_default();
                    parent_name != "tmux" && parent_name != "tmux:"
                }) {
                    return Some(process.pid().as_u32());
                }
            }
        }
    }
    None
}
