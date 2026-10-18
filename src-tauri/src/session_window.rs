//! Resident floating session panel: a ~380px cross-agent session list that
//! works with no deck hardware attached.
//!
//! The window is deliberately non-focusable: background refreshes and clicks
//! must never steal key focus from the Agent surface being raised. Position,
//! collapse and visibility persist so the panel reappears where the user left
//! it, clamped back into a visible monitor when displays change.

use std::{
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Mutex,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};

pub const WINDOW_LABEL: &str = "session-panel";

const PANEL_WIDTH: f64 = 380.0;
const PANEL_HEIGHT: f64 = 460.0;
const COLLAPSED_HEIGHT: f64 = 44.0;
/// Extra room for one notification card over the collapsed bar. The card itself
/// is sized to this, so the window never clips it.
const CARD_HEIGHT: f64 = 96.0;
const SCREEN_MARGIN: f64 = 18.0;
const MOVE_SETTLE: Duration = Duration::from_millis(600);
const DISPLAY_CHECK: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SessionPanelPreferences {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub collapsed: bool,
    pub visible: bool,
    /// Which list view the panel was left in: false = 需要关注 queue,
    /// true = 全部会话.
    pub show_all: bool,
    pub tab: crate::session_tabs::Tab,
    #[serde(default = "default_tab_order")]
    pub tab_order: [crate::session_tabs::Tab; 3],
}

impl Default for SessionPanelPreferences {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            collapsed: false,
            visible: true,
            show_all: false,
            tab: crate::session_tabs::Tab::Attention,
            tab_order: crate::session_tabs::DEFAULT_ORDER,
        }
    }
}

fn default_tab_order() -> [crate::session_tabs::Tab; 3] {
    crate::session_tabs::DEFAULT_ORDER
}

static PREFERENCES_LOCK: Mutex<()> = Mutex::new(());
static WINDOW_LAYOUT_LOCK: Mutex<()> = Mutex::new(());
static NOTIFICATION_ACTIVE: AtomicBool = AtomicBool::new(false);

fn panel_height(collapsed: bool, notification_active: bool) -> f64 {
    if !collapsed {
        PANEL_HEIGHT
    } else if notification_active {
        COLLAPSED_HEIGHT + CARD_HEIGHT
    } else {
        COLLAPSED_HEIGHT
    }
}

fn preferences_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|root| root.join("Agent Deck Companion").join("session-panel.json"))
}

fn read_preferences() -> SessionPanelPreferences {
    let mut preferences: SessionPanelPreferences = preferences_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default();
    preferences.tab_order = crate::session_tabs::validate_order(&preferences.tab_order)
        .unwrap_or(crate::session_tabs::DEFAULT_ORDER);
    preferences.show_all = preferences.tab != crate::session_tabs::Tab::Attention;
    preferences
}

pub fn load_preferences() -> SessionPanelPreferences {
    let _guard = PREFERENCES_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    read_preferences()
}

fn update_preferences(update: impl FnOnce(&mut SessionPanelPreferences)) {
    let _guard = PREFERENCES_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut preferences = read_preferences();
    update(&mut preferences);
    save_preferences(preferences);
}

fn save_preferences(preferences: SessionPanelPreferences) {
    let Some(path) = preferences_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(contents) = serde_json::to_string_pretty(&preferences) {
        let temporary = path.with_extension("json.tmp");
        if let Err(error) =
            fs::write(&temporary, contents).and_then(|_| fs::rename(&temporary, &path))
        {
            eprintln!("[session_panel] cannot save preferences: {error}");
        }
    }
}

pub fn create_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let preferences = load_preferences();
    let height = if preferences.collapsed {
        COLLAPSED_HEIGHT
    } else {
        PANEL_HEIGHT
    };
    let window = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("/".into()))
        .title("会话")
        .inner_size(PANEL_WIDTH, height)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .shadow(true)
        .visible(preferences.visible)
        .build()?;
    restore_position(&window, &preferences);
    attach_move_memory(app, &window);
    Ok(window)
}

#[derive(Clone, Copy)]
struct ScreenRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

