use std::sync::Arc;

use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    AppHandle,
};

use crate::state::{AppRuntime, AppSnapshot, UiLocale};

/// The menu-bar menu is native, so its strings cannot come from the WebView
/// translations. Every label is picked from the same locale the companion
/// window last selected.
fn label(locale: UiLocale, zh: &'static str, en: &'static str) -> &'static str {
    match locale {
        UiLocale::Zh => zh,
        UiLocale::En => en,
    }
}

fn active_agent_name<'a>(snapshot: &'a AppSnapshot, locale: UiLocale) -> &'a str {
    snapshot
        .active_agent
        .as_deref()
        .and_then(|id| snapshot.agents.iter().find(|agent| agent.id == id))
        .map(|agent| agent.display_name.as_str())
        .unwrap_or_else(|| label(locale, "无 Agent", "No Agent"))
}

pub fn build_tray_menu(
    app: &AppHandle,
    state: &Arc<AppRuntime>,
) -> tauri::Result<Menu<tauri::Wry>> {
    let snapshot = state.read();
    let preferences = snapshot.hint_preferences;
    let locale = snapshot.ui_locale;
    let active_name = active_agent_name(&snapshot, locale);
    let active = MenuItem::with_id(
        app,
        "active_agent",
        format!("{}{}", label(locale, "当前：", "Current: "), active_name),
        false,
        None::<&str>,
    )?;
    let show = MenuItem::with_id(
        app,
        "show_function_hint",
        label(locale, "显示功能键提示", "Show Function Key Hint"),
        true,
        None::<&str>,
    )?;
    let auto = CheckMenuItem::with_id(
        app,
        "auto_function_hint",
        label(
            locale,
            "切换 Agent 时自动显示",
            "Show automatically when switching Agent",
        ),
        true,
        preferences.auto_show,
        None::<&str>,
    )?;
    let pinned = CheckMenuItem::with_id(
        app,
        "pin_function_hint",
        label(locale, "固定功能键提示", "Pin Function Key Hint"),
        true,
        preferences.pinned,
        None::<&str>,
    )?;
    let open = MenuItem::with_id(
        app,
        "open_companion",
        label(locale, "打开 Companion", "Open Companion"),
        true,
        None::<&str>,
    )?;
    let panel = MenuItem::with_id(
        app,
        "toggle_session_panel",
        label(locale, "显示 / 隐藏会话浮窗", "Show / Hide Session Panel"),
        true,
        None::<&str>,
    )?;
    let hardware = CheckMenuItem::with_id(
        app,
        "esp32_hardware",
        label(locale, "ESP32 硬件支持", "ESP32 Hardware Support"),
        true,
        state
            .hardware_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        None::<&str>,
    )?;
    let separator_a = PredefinedMenuItem::separator(app)?;
    let separator_b = PredefinedMenuItem::separator(app)?;
    let quit = PredefinedMenuItem::quit(
        app,
        Some(label(
            locale,
            "退出 Agent Deck Companion",
            "Quit Agent Deck Companion",
        )),
    )?;
    Menu::with_items(
        app,
        &[
            &active,
            &show,
            &separator_a,
            &auto,
            &pinned,
            &separator_b,
            &open,
            &panel,
            &hardware,
            &quit,
        ],
    )
}

pub fn refresh_tray_menu(app: &AppHandle, state: &Arc<AppRuntime>) {
    let app = app.clone();
    let state = state.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let Some(tray) = app.tray_by_id("agent-deck") else {
            return;
        };
        if let Ok(menu) = build_tray_menu(&app, &state) {
            let _ = tray.set_menu(Some(menu));
        }
    });
}

pub fn update_tray_status(app: &AppHandle, snapshot: &AppSnapshot) {
    let name = active_agent_name(snapshot, snapshot.ui_locale);
    let short_name = snapshot
        .active_agent
        .as_deref()
        .map(|id| id.to_ascii_uppercase())
        .unwrap_or_else(|| "DECK".into());
    let surface = snapshot
        .active_surface
        .map(|value| format!(" · {value:?}"))
        .unwrap_or_default();
    let tooltip = format!("Agent Deck · {name}{surface}");
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let Some(tray) = app.tray_by_id("agent-deck") else {
            return;
        };
        let _ = tray.set_title(Some(short_name));
        let _ = tray.set_tooltip(Some(tooltip));
    });
}
