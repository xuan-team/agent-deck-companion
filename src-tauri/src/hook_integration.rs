use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::Serialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::plugin::PluginRegistry;

const MARKER: &str = "agent-deck-hook";
const OPENCODE_PLUGIN: &str = include_str!("../../integrations/opencode-v2/index.js");
const OPENCODE_TUI: &str = include_str!("../../integrations/opencode-v2/tui.js");
const OPENCODE_MARKER: &str =
    "// agent-deck-companion-opencode-v2 -- managed by Agent Deck Companion.";
static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize)]
pub struct HookIntegrationState {
    pub agent_id: String,
    pub display_name: String,
    pub installed: bool,
    pub config_path: String,
    pub detail: String,
    /// `hooks` (a lifecycle bridge written into an agent's own config) or
    /// `plugin` (installed into DSH's profile through DSH's own installer).
    /// The Hooks page renders both, but only `hooks` entries have mock actions.
    pub kind: &'static str,
}

#[derive(Clone)]
struct HookAdapter {
    id: String,
    display_name: String,
    config_relative: String,
    events: Vec<String>,
    handler_format: String,
}

fn home_dir() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "cannot locate the user home directory".to_string())
}

fn companion_dir(home: &Path) -> PathBuf {
    home.join(".agent-deck-companion")
}

fn bridge_path(home: &Path) -> PathBuf {
    companion_dir(home).join("bin").join(MARKER)
}

fn token_path(home: &Path) -> PathBuf {
    companion_dir(home).join("hook-token")
}

fn adapter(agent_id: &str) -> Result<HookAdapter, String> {
    let registry = PluginRegistry::load();
    let plugin = registry
        .get(agent_id)
        .ok_or_else(|| format!("unsupported hook agent: {agent_id}"))?;
    let hook = plugin
        .hooks
        .as_ref()
        .ok_or_else(|| format!("agent has no hook descriptor: {agent_id}"))?;
    Ok(HookAdapter {
        id: plugin.id.clone(),
        display_name: plugin.display_name.clone(),
        config_relative: hook.config_relative.clone(),
        events: hook.events.clone(),
        handler_format: hook.handler_format.clone(),
    })
}

fn read_json(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))
}

