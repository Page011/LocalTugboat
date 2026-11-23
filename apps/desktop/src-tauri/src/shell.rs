//! The Tauri shell around the backend: main window, close-to-tray, tray icon and menu, the
//! thumbnail scheme, and the emitter that carries backend events to the webview.

use std::sync::{Arc, Mutex};

use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, DragDropEvent, Emitter as _, Manager, WindowEvent,
};

use crate::{
    backend::{Backend, Emitter},
    commands::MAIN,
    drop::NativeDrop,
    tray::{self, TrayState},
    updates,
};

pub const TRAY_ID: &str = "tugboat";
/// The state the tray shows now, so a display scale change can redraw it without waiting for the next sync event.
static TRAY_NOW: Mutex<TrayState> = Mutex::new(TrayState::Offline);
/// Autostart launches with this flag: start hidden in the tray.
pub const MINIMIZED_FLAG: &str = "--minimized";
/// URL scheme that serves item thumbnails to the webview: `snapthumb://<item id>`.
pub const THUMB_SCHEME: &str = "snapthumb";

/// Backend events -> webview events and tray updates.
pub struct TauriEmitter {
    pub app: AppHandle,
}

impl Emitter for TauriEmitter {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        if let Err(e) = self.app.emit_to(MAIN, event, payload) {
            tracing::warn!("event {event} not delivered: {e}");
        }
    }

    fn tray(&self, state: TrayState, tooltip: &str) {
        *TRAY_NOW.lock().unwrap_or_else(|p| p.into_inner()) = state;
        let Some(t) = self.app.tray_by_id(TRAY_ID) else { return };
        set_tray_icon(&self.app, state);
        if let Err(e) = t.set_tooltip(Some(tooltip)) {
            tracing::warn!("tray tooltip not updated: {e}");
        }
    }
}

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Closing the window hides it; Quit (tray menu or Settings) is the way out. Files dragged onto the
/// window are copied into the library: this native event is the ONLY place a copy's paths come from.
pub fn on_window_event(window: &tauri::Window, event: &WindowEvent) {
    if window.label() != MAIN {
        return;
    }
    match event {
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = window.hide();
        }
        WindowEvent::DragDrop(e) => on_drag_drop(window.app_handle(), e),
        _ => {}
    }
}

fn on_drag_drop(app: &AppHandle, e: &DragDropEvent) {
    let Some(state) = app.try_state::<Arc<Backend>>() else { return };
    let backend: Arc<Backend> = state.inner().clone();
    match e {
        DragDropEvent::Enter { paths, .. } => backend.drop_enter(paths.clone()),
        DragDropEvent::Leave => backend.drop_leave(),
        DragDropEvent::Drop { paths, .. } => {
            let files = NativeDrop::from_native(paths);
            // The folder is fixed here, at the drop; a drop that waits its turn keeps it.
            let target = backend.drop_end();
            // A big copy must not freeze the window: its own thread. The result always arrives as an event.
            if let Err(e) = std::thread::Builder::new().name("tugboat-drop".into()).spawn(move || {
                backend.drop_files(files, target);
            }) {
                tracing::error!("drop thread did not start: {e}");
            }
        }
        _ => {}
    }
}

/// Tray glyph edge for the primary monitor's scale (16 px at 100%, 20 at 125%, 24 at 150%, 32 at 200%, ...).
fn tray_icon_size(app: &AppHandle) -> u32 {
    let scale = app.primary_monitor().ok().flatten().map_or(1.0, |m| m.scale_factor());
    tray::size_for_scale(scale)
}

/// The icon for `state` at the current scale. An icon that cannot be read (never expected: they are compiled in and
/// tested) is logged and the tray keeps its last icon.
fn tray_image(app: &AppHandle, state: TrayState) -> Option<Image<'static>> {
    let size = tray_icon_size(app);
    match tray::glyph_rgba(state, size) {
        Ok(rgba) => Some(Image::new_owned(rgba, size, size)),
        Err(e) => {
            tracing::warn!("tray icon not drawn: {e}");
            None
        }
    }
}

fn set_tray_icon(app: &AppHandle, state: TrayState) {
    let (Some(t), Some(icon)) = (app.tray_by_id(TRAY_ID), tray_image(app, state)) else { return };
    if let Err(e) = t.set_icon(Some(icon)) {
        tracing::warn!("tray icon not updated: {e}");
    }
}

