use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType},
    event_source::{CGEventSource, CGEventSourceStateID},
};

use crate::agent_actions::{ActionPlan, KeyModifiers};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
}

pub fn has_or_request_access() -> bool {
    // Posting and listening are separate permissions. Creating a CGEvent does
    // not prove macOS will allow it to be delivered.
    if unsafe { CGPreflightPostEventAccess() } {
        return true;
    }
    static REQUESTED: AtomicBool = AtomicBool::new(false);
    if !REQUESTED.swap(true, Ordering::AcqRel) {
        let _ = crate::permissions::request_macos_permission("accessibility".into(), false);
        unsafe { CGRequestPostEventAccess() };
    }
    // Let the existing System Events fallback attempt delivery. Its failures
    // are logged rather than silently treating event creation as delivery.
    true
}

pub fn post(plan: ActionPlan, herdr_prefix: Option<&str>) {
    match plan {
        ActionPlan::Shortcut {
            key_code,
            modifiers,
        } => {
            if !post_key(key_code, flags(modifiers)) {
                post_key_applescript(key_code, modifiers);
            }
        }
        ActionPlan::TextInput { text, submit } => {
            for character in text.chars() {
                let Some((key_code, modifiers)) = character_key(character) else {
                    return;
                };
                if !post_key(key_code, flags(modifiers)) {
                    post_key_applescript(key_code, modifiers);
                }
            }
            if submit {
                if !post_key(36, CGEventFlags::CGEventFlagNull) {
                    post_key_applescript(36, KeyModifiers::NONE);
                }
            }
        }
        ActionPlan::SlashCommand { text } => {
            post(
                ActionPlan::TextInput {
                    text,
                    submit: false,
                },
                None,
            );
            // Give the web composer time to render its autocomplete popup.
            // Enter #1 accepts the highlighted suggestion (the popup would
            // otherwise swallow a submit), Enter #2 submits the command. When
            // no popup appears the first Enter already submits and the second
            // lands on an empty composer as a no-op.
            thread::sleep(Duration::from_millis(350));
            if !post_key(36, CGEventFlags::CGEventFlagNull) {
                post_key_applescript(36, KeyModifiers::NONE);
            }
            thread::sleep(Duration::from_millis(150));
            if !post_key(36, CGEventFlags::CGEventFlagNull) {
                post_key_applescript(36, KeyModifiers::NONE);
            }
        }
        ActionPlan::HerdrPrefixedShortcut {
            key_code,
            modifiers,
        } => {
            let Some((prefix_code, prefix_modifiers)) = herdr_prefix.and_then(parse_shortcut)
            else {
                eprintln!("[post] no prefix parsed from {:?}", herdr_prefix);
                return;
            };
            eprintln!(
                "[post] prefix: code={} mods={:?} key: code={} mods={:?}",
                prefix_code, prefix_modifiers, key_code, modifiers,
            );
            // System Events delivers terminal chords in the same form as a
            // physical keyboard. This is more reliable for Herdr's prefix
            // state machine than two independent HID CGEvents.
            if post_herdr_prefix_applescript(prefix_code, prefix_modifiers, key_code, modifiers) {
                eprintln!("[post] herdr AppleScript chord delivered");
                return;
            }
            // Send the configured prefix as a real key event. Herdr/Alacritty
            // expects Ctrl+S itself, not a synthetic FlagsChanged event with a
            // dummy key code (which is easy for terminal input layers to drop).
            // Keep a visible inter-key gap. Herdr's prefix dispatcher is a
            // terminal key-chord state machine; very short gaps can be
            // swallowed by Alacritty/pty scheduling even though the events
            // technically reached CoreGraphics.
            let prefix_ok = post_key(prefix_code, flags(prefix_modifiers));
            if !prefix_ok {
                post_key_applescript(prefix_code, prefix_modifiers);
            }
            thread::sleep(Duration::from_millis(100));
            let key_ok = post_key(key_code, flags(modifiers));
            eprintln!("[post] prefix_result={prefix_ok} key_result={key_ok}");
            if !key_ok {
                post_key_applescript(key_code, modifiers);
            }
        }
        ActionPlan::LeaderSequence {
            leader,
            key_code,
            modifiers,
        } => {
            let Some((leader_code, leader_modifiers)) = parse_shortcut(leader) else {
                eprintln!("[post] invalid plugin leader={leader}");
                return;
            };
            if !post_herdr_prefix_applescript(leader_code, leader_modifiers, key_code, modifiers) {
                post_key(leader_code, flags(leader_modifiers));
                thread::sleep(Duration::from_millis(100));
                post_key(key_code, flags(modifiers));
            }
        }
        ActionPlan::ChatgptFork => {
            eprintln!("[post] ChatGPT fork must be executed by the ChatGPT adapter");
        }
        ActionPlan::ChatgptVoiceHold => {
            eprintln!("[post] ChatGPT voice hold must be executed by the ChatGPT adapter");
        }
        ActionPlan::ChatgptApprove => {
            eprintln!("[post] ChatGPT approve must be executed via native HID (ACT07)");
        }
        ActionPlan::ChatgptReject => {
            eprintln!("[post] ChatGPT reject must be executed via native HID (ACT08)");
        }
        ActionPlan::ChatgptFast => {
            eprintln!("[post] ChatGPT fast must be executed via native HID (ACT06)");
        }
        ActionPlan::ActivePanel { .. } => {
            eprintln!("[post] Herdr Agent panel focus must be executed by the Herdr adapter");
        }
    }
}