/// Physical coordinates throughout; i64 arithmetic also accepts a corrupt
/// preferences file without overflowing the app's setup callback.
fn clamp_position(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    screens: &[ScreenRect],
) -> Option<(i32, i32)> {
    // Choose the monitor containing most of the panel, not the first monitor
    // with a single overlapping pixel. This keeps a cross-screen drag stable.
    let screen = screens
        .iter()
        .max_by_key(|screen| {
            let overlap_width = (i64::from(x) + i64::from(width))
                .min(i64::from(screen.x) + i64::from(screen.width))
                - i64::from(x).max(i64::from(screen.x));
            let overlap_height = (i64::from(y) + i64::from(height))
                .min(i64::from(screen.y) + i64::from(screen.height))
                - i64::from(y).max(i64::from(screen.y));
            overlap_width.max(0) * overlap_height.max(0)
        })
        .filter(|screen| {
            i64::from(x) < i64::from(screen.x) + i64::from(screen.width)
                && i64::from(x) + i64::from(width) > i64::from(screen.x)
                && i64::from(y) < i64::from(screen.y) + i64::from(screen.height)
                && i64::from(y) + i64::from(height) > i64::from(screen.y)
        })
        .or_else(|| screens.first())?;
    let min_x = i64::from(screen.x);
    let min_y = i64::from(screen.y);
    let max_x = min_x + i64::from(screen.width.saturating_sub(width));
    let max_y = min_y + i64::from(screen.height.saturating_sub(height));
    Some((
        i64::from(x).clamp(min_x, max_x) as i32,
        i64::from(y).clamp(min_y, max_y) as i32,
    ))
}

fn restore_position(window: &WebviewWindow, preferences: &SessionPanelPreferences) {
    let Ok(monitors) = window.available_monitors() else {
        return;
    };
    let screens = monitors
        .iter()
        .map(|monitor| {
            let area = monitor.work_area();
            ScreenRect {
                x: area.position.x,
                y: area.position.y,
                width: area.size.width,
                height: area.size.height,
            }
        })
        .collect::<Vec<_>>();
    let Ok(size) = window.outer_size() else {
        return;
    };
    let position = preferences.x.zip(preferences.y).or_else(|| {
        let monitor = window.primary_monitor().ok().flatten()?;
        let area = monitor.work_area();
        Some((
            (area.position.x as f64 + area.size.width as f64
                - size.width as f64
                - SCREEN_MARGIN * monitor.scale_factor())
            .round() as i32,
            (area.position.y as f64 + SCREEN_MARGIN * monitor.scale_factor()).round() as i32,
        ))
    });
    if let Some((x, y)) =
        position.and_then(|(x, y)| clamp_position(x, y, size.width, size.height, &screens))
    {
        let target = PhysicalPosition::new(x, y);
        if window.outer_position().ok() != Some(target) {
            let _ = window.set_position(target);
        }
    }
}

fn attach_move_memory(app: &AppHandle, window: &WebviewWindow) {
    let (tx, rx) = mpsc::channel::<Option<()>>();
    window.on_window_event(move |event| {
        if matches!(event, WindowEvent::Destroyed) {
            let _ = tx.send(None);
        } else if matches!(
            event,
            WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. }
        ) {
            let _ = tx.send(Some(()));
        }
    });
    let saver = app.clone();
    thread::spawn(move || {
        let mut displays = display_layout(&saver);
        loop {
            match rx.recv_timeout(DISPLAY_CHECK) {
                Ok(Some(())) => {}
                Ok(None) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    let current = display_layout(&saver);
                    if current.is_some() && current != displays {
                        save_position(&saver);
                        displays = current;
                    }
                    continue;
                }
            }
            // Dragging fires continuously; persist once the window settles.
            loop {
                match rx.recv_timeout(MOVE_SETTLE) {
                    Ok(Some(())) => continue,
                    Ok(None) => return,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            save_position(&saver);
            displays = display_layout(&saver);
        }
    });
}

// Work area, scale and origin all matter (hotplug, resolution and Dock changes).
// Only a changed layout triggers a window operation; no background animation.
fn display_layout(app: &AppHandle) -> Option<Vec<(i32, i32, u32, u32, u64)>> {
    let window = app.get_webview_window(WINDOW_LABEL)?;
    Some(
        window
            .available_monitors()
            .ok()?
            .iter()
            .map(|monitor| {
                let area = monitor.work_area();
                (
                    area.position.x,
                    area.position.y,
                    area.size.width,
                    area.size.height,
                    monitor.scale_factor().to_bits(),
                )
            })
            .collect(),
    )
}

fn current_preferences(window: &WebviewWindow) -> SessionPanelPreferences {
    let mut preferences = load_preferences();
    if let Ok(position) = window.outer_position() {
        preferences.x = Some(position.x);
        preferences.y = Some(position.y);
    }
    preferences
}

fn save_position(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    let Ok(position) = window.outer_position() else {
        return;
    };
    let mut preferences = load_preferences();
    preferences.x = Some(position.x);
    preferences.y = Some(position.y);
    restore_position(&window, &preferences);
    let Ok(position) = window.outer_position() else {
        return;
    };
    update_preferences(|preferences| {
        preferences.x = Some(position.x);
        preferences.y = Some(position.y);
    });
}