/// Draw the tray again for the state it shows now (display scale changed).
fn refresh_tray(app: &AppHandle) {
    let state = *TRAY_NOW.lock().unwrap_or_else(|p| p.into_inner());
    set_tray_icon(app, state);
}

/// Follow the display scale: Windows sends the main window WM_DPICHANGED / WM_DISPLAYCHANGE when the scale can have
/// changed. The window subclass lives as long as the window (the app).
#[cfg(windows)]
pub fn watch_display(app: &AppHandle) {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        UI::Shell::{DefSubclassProc, SetWindowSubclass},
    };
    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, data: usize) -> LRESULT {
        if tray::needs_new_icon(msg) {
            // SAFETY: `data` is the leaked Box<AppHandle> given to SetWindowSubclass below; it is never freed.
            let app = unsafe { &*(data as *const AppHandle) };
            refresh_tray(app);
        }
        // SAFETY: forwards the untouched message to the next handler, as DefSubclassProc requires.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }
    let Some(window) = app.get_webview_window(MAIN) else { return };
    let Ok(raw) = window.hwnd() else { return };
    let hwnd = HWND(raw.0);
    let data = Box::into_raw(Box::new(app.clone())) as usize;
    // SAFETY: called on the main thread that owns the window; `proc` matches SUBCLASSPROC; `data` stays valid.
    let ok = unsafe { SetWindowSubclass(hwnd, Some(proc), 0x7447, data) }.as_bool();
    if !ok {
        tracing::warn!("tray follows a display scale change only at the next sync event: window subclass failed");
    }
}

#[cfg(not(windows))]
pub fn watch_display(_app: &AppHandle) {}

/// The tray menu: Open, Quit, and "Install update X.Y.Z" between them while a verified update waits.
fn tray_menu(app: &AppHandle, update: Option<&str>) -> tauri::Result<Menu<tauri::Wry>> {
    let open = MenuItem::with_id(app, "open", "Open Tugboat", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    match update {
        Some(version) => {
            let install = MenuItem::with_id(app, "install_update", updates::tray_item_text(version), true, None::<&str>)?;
            Menu::with_items(app, &[&open, &install, &quit])
        }
        None => Menu::with_items(app, &[&open, &quit]),
    }
}

/// Show (Some) or remove (None) the "Install update" item.
pub fn set_update_item(app: &AppHandle, version: Option<&str>) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    match tray_menu(app, version) {
        Ok(menu) => {
            if let Err(e) = tray.set_menu(Some(menu)) {
                tracing::warn!("tray menu not updated: {e}");
            }
        }
        Err(e) => tracing::warn!("tray menu not built: {e}"),
    }
}

/// Hide or show the tray icon (hidden for the moment the update installer takes over).
pub fn set_tray_visible(app: &AppHandle, visible: bool) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(e) = tray.set_visible(visible) {
            tracing::warn!("tray icon visibility not changed: {e}");
        }
    }
}

pub fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = tray_menu(app, None)?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tray_image(app, TrayState::Offline).ok_or_else(|| tauri::Error::InvalidIcon(std::io::Error::other("tray icon missing")))?)
        .tooltip("Tugboat")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, e| match e.id.as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            "install_update" => updates::install_from_tray(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Serve `snapthumb://<item id>` (Windows: http://snapthumb.localhost/<id>). Only the main
/// webview may ask; an unknown id, an item without a thumbnail or anything outside the
/// thumbnails folder is a 404.
pub fn thumb_response(ctx: tauri::UriSchemeContext<'_, tauri::Wry>, request: tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    let respond = |status: u16, body: Vec<u8>| {
        // A 404 is never cached: the thumbnail may be made a moment later and the tile asks again.
        let cache = if status == 200 { "private, max-age=3600" } else { "no-store" };
        tauri::http::Response::builder()
            .status(status)
            .header("Content-Type", "image/jpeg")
            .header("Cache-Control", cache)
            .body(body)
            .unwrap_or_default()
    };
    if ctx.webview_label() != MAIN {
        return respond(403, Vec::new());
    }
    let id = request.uri().path().trim_start_matches('/').to_string();
    let backend = ctx.app_handle().try_state::<Arc<Backend>>();
    match backend.and_then(|b| b.thumbnail(&id)) {
        Some(bytes) => respond(200, bytes),
        None => respond(404, Vec::new()),
    }
}