fn atomic_write_json(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid config path: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let original_permissions = path.metadata().ok().map(|metadata| metadata.permissions());
    if path.exists() {
        let backup = path.with_extension("json.agent-deck-backup");
        if !backup.exists() {
            fs::copy(path, &backup)
                .map_err(|error| format!("failed to back up {}: {error}", path.display()))?;
        }
    }
    let temp = path.with_extension(format!("json.agent-deck-{}.tmp", std::process::id()));
    let content = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    fs::write(&temp, content)
        .map_err(|error| format!("failed to write {}: {error}", temp.display()))?;
    if let Some(permissions) = original_permissions {
        fs::set_permissions(&temp, permissions)
            .map_err(|error| format!("failed to preserve config permissions: {error}"))?;
    }
    fs::rename(&temp, path)
        .map_err(|error| format!("failed to replace {}: {error}", path.display()))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("failed to secure {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

fn ensure_bridge(home: &Path) -> Result<PathBuf, String> {
    let bin_dir = companion_dir(home).join("bin");
    fs::create_dir_all(&bin_dir)
        .map_err(|error| format!("failed to create {}: {error}", bin_dir.display()))?;

    let token_file = token_path(home);
    if !token_file.exists() {
        fs::write(&token_file, Uuid::new_v4().to_string())
            .map_err(|error| format!("failed to create hook token: {error}"))?;
        set_mode(&token_file, 0o600)?;
    }

    let path = bridge_path(home);
    let token = shell_quote(&token_file.to_string_lossy());
    let allowed_agents = adapters()
        .iter()
        .map(|adapter| adapter.id.as_str())
        .collect::<Vec<_>>()
        .join("|");
    let script = format!(
        "#!/bin/sh\nagent=\"$1\"\ncase \"$agent\" in {allowed_agents}) ;; *) exit 0 ;; esac\ntoken=$(/bin/cat {token})\n/usr/bin/curl --silent --max-time 1 --output /dev/null --header \"content-type: application/json\" --header \"x-agent-deck-token: $token\" --header \"x-agent-deck-pid: $PPID\" --header \"x-agent-deck-herdr-socket: $HERDR_SOCKET_PATH\" --header \"x-agent-deck-herdr-pane: $HERDR_PANE_ID\" --header \"x-agent-deck-tmux: $TMUX\" --header \"x-agent-deck-tmux-pane: $TMUX_PANE\" --request POST --data-binary @- \"http://127.0.0.1:34115/api/hooks/$agent\" 2>/dev/null || true\nexit 0\n"
    );
    fs::write(&path, script).map_err(|error| format!("failed to write hook bridge: {error}"))?;
    set_mode(&path, 0o700)?;
    Ok(path)
}

pub fn refresh_bridge_if_present() -> Result<(), String> {
    let _guard = CONFIG_WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = home_dir()?;
    if bridge_path(&home).exists() {
        ensure_bridge(&home)?;
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn handler(adapter: &HookAdapter, bridge: &Path) -> Value {
    if adapter.handler_format == "claude" {
        json!({
            "type": "command",
            "command": bridge.to_string_lossy(),
            "args": [adapter.id],
            "async": true,
            "timeout": 2
        })
    } else if adapter.handler_format == "qwen" {
        json!({
            "type": "command",
            "command": format!("{} {}", shell_quote(&bridge.to_string_lossy()), adapter.id),
            "name": format!("Agent Deck Companion ({})", adapter.id),
            "async": true,
            "timeout": 2000
        })
    } else {
        json!({
            "type": "command",
            "command": format!("{} {}", shell_quote(&bridge.to_string_lossy()), adapter.id),
            "timeout": 2
        })
    }
}

fn is_ours(value: &Value, bridge: &Path) -> bool {
    let Some(command) = value.get("command").and_then(Value::as_str) else {
        return false;
    };
    let path = bridge.to_string_lossy();
    if command == path {
        return true;
    }
    // Legacy shell-form handlers are ours only when they invoke this exact
    // bridge with one supported agent, never because a script mentions a marker.
    adapters()
        .iter()
        .any(|adapter| command == format!("{} {}", shell_quote(&path), adapter.id))
}

fn remove_ours(value: &mut Value, bridge: &Path) {
    let Some(hooks) = value.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    hooks.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            return true;
        };
        let original_groups = groups.len();
        groups.retain_mut(|group| {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let original = handlers.len();
                handlers.retain(|handler| !is_ours(handler, bridge));
                !handlers.is_empty() || original == 0
            } else {
                true
            }
        });
        !groups.is_empty() || original_groups == 0
    });
    if hooks.is_empty() {
        value.as_object_mut().unwrap().remove("hooks");
    }
}

fn add_hooks(value: &mut Value, adapter: HookAdapter, bridge: &Path) -> Result<(), String> {
    if !value.is_object() {
        return Err("hook configuration root must be a JSON object".into());
    }
    remove_ours(value, bridge);
    let root = value.as_object_mut().expect("checked object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| "existing hooks setting must be a JSON object".to_string())?;
    for event in &adapter.events {
        let groups = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("existing hooks.{event} must be an array"))?;
        groups.push(json!({ "hooks": [handler(&adapter, bridge)] }));
    }
    Ok(())
}

fn config_path(home: &Path, adapter: &HookAdapter) -> PathBuf {
    home.join(&adapter.config_relative)
}

fn adapters() -> Vec<HookAdapter> {
    let registry = PluginRegistry::load();
    registry
        .all()
        .iter()
        .filter_map(|plugin| {
            let hook = plugin.hooks.as_ref()?;
            Some(HookAdapter {
                id: plugin.id.clone(),
                display_name: plugin.display_name.clone(),
                config_relative: hook.config_relative.clone(),
                events: hook.events.clone(),
                handler_format: hook.handler_format.clone(),
            })
        })
        .collect()
}

/// Hook integrations plus the DSH profile plugin.
///
/// The plugin is installed through DSH's own CLI rather than by editing an agent
/// config, so it carries `kind: "plugin"`; the page renders both in one list and
/// routes the button by kind.
pub fn integration_states_with_dsh(
    resource_dir: Option<&Path>,
) -> Result<Vec<HookIntegrationState>, String> {
    let mut states = integration_states_at(&home_dir()?)?;
    states.push(dsh_plugin_state(resource_dir));
    Ok(states)
}