pub fn set_visible(app: &AppHandle, visible: bool) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        if visible {
            restore_position(&window, &current_preferences(&window));
            let _ = window.show();
        } else {
            let _ = window.hide();
        }
    }
    let actual_visible = app
        .get_webview_window(WINDOW_LABEL)
        .is_some_and(|window| window.is_visible().unwrap_or(false));
    if let Some(state) = app.try_state::<std::sync::Arc<crate::state::AppRuntime>>() {
        state
            .desktop_notifications
            .set_panel_visible(actual_visible);
    }
    update_preferences(|preferences| preferences.visible = visible);
    let _ = app.emit("session_panel_visibility_changed", actual_visible);
}

pub fn toggle(app: &AppHandle) {
    let visible = app
        .get_webview_window(WINDOW_LABEL)
        .is_some_and(|window| !window.is_visible().unwrap_or(false));
    set_visible(app, visible);
}

/// Overlay updates on an expanded panel do not change its native dimensions.
/// Avoid redundant resize requests and their WebView layout / window events.
fn resize_panel(window: &WebviewWindow, height: f64) -> Result<(), String> {
    let desired = LogicalSize::new(PANEL_WIDTH, height);
    let scale = window.scale_factor().map_err(|error| error.to_string())?;
    if window.inner_size().ok() == Some(desired.to_physical::<u32>(scale)) {
        return Ok(());
    }
    window.set_size(desired).map_err(|error| error.to_string())
}

pub fn set_collapsed(app: &AppHandle, collapsed: bool) -> Result<(), String> {
    let _layout = WINDOW_LAYOUT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let window = app
        .get_webview_window(WINDOW_LABEL)
        .ok_or("会话浮窗尚未就绪")?;
    let height = panel_height(collapsed, NOTIFICATION_ACTIVE.load(Ordering::SeqCst));
    let position = current_preferences(&window);
    resize_panel(&window, height)?;
    update_preferences(|preferences| preferences.collapsed = collapsed);
    let _ = app.emit("session_panel_preferences_changed", load_preferences());
    restore_position(&window, &position);
    Ok(())
}

/// Persist which list view (attention queue vs all) the user last chose.
pub fn set_show_all(show_all: bool) {
    set_tab(if show_all {
        crate::session_tabs::Tab::Others
    } else {
        crate::session_tabs::Tab::Attention
    });
}
pub fn set_tab(tab: crate::session_tabs::Tab) {
    update_preferences(|preferences| {
        preferences.tab = tab;
        preferences.show_all = tab != crate::session_tabs::Tab::Attention;
    });
}
pub fn set_tab_order(order: [crate::session_tabs::Tab; 3]) {
    update_preferences(|preferences| preferences.tab_order = order);
}

/// Explicit notification click may reveal the attention list; background
/// notification delivery never calls this or alters the user's window preference.
pub fn reveal_attention(app: &AppHandle) {
    if let Some(state) = app.try_state::<std::sync::Arc<crate::state::AppRuntime>>() {
        if crate::session_navigation::active(&state) {
            crate::session_navigation::reveal_attention(app, &state);
            return;
        }
    }
    set_show_all(false);
    set_visible(app, true);
    let _ = set_collapsed(app, false);
    let _ = app.emit("session_panel_preferences_changed", load_preferences());
}