/// Run ChatGPT Desktop's current fork flow. Its picker is not exposed through
/// macOS Accessibility in the currently supported app build, so this uses the
/// known second-entry keyboard sequence.
pub fn chatgpt_fork() -> bool {
    post(
        ActionPlan::TextInput {
            text: "/fork",
            submit: true,
        },
        None,
    );
    thread::sleep(Duration::from_millis(300));
    post_key_or_applescript(125, KeyModifiers::NONE);
    thread::sleep(Duration::from_millis(80));
    post_key_or_applescript(36, KeyModifiers::NONE);
    true
}

static CHATGPT_VOICE_HELD: AtomicBool = AtomicBool::new(false);

/// Toggle ChatGPT's push-to-talk chord. The first call holds Ctrl+Shift+D;
/// the next call releases D, Shift, then Ctrl.
pub fn toggle_chatgpt_voice_hold() -> bool {
    if CHATGPT_VOICE_HELD.swap(true, Ordering::SeqCst) {
        stop_chatgpt_voice_hold();
        return false;
    }

    let control = CGEventFlags::CGEventFlagControl;
    let control_shift = control | CGEventFlags::CGEventFlagShift;
    let pressed = post_modifier_state(59, control, true)
        && post_modifier_state(56, control_shift, true)
        && post_key_state(2, control_shift, true);
    if !pressed {
        stop_chatgpt_voice_hold();
        return false;
    }
    true
}

pub fn stop_chatgpt_voice_hold() {
    if !CHATGPT_VOICE_HELD.swap(false, Ordering::SeqCst) {
        return;
    }
    let control = CGEventFlags::CGEventFlagControl;
    let control_shift = control | CGEventFlags::CGEventFlagShift;
    let _ = post_key_state(2, control_shift, false);
    let _ = post_modifier_state(56, control, false);
    let _ = post_modifier_state(59, CGEventFlags::CGEventFlagNull, false);
}

fn post_key_or_applescript(key_code: u16, modifiers: KeyModifiers) {
    if !post_key(key_code, flags(modifiers)) {
        post_key_applescript(key_code, modifiers);
    }
}

/// Toggle the macOS Dictation shortcut configured as “Press Fn Key Twice”.
/// Dictation remains owned by macOS, so recognized text is inserted directly
/// into the currently focused Agent input without exposing audio to this app.
pub fn toggle_dictation() {
    const FUNCTION_KEY_CODE: u16 = 63;
    post_modifier_key(FUNCTION_KEY_CODE, CGEventFlags::CGEventFlagSecondaryFn);
    thread::sleep(Duration::from_millis(120));
    post_modifier_key(FUNCTION_KEY_CODE, CGEventFlags::CGEventFlagSecondaryFn);
}

