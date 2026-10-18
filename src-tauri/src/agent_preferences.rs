//! Persistent per-agent monitoring switches, everything on by default.
//!
//! The companion monitors every known agent unless the user turns one off in
//! Settings. A disabled agent keeps its A1–A5 slot binding — this is a
//! monitoring preference, not a profile edit — but stops being detected,
//! polled and projected, so it leaves the session lists and the deck lights.
//!
//! Storage mirrors `hardware_mode`: one small JSON file in the config
//! directory, tolerant of a missing or corrupt file.

use std::{collections::BTreeSet, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AgentPreferences {
    /// Agent ids the user turned off. Anything absent stays monitored, so a
    /// new integration arrives enabled without a migration.
    pub disabled: BTreeSet<String>,
}

fn preferences_path() -> Option<PathBuf> {
    // An isolated QA run must never read or write the user's real switch file.
    #[cfg(debug_assertions)]
    if std::env::var_os("AGENT_DECK_QA_PROFILE").is_some() {
        return Some(std::env::temp_dir().join("agent-deck-qa-agents.json"));
    }
    dirs::config_dir().map(|root| root.join("Agent Deck Companion").join("agents.json"))
}

pub fn load() -> AgentPreferences {
    preferences_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

/// Record whether `agent_id` is monitored. A write failure is reported to the
/// caller rather than swallowed, because the in-memory switch and the file
/// would otherwise disagree after a restart.
pub fn set_disabled(agent_id: &str, disabled: bool) -> Result<(), String> {
    let mut preferences = load();
    if disabled {
        preferences.disabled.insert(agent_id.to_owned());
    } else {
        preferences.disabled.remove(agent_id);
    }
    save(&preferences)
}

fn save(preferences: &AgentPreferences) -> Result<(), String> {
    let Some(path) = preferences_path() else {
        return Err("no configuration directory available".into());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let contents = serde_json::to_string_pretty(preferences)
        .map_err(|error| format!("cannot serialize agent preferences: {error}"))?;
    fs::write(&path, contents).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::AgentPreferences;

    #[test]
    fn every_agent_is_monitored_without_a_saved_preference() {
        assert!(AgentPreferences::default().disabled.is_empty());
    }

    #[test]
    fn a_corrupt_or_partial_preferences_file_stays_tolerable() {
        assert!(serde_json::from_str::<AgentPreferences>("not json").is_err());
        let empty: AgentPreferences = serde_json::from_str("{}").expect("empty object");
        assert!(empty.disabled.is_empty());
        let partial: AgentPreferences = serde_json::from_str(r#"{"disabled":["dsh"]}"#).unwrap();
        assert!(partial.disabled.contains("dsh"));
    }
}