pub fn dsh_plugin_state(resource_dir: Option<&Path>) -> HookIntegrationState {
    let dsh_home = crate::dsh::home_dir();
    let state = crate::dsh_plugin::state(&dsh_home, crate::dsh_plugin::bridge_dir(resource_dir));
    let config_path = match state.profile.as_deref() {
        Some(profile) => dsh_home.join("profiles").join(profile),
        None => dsh_home.join("profiles"),
    };
    HookIntegrationState {
        agent_id: crate::dsh_plugin::INTEGRATION_ID.into(),
        display_name: "DeepSeek Harness 插件".into(),
        installed: state.installed,
        config_path: config_path.to_string_lossy().into_owned(),
        detail: state.detail,
        kind: "plugin",
    }
}

fn integration_states_at(home: &Path) -> Result<Vec<HookIntegrationState>, String> {
    adapters()
        .into_iter()
        .map(|adapter| {
            let path = config_path(home, &adapter);
            let installed = if adapter.handler_format == "opencode_v2" {
                fs::read_to_string(&path).is_ok_and(|text| text == OPENCODE_PLUGIN)
                    && fs::read_to_string(path.with_file_name("tui.js"))
                        .is_ok_and(|text| text == OPENCODE_TUI)
            } else if path.exists() {
                let value = read_json(&path)?;
                value
                    .get("hooks")
                    .and_then(Value::as_object)
                    .is_some_and(|hooks| {
                        hooks
                            .values()
                            .filter_map(Value::as_array)
                            .flatten()
                            .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                            .flatten()
                            .any(|handler| is_ours(handler, &bridge_path(home)))
                    })
            } else {
                false
            };
            Ok(HookIntegrationState {
                agent_id: adapter.id,
                display_name: adapter.display_name,
                installed,
                config_path: path.to_string_lossy().into_owned(),
                detail: if installed {
                    if adapter.handler_format == "opencode_v2" {
                        "已安装 v2 事件插件；新启动的 OpenCode 加载后生效".into()
                    } else {
                        "已连接统一事件桥".into()
                    }
                } else {
                    "尚未安装".into()
                },
                kind: "hooks",
            })
        })
        .collect()
}

pub fn install(agent_id: &str) -> Result<Vec<HookIntegrationState>, String> {
    let _guard = CONFIG_WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = home_dir()?;
    install_at(&home, agent_id)?;
    integration_states_at(&home)
}

