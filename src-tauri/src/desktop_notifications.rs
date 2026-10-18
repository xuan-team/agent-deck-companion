//! Desktop notification policy/settings; OS APIs live behind the macOS adapter.
use crate::notifications::SessionNotification;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

#[cfg(target_os = "macos")]
#[path = "macos_notifications.rs"]
mod macos;

#[derive(Debug, Clone, Serialize)]
pub struct NotificationView {
    pub enabled: bool,
    pub available: bool,
    pub permission: String,
    pub last_error: Option<String>,
    pub last_submission_ms: Option<u128>,
    pub last_click: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct Preferences {
    enabled: bool,
}

#[derive(Default)]
struct Inbox {
    pending: Vec<SessionNotification>,
}

pub struct NotificationRuntime {
    enabled: AtomicBool,
    panel_visible: AtomicBool,
    view: Mutex<NotificationView>,
    inbox: Mutex<Inbox>,
    sync_queued: AtomicBool,
    #[cfg(target_os = "macos")]
    tx: Mutex<Option<std::sync::mpsc::Sender<macos::Command>>>,
}

fn preferences_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|root| {
        root.join("Agent Deck Companion")
            .join("desktop-notifications.json")
    })
}

impl NotificationRuntime {
    pub fn new() -> Self {
        let enabled = preferences_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Preferences>(&b).ok())
            .unwrap_or_default()
            .enabled;
        Self {
            enabled: AtomicBool::new(enabled),
            panel_visible: AtomicBool::new(true),
            view: Mutex::new(NotificationView {
                enabled,
                available: false,
                permission: "unavailable".into(),
                last_error: None,
                last_submission_ms: None,
                last_click: None,
            }),
            inbox: Mutex::new(Inbox::default()),
            sync_queued: AtomicBool::new(false),
            #[cfg(target_os = "macos")]
            tx: Mutex::new(None),
        }
    }
    pub fn view(&self) -> NotificationView {
        self.view.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn set_panel_visible(&self, visible: bool) {
        self.panel_visible.store(visible, Ordering::SeqCst);
    }
    pub fn system_channel(&self) -> bool {
        let view = self.view();
        self.enabled.load(Ordering::SeqCst)
            && !self.panel_visible.load(Ordering::SeqCst)
            && view.available
            && view.permission == "granted"
    }
    pub fn set_enabled(&self, enabled: bool) -> Result<NotificationView, String> {
        // Serialize file replacement and runtime changes so concurrent settings
        // commands cannot leave the saved preference and live switch disagreeing.
        let mut view = self.view.lock().unwrap_or_else(|p| p.into_inner());
        let path = preferences_path().ok_or("configuration directory unavailable")?;
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(&Preferences { enabled }).map_err(|e| e.to_string())?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .and_then(|_| std::fs::rename(&temporary, &path))
            .map_err(|e| e.to_string())?;
        self.enabled.store(enabled, Ordering::SeqCst);
        view.enabled = enabled;
        Ok(view.clone())
    }
    pub fn refresh_permission(&self) {
        #[cfg(target_os = "macos")]
        self.send(macos::Command::Refresh);
    }
    pub fn request_permission(&self) -> Result<(), String> {
        if !self.view().available {
            return Err("System notifications require the packaged macOS app".into());
        }
        #[cfg(target_os = "macos")]
        self.send(macos::Command::Authorize);
        Ok(())
    }
    pub fn test_notification(&self) -> Result<(), String> {
        let view = self.view();
        if !view.available || view.permission != "granted" {
            return Err("Notification permission is required".into());
        }
        #[cfg(target_os = "macos")]
        self.send(macos::Command::Test);
        Ok(())
    }
    /// Nonblocking, bounded hand-off. A failed send leaves the attention list
    /// intact, records a diagnostic and never replays the event on a new channel.
    pub fn sync(&self, fresh: Vec<SessionNotification>) {
        #[cfg(target_os = "macos")]
        {
            if self.tx.lock().unwrap_or_else(|p| p.into_inner()).is_none() {
                return;
            }
            let mut inbox = self.inbox.lock().unwrap_or_else(|p| p.into_inner());
            for event in fresh {
                inbox.pending.retain(|old| old.key != event.key);
                if inbox.pending.len() < 128 {
                    inbox.pending.push(event);
                }
            }
            if !self.sync_queued.swap(true, Ordering::SeqCst) {
                self.send(macos::Command::Sync);
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = fresh;
    }
    #[cfg(target_os = "macos")]
    fn send(&self, command: macos::Command) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            if tx.send(command).is_err() {
                self.sync_queued.store(false, Ordering::SeqCst);
                let mut view = self.view.lock().unwrap_or_else(|p| p.into_inner());
                view.available = false;
                view.last_error = Some(
                    "Notification worker unavailable; attention list remains available".into(),
                );
            }
        }
    }
}

pub fn start(app: tauri::AppHandle, state: std::sync::Arc<crate::state::AppRuntime>) {
    #[cfg(target_os = "macos")]
    macos::start(app, state);
    #[cfg(not(target_os = "macos"))]
    let _ = (app, state);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidden_and_granted_and_enabled_are_all_required_for_system_channel() {
        let runtime = NotificationRuntime::new();
        runtime.enabled.store(true, Ordering::SeqCst);
        {
            let mut view = runtime.view.lock().unwrap();
            view.available = true;
            view.permission = "granted".into();
        }
        assert!(!runtime.system_channel());
        runtime.set_panel_visible(false);
        assert!(runtime.system_channel());
        runtime.enabled.store(false, Ordering::SeqCst);
        assert!(!runtime.system_channel());
        runtime.enabled.store(true, Ordering::SeqCst);
        runtime.view.lock().unwrap().permission = "denied".into();
        assert!(!runtime.system_channel());
        runtime.view.lock().unwrap().permission = "not_requested".into();
        assert!(!runtime.system_channel());
        {
            let mut view = runtime.view.lock().unwrap();
            view.permission = "granted".into();
            view.available = false;
        }
        assert!(!runtime.system_channel());
    }
    #[test]
    fn unavailable_adapter_reports_permission_and_test_failure_without_panicking() {
        let runtime = NotificationRuntime::new();
        assert!(runtime.request_permission().is_err());
        assert!(runtime.test_notification().is_err());
    }
}
