//! Persistent switch for ESP32 deck hardware, off by default.
//!
//! The companion must run as a standalone software tool: with no deck
//! attached it must not install the F-key event tap (which would demand
//! Input Monitoring permission for a hardware-only feature) nor poll the
//! vendor HID device. Turning the switch on is the entry point for later
//! hardware use; already-spawned listeners cannot be revoked in-process,
//! so switching off stops pushes immediately but full teardown needs a
//! restart.

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HardwarePreferences {
    pub enabled: bool,
}

impl Default for HardwarePreferences {
    fn default() -> Self {
        Self { enabled: false }
    }
}

fn preferences_path() -> Option<PathBuf> {
    dirs::config_dir().map(|root| root.join("Agent Deck Companion").join("hardware.json"))
}

pub fn load() -> HardwarePreferences {
    #[cfg(debug_assertions)]
    if std::env::var_os("AGENT_DECK_QA_PROFILE").is_some() {
        return HardwarePreferences { enabled: false };
    }

    preferences_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

pub fn set_enabled(enabled: bool) {
    let Some(path) = preferences_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let contents = match serde_json::to_string_pretty(&HardwarePreferences { enabled }) {
        Ok(contents) => contents,
        Err(error) => {
            eprintln!("[hardware] cannot serialize preferences: {error}");
            return;
        }
    };
    if let Err(error) = fs::write(&path, contents) {
        eprintln!("[hardware] cannot write {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::HardwarePreferences;

    #[test]
    fn hardware_is_off_without_any_saved_preference() {
        assert!(!HardwarePreferences::default().enabled);
    }

    #[test]
    fn a_corrupt_preferences_file_falls_back_to_software_mode() {
        let parsed: Result<HardwarePreferences, _> = serde_json::from_str("not json");
        assert!(parsed.is_err());
        let partial: HardwarePreferences =
            serde_json::from_str("{}").expect("empty object is tolerable");
        assert!(!partial.enabled);
    }
}