/// Start ChatGPT Desktop's own composer dictation command. ChatGPT exposes
/// `composer.startDictation` as Control+Shift+D in its desktop keymap.
pub fn start_chatgpt_dictation() {
    post_key(
        2,
        CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagShift,
    );
}

/// Move ChatGPT Desktop's reasoning menu by exactly one item. The caller
/// reads the persisted desktop state before every step and confirms the new
/// state afterwards, so a lost or externally changed menu cannot compound.
pub fn set_chatgpt_reasoning_step(increase: bool) {
    post(
        ActionPlan::TextInput {
            text: "/reasoning",
            submit: true,
        },
        None,
    );
    thread::sleep(Duration::from_millis(260));
    post_key_or_applescript(if increase { 125 } else { 126 }, KeyModifiers::NONE);
    thread::sleep(Duration::from_millis(70));
    post_key_or_applescript(36, KeyModifiers::NONE);
}

fn post_modifier_key(key_code: u16, modifier: CGEventFlags) {
    for key_down in [true, false] {
        let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
            return;
        };
        let Ok(event) = CGEvent::new_keyboard_event(source, key_code, key_down) else {
            return;
        };
        event.set_type(CGEventType::FlagsChanged);
        event.set_flags(if key_down {
            modifier
        } else {
            CGEventFlags::CGEventFlagNull
        });
        event.post(CGEventTapLocation::HID);
        thread::sleep(Duration::from_millis(12));
    }
}

fn flags(modifiers: KeyModifiers) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if modifiers.contains(KeyModifiers::COMMAND) {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if modifiers.contains(KeyModifiers::OPTION) {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    flags
}

fn post_key(key_code: u16, flags: CGEventFlags) -> bool {
    if !unsafe { CGPreflightPostEventAccess() } {
        eprintln!("[macos_input] CGEvent posting denied; trying System Events for key={key_code}");
        return false;
    }
    for key_down in [true, false] {
        let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
            return false;
        };
        let Ok(event) = CGEvent::new_keyboard_event(source, key_code, key_down) else {
            return false;
        };
        event.set_flags(flags);
        event.post(CGEventTapLocation::HID);
        thread::sleep(Duration::from_millis(12));
    }
    true
}

fn post_key_state(key_code: u16, flags: CGEventFlags, key_down: bool) -> bool {
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return false;
    };
    let Ok(event) = CGEvent::new_keyboard_event(source, key_code, key_down) else {
        return false;
    };
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
    thread::sleep(Duration::from_millis(12));
    true
}

fn post_modifier_state(key_code: u16, flags: CGEventFlags, key_down: bool) -> bool {
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return false;
    };
    let Ok(event) = CGEvent::new_keyboard_event(source, key_code, key_down) else {
        return false;
    };
    event.set_type(CGEventType::FlagsChanged);
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
    thread::sleep(Duration::from_millis(12));
    true
}

