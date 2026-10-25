//! macOS privacy probes never prompt during polling.
#[derive(serde::Serialize)]
pub struct Permissions {
    supported: bool,
    accessibility: bool,
    input_monitoring: bool,
}

#[cfg(target_os = "macos")]
pub fn input_monitoring_granted() -> bool {
    native::input_monitoring_granted()
}

#[cfg(target_os = "macos")]
mod native {
    use core_foundation::{
        base::TCFType, boolean::CFBoolean, dictionary::CFDictionary, string::CFString,
    };
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult,
    };

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(
            options: core_foundation::dictionary::CFDictionaryRef,
        ) -> bool;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
    }
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDCheckAccess(request_type: i32) -> i32;
        fn IOHIDRequestAccess(request_type: i32) -> bool;
    }

    // IOHIDRequestType and IOHIDAccessType from IOHIDLib.h.
    const LISTEN_EVENT: i32 = 1;
    const ACCESS_GRANTED: i32 = 0;

    pub(super) fn input_monitoring_granted() -> bool {
        unsafe {
            CGPreflightListenEventAccess() || IOHIDCheckAccess(LISTEN_EVENT) == ACCESS_GRANTED
        }
    }

    pub fn status() -> super::Permissions {
        super::Permissions {
            supported: true,
            accessibility: unsafe { AXIsProcessTrusted() },
            input_monitoring: input_monitoring_granted(),
        }
    }

    /// TCC registers Input Monitoring from an actual listen attempt;
    /// `CGRequestListenEventAccess` alone only "potentially prompts".
    fn probe_listen_event_tap() {
        let _ = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            vec![CGEventType::KeyDown],
            |_, _, _| CallbackResult::Keep,
        );
    }

    pub fn request(kind: &str) {
        unsafe {
            if kind == "accessibility" {
                let options = CFDictionary::from_CFType_pairs(&[(
                    CFString::new("AXTrustedCheckOptionPrompt").as_CFType(),
                    CFBoolean::true_value().as_CFType(),
                )]);
                AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef());
            } else {
                IOHIDRequestAccess(LISTEN_EVENT);
                CGRequestListenEventAccess();
                if !input_monitoring_granted() {
                    probe_listen_event_tap();
                }
            }
        }
    }
}

#[tauri::command]
pub fn get_macos_permissions() -> Permissions {
    #[cfg(target_os = "macos")]
    {
        native::status()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Permissions {
            supported: false,
            accessibility: false,
            input_monitoring: false,
        }
    }
}

/// Requests and settings links are explicit user actions, never automatic retries.
#[tauri::command]
pub fn request_macos_permission(kind: String, open_settings: bool) -> Result<Permissions, String> {
    let pane = match kind.as_str() {
        "accessibility" => "Privacy_Accessibility",
        "input_monitoring" => "Privacy_ListenEvent",
        _ => return Err("unknown permission".into()),
    };
    #[cfg(target_os = "macos")]
    {
        if open_settings {
            let result = std::process::Command::new("/usr/bin/open")
                .arg(format!(
                    "x-apple.systempreferences:com.apple.preference.security?{pane}"
                ))
                .status()
                .map_err(|e| e.to_string())?;
            if !result.success() {
                return Err("cannot open Privacy settings".into());
            }
        } else {
            native::request(&kind);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (pane, open_settings);
    Ok(get_macos_permissions())
}
