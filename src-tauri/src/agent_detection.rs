//! Agent process detection.
//!
//! Each agent declares a [`DetectionPattern`] describing how to recognise its
//! process in the system process list. The built-in patterns mirror what a
//! future `plugin.json` `detection` field would provide, so migrating to a
//! plugin-driven registry later is a matter of loading the same struct from
//! JSON instead of constructing it here.

use std::path::Path;

use sysinfo::System;

/// Declarative process-match description for one agent.
///
/// A process matches when **any** of the positive signals fire **and** none of
/// the exclusion signals fire. This is the same shape the plugin descriptor
/// will expose.
pub struct DetectionPattern {
    /// Process-name matches (case-insensitive).
    pub executable_names: &'static [&'static str],
    /// Substring matches against any command-line argument.
    pub argv_patterns: &'static [&'static str],
    /// Case-insensitive filename match against any argument path.
    pub argv_filename_names: &'static [&'static str],
    /// If any argument contains one of these substrings the process is
    /// excluded even when a positive signal matched.
    pub exclude_argv_patterns: &'static [&'static str],
}

impl DetectionPattern {
    fn matches(&self, process: &sysinfo::Process) -> bool {
        let name = process.name().to_string_lossy();
        let arguments: Vec<_> = process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect();

        // Exclusion check first — cheaper and avoids false positives.
        if self
            .exclude_argv_patterns
            .iter()
            .any(|pattern| arguments.iter().any(|arg| arg.contains(pattern)))
        {
            return false;
        }

        // Process name match.
        if self
            .executable_names
            .iter()
            .any(|expected| name.eq_ignore_ascii_case(expected))
        {
            return true;
        }

        // Argument substring match.
        if self
            .argv_patterns
            .iter()
            .any(|pattern| arguments.iter().any(|arg| arg.contains(pattern)))
        {
            return true;
        }

        // Argument filename match (skip URL-like arguments).
        if arguments.iter().any(|arg| {
            !arg.contains("://")
                && Path::new(arg.as_ref())
                    .file_name()
                    .is_some_and(|file_name| {
                        self.argv_filename_names
                            .iter()
                            .any(|expected| file_name.eq_ignore_ascii_case(expected))
                    })
        }) {
            return true;
        }

        false
    }
}

/// Look up the built-in detection pattern for `agent_id`.
///
/// Returns `None` for agents that use purely generic name matching (herdr)
/// — callers should fall back to comparing the process name against the agent's
/// configured `executable` field.
fn built_in_pattern(agent_id: &str) -> Option<&'static DetectionPattern> {
    static QWEN: DetectionPattern = DetectionPattern {
        executable_names: &["qwen"],
        argv_patterns: &[
            "/@qwen-code/qwen-code/",
            "/.qwen/updates/",
            "/qwen-code/lib/cli-entry.js",
        ],
        argv_filename_names: &["qwen"],
        exclude_argv_patterns: &[],
    };
    static CODEX: DetectionPattern = DetectionPattern {
        executable_names: &["codex"],
        argv_patterns: &[],
        argv_filename_names: &["codex"],
        exclude_argv_patterns: &["/ChatGPT.app/"],
    };
    static CLAUDE: DetectionPattern = DetectionPattern {
        executable_names: &["claude"],
        argv_patterns: &[],
        argv_filename_names: &["claude"],
        exclude_argv_patterns: &[],
    };
    static DSH: DetectionPattern = DetectionPattern {
        executable_names: &["dsh", "deepseek-harness"],
        argv_patterns: &["/@deepseek-ai/dsh/"],
        argv_filename_names: &["dsh"],
        exclude_argv_patterns: &[],
    };

    match agent_id {
        "qwen" => Some(&QWEN),
        "codex" => Some(&CODEX),
        "claude" => Some(&CLAUDE),
        "dsh" => Some(&DSH),
        _ => None,
    }
}

/// Return `true` when `process` looks like a running instance of `agent_id`.
///
/// Uses the agent-specific [`DetectionPattern`] when one is registered,
/// otherwise falls back to a generic executable-name comparison against
/// `executable` (the value from the agent's built-in definition or plugin
/// descriptor).
pub fn process_matches(process: &sysinfo::Process, agent_id: &str, executable: &str) -> bool {
    if let Some(pattern) = built_in_pattern(agent_id) {
        return pattern.matches(process);
    }
    // Herdr is a terminal multiplexer, so its managed panes inherit
    // HERDR_* variables. Do not mistake an arbitrary command running inside
    // such a pane (for example `npm run tauri:dev`) for the Herdr process
    // merely because sysinfo reports the pane's process name ambiguously.
    if agent_id == "herdr" {
        return process
            .exe()
            .and_then(|path| path.file_name())
            .is_some_and(|name| name.eq_ignore_ascii_case("herdr"));
    }
    // Generic fallback: match the configured executable name.
    let name = process.name().to_string_lossy();
    if name.eq_ignore_ascii_case(executable) {
        return true;
    }
    process
        .cmd()
        .iter()
        .filter(|part| !part.to_string_lossy().contains("://"))
        .filter_map(|part| Path::new(part).file_name())
        .any(|file_name| file_name.to_string_lossy().eq_ignore_ascii_case(executable))
}