/// Fallback: use AppleScript to post a key event (requires Accessibility permission).
fn post_key_applescript(key_code: u16, modifiers: KeyModifiers) {
    let mut parts = Vec::new();
    if modifiers.contains(KeyModifiers::COMMAND) {
        parts.push("command down");
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("control down");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift down");
    }
    if modifiers.contains(KeyModifiers::OPTION) {
        parts.push("option down");
    }
    let using = if parts.is_empty() {
        String::new()
    } else {
        format!(" using {{{}}}", parts.join(", "))
    };
    let script = format!(
        "tell application \"System Events\" to key code {}{}",
        key_code, using
    );
    match std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()
    {
        Ok(output) if output.status.success() => {
            eprintln!("[macos_input] System Events sent key={key_code}");
        }
        Ok(output) => eprintln!(
            "[macos_input] System Events failed for key={key_code}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => eprintln!("[macos_input] unable to run System Events: {error}"),
    }
}

fn post_herdr_prefix_applescript(
    prefix_code: u16,
    prefix_modifiers: KeyModifiers,
    key_code: u16,
    modifiers: KeyModifiers,
) -> bool {
    fn using_clause(modifiers: KeyModifiers) -> String {
        let mut parts = Vec::new();
        if modifiers.contains(KeyModifiers::COMMAND) {
            parts.push("command down");
        }
        if modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("control down");
        }
        if modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("shift down");
        }
        if modifiers.contains(KeyModifiers::OPTION) {
            parts.push("option down");
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" using {{{}}}", parts.join(", "))
        }
    }

    let script = format!(
        "tell application \"System Events\"\n  key code {}{}\n  delay 0.1\n  key code {}{}\nend tell",
        prefix_code,
        using_clause(prefix_modifiers),
        key_code,
        using_clause(modifiers),
    );
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn character_key(character: char) -> Option<(u16, KeyModifiers)> {
    let key = match character {
        'a' => 0,
        'b' => 11,
        'c' => 8,
        'd' => 2,
        'e' => 14,
        'f' => 3,
        'g' => 5,
        'h' => 4,
        'i' => 34,
        'j' => 38,
        'k' => 40,
        'l' => 37,
        'm' => 46,
        'n' => 45,
        'o' => 31,
        'p' => 35,
        'q' => 12,
        'r' => 15,
        's' => 1,
        't' => 17,
        'u' => 32,
        'v' => 9,
        'w' => 13,
        'x' => 7,
        'y' => 16,
        'z' => 6,
        '/' => 44,
        ' ' => 49,
        '?' => return Some((44, KeyModifiers::SHIFT)),
        _ => return None,
    };
    Some((key, KeyModifiers::NONE))
}

fn parse_shortcut(shortcut: &str) -> Option<(u16, KeyModifiers)> {
    let normalized = shortcut.trim().to_ascii_lowercase();
    let mut parts = normalized.split('+').collect::<Vec<_>>();
    let key = parts.pop()?;
    let mut modifiers = KeyModifiers::NONE;
    for modifier in parts {
        modifiers = modifiers.union(match modifier {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "shift" => KeyModifiers::SHIFT,
            "alt" | "option" => KeyModifiers::OPTION,
            "cmd" | "command" | "super" => KeyModifiers::COMMAND,
            _ => return None,
        });
    }
    let (key_code, key_modifiers) = match key {
        "enter" => (36, KeyModifiers::NONE),
        "tab" => (48, KeyModifiers::NONE),
        "space" => (49, KeyModifiers::NONE),
        "esc" | "escape" => (53, KeyModifiers::NONE),
        "left" => (123, KeyModifiers::NONE),
        "right" => (124, KeyModifiers::NONE),
        "down" => (125, KeyModifiers::NONE),
        "up" => (126, KeyModifiers::NONE),
        "minus" | "-" => (27, KeyModifiers::NONE),
        "slash" | "/" => (44, KeyModifiers::NONE),
        "f1" => (122, KeyModifiers::NONE),
        "f2" => (120, KeyModifiers::NONE),
        "f3" => (99, KeyModifiers::NONE),
        "f4" => (118, KeyModifiers::NONE),
        "f5" => (96, KeyModifiers::NONE),
        "f6" => (97, KeyModifiers::NONE),
        "f7" => (98, KeyModifiers::NONE),
        "f8" => (100, KeyModifiers::NONE),
        "f9" => (101, KeyModifiers::NONE),
        "f10" => (109, KeyModifiers::NONE),
        "f11" => (103, KeyModifiers::NONE),
        "f12" => (111, KeyModifiers::NONE),
        value if value.len() == 1 => character_key(value.chars().next()?)?,
        _ => return None,
    };
    Some((key_code, modifiers.union(key_modifiers)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlisted_agent_command_alphabet_is_supported() {
        for character in "/clear/btw /branch?/compact/resume/help".chars() {
            assert!(character_key(character).is_some(), "missing {character:?}");
        }
    }

    #[test]
    fn herdr_prefix_supports_configured_modifier_chords() {
        assert_eq!(parse_shortcut("ctrl+s"), Some((1, KeyModifiers::CONTROL)));
        assert_eq!(
            parse_shortcut("ctrl+alt+b"),
            Some((11, KeyModifiers::CONTROL.union(KeyModifiers::OPTION)))
        );
        assert_eq!(parse_shortcut("f12"), Some((111, KeyModifiers::NONE)));
        assert_eq!(parse_shortcut("f13"), None);
    }
}
