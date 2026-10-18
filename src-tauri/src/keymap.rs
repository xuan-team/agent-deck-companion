//! Resolves profile overrides into safe, executable key plans.

use crate::{
    agent_actions::{
        self, ActionCatalogItem, ActionPlan, AgentSurface, FunctionKeyState, KeyModifiers,
        ResolvedAgentBinding,
    },
    profile::{AppConfig, ConfiguredAction, FUNCTION_KEYS},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAction {
    VoiceInputToggle,
    SessionPanelToggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveExecution {
    Agent(ActionPlan),
    System(SystemAction),
}

pub fn surface_key(surface: AgentSurface) -> &'static str {
    match surface {
        AgentSurface::Desktop => "desktop",
        AgentSurface::Terminal | AgentSurface::HerdrPane => "terminal",
        AgentSurface::Web => "web",
        AgentSurface::Unknown => "unknown",
    }
}

fn from_agent_binding(
    key: &str,
    binding: ResolvedAgentBinding,
    source: &str,
    customized: bool,
) -> (FunctionKeyState, Option<EffectiveExecution>) {
    (
        FunctionKeyState {
            key: key.into(),
            label: binding.label.into(),
            hint: binding.hint.into(),
            supported: true,
            action_id: Some(binding.action_id.into()),
            source: source.into(),
            customized,
        },
        Some(EffectiveExecution::Agent(binding.plan)),
    )
}

fn invalid(key: &str, action_id: Option<&str>, source: &str, customized: bool) -> FunctionKeyState {
    FunctionKeyState {
        key: key.into(),
        label: "UNAVAILABLE".into(),
        hint: "当前界面不支持".into(),
        supported: false,
        action_id: action_id.map(Into::into),
        source: source.into(),
        customized,
    }
}

fn shortcut_modifiers(values: &[String]) -> Option<KeyModifiers> {
    let mut modifiers = KeyModifiers::NONE;
    for value in values {
        modifiers = modifiers.union(match value.as_str() {
            "command" => KeyModifiers::COMMAND,
            "control" => KeyModifiers::CONTROL,
            "shift" => KeyModifiers::SHIFT,
            "option" => KeyModifiers::OPTION,
            _ => return None,
        });
    }
    Some(modifiers)
}

fn shortcut_hint(key_code: u16, modifiers: &[String]) -> String {
    let mut parts = modifiers
        .iter()
        .map(|value| match value.as_str() {
            "command" => "⌘",
            "control" => "Ctrl+",
            "shift" => "Shift+",
            "option" => "⌥",
            _ => "",
        })
        .collect::<String>();
    parts.push_str(&format!("Key {key_code}"));
    parts
}

pub fn effective_key(
    config: &AppConfig,
    agent_id: &str,
    surface: AgentSurface,
    key: &str,
) -> (FunctionKeyState, Option<EffectiveExecution>) {
    let key = key.to_ascii_uppercase();
    let surface_name = surface_key(surface);
    let customized = config.has_agent_override(agent_id, surface_name, &key);
    if let Some(action) = config.configured_action(agent_id, surface_name, &key) {
        let source = if customized {
            "user_override"
        } else {
            "profile_default"
        };
        return match action {
            ConfiguredAction::AgentAction { action_id } => {
                if let Some(binding) =
                    agent_actions::binding_for_action(Some(agent_id), surface, action_id)
                {
                    from_agent_binding(&key, binding, source, customized)
                } else {
                    (invalid(&key, Some(action_id), source, customized), None)
                }
            }
            ConfiguredAction::SystemAction { action_id } if action_id == "voice_input.toggle" => (
                FunctionKeyState {
                    key,
                    label: "VOICE".into(),
                    hint: voice_input_hint(agent_id, surface).into(),
                    supported: true,
                    action_id: Some(action_id.clone()),
                    source: source.into(),
                    customized,
                },
                Some(EffectiveExecution::System(SystemAction::VoiceInputToggle)),
            ),
            ConfiguredAction::SystemAction { action_id } if action_id == "session_panel.toggle" => {
                (
                    FunctionKeyState {
                        key,
                        label: "PANEL".into(),
                        hint: "隐藏 / 展示会话浮窗".into(),
                        supported: true,
                        action_id: Some(action_id.clone()),
                        source: source.into(),
                        customized,
                    },
                    Some(EffectiveExecution::System(SystemAction::SessionPanelToggle)),
                )
            }
            ConfiguredAction::SystemAction { action_id } => {
                (invalid(&key, Some(action_id), source, customized), None)
            }
            ConfiguredAction::KeyboardShortcut {
                key_code,
                modifiers,
            } => {
                let Some(parsed) = shortcut_modifiers(modifiers) else {
                    return (invalid(&key, None, source, customized), None);
                };
                (
                    FunctionKeyState {
                        key,
                        label: "SHORTCUT".into(),
                        hint: shortcut_hint(*key_code, modifiers),
                        supported: true,
                        action_id: Some("keyboard_shortcut".into()),
                        source: source.into(),
                        customized,
                    },
                    Some(EffectiveExecution::Agent(ActionPlan::Shortcut {
                        key_code: *key_code,
                        modifiers: parsed,
                    })),
                )
            }
            ConfiguredAction::Disabled => (
                FunctionKeyState {
                    key,
                    label: "—".into(),
                    hint: "已禁用".into(),
                    supported: false,
                    action_id: Some("disabled".into()),
                    source: source.into(),
                    customized,
                },
                None,
            ),
        };
    }

    if let Some(binding) = agent_actions::default_binding(Some(agent_id), surface, &key) {
        from_agent_binding(&key, binding, "adapter_default", false)
    } else if let Some((label, hint, action_id)) =
        agent_actions::default_binding_unresolved(Some(agent_id), surface, &key)
    {
        // Binding exists but is unsupported (plan=None) — preserve original label
        (
            FunctionKeyState {
                key: key.into(),
                label: label.into(),
                hint: hint.into(),
                supported: false,
                action_id: Some(action_id.into()),
                source: "adapter_default".into(),
                customized: false,
            },
            None,
        )
    } else {
        (invalid(&key, None, "adapter_default", false), None)
    }
}

pub fn effective_keys(
    config: &AppConfig,
    agent_id: Option<&str>,
    surface: AgentSurface,
) -> Vec<FunctionKeyState> {
    let Some(agent_id) = agent_id else {
        return FUNCTION_KEYS
            .iter()
            .map(|key| {
                if matches!(config.active().global_keys.get(*key), Some(ConfiguredAction::SystemAction { action_id }) if action_id == "session_panel.toggle") {
                    effective_key(config, "__no_agent__", surface, key).0
                } else {
                    invalid(key, None, "no_agent", false)
                }
            })
            .collect();
    };
    FUNCTION_KEYS
        .iter()
        .map(|key| effective_key(config, agent_id, surface, key).0)
        .collect()
}

pub fn action_catalog(agent_id: Option<&str>, surface: AgentSurface) -> Vec<ActionCatalogItem> {
    let mut catalog = agent_actions::action_catalog(agent_id, surface);
    catalog.push(ActionCatalogItem {
        action_id: "session_panel.toggle".into(),
        label: "PANEL".into(),
        hint: "隐藏 / 展示会话浮窗".into(),
    });
    catalog.push(ActionCatalogItem {
        action_id: "voice_input.toggle".into(),
        label: "VOICE".into(),
        hint: agent_id
            .map(|agent_id| voice_input_hint(agent_id, surface))
            .unwrap_or("语音输入")
            .into(),
    });
    catalog
}

fn voice_input_hint(agent_id: &str, surface: AgentSurface) -> &'static str {
    if agent_id == "chatgpt" && surface == AgentSurface::Desktop {
        "启动 ChatGPT 原生听写"
    } else {
        "开始或停止 macOS 系统听写"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_toggle_is_available_before_selecting_an_agent() {
        let keys = effective_keys(&AppConfig::default(), None, AgentSurface::Unknown);
        assert!(keys[..9].iter().all(|key| !key.supported));
        assert_eq!(keys[9].action_id.as_deref(), Some("session_panel.toggle"));
        assert!(keys[9].supported);
    }

    #[test]
    fn no_global_voice_default_is_applied() {
        let config = AppConfig::default();
        let (key, execution) = effective_key(&config, "claude", AgentSurface::Terminal, "K7");
        assert_eq!(key.label, "STOP");
        assert_eq!(key.source, "adapter_default");
        assert!(execution.is_some());
    }

    #[test]
    fn agent_override_precedes_global_default() {
        let mut config = AppConfig::default();
        config
            .set_agent_action(
                "claude",
                "terminal",
                "K7",
                ConfiguredAction::AgentAction {
                    action_id: "stop".into(),
                },
            )
            .unwrap();
        let (key, _) = effective_key(&config, "claude", AgentSurface::Terminal, "K7");
        assert_eq!(key.label, "STOP");
        assert!(key.customized);
    }
}