/// Read an environment variable value from a process's `/proc`-style environ.
pub fn environment_value(process: &sysinfo::Process, key: &str) -> Option<String> {
    process.environ().iter().find_map(|entry| {
        let entry = entry.to_string_lossy();
        entry
            .strip_prefix(key)
            .and_then(|value| value.strip_prefix('='))
            .map(str::to_owned)
    })
}

/// Find the terminal-app bundle id hosting `pid`, walking up the process tree.
///
/// Handles tmux: when the traversal reaches a `tmux` server or client, it
/// redirects to a tmux client and continues walking up from there to find the
/// real terminal emulator.
#[cfg(target_os = "macos")]
pub fn terminal_host_bundle_for_pid(pid: u32) -> Option<&'static str> {
    let system = System::new_all();
    let mut current = sysinfo::Pid::from_u32(pid);

    for _ in 0..12 {
        let process = system.process(current)?;
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        let executable = process
            .exe()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();

        // tmux client or server — redirect to a client's terminal.
        if name == "tmux" || name == "tmux:" || executable.contains("/tmux") {
            if let Some(client_bundle) = find_tmux_client_terminal(&system) {
                return Some(client_bundle);
            }
            // No client found; fall through to normal parent walk.
        }

        let bundle = match_terminal_bundle(&name, &executable);
        if bundle.is_some() {
            return bundle;
        }
        current = process.parent()?;
    }
    None
}

#[cfg(target_os = "macos")]
fn match_terminal_bundle(name: &str, executable: &str) -> Option<&'static str> {
    if name == "alacritty" || executable.contains("/Alacritty.app/") {
        Some("org.alacritty")
    } else if name == "terminal" || executable.contains("/Terminal.app/") {
        Some("com.apple.Terminal")
    } else if name == "iterm2"
        || executable.contains("/iTerm.app/")
        || executable.contains("/iTerm2.app/")
    {
        Some("com.googlecode.iterm2")
    } else if name == "warp" || executable.contains("/Warp.app/") {
        Some("dev.warp.Warp-Stable")
    } else if executable.contains("/Visual Studio Code.app/") {
        Some("com.microsoft.VSCode")
    } else {
        None
    }
}

/// Raise the terminal window that is likely hosting `agent_id`. Bundle
/// activation alone is insufficient when several Alacritty/iTerm windows are
/// open; CLI agents commonly put their name in the terminal title.
#[cfg(target_os = "macos")]
pub fn focus_terminal_window_with_hints(
    agent_id: &str,
    bundle: &str,
    extra_hints: &[String],
) -> bool {
    let process = match bundle {
        "org.alacritty" => "Alacritty",
        "com.googlecode.iterm2" => "iTerm2",
        "com.apple.Terminal" => "Terminal",
        "dev.warp.Warp-Stable" => "Warp",
        "com.microsoft.VSCode" => "Code",
        _ => return false,
    };
    let hints: &[&str] = match agent_id {
        "qwen" => &["qwen"],
        "claude" => &["claude"],
        "herdr" => &["herdr", "topeM1", "["],
        _ => &[],
    };
    if hints.is_empty() && extra_hints.is_empty() {
        return false;
    }
    let escaped_process = process.replace('"', "\\\"");
    let mut hint_values = hints
        .iter()
        .map(|hint| (*hint).to_string())
        .collect::<Vec<_>>();
    hint_values.extend(extra_hints.iter().cloned());
    let conditions = hint_values
        .iter()
        .map(|hint| {
            let escaped = hint.replace('"', "\\\"");
            format!("windowName contains \"{}\"", escaped)
        })
        .collect::<Vec<_>>()
        .join(" or ");
    let script = format!(
        r#"tell application "System Events"
    tell process "{}"
        set frontmost to true
        repeat with w in windows
            set windowName to name of w
            if {} then
                perform action "AXRaise" of w
                return true
            end if
        end repeat
    end tell
end tell
return false"#,
        escaped_process, conditions
    );
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
}

/// Walk all processes looking for a tmux client, then walk up from the client
/// to find the hosting terminal.
#[cfg(target_os = "macos")]
fn find_tmux_client_terminal(system: &System) -> Option<&'static str> {
    let tmux_client = system.processes().iter().find(|(_, process)| {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        (name == "tmux" || name == "tmux:")
            && environment_value(process, "TMUX").is_some()
            && process.parent().is_some_and(|parent| {
                let parent_name = system
                    .process(parent)
                    .map(|p| p.name().to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                // A tmux client's parent is typically a shell, NOT another tmux.
                parent_name != "tmux" && parent_name != "tmux:"
            })
    });

    let (_, client_process) = tmux_client?;
    let mut current = client_process.parent()?;

    for _ in 0..12 {
        let process = system.process(current)?;
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        let executable = process
            .exe()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();

        let bundle = match_terminal_bundle(&name, &executable);
        if bundle.is_some() {
            return bundle;
        }
        current = process.parent()?;
    }
    None
}
