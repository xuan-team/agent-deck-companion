//! Best-effort window activation for external agent windows on Windows.
//!
//! The companion never owns these windows: CLI agents run inside Windows
//! Terminal, conhost, VS Code or another terminal host, and desktop agents own
//! their own top-level window. A process id therefore identifies a candidate,
//! not necessarily the window that must come forward, so the process ancestry
//! and the terminal title are both used as evidence.

use std::collections::HashSet;
use std::ffi::c_void;

use sysinfo::System;
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindow, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow,
    ShowWindow, GW_OWNER, SW_RESTORE, SW_SHOWNORMAL,
};

const MAX_ANCESTOR_DEPTH: usize = 8;

#[derive(Debug, Clone)]
struct WindowInfo {
    hwnd: HWND,
    pid: u32,
    title: String,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> i32 {
    let windows = &mut *(lparam as *mut Vec<WindowInfo>);
    if IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, GW_OWNER).is_null() {
        return 1;
    }
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 {
        return 1;
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
    if copied <= 0 {
        return 1;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    windows.push(WindowInfo {
        hwnd,
        pid,
        title: String::from_utf16_lossy(&buffer[..copied as usize]),
    });
    1
}

fn visible_windows() -> Vec<WindowInfo> {
    let mut windows = Vec::<WindowInfo>::new();
    unsafe {
        EnumWindows(
            Some(collect_window),
            &mut windows as *mut Vec<WindowInfo> as LPARAM,
        );
    }
    windows
}

fn process_lineage(system: &System, pid: u32) -> HashSet<u32> {
    let mut lineage = HashSet::new();
    let mut current = sysinfo::Pid::from_u32(pid);
    for _ in 0..MAX_ANCESTOR_DEPTH {
        if !lineage.insert(current.as_u32()) {
            break;
        }
        let Some(process) = system.process(current) else {
            break;
        };
        let Some(parent) = process.parent() else {
            break;
        };
        current = parent;
    }
    lineage
}

fn terminal_host(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "windowsterminal",
        "conhost",
        "cmd",
        "powershell",
        "pwsh",
        "alacritty",
        "wezterm",
        "mintty",
        "wt",
        "code",
        "cursor",
        "windsurf",
        "hyper",
        "tabby",
    ]
    .iter()
    .any(|host| name == *host || name == format!("{host}.exe"))
}

fn process_name(system: &System, pid: u32) -> String {
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|process| process.name().to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn project_name(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?.trim().trim_end_matches(['\\', '/']);
    if cwd.is_empty() {
        return None;
    }
    cwd.rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Focus the visible top-level window most likely belonging to `pid`.
pub fn focus_process_window(pid: Option<u32>, hints: &[String], cwd: Option<&str>) -> bool {
    let windows = visible_windows();
    if windows.is_empty() {
        return false;
    }
    let system = System::new_all();
    let lineage = pid.map(|pid| process_lineage(&system, pid));
    let project = project_name(cwd);
    let mut best: Option<(i32, HWND)> = None;
    for window in &windows {
        let title = window.title.to_ascii_lowercase();
        let mut score = 0;
        if lineage
            .as_ref()
            .is_some_and(|lineage| lineage.contains(&window.pid))
        {
            score += 100;
        }
        let owner = process_name(&system, window.pid);
        let hint_match = hints
            .iter()
            .filter(|hint| !hint.is_empty())
            .any(|hint| title.contains(&hint.to_ascii_lowercase()));
        let project_match = project
            .as_deref()
            .is_some_and(|project| title.contains(&project.to_ascii_lowercase()));
        if hint_match {
            score += 20;
        }
        if project_match {
            score += 20;
        }
        // Without a lineage or a title match, an arbitrary terminal window is
        // not evidence for this agent; do not steal the user's foreground.
        if score == 0 {
            continue;
        }
        if terminal_host(&owner) {
            score += 10;
        } else if !lineage
            .as_ref()
            .is_some_and(|lineage| lineage.contains(&window.pid))
        {
            // A non-terminal owner is only a candidate through ancestry.
            continue;
        }
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, window.hwnd));
        }
    }
    let Some((_, hwnd)) = best else {
        // ConPTY breaks the process ancestry between an agent and Windows
        // Terminal, and the terminal title is user-controlled. With exactly one
        // terminal window open, that window is still the only plausible host.
        let terminal_windows = windows
            .iter()
            .filter(|window| terminal_host(&process_name(&system, window.pid)))
            .collect::<Vec<_>>();
        if terminal_windows.len() == 1 {
            return focus_window(terminal_windows[0].hwnd);
        }
        return false;
    };
    focus_window(hwnd)
}

fn focus_window(hwnd: HWND) -> bool {
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, std::ptr::null_mut())
        };
        let current_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(current_thread, foreground_thread, 1) != 0;
        BringWindowToTop(hwnd);
        let raised = SetForegroundWindow(hwnd) != 0;
        if attached {
            AttachThreadInput(current_thread, foreground_thread, 0);
        }
        raised || GetForegroundWindow() == hwnd
    }
}

/// Open a URL or a registered custom protocol with the user's default handler.
///
/// Only schemes the companion itself generates are accepted, so hook payloads
/// cannot turn this into an arbitrary command launcher.
pub fn open_url(url: &str) -> bool {
    let allowed = ["http://", "https://", "codex://"];
    if !allowed.iter().any(|prefix| url.starts_with(prefix)) {
        return false;
    }
    let operation = wide("open");
    let file = wide(url);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut::<c_void>(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    result as isize > 32
}
