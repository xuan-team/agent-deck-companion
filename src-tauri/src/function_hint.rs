//! Platform window helpers for the compact K1-K10 cheat sheet.
//!
//! The window is deliberately non-focusable: showing a hint must never steal
//! keystrokes from the Agent surface that was just activated.

use tauri::{
    image::Image, AppHandle, Manager, PhysicalPosition, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

pub const WINDOW_LABEL: &str = "function-hint";

const WINDOW_WIDTH: f64 = 650.0;
const WINDOW_HEIGHT: f64 = 210.0;
const SCREEN_MARGIN: f64 = 18.0;
const MENU_BAR_GAP: f64 = 42.0;

/// A monochrome 2x5 key grid that remains legible as a macOS template icon.
pub fn menu_bar_icon() -> Image<'static> {
    const SIZE: usize = 18;
    let mut rgba = vec![0_u8; SIZE * SIZE * 4];
    for row in 0..2 {
        for column in 0..5 {
            let origin_x = 2 + column * 3;
            let origin_y = 4 + row * 6;
            for y in origin_y..origin_y + 4 {
                for x in origin_x..origin_x + 2 {
                    let offset = (y * SIZE + x) * 4;
                    rgba[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
    }
    Image::new_owned(rgba, SIZE as u32, SIZE as u32)
}

pub fn create_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("/".into()))
        .title("功能键提示")
        .inner_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .shadow(true)
        .visible(false)
        .build()
}

pub fn position_near_menu_bar(app: &AppHandle, window: &WebviewWindow) {
    let cursor = app.cursor_position().ok();
    let monitor = cursor
        .and_then(|point| app.monitor_from_point(point.x, point.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());

    let Some(monitor) = monitor else {
        return;
    };
    let scale = monitor.scale_factor();
    let size = monitor.size();
    let origin = monitor.position();
    let x = origin.x as f64 + size.width as f64 - (WINDOW_WIDTH + SCREEN_MARGIN) * scale;
    let y = origin.y as f64 + MENU_BAR_GAP * scale;
    let _ = window.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32));
}

pub fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        position_near_menu_bar(app, &window);
        let _ = window.show();
    }
}

pub fn hide(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.hide();
    }
}

pub fn toggle(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        position_near_menu_bar(app, &window);
        let _ = window.show();
    }
}
