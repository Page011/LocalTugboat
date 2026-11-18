//! Tugboat desktop. Rust owns the logic (Core, the library folder, keys, network,
//! firewall check, tray); the webview is UI only and talks through `commands`.
//!
//! - `backend`: everything, with no Tauri types (so it is tested without a window).
//! - `library`: the PC library folder (placement, watcher, rescan, pauses).
//! - `commands` + `shell`: the Tauri layer (commands, tray, close-to-tray, thumbnail scheme).

pub mod applog;
pub mod backend;
pub mod commands;
pub mod drop;
pub mod error;
pub mod firewall;
pub mod keys;
pub mod library;
pub mod paths;
pub mod qr;
pub mod shell;
pub mod thumbs;
pub mod tray;
pub mod winfs;

#[cfg(test)]
mod bindings;

use std::sync::Arc;

use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use backend::{Backend, StartOptions};

/// Show a fatal start-up problem and quit: a half-started app must never look like an empty one.
fn fatal(app: &tauri::AppHandle, title: &str, message: String) {
    tracing::error!("{title}: {message}");
    app.dialog().message(message).title(title).kind(MessageDialogKind::Error).show(|_| std::process::exit(1));
}

pub fn run() {
    tauri::Builder::default()
        // Must be first: a second launch shows the running window instead of starting another.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| shell::show_main(app)))
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec![shell::MINIMIZED_FLAG])))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .register_uri_scheme_protocol(shell::THUMB_SCHEME, shell::thumb_response)
        .on_window_event(shell::on_window_event)
        .setup(|app| {
            let handle = app.handle().clone();
            let paths = match paths::resolve_from_env() {
                Ok(p) => p,
                Err(e) => {
                    fatal(&handle, "Tugboat cannot start", e);
                    return Ok(());
                }
            };
            // Before anything can fail: a start-up error must reach the log too.
            match applog::init(&paths.data_dir.join("logs")) {
                Ok(()) => tracing::info!("Tugboat {} starting", handle.package_info().version),
                Err(e) => eprintln!("no app log: {e}"),
            }
            let store = match keys::CredentialStore::new(keys::SERVICE, &paths.key_user) {
                Ok(s) => s,
                Err(e) => {
                    fatal(&handle, "Tugboat cannot start", e.to_string());
                    return Ok(());
                }
            };
            let bind_env = std::env::var(paths::BIND_ENV).ok();
            let mdns = paths::mdns_allowed(bind_env.as_deref());
            let opts = StartOptions {
                paths,
                bind_env,
                app_version: handle.package_info().version.to_string(),
                device_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
                library: library::LibraryOptions::default(),
                mdns,
                check_firewall: paths::firewall_check_allowed(std::env::var(paths::PROFILE_ENV).is_ok(), std::env::var(paths::FIREWALL_ENV).ok().as_deref()),
            };
            shell::build_tray(&handle)?;
            shell::watch_display(&handle);
            let emitter = Arc::new(shell::TauriEmitter { app: handle.clone() });
            let started = tauri::async_runtime::block_on(Backend::start(opts, &store, emitter));
            match started {
                Ok(backend) => {
                    first_run_autostart(&handle, &backend);
                    app.manage(backend);
                }
                Err(e) => {
                    fatal(&handle, "Tugboat cannot start", format!("{e}\n\nFix the problem, then start Tugboat again."));
                    return Ok(());
                }
            }
            if !std::env::args().any(|a| a == shell::MINIMIZED_FLAG) {
                shell::show_main(&handle);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::sync_status,
            commands::folders,
            commands::items,
            commands::item_set_pinned,
            commands::item_move,
            commands::item_rename,
            commands::item_delete,
            commands::item_restore,
            commands::item_open,
            commands::item_reveal,
            commands::activity,
            commands::settings_get,
            commands::settings_save,
            commands::paired_device,
            commands::pairing_begin,
            commands::pairing_confirm,
            commands::pairing_cancel,
            commands::unpair,
            commands::library_status,
            commands::library_choose,
            commands::library_locate,
            commands::library_resolve_missing,
            commands::library_rescan,
            commands::drop_target_set,
            commands::library_open_folder,
            commands::firewall_status,
            commands::firewall_recheck,
            commands::firewall_fix,
            commands::autostart_get,
            commands::autostart_set,
            commands::quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tugboat");
}

/// Start with Windows is ON by default: registered once, on the first run of a release build
/// (a dev or QA exe must never be what Windows launches at sign-in). The user can switch it off
/// in Settings and it stays off.
fn first_run_autostart(app: &tauri::AppHandle, backend: &Backend) {
    use tauri_plugin_autostart::ManagerExt;
    const KEY: &str = "autostart_initialised";
    if cfg!(debug_assertions) || backend.paths_profile_active() {
        return;
    }
    match backend.core.get_value(KEY) {
        Ok(None) => {
            if let Err(e) = app.autolaunch().enable() {
                tracing::error!("start with Windows could not be turned on: {e}");
                return; // not marked done: tried again next start
            }
            if let Err(e) = backend.core.set_value(KEY, "1") {
                tracing::error!("could not remember the autostart default: {e}");
            }
        }
        Ok(Some(_)) => {}
        Err(e) => tracing::error!("autostart default not checked: {e}"),
    }
}
