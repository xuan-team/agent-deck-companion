//! Local-first profile configuration for physical Agent and function keys.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u32 = 3;
pub const AGENT_SLOTS: [&str; 5] = ["A1", "A2", "A3", "A4", "A5"];
pub const FUNCTION_KEYS: [&str; 10] = ["K1", "K2", "K3", "K4", "K5", "K6", "K7", "K8", "K9", "K10"];
pub const BUILT_IN_AGENTS: [&str; 7] = [
    "qwen",
    "herdr",
    "codex",
    "chatgpt",
    "claude",
    "dsh",
    "companion",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub version: u32,
    pub active_profile: String,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub agent_slots: BTreeMap<String, AgentSlotConfig>,
    #[serde(default)]
    pub global_keys: BTreeMap<String, ConfiguredAction>,
    #[serde(default)]
    pub agent_keys: BTreeMap<String, BTreeMap<String, BTreeMap<String, ConfiguredAction>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSlotConfig {
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConfiguredAction {
    AgentAction {
        action_id: String,
    },
    SystemAction {
        action_id: String,
    },
    KeyboardShortcut {
        key_code: u16,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    Disabled,
}

impl Default for AppConfig {
    fn default() -> Self {
        let agent_slots = [
            ("A1", "qwen"),
            ("A2", "herdr"),
            ("A3", "chatgpt"),
            ("A4", "claude"),
            ("A5", "companion"),
        ]
        .into_iter()
        .map(|(slot, agent_id)| {
            (
                slot.into(),
                AgentSlotConfig {
                    agent_id: agent_id.into(),
                },
            )
        })
        .collect();
        let profile = Profile {
            name: "默认开发".into(),
            agent_slots,
            global_keys: [(
                "K10".into(),
                ConfiguredAction::SystemAction {
                    action_id: "session_panel.toggle".into(),
                },
            )]
            .into_iter()
            .collect(),
            agent_keys: BTreeMap::new(),
        };
        Self {
            version: CONFIG_VERSION,
            active_profile: "default".into(),
            profiles: [("default".into(), profile)].into_iter().collect(),
        }
    }
}

impl AppConfig {
    pub fn active(&self) -> &Profile {
        self.profiles
            .get(&self.active_profile)
            .expect("validated profile must exist")
    }

    pub fn active_mut(&mut self) -> &mut Profile {
        self.profiles
            .get_mut(&self.active_profile)
            .expect("validated profile must exist")
    }

    pub fn assigned_agent(&self, slot: &str) -> Option<&str> {
        self.active()
            .agent_slots
            .get(&slot.to_ascii_uppercase())
            .map(|config| config.agent_id.as_str())
    }

    /// Assign an Agent to a physical slot. If it already occupies another
    /// slot, swap both assignments so every built-in Agent remains reachable.
    pub fn assign_agent(&mut self, slot: &str, agent_id: &str) -> Result<(), String> {
        let slot = slot.to_ascii_uppercase();
        if !AGENT_SLOTS.contains(&slot.as_str()) {
            return Err(format!("unknown Agent slot: {slot}"));
        }
        let profile = self.active_mut();
        let previous = profile
            .agent_slots
            .get(&slot)
            .map(|entry| entry.agent_id.clone())
            .ok_or_else(|| format!("missing Agent slot: {slot}"))?;
        if previous == agent_id {
            return Ok(());
        }
        let occupied_slot = profile
            .agent_slots
            .iter()
            .find(|(_, entry)| entry.agent_id == agent_id)
            .map(|(key, _)| key.clone());
        profile.agent_slots.get_mut(&slot).unwrap().agent_id = agent_id.into();
        if let Some(occupied_slot) = occupied_slot {
            profile
                .agent_slots
                .get_mut(&occupied_slot)
                .unwrap()
                .agent_id = previous;
        }
        Ok(())
    }

    pub fn configured_action(
        &self,
        agent_id: &str,
        surface: &str,
        key: &str,
    ) -> Option<&ConfiguredAction> {
        let profile = self.active();
        profile
            .agent_keys
            .get(agent_id)
            .and_then(|surfaces| surfaces.get(surface))
            .and_then(|keys| keys.get(&key.to_ascii_uppercase()))
            .or_else(|| profile.global_keys.get(&key.to_ascii_uppercase()))
    }

    pub fn has_agent_override(&self, agent_id: &str, surface: &str, key: &str) -> bool {
        self.active()
            .agent_keys
            .get(agent_id)
            .and_then(|surfaces| surfaces.get(surface))
            .is_some_and(|keys| keys.contains_key(&key.to_ascii_uppercase()))
    }

    pub fn set_agent_action(
        &mut self,
        agent_id: &str,
        surface: &str,
        key: &str,
        action: ConfiguredAction,
    ) -> Result<(), String> {
        let key = key.to_ascii_uppercase();
        if !FUNCTION_KEYS.contains(&key.as_str()) {
            return Err(format!("unknown function key: {key}"));
        }
        self.active_mut()
            .agent_keys
            .entry(agent_id.into())
            .or_default()
            .entry(surface.into())
            .or_default()
            .insert(key, action);
        Ok(())
    }

    pub fn reset_agent_action(&mut self, agent_id: &str, surface: &str, key: &str) {
        if let Some(surfaces) = self.active_mut().agent_keys.get_mut(agent_id) {
            if let Some(keys) = surfaces.get_mut(surface) {
                keys.remove(&key.to_ascii_uppercase());
                if keys.is_empty() {
                    surfaces.remove(surface);
                }
            }
            if surfaces.is_empty() {
                self.active_mut().agent_keys.remove(agent_id);
            }
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    // Debug-only isolation for running the event-path QA without editing the
    // user's profile. Production builds always use the normal local path.
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("AGENT_DECK_QA_PROFILE") {
        return Some(path.into());
    }
    dirs::config_dir().map(|root| root.join("Agent Deck Companion").join("profiles.json"))
}

pub fn load() -> AppConfig {
    load_with_agents(&[])
}

pub fn load_with_agents(extra_agents: &[String]) -> AppConfig {
    config_path()
        .and_then(|path| load_from(&path, extra_agents).ok())
        .unwrap_or_default()
}

pub fn save(config: &AppConfig) -> Result<(), String> {
    save_with_agents(config, &[])
}

pub fn save_with_agents(config: &AppConfig, extra_agents: &[String]) -> Result<(), String> {
    let path = config_path().ok_or_else(|| "configuration directory unavailable".to_string())?;
    save_to(&path, config, extra_agents)
}

fn validate(config: AppConfig, extra_agents: &[String]) -> Result<AppConfig, String> {
    if config.version != CONFIG_VERSION {
        return Err(format!("unsupported profile version: {}", config.version));
    }
    let profile = config
        .profiles
        .get(&config.active_profile)
        .ok_or_else(|| "active profile does not exist".to_string())?;
    if !AGENT_SLOTS
        .iter()
        .all(|slot| profile.agent_slots.contains_key(*slot))
    {
        return Err("profile must configure A1-A5".into());
    }
    let assigned_agents = AGENT_SLOTS
        .iter()
        .filter_map(|slot| profile.agent_slots.get(*slot))
        .map(|slot| slot.agent_id.as_str())
        .collect::<Vec<_>>();
    if assigned_agents.iter().any(|agent_id| {
        !BUILT_IN_AGENTS.contains(agent_id) && !extra_agents.iter().any(|extra| extra == agent_id)
    }) {
        return Err("profile contains an unknown Agent assignment".into());
    }
    if assigned_agents.iter().any(|agent_id| {
        assigned_agents
            .iter()
            .filter(|assigned| *assigned == agent_id)
            .count()
            != 1
    }) {
        return Err("each Agent slot must have a unique assignment".into());
    }
    let has_plugin_assignment = assigned_agents
        .iter()
        .any(|agent_id| extra_agents.iter().any(|extra| extra == agent_id));
    let required_agents = ["qwen", "herdr", "claude", "dsh"];
    let missing_required = required_agents.iter().any(|agent_id| {
        assigned_agents
            .iter()
            .filter(|assigned| *assigned == agent_id)
            .count()
            != 1
    });
    let codex_or_chatgpt_count = assigned_agents
        .iter()
        .filter(|assigned| **assigned == "codex" || **assigned == "chatgpt")
        .count();
    if !has_plugin_assignment
        && !assigned_agents.contains(&"companion")
        && (missing_required || codex_or_chatgpt_count != 1)
    {
        return Err("profile must assign every built-in Agent exactly once".into());
    }
    Ok(config)
}

fn load_from(path: &Path, extra_agents: &[String]) -> Result<AppConfig, String> {
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let config = migrate(serde_json::from_str(&contents).map_err(|error| error.to_string())?);
    validate(config, extra_agents)
}

fn migrate(mut config: AppConfig) -> AppConfig {
    if config.version == 2 {
        for profile in config.profiles.values_mut() {
            if matches!(
                profile.global_keys.get("K10"),
                Some(ConfiguredAction::SystemAction { action_id }) if action_id == "voice_input.toggle"
            ) {
                profile.global_keys.remove("K10");
            }
        }
        config.version = CONFIG_VERSION;
    }
    config
}

fn save_to(path: &Path, config: &AppConfig, extra_agents: &[String]) -> Result<(), String> {
    validate(config.clone(), extra_agents)?;
    let parent = path
        .parent()
        .ok_or_else(|| "profile path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    let contents = serde_json::to_vec_pretty(config).map_err(|error| error.to_string())?;
    let mut file = fs::File::create(&temporary).map_err(|error| error.to_string())?;
    file.write_all(&contents)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_offer_companion_and_panel_toggle() {
        let config = AppConfig::default();
        assert_eq!(config.assigned_agent("a1"), Some("qwen"));
        assert_eq!(config.assigned_agent("A5"), Some("companion"));
        assert_eq!(
            config.configured_action("claude", "terminal", "k10"),
            Some(&ConfiguredAction::SystemAction {
                action_id: "session_panel.toggle".into()
            })
        );
        assert!(validate(config, &[]).is_ok());
    }

    #[test]
    fn v2_migration_removes_the_legacy_global_voice_default() {
        let mut config = AppConfig::default();
        config.version = 2;
        config.active_mut().global_keys.insert(
            "K10".into(),
            ConfiguredAction::SystemAction {
                action_id: "voice_input.toggle".into(),
            },
        );
        let migrated = migrate(config);
        assert_eq!(migrated.version, CONFIG_VERSION);
        assert!(migrated.active().global_keys.get("K10").is_none());
    }

    #[test]
    fn existing_profiles_keep_their_a5_and_k10_on_load() {
        let mut old = AppConfig::default();
        old.assign_agent("A5", "dsh").unwrap();
        old.active_mut().global_keys.clear();
        let loaded = validate(migrate(old), &[]).unwrap();
        assert_eq!(loaded.assigned_agent("A5"), Some("dsh"));
        assert!(loaded.active().global_keys.is_empty());
    }

    #[test]
    fn assigning_an_occupied_agent_swaps_slots() {
        let mut config = AppConfig::default();
        config.assign_agent("A1", "claude").unwrap();
        assert_eq!(config.assigned_agent("A1"), Some("claude"));
        assert_eq!(config.assigned_agent("A4"), Some("qwen"));
    }

    #[test]
    fn duplicate_or_unknown_agent_slots_are_rejected() {
        let mut duplicate = AppConfig::default();
        duplicate
            .active_mut()
            .agent_slots
            .get_mut("A1")
            .unwrap()
            .agent_id = "claude".into();
        assert!(validate(duplicate, &[]).is_err());

        let mut unknown = AppConfig::default();
        unknown
            .active_mut()
            .agent_slots
            .get_mut("A1")
            .unwrap()
            .agent_id = "custom-agent".into();
        assert!(validate(unknown, &[]).is_err());
    }

    #[test]
    fn plugin_agent_is_allowed_when_registry_declares_it() {
        let mut config = AppConfig::default();
        config
            .active_mut()
            .agent_slots
            .get_mut("A5")
            .unwrap()
            .agent_id = "opencode".into();
        assert!(validate(config, &["opencode".into()]).is_ok());
    }

    #[test]
    fn profile_round_trips_atomically() {
        let directory = std::env::temp_dir().join(format!("agent-deck-{}", uuid::Uuid::new_v4()));
        let path = directory.join("profiles.json");
        let mut expected = AppConfig::default();
        expected
            .set_agent_action("claude", "terminal", "K1", ConfiguredAction::Disabled)
            .unwrap();
        save_to(&path, &expected, &[]).unwrap();
        assert_eq!(load_from(&path, &[]).unwrap(), expected);
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