fn install_at(home: &Path, agent_id: &str) -> Result<(), String> {
    let adapter = adapter(agent_id)?;
    let bridge = ensure_bridge(home)?;
    let path = config_path(home, &adapter);
    if adapter.handler_format == "opencode_v2" {
        let files = [
            (path.clone(), OPENCODE_PLUGIN),
            (path.with_file_name("tui.js"), OPENCODE_TUI),
        ];
        for (file, _) in &files {
            if file.exists()
                && !fs::read_to_string(file)
                    .map_err(|e| e.to_string())?
                    .starts_with(OPENCODE_MARKER)
            {
                return Err(format!(
                    "refusing to overwrite an unrelated plugin: {}",
                    file.display()
                ));
            }
        }
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        for (file, content) in files {
            let temp = file.with_extension("js.agent-deck.tmp");
            fs::write(&temp, content).map_err(|e| e.to_string())?;
            set_mode(&temp, 0o600)?;
            fs::rename(temp, file).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    let mut value = read_json(&path)?;
    add_hooks(&mut value, adapter, &bridge)?;
    atomic_write_json(&path, &value)
}

pub fn uninstall(agent_id: &str) -> Result<Vec<HookIntegrationState>, String> {
    let _guard = CONFIG_WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let home = home_dir()?;
    let adapter = adapter(agent_id)?;
    let path = config_path(&home, &adapter);
    if adapter.handler_format == "opencode_v2" {
        let files = [path.clone(), path.with_file_name("tui.js")];
        for file in &files {
            if file.exists()
                && !fs::read_to_string(file)
                    .map_err(|e| e.to_string())?
                    .starts_with(OPENCODE_MARKER)
            {
                return Err("refusing to remove an unrelated OpenCode plugin".into());
            }
        }
        for file in files {
            if file.exists() {
                fs::remove_file(file).map_err(|e| e.to_string())?;
            }
        }
    } else if path.exists() {
        let mut value = read_json(&path)?;
        remove_ours(&mut value, &bridge_path(&home));
        atomic_write_json(&path, &value)?;
    }
    integration_states_at(&home)
}

pub fn token_matches(candidate: Option<&str>) -> bool {
    let Ok(home) = home_dir() else {
        return false;
    };
    let Ok(expected) = fs::read_to_string(token_path(&home)) else {
        return false;
    };
    candidate.is_some_and(|candidate| candidate.trim() == expected.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> PathBuf {
        std::env::temp_dir().join(format!("agent-deck-hooks-{}", Uuid::new_v4()))
    }

    #[test]
    fn opencode_v2_installs_plugin_without_touching_user_config() {
        let home = temp_home();
        let config = home.join(".config/opencode/opencode.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "{\"plugins\":[\"keep-me\"]}").unwrap();
        install_at(&home, "opencode").unwrap();
        install_at(&home, "opencode").unwrap();
        let path = config_path(&home, &adapter("opencode").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), OPENCODE_PLUGIN);
        assert_eq!(
            fs::read_to_string(config).unwrap(),
            "{\"plugins\":[\"keep-me\"]}"
        );
        assert!(integration_states_at(&home)
            .unwrap()
            .iter()
            .any(|s| s.agent_id == "opencode" && s.installed));
        fs::write(&path, "// user's unrelated plugin").unwrap();
        assert!(install_at(&home, "opencode").is_err());
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "// user's unrelated plugin"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn install_preserves_settings_and_is_idempotent() {
        let home = temp_home();
        let config = home.join(".claude/settings.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, r#"{"model":"sonnet","hooks":{"Stop":[{"hooks":[{"type":"command","command":"keep-me"}]}]}}"#).unwrap();

        install_at(&home, "claude").unwrap();
        install_at(&home, "claude").unwrap();
        let value = read_json(&config).unwrap();
        assert_eq!(value["model"], "sonnet");
        let text = serde_json::to_string(&value).unwrap();
        assert!(text.contains("keep-me"));
        let installed_count = value["hooks"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|groups| groups.as_array().unwrap())
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .filter(|handler| is_ours(handler, &bridge_path(&home)))
            .count();
        assert_eq!(installed_count, adapter("claude").unwrap().events.len());

        remove_ours(&mut read_json(&config).unwrap(), &bridge_path(&home));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn qoder_install_keeps_existing_settings_and_names_the_qoder_agent() {
        let home = temp_home();
        let config = home.join(".qoder/settings.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            r#"{"model":"auto","permissions":{"allow":["Bash"]}}"#,
        )
        .unwrap();

        install_at(&home, "qoder").unwrap();
        install_at(&home, "qoder").unwrap();
        let value = read_json(&config).unwrap();
        assert_eq!(value["model"], "auto");
        assert_eq!(value["permissions"]["allow"][0], "Bash");
        let handlers = value["hooks"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|groups| groups.as_array().unwrap())
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .filter(|handler| is_ours(handler, &bridge_path(&home)))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(handlers.len(), adapter("qoder").unwrap().events.len());
        // The bridge routes by its argument, so every handler must pass `qoder`.
        assert!(handlers
            .iter()
            .all(|handler| handler["args"].as_array().unwrap()[0] == "qoder"));

        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn uninstall_prunes_only_companion_handlers() {
        let home = temp_home();
        install_at(&home, "qwen").unwrap();
        let config = home.join(".qwen/settings.json");
        let mut value = read_json(&config).unwrap();
        remove_ours(&mut value, &bridge_path(&home));
        assert!(!serde_json::to_string(&value).unwrap().contains(MARKER));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn qoder_backup_and_removal_preserve_unrelated_marker_commands() {
        let home = temp_home();
        let config = home.join(".qoder/settings.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = json!({"model":"auto", "hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo agent-deck-hook"}]}], "Custom": []}});
        fs::write(&config, serde_json::to_vec(&original).unwrap()).unwrap();
        install_at(&home, "qoder").unwrap();
        install_at(&home, "qoder").unwrap();
        assert_eq!(
            read_json(&config.with_extension("json.agent-deck-backup")).unwrap(),
            original
        );
        let mut installed = read_json(&config).unwrap();
        remove_ours(&mut installed, &bridge_path(&home));
        assert_eq!(installed, original);
        fs::remove_dir_all(home).unwrap();
    }
}
