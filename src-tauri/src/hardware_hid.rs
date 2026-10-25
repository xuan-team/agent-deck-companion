//! macOS keyboard-HID bridge for the ESP32's 15 physical keys.
//!
//! The firmware exposes F16..F20 in three modifier banks. This module only
//! translates those keyboard events; execution remains in `process_event`, the
//! same path used by serial input and the HTTP mock API.

use std::{sync::Arc, thread, time::Duration};

use core_foundation::runloop::CFRunLoop;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};
use tauri::AppHandle;

use crate::{emit_state, process_event, AppRuntime, DeviceEvent};

fn f_key_offset(key_code: i64) -> Option<u8> {
    match key_code {
        106 => Some(0), // F16
        64 => Some(1),  // F17
        79 => Some(2),  // F18
        80 => Some(3),  // F19
        90 => Some(4),  // F20
        _ => None,
    }
}

fn logical_key(key_code: i64, flags: CGEventFlags) -> Option<String> {
    let offset = f_key_offset(key_code)?;
    let control = flags.contains(CGEventFlags::CGEventFlagControl);
    let shift = flags.contains(CGEventFlags::CGEventFlagShift);

    match (control, shift) {
        (false, false) => Some(format!("A{}", offset + 1)),
        (true, false) => Some(format!("K{}", offset + 1)),
        (true, true) => Some(format!("K{}", offset + 6)),
        (false, true) => None,
    }
}

pub fn start(app: AppHandle, state: Arc<AppRuntime>) {
    thread::spawn(move || {
        // Granting Input Monitoring after launch leaves the first tap attempt
        // unauthorized. Retry until the permission shows up instead of forcing
        // the user to restart the whole app.
        let mut retrying = false;
        loop {
            if retrying {
                thread::sleep(Duration::from_secs(5));
                // Only probe while waiting. Creating a tap can itself request
                // access, so don't repeat listen attempts without permission.
                if !crate::permissions::input_monitoring_granted() {
                    continue;
                }
            }
            let callback_app = app.clone();
            let callback_state = state.clone();
            let ready_app = app.clone();
            let ready_state = state.clone();
            let result = CGEventTap::with_enabled(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::KeyDown],
                move |_proxy, event_type, event: &CGEvent| {
                    if event_type as u32 == CGEventType::KeyDown as u32
                        && event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) == 0
                    {
                        let key_code =
                            event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                        if let Some(key) = logical_key(key_code, event.get_flags()) {
                            let companion = callback_state
                                .config
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .assigned_agent(&key)
                                .is_some_and(|id| id == "companion");
                            if companion
                                || crate::session_navigation::active(&callback_state)
                                || crate::vendor_hid::navigation_captures(&key)
                            {
                                return CallbackResult::Keep;
                            }
                            process_event(
                                DeviceEvent {
                                    navigation_stamp: None,
                                    kind: "key_down".into(),
                                    key: Some(key),
                                    delta: None,
                                },
                                &callback_app,
                                &callback_state,
                            );
                        }
                    }
                    CallbackResult::Keep
                },
                move || {
                    {
                        let mut snapshot =
                            ready_state.snapshot.write().unwrap_or_else(|poisoned| {
                                eprintln!("[state] snapshot write lock poisoned, recovering");
                                poisoned.into_inner()
                            });
                        snapshot.device.mode = "hid".into();
                        snapshot.device.connected = true;
                        snapshot.device.port = Some("macOS keyboard HID".into());
                    }
                    emit_state(&ready_app, &ready_state);
                    eprintln!("[hardware_hid] listening for ESP32 F16..F20 key banks");
                    CFRunLoop::run_current();
                },
            );

            match result {
                Ok(()) => return,
                Err(()) => {
                    if !retrying {
                        eprintln!(
                            "[hardware_hid] unable to install event tap; retrying when Input Monitoring is granted"
                        );
                    }
                    // Authorization can change between tap creation and the
                    // preflight check, or take time to reach WindowServer.
                    // A granted preflight must not permanently stop retries.
                    retrying = true;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_three_f_key_banks_to_deck_keys() {
        let none = CGEventFlags::CGEventFlagNull;
        let control = CGEventFlags::CGEventFlagControl;
        let control_shift = control | CGEventFlags::CGEventFlagShift;

        // Brightness-conflicting legacy keys must no longer select agents.
        for code in [105, 107, 113] {
            assert_eq!(logical_key(code, none), None);
            assert_eq!(logical_key(code, control), None);
        }
        for (offset, code) in [106, 64, 79, 80, 90].into_iter().enumerate() {
            assert_eq!(logical_key(code, none), Some(format!("A{}", offset + 1)));
            assert_eq!(logical_key(code, control), Some(format!("K{}", offset + 1)));
            assert_eq!(
                logical_key(code, control_shift),
                Some(format!("K{}", offset + 6))
            );
        }
        assert_eq!(logical_key(106, none).as_deref(), Some("A1"));
        assert_eq!(logical_key(90, none).as_deref(), Some("A5"));
        assert_eq!(logical_key(106, control).as_deref(), Some("K1"));
        assert_eq!(logical_key(90, control).as_deref(), Some("K5"));
        assert_eq!(logical_key(106, control_shift).as_deref(), Some("K6"));
        assert_eq!(logical_key(90, control_shift).as_deref(), Some("K10"));
        assert_eq!(logical_key(106, CGEventFlags::CGEventFlagShift), None);
    }
}
