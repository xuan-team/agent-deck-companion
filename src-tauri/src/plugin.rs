//! Safe, declarative third-party Agent plugins.
//!
//! Plugins describe detection and allowlisted input actions. They never carry
//! shell commands, AppleScript, or executable paths to run.

use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::agent_actions::{
    ActionCatalogItem, ActionPlan, AgentSurface, FunctionKeyState, KeyModifiers,
};

pub const MANIFEST_VERSION: u32 = 1;
const BUILTIN_IDS: &[&str] = &[
    "qwen",
    "herdr",
    "codex",
    "chatgpt",
    "claude",
    "dsh",
    "companion",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginManifest {
    pub manifest_version: u32,
    pub id: String,
    pub display_name: String,
    pub detection: Detection,
    pub surfaces: Vec<Surface>,
    pub default_surface: Option<Surface>,
    #[serde(default)]
    pub terminal_title_hints: Vec<String>,
    pub keymap: BTreeMap<String, BTreeMap<String, KeyBinding>>,
    #[serde(default)]
    pub built_in: bool,
    #[serde(default)]
    pub hooks: Option<HookDescriptor>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookDescriptor {
    pub config_relative: String,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default = "default_hook_format")]
    pub handler_format: String,
}

fn default_hook_format() -> String {
    "codex".into()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Detection {
    #[serde(default)]
    pub process_names: Vec<String>,
    #[serde(default)]
    pub argv_patterns: Vec<String>,
    #[serde(default)]
    pub exclude_argv_patterns: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Desktop,
    Terminal,
    HerdrPane,
    Web,
}

impl From<Surface> for AgentSurface {
    fn from(value: Surface) -> Self {
        match value {
            Surface::Desktop => Self::Desktop,
            Surface::Terminal => Self::Terminal,
            Surface::HerdrPane => Self::HerdrPane,
            Surface::Web => Self::Web,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeyBinding {
    pub label: String,
    pub hint: String,
    pub action: PluginAction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginAction {
    ActivePanel {
        slot: u8,
    },
    Shortcut {
        key: String,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    TextInput {
        text: String,
        #[serde(default)]
        submit: bool,
    },
    SlashCommand {
        text: String,
    },
    HerdrPrefix {
        key: String,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    Leader {
        leader: String,
        key: String,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    ChatgptFork,
    ChatgptVoiceHold,
    ChatgptApprove,
    ChatgptReject,
    ChatgptFast,
    VoiceInput,
    Disabled,
}

#[derive(Debug, Clone)]
pub struct PluginRegistry {
    plugins: Vec<PluginManifest>,
}

impl PluginRegistry {
    pub fn load() -> Self {
        let mut plugins = Vec::new();
        for contents in [
            include_str!("../../plugins/builtin/chatgpt/plugin.json"),
            include_str!("../../plugins/builtin/codex/plugin.json"),
            include_str!("../../plugins/builtin/herdr/plugin.json"),
            include_str!("../../plugins/builtin/qwen/plugin.json"),
            include_str!("../../plugins/builtin/claude/plugin.json"),
            include_str!("../../plugins/builtin/qoder/plugin.json"),
            include_str!("../../plugins/builtin/dsh/plugin.json"),
        ] {
            if let Ok(builtin) = serde_json::from_str::<PluginManifest>(contents)
                .map_err(|error| error.to_string())
                .and_then(validate)
            {
                plugins.push(builtin);
            }
        }
        // OpenCode is shipped as the first built-in manifest, but still goes
        // through the exact same parser and validation as user plugins.
        if let Ok(opencode) = serde_json::from_str::<PluginManifest>(include_str!(
            "../../plugins/opencode/plugin.json"
        ))
        .map_err(|error| error.to_string())
        .and_then(validate)
        {
            plugins.push(opencode);
        }
        let mut dirs = Vec::new();
        if let Some(config) = dirs::config_dir() {
            dirs.push(config.join("Agent Deck Companion/plugins"));
        }
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".agent-deck-companion/plugins"));
        }
        for dir in dirs {
            Self::load_dir(&dir, &mut plugins);
        }
        plugins.sort_by(|a, b| a.id.cmp(&b.id));
        plugins.dedup_by(|a, b| a.id == b.id);
        Self { plugins }
    }

    fn load_dir(dir: &Path, output: &mut Vec<PluginManifest>) {
        let Ok(mut entries) =
            fs::read_dir(dir).map(|entries| entries.flatten().collect::<Vec<_>>())
        else {
            return;
        };
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            if !entry.path().is_dir() {
                continue;
            }
            let path = entry.path().join("plugin.json");
            match fs::metadata(&path)
                .and_then(|meta| {
                    if meta.len() > 256 * 1024 {
                        return Err(std::io::Error::other("manifest too large"));
                    }
                    Ok(())
                })
                .and_then(|_| fs::read_to_string(&path))
            {
                Ok(contents) => match serde_json::from_str::<PluginManifest>(&contents)
                    .map_err(|error| error.to_string())
                    .and_then(validate)
                {
                    Ok(plugin) => output.push(plugin),
                    Err(error) => eprintln!("[plugin] ignored {}: {}", path.display(), error),
                },
                Err(error) => eprintln!("[plugin] ignored {}: {}", path.display(), error),
            }
        }
    }

    pub fn all(&self) -> &[PluginManifest] {
        &self.plugins
    }
    pub fn get(&self, id: &str) -> Option<&PluginManifest> {
        self.plugins.iter().find(|p| p.id == id)
    }
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.plugins
            .iter()
            .filter(|p| !p.built_in)
            .map(|p| p.id.as_str())
    }

    pub fn hook(&self, id: &str) -> Option<&HookDescriptor> {
        self.get(id).and_then(|plugin| plugin.hooks.as_ref())
    }

    pub fn process_matches(&self, id: &str, process_name: &str, argv: &[String]) -> bool {
        let Some(plugin) = self.get(id) else {
            return false;
        };
        if plugin
            .detection
            .exclude_argv_patterns
            .iter()
            .any(|pattern| argv.iter().any(|arg| arg.contains(pattern)))
        {
            return false;
        }
        plugin
            .detection
            .process_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(process_name))
            || plugin
                .detection
                .argv_patterns
                .iter()
                .any(|pattern| argv.iter().any(|arg| arg.contains(pattern)))
    }

    pub fn preferred_surface(&self, id: &str) -> Option<AgentSurface> {
        self.get(id)
            .and_then(|p| p.default_surface.or_else(|| p.surfaces.first().copied()))
            .map(Into::into)
    }

    pub fn keys(&self, id: &str, surface: AgentSurface) -> Vec<FunctionKeyState> {
        let Some(plugin) = self.get(id) else {
            return Vec::new();
        };
        let surface = surface_name(surface);
        let bindings = plugin.keymap.get(surface);
        (1..=10)
            .map(|index| {
                let key = format!("K{index}");
                bindings.and_then(|map| map.get(&key)).map_or_else(
                    || FunctionKeyState {
                        key: key.clone(),
                        label: "UNAVAILABLE".into(),
                        hint: "当前界面不支持".into(),
                        supported: false,
                        action_id: None,
                        source: "plugin".into(),
                        customized: false,
                    },
                    |binding| FunctionKeyState {
                        key: key.clone(),
                        label: binding.label.clone(),
                        hint: binding.hint.clone(),
                        supported: !matches!(binding.action, PluginAction::Disabled),
                        action_id: Some(plugin_action_id(binding)),
                        source: "plugin".into(),
                        customized: false,
                    },
                )
            })
            .collect()
    }

    pub fn plan(
        &self,
        id: &str,
        surface: AgentSurface,
        key: &str,
    ) -> Option<(FunctionKeyState, ActionPlan)> {
        let plugin = self.get(id)?;
        let binding = plugin.keymap.get(surface_name(surface))?.get(key)?;
        let plan = action_plan(&binding.action)?;
        Some((
            FunctionKeyState {
                key: key.into(),
                label: binding.label.clone(),
                hint: binding.hint.clone(),
                supported: true,
                action_id: Some(plugin_action_id(binding)),
                source: "plugin".into(),
                customized: false,
            },
            plan,
        ))
    }

    pub fn plan_by_action(
        &self,
        id: &str,
        surface: AgentSurface,
        action_id: &str,
    ) -> Option<ActionPlan> {
        let plugin = self.get(id)?;
        plugin
            .keymap
            .get(surface_name(surface))?
            .values()
            .find(|binding| plugin_action_id(binding) == action_id)
            .and_then(|binding| action_plan(&binding.action))
    }

    pub fn catalog(&self, id: &str, surface: AgentSurface) -> Vec<ActionCatalogItem> {
        let Some(plugin) = self.get(id) else {
            return Vec::new();
        };
        plugin
            .keymap
            .get(surface_name(surface))
            .into_iter()
            .flat_map(|bindings| bindings.values())
            .filter(|binding| !matches!(binding.action, PluginAction::Disabled))
            .map(|binding| ActionCatalogItem {
                action_id: plugin_action_id(binding),
                label: binding.label.clone(),
                hint: binding.hint.clone(),
            })
            .collect()
    }
}

fn validate(plugin: PluginManifest) -> Result<PluginManifest, String> {
    if plugin.manifest_version != MANIFEST_VERSION {
        return Err(format!(
            "unsupported manifest_version: {}",
            plugin.manifest_version
        ));
    }
    if !valid_id(&plugin.id) || (BUILTIN_IDS.contains(&plugin.id.as_str()) && !plugin.built_in) {
        return Err("invalid or reserved plugin id".into());
    }
    if plugin.display_name.trim().is_empty() || plugin.display_name.len() > 120 {
        return Err("invalid display_name".into());
    }
    if let Some(hook) = &plugin.hooks {
        if !matches!(
            hook.handler_format.as_str(),
            "codex" | "claude" | "qwen" | "opencode_v2"
        ) {
            return Err("unsupported hook handler_format".into());
        }
    }
    if plugin.id != "chatgpt"
        && plugin.keymap.values().flatten().any(|(_, binding)| {
            matches!(
                &binding.action,
                PluginAction::ChatgptFork
                    | PluginAction::ChatgptVoiceHold
                    | PluginAction::ChatgptApprove
                    | PluginAction::ChatgptReject
                    | PluginAction::ChatgptFast
            )
        })
    {
        return Err("ChatGPT-specific actions are reserved for the ChatGPT builtin".into());
    }
    if plugin.surfaces.is_empty() || plugin.surfaces.len() > 4 {
        return Err("invalid surfaces".into());
    }
    if plugin.keymap.values().map(|map| map.len()).sum::<usize>() > 40 {
        return Err("too many key bindings".into());
    }
    if plugin.keymap.values().flatten().any(|(_, binding)| {
        matches!(&binding.action, PluginAction::ActivePanel { slot } if !(1..=10).contains(slot))
    }) {
        return Err("active_panel slot must be between 1 and 10".into());
    }
    Ok(plugin)
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}
fn surface_name(surface: AgentSurface) -> &'static str {
    match surface {
        AgentSurface::Desktop => "desktop",
        AgentSurface::Terminal => "terminal",
        // A CLI plugin can use the same terminal keymap when hosted inside
        // Herdr; a separate herdr_pane map remains available to built-ins.
        AgentSurface::HerdrPane => "terminal",
        AgentSurface::Web => "web",
        AgentSurface::Unknown => "unknown",
    }
}

fn action_plan(action: &PluginAction) -> Option<ActionPlan> {
    match action {
        PluginAction::ActivePanel { slot } => Some(ActionPlan::ActivePanel { slot: *slot }),
        PluginAction::Shortcut { key, modifiers } => Some(ActionPlan::Shortcut {
            key_code: key_code(key)?.0,
            modifiers: parse_modifiers(modifiers)?.union(key_code(key)?.1),
        }),
        PluginAction::TextInput { text, submit } => Some(ActionPlan::TextInput {
            text: Box::leak(text.clone().into_boxed_str()),
            submit: *submit,
        }),
        PluginAction::SlashCommand { text } => Some(ActionPlan::SlashCommand {
            text: Box::leak(text.clone().into_boxed_str()),
        }),
        PluginAction::HerdrPrefix { key, modifiers } => Some(ActionPlan::HerdrPrefixedShortcut {
            key_code: key_code(key)?.0,
            modifiers: parse_modifiers(modifiers)?.union(key_code(key)?.1),
        }),
        PluginAction::Leader {
            leader,
            key,
            modifiers,
        } => Some(ActionPlan::LeaderSequence {
            leader: Box::leak(leader.clone().into_boxed_str()),
            key_code: key_code(key)?.0,
            modifiers: parse_modifiers(modifiers)?.union(key_code(key)?.1),
        }),
        PluginAction::ChatgptFork => Some(ActionPlan::ChatgptFork),
        PluginAction::ChatgptVoiceHold => Some(ActionPlan::ChatgptVoiceHold),
        PluginAction::ChatgptApprove => Some(ActionPlan::ChatgptApprove),
        PluginAction::ChatgptReject => Some(ActionPlan::ChatgptReject),
        PluginAction::ChatgptFast => Some(ActionPlan::ChatgptFast),
        PluginAction::VoiceInput | PluginAction::Disabled => None,
    }
}

fn plugin_action_id(binding: &KeyBinding) -> String {
    match binding.action {
        PluginAction::ActivePanel { slot } => format!("active_panel.{slot}"),
        _ => binding.label.to_ascii_lowercase().replace(' ', "_"),
    }
}

pub(crate) fn parse_modifiers(values: &[String]) -> Option<KeyModifiers> {
    values.iter().try_fold(KeyModifiers::NONE, |result, value| {
        Some(result.union(match value.to_ascii_lowercase().as_str() {
            "command" | "cmd" => KeyModifiers::COMMAND,
            "control" | "ctrl" => KeyModifiers::CONTROL,
            "shift" => KeyModifiers::SHIFT,
            "option" | "alt" => KeyModifiers::OPTION,
            _ => return None,
        }))
    })
}

pub(crate) fn key_code(key: &str) -> Option<(u16, KeyModifiers)> {
    Some((
        match key.to_ascii_lowercase().as_str() {
            "a" => 0,
            "b" => 11,
            "c" => 8,
            "d" => 2,
            "e" => 14,
            "f" => 3,
            "g" => 5,
            "h" => 4,
            "i" => 34,
            "j" => 38,
            "k" => 40,
            "l" => 37,
            "m" => 46,
            "n" => 45,
            "o" => 31,
            "p" => 35,
            "q" => 12,
            "r" => 15,
            "s" => 1,
            "t" => 17,
            "u" => 32,
            "v" => 9,
            "w" => 13,
            "x" => 7,
            "y" => 16,
            "z" => 6,
            "enter" => 36,
            "tab" => 48,
            "esc" | "escape" => 53,
            "space" => 49,
            "?" => 44,
            "/" => 44,
            "." => 47,
            "[" => 33,
            "]" => 30,
            "minus" | "-" => 27,
            "left" => 123,
            "right" => 124,
            "down" => 125,
            "up" => 126,
            _ => return None,
        },
        if key == "?" {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_standalone_qwen_cli_entry_without_matching_generic_node() {
        let registry = PluginRegistry::load();
        assert!(registry.process_matches(
            "qwen",
            "node",
            &[
                "/Users/test/.local/lib/qwen-code/node/bin/node".into(),
                "/Users/test/.local/lib/qwen-code/lib/cli-entry.js".into(),
            ]
        ));
        assert!(!registry.process_matches(
            "qwen",
            "node",
            &[
                "/usr/local/bin/node".into(),
                "/work/demo/cli-entry.js".into(),
            ]
        ));
    }

    #[test]
    fn every_bundled_manifest_actually_loads() {
        // A manifest in the include list is skipped silently when it fails to
        // parse or validate, so shipping a broken one would look like the
        // agent simply has no plugin at all.
        let registry = PluginRegistry::load();
        for id in [
            "chatgpt", "codex", "herdr", "qwen", "claude", "qoder", "dsh",
        ] {
            assert!(
                registry.get(id).is_some(),
                "bundled {id} plugin did not load"
            );
        }
    }

    #[test]
    fn qoder_hooks_install_into_the_user_settings() {
        let registry = PluginRegistry::load();
        let hook = registry.hook("qoder").expect("qoder hook descriptor");
        assert_eq!(hook.config_relative, ".qoder/settings.json");
        // The deck's status model needs the permission, completion and failure
        // edges, not just tool traffic.
        for event in [
            "PermissionRequest",
            "UserPromptSubmit",
            "TaskCompleted",
            "StopFailure",
            "SessionEnd",
        ] {
            assert!(hook.events.iter().any(|seen| seen == event));
        }
    }

    #[test]
    fn rejects_reserved_ids() {
        assert!(validate(serde_json::from_str::<PluginManifest>(r#"{"manifest_version":1,"id":"qwen","display_name":"x","detection":{},"surfaces":["terminal"],"default_surface":"terminal","keymap":{}}"#).unwrap()).is_err());
    }

    #[test]
    fn bundled_opencode_manifest_is_available() {
        let registry = PluginRegistry::load();
        let plugin = registry.get("opencode").expect("bundled OpenCode plugin");
        assert_eq!(plugin.display_name, "OpenCode");
        assert_eq!(
            registry.preferred_surface("opencode"),
            Some(AgentSurface::Terminal)
        );
        assert!(registry
            .plan("opencode", AgentSurface::Terminal, "K1")
            .is_some());
    }

    #[test]
    fn builtins_use_the_same_manifest_pipeline() {
        let registry = PluginRegistry::load();
        assert!(registry.get("codex").is_some_and(|plugin| plugin.built_in));
        assert!(registry
            .get("chatgpt")
            .is_some_and(|plugin| plugin.built_in));
        assert!(registry.get("herdr").is_some_and(|plugin| plugin.built_in));
        assert!(registry
            .plan("codex", AgentSurface::Terminal, "K1")
            .is_some());
        assert!(registry
            .plan("chatgpt", AgentSurface::Desktop, "K1")
            .is_some());
        assert!(matches!(
            registry.plan("chatgpt", AgentSurface::Desktop, "K4"),
            Some((_, ActionPlan::ChatgptApprove))
        ));
        assert!(matches!(
            registry.plan("chatgpt", AgentSurface::Desktop, "K6"),
            Some((_, ActionPlan::ChatgptFork))
        ));
        assert!(matches!(
            registry.plan("chatgpt", AgentSurface::Desktop, "K10"),
            Some((_, ActionPlan::ChatgptVoiceHold))
        ));
        assert!(registry
            .plan("herdr", AgentSurface::HerdrPane, "K2")
            .is_some());
        assert!(matches!(
            registry.plan("qwen", AgentSurface::Terminal, "K1"),
            Some((_, ActionPlan::ActivePanel { slot: 1 }))
        ));
        assert!(registry
            .catalog("claude", AgentSurface::Terminal)
            .iter()
            .any(|item| item.action_id == "active_panel.3"));
        assert!(matches!(
            registry.plan_by_action("claude", AgentSurface::Terminal, "active_panel.2"),
            Some(ActionPlan::ActivePanel { slot: 2 })
        ));
    }

    #[test]
    fn rejects_active_panel_slots_outside_the_hardware_range() {
        let manifest = serde_json::from_str::<PluginManifest>(
            r#"{"manifest_version":1,"id":"panel-test","display_name":"Panel Test","detection":{},"surfaces":["terminal"],"default_surface":"terminal","keymap":{"terminal":{"K7":{"label":"PANEL","hint":"Panel","action":{"type":"active_panel","slot":11}}}}}"#,
        )
        .unwrap();
        assert!(validate(manifest).is_err());
    }
}
