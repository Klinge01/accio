use crate::state::AppState;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};

pub fn hide(window: &WebviewWindow) -> tauri::Result<()> {
    if window.is_visible()? {
        let state = window.state::<AppState>();
        *state.hidden_at.lock().unwrap() = Some(Instant::now());
        window.hide()?;
    }
    Ok(())
}

pub fn toggle(app: &AppHandle) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("panel") else {
        return Ok(());
    };
    if window.is_visible()? {
        return hide(&window);
    }

    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|cursor| app.monitor_from_point(cursor.x, cursor.y).ok().flatten())
        .or_else(|| window.current_monitor().ok().flatten());
    if let Some(monitor) = monitor {
        let area = monitor.work_area();
        let size = window.outer_size()?;
        let x = area.position.x + (area.size.width as i64 - size.width as i64).max(0) as i32 / 2;
        let y = area.position.y + (area.size.height as f64 * 0.2) as i32;
        window.set_position(PhysicalPosition::new(x, y))?;
    }
    window.show()?;
    window.set_focus()?;
    let state = app.state::<AppState>();
    let hidden_ms = state
        .hidden_at
        .lock()
        .unwrap()
        .take()
        .map_or(0, |time| time.elapsed().as_millis() as u64);
    window.emit("panel://shown", hidden_ms)?;
    let provider = state.provider.lock().unwrap().clone();
    tauri::async_runtime::spawn(async move { provider.warm().await });
    Ok(())
}