/// Grow the collapsed bar just enough to show one notification card, then return
/// it to the height the user chose. The saved `collapsed` preference is never
/// touched, so a notification cannot un-collapse the panel (§9.1). An expanded
/// panel already has room and is left alone.
pub fn set_notification_overlay(app: &AppHandle, active: bool) -> Result<(), String> {
    let _layout = WINDOW_LAYOUT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    NOTIFICATION_ACTIVE.store(active, Ordering::SeqCst);
    let window = app
        .get_webview_window(WINDOW_LABEL)
        .ok_or("会话浮窗尚未就绪")?;
    let navigating = app
        .try_state::<std::sync::Arc<crate::state::AppRuntime>>()
        .is_some_and(|state| crate::session_navigation::active(&state));
    let height = panel_height(!navigating && load_preferences().collapsed, active);
    let position = current_preferences(&window);
    resize_panel(&window, height)?;
    restore_position(&window, &position);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_window_preferences_keep_geometry_and_default_new_tab_order() {
        let preferences: super::SessionPanelPreferences = serde_json::from_str(
            r#"{"x":10,"y":20,"collapsed":true,"visible":false,"show_all":true}"#,
        )
        .unwrap();
        assert_eq!(preferences.x, Some(10));
        assert!(!preferences.visible);
        assert!(preferences.collapsed);
        assert_eq!(preferences.tab, crate::session_tabs::Tab::Attention);
        assert_eq!(preferences.tab_order, crate::session_tabs::DEFAULT_ORDER);
        let changed = super::SessionPanelPreferences {
            tab: crate::session_tabs::Tab::Codex,
            tab_order: [
                crate::session_tabs::Tab::Attention,
                crate::session_tabs::Tab::Others,
                crate::session_tabs::Tab::Codex,
            ],
            ..preferences
        };
        assert_eq!(
            serde_json::from_str::<super::SessionPanelPreferences>(
                &serde_json::to_string(&changed).unwrap()
            )
            .unwrap(),
            changed
        );
    }

    use super::SessionPanelPreferences;

    #[test]
    fn a_partial_preferences_file_falls_back_to_the_visible_panel() {
        let parsed: SessionPanelPreferences =
            serde_json::from_str(r#"{"collapsed": true}"#).expect("partial json");
        assert_eq!(
            parsed,
            SessionPanelPreferences {
                x: None,
                y: None,
                collapsed: true,
                visible: true,
                show_all: false,
                tab: crate::session_tabs::Tab::Attention,
                tab_order: crate::session_tabs::DEFAULT_ORDER,
            }
        );
        assert!(
            !SessionPanelPreferences::default().show_all,
            "the panel reopens into the attention queue, never the full list"
        );
    }

    #[test]
    fn an_unreadable_preferences_file_never_hides_the_panel_on_launch() {
        let parsed: Result<SessionPanelPreferences, _> = serde_json::from_str("not json");
        assert!(parsed.is_err());
        assert!(SessionPanelPreferences::default().visible);
    }
    #[test]
    fn physical_position_is_clamped_fully_into_the_remaining_screen() {
        use super::{clamp_position, ScreenRect};
        let screen = ScreenRect {
            x: -1920,
            y: 40,
            width: 1920,
            height: 1040,
        };
        assert_eq!(
            clamp_position(-1, 1079, 760, 920, &[screen]),
            Some((-760, 160))
        );
        assert_eq!(
            clamp_position(i32::MAX, i32::MIN, 760, 920, &[screen]),
            Some((-760, 40))
        );
        assert_eq!(
            clamp_position(-1800, 100, 380, 460, &[screen]),
            Some((-1800, 100))
        );
        assert_eq!(clamp_position(0, 0, 380, 460, &[]), None);
    }
    #[test]
    fn cross_screen_drag_uses_the_largest_overlap() {
        use super::{clamp_position, ScreenRect};
        let left = ScreenRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let right = ScreenRect {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            clamp_position(1900, 100, 380, 460, &[left, right]),
            Some((1920, 100))
        );
        assert_eq!(
            clamp_position(1700, 100, 380, 460, &[right, left]),
            Some((1540, 100))
        );
        // Expansion at the bottom preserves a fully visible panel.
        assert_eq!(
            clamp_position(2200, 1000, 380, 460, &[left, right]),
            Some((2200, 620))
        );
        // Removing the right monitor returns the panel to the surviving one.
        assert_eq!(
            clamp_position(2200, 100, 380, 460, &[left]),
            Some((1540, 100))
        );
    }

    #[test]
    fn collapsing_with_a_live_card_reserves_space_in_either_order() {
        assert_eq!(
            super::panel_height(true, true),
            super::COLLAPSED_HEIGHT + super::CARD_HEIGHT
        );
        assert_eq!(super::panel_height(false, true), super::PANEL_HEIGHT);
        assert_eq!(super::panel_height(true, false), super::COLLAPSED_HEIGHT);
    }
}

/// Temporary hardware selection does not overwrite persistent window preferences.
pub fn apply_temporary(
    app: &AppHandle,
    visible: bool,
    collapsed: bool,
    tab: crate::session_tabs::Tab,
) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let position = current_preferences(&window);
        let _ = resize_panel(
            &window,
            panel_height(collapsed, NOTIFICATION_ACTIVE.load(Ordering::SeqCst)),
        );
        restore_position(&window, &position);
        if visible {
            let _ = window.show();
        } else {
            let _ = window.hide();
        }
        let actual = window.is_visible().unwrap_or(false);
        if let Some(state) = app.try_state::<std::sync::Arc<crate::state::AppRuntime>>() {
            state.desktop_notifications.set_panel_visible(actual);
        }
        let _ = app.emit("session_panel_visibility_changed", actual);
    }
    let mut preferences = load_preferences();
    preferences.visible = visible;
    preferences.collapsed = collapsed;
    preferences.tab = tab;
    preferences.show_all = tab != crate::session_tabs::Tab::Attention;
    let _ = app.emit("session_panel_preferences_changed", preferences);
}
