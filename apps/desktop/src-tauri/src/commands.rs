//! Tauri commands: the one door between the webview and Rust. Each is a thin call into
//! `Backend`. Tauri capabilities gate plugin commands only, not these, so EVERY command checks
//! the calling window's label itself: only `main` may call them (a second window, an
//! injected iframe-hosting window or a future pop-out gets "forbidden").

use std::sync::Arc;

use tugboat_core::{
    events::SyncStatus, ActivityEntry, FolderId, FolderView, ItemId, ItemQuery, ItemView, Page, Settings,
};
use tauri::{AppHandle, State, WebviewWindow};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt as _;

use crate::{
    backend::{AppInfo, Backend, PairedDevice, PairingStart},
    error::{CmdError, CmdResult},
    firewall::FirewallInfo,
    library::LibraryStatus,
    updates::{self, UpdateReady, Updates},
};

pub const MAIN: &str = "main";

/// The caller rule, on the label alone so it is testable without a window.
pub fn check_caller(label: &str) -> CmdResult<()> {
    if label == MAIN {
        Ok(())
    } else {
        Err(CmdError::forbidden(label))
    }
}

fn allow(window: &WebviewWindow) -> CmdResult<()> {
    check_caller(window.label())
}

type Be<'a> = State<'a, Arc<Backend>>;
type Upd<'a> = State<'a, Arc<Updates>>;

#[tauri::command(async)]
pub fn app_info(window: WebviewWindow, be: Be) -> CmdResult<AppInfo> {
    allow(&window)?;
    Ok(be.app_info())
}

#[tauri::command(async)]
pub fn sync_status(window: WebviewWindow, be: Be) -> CmdResult<SyncStatus> {
    allow(&window)?;
    Ok(be.status())
}

#[tauri::command(async)]
pub fn folders(window: WebviewWindow, be: Be) -> CmdResult<Vec<FolderView>> {
    allow(&window)?;
    be.folders()
}

#[tauri::command(async)]
pub fn items(window: WebviewWindow, be: Be, query: ItemQuery) -> CmdResult<Page<ItemView>> {
    allow(&window)?;
    be.items(query)
}

#[tauri::command(async)]
pub fn item_set_pinned(window: WebviewWindow, be: Be, id: ItemId, pinned: bool) -> CmdResult<()> {
    allow(&window)?;
    be.set_pinned(id, pinned)
}

#[tauri::command(async)]
pub fn item_move(window: WebviewWindow, be: Be, id: ItemId, folder: FolderId) -> CmdResult<()> {
    allow(&window)?;
    be.move_item(id, folder)
}

#[tauri::command(async)]
pub fn item_rename(window: WebviewWindow, be: Be, id: ItemId, name: String) -> CmdResult<()> {
    allow(&window)?;
    be.rename_item(id, &name)
}

#[tauri::command(async)]
pub fn item_delete(window: WebviewWindow, be: Be, id: ItemId) -> CmdResult<()> {
    allow(&window)?;
    be.delete_item(id)
}

#[tauri::command(async)]
pub fn item_restore(window: WebviewWindow, be: Be, id: ItemId) -> CmdResult<()> {
    allow(&window)?;
    be.restore_item(id)
}

#[tauri::command(async)]
pub fn item_purge(window: WebviewWindow, be: Be, id: ItemId) -> CmdResult<()> {
    allow(&window)?;
    be.purge_item(id)
}

#[tauri::command(async)]
pub fn bin_empty(window: WebviewWindow, be: Be, shown: u32, token: String) -> CmdResult<u32> {
    allow(&window)?;
    be.empty_bin(shown, &token)
}

#[tauri::command(async)]
pub fn bin_count(window: WebviewWindow, be: Be) -> CmdResult<tugboat_core::BinCount> {
    allow(&window)?;
    be.bin_count()
}

/// Open the item's file in the default app.
#[tauri::command(async)]
pub fn item_open(window: WebviewWindow, app: AppHandle, be: Be, id: ItemId) -> CmdResult<()> {
    allow(&window)?;
    let path = be.item_path(id)?;
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(|e| CmdError::new("open", format!("COULD NOT OPEN - {e}")))
}

/// Show the item's file in Explorer.
#[tauri::command(async)]
pub fn item_reveal(window: WebviewWindow, app: AppHandle, be: Be, id: ItemId) -> CmdResult<()> {
    allow(&window)?;
    let path = be.item_path(id)?;
    app.opener().reveal_item_in_dir(path).map_err(|e| CmdError::new("open", format!("COULD NOT SHOW - {e}")))
}

#[tauri::command(async)]
pub fn activity(window: WebviewWindow, be: Be, before: Option<i64>, limit: u32) -> CmdResult<Vec<ActivityEntry>> {
    allow(&window)?;
    be.activity(before, limit)
}

#[tauri::command(async)]
pub fn settings_get(window: WebviewWindow, be: Be) -> CmdResult<Settings> {
    allow(&window)?;
    be.settings()
}

#[tauri::command]
pub async fn settings_save(window: WebviewWindow, be: Be<'_>, settings: Settings) -> CmdResult<()> {
    allow(&window)?;
    be.save_settings(settings).await
}

#[tauri::command(async)]
pub fn paired_device(window: WebviewWindow, be: Be) -> CmdResult<Option<PairedDevice>> {
    allow(&window)?;
    be.paired_device()
}

#[tauri::command(async)]
pub fn pairing_begin(window: WebviewWindow, be: Be) -> CmdResult<PairingStart> {
    allow(&window)?;
    be.pairing_begin()
}

#[tauri::command]
pub async fn pairing_confirm(window: WebviewWindow, be: Be<'_>, accept: bool) -> CmdResult<()> {
    allow(&window)?;
    be.pairing_confirm(accept).await
}

#[tauri::command(async)]
pub fn pairing_cancel(window: WebviewWindow, be: Be) -> CmdResult<()> {
    allow(&window)?;
    be.pairing_cancel();
    Ok(())
}

#[tauri::command]
pub async fn unpair(window: WebviewWindow, be: Be<'_>) -> CmdResult<()> {
    allow(&window)?;
    be.unpair().await
}

#[tauri::command(async)]
pub fn library_status(window: WebviewWindow, be: Be) -> CmdResult<LibraryStatus> {
    allow(&window)?;
    Ok(be.library_status())
}

#[tauri::command(async)]
pub fn library_choose(window: WebviewWindow, be: Be, path: String, accept_cloud: bool) -> CmdResult<()> {
    allow(&window)?;
    be.library_choose(&path, accept_cloud)
}

#[tauri::command(async)]
pub fn library_locate(window: WebviewWindow, be: Be, path: String) -> CmdResult<()> {
    allow(&window)?;
    be.library_locate(&path)
}

#[tauri::command(async)]
pub fn library_resolve_missing(window: WebviewWindow, be: Be, bin: bool) -> CmdResult<()> {
    allow(&window)?;
    be.library_resolve_missing(bin)
}

#[tauri::command(async)]
pub fn library_rescan(window: WebviewWindow, be: Be) -> CmdResult<()> {
    allow(&window)?;
    be.library_rescan();
    Ok(())
}

/// The gallery says which folder it is showing (null = All = Inbox): a folder id, never a path. What a
/// dropped file does is decided by the window's native drop event alone (see drop.rs).
#[tauri::command(async)]
pub fn drop_target_set(window: WebviewWindow, be: Be, folder: Option<FolderId>) -> CmdResult<()> {
    allow(&window)?;
    be.drop_target_set(folder);
    Ok(())
}

/// Open the library folder in Explorer.
#[tauri::command(async)]
pub fn library_open_folder(window: WebviewWindow, app: AppHandle, be: Be) -> CmdResult<()> {
    allow(&window)?;
    let root = be.library.root().ok_or_else(|| CmdError::new("not_configured", "LIBRARY NOT CHOSEN"))?;
    app.opener().open_path(root.to_string_lossy(), None::<&str>).map_err(|e| CmdError::new("open", format!("COULD NOT OPEN - {e}")))
}

#[tauri::command(async)]
pub fn firewall_status(window: WebviewWindow, be: Be) -> CmdResult<FirewallInfo> {
    allow(&window)?;
    Ok(be.firewall())
}

#[tauri::command(async)]
pub fn firewall_recheck(window: WebviewWindow, be: Be) -> CmdResult<FirewallInfo> {
    allow(&window)?;
    Ok(be.firewall_recheck())
}

#[tauri::command(async)]
pub fn firewall_fix(window: WebviewWindow, be: Be) -> CmdResult<()> {
    allow(&window)?;
    be.firewall_fix()
}

#[tauri::command(async)]
pub fn autostart_get(window: WebviewWindow, app: AppHandle) -> CmdResult<bool> {
    allow(&window)?;
    app.autolaunch().is_enabled().map_err(|e| CmdError::new("autostart", e.to_string()))
}

#[tauri::command(async)]
pub fn autostart_set(window: WebviewWindow, app: AppHandle, enabled: bool) -> CmdResult<()> {
    allow(&window)?;
    let al = app.autolaunch();
    let r = if enabled { al.enable() } else { al.disable() };
    r.map_err(|e| CmdError::new("autostart", e.to_string()))
}

/// The "Check for updates" switch (on by default; kept on this PC only).
#[tauri::command(async)]
pub fn update_check_get(window: WebviewWindow, up: Upd) -> CmdResult<bool> {
    allow(&window)?;
    Ok(up.enabled())
}

#[tauri::command(async)]
pub fn update_check_set(window: WebviewWindow, up: Upd, enabled: bool) -> CmdResult<()> {
    allow(&window)?;
    up.set_enabled(enabled)
}

/// The downloaded and verified update waiting to be installed, if any.
#[tauri::command(async)]
pub fn update_ready(window: WebviewWindow, up: Upd) -> CmdResult<Option<UpdateReady>> {
    allow(&window)?;
    Ok(up.ready())
}

/// [Install] on the update banner. Resolves to false when the user said no to the Windows prompt (nothing changed,
/// the banner stays). When the installer starts, this app exits and the installer starts it again.
#[tauri::command]
pub async fn update_install(window: WebviewWindow, app: AppHandle, up: Upd<'_>) -> CmdResult<bool> {
    allow(&window)?;
    let up: Arc<Updates> = up.inner().clone();
    tauri::async_runtime::spawn_blocking(move || updates::install(&app, &up))
        .await
        .map_err(|e| CmdError::new("update", format!("UPDATE NOT INSTALLED - {e}")))?
}

/// Quit for real (closing the window only hides it to the tray).
#[tauri::command(async)]
pub fn quit_app(window: WebviewWindow, app: AppHandle) -> CmdResult<()> {
    allow(&window)?;
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command declares the caller window and checks it first. A new command that forgets
    /// would be callable from any window; this reads the source so that cannot slip through.
    #[test]
    fn every_command_checks_the_caller_window() {
        let src = include_str!("commands.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        let chunks: Vec<&str> = body.split("#[tauri::command").skip(1).collect();
        assert!(chunks.len() >= 30, "found {} commands", chunks.len());
        for c in &chunks {
            let name = c.split("fn ").nth(1).and_then(|r| r.split('(').next()).unwrap_or("?");
            assert!(c.contains("window: WebviewWindow"), "{name} does not take the calling window");
            let first_stmt = c.split('{').nth(1).unwrap_or("").trim_start();
            assert!(first_stmt.starts_with("allow(&window)?;"), "{name} must check the caller before anything else");
        }
        // And every command is in the handler list, so a checked command is also reachable.
        let lib = include_str!("lib.rs");
        for c in &chunks {
            let name = c.split("fn ").nth(1).and_then(|r| r.split('(').next()).unwrap();
            assert!(lib.contains(&format!("commands::{name},")), "{name} is not registered in lib.rs");
        }
    }

    /// No command lets the page name a file to read, copy or open: a string, path or list argument
    /// must be on this list, and the list is the two folder pickers (the OS dialog chooses the folder)
    /// and the item rename (a display name, never a path). A dropped file's path comes from the
    /// window's native drop event only (drop.rs).
    #[test]
    fn no_command_takes_a_path_or_a_string_except_the_listed_ones() {
        let allowed: &[(&str, &str)] = &[("library_choose", "path"), ("library_locate", "path"), ("item_rename", "name")];
        let src = include_str!("commands.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        for c in body.split("#[tauri::command").skip(1) {
            let name = c.split("fn ").nth(1).and_then(|r| r.split('(').next()).unwrap_or("?");
            let params = c.split('(').nth(1).and_then(|r| r.split(") ->").next()).unwrap_or("");
            for p in params.split(',') {
                let Some((pname, ty)) = p.split_once(':') else { continue };
                let (pname, ty) = (pname.trim(), ty.trim());
                let risky = ty.contains("String") || ty.contains("Path") || ty.contains("Vec<") || ty.contains("&str");
                if risky {
                    assert!(
                        allowed.contains(&(name, pname)),
                        "command {name} takes `{pname}: {ty}`: text or a path from the page. Add it to the list only if it cannot be a path"
                    );
                }
            }
        }
    }

    #[test]
    fn only_the_main_window_may_call_commands() {
        assert!(check_caller("main").is_ok());
        for label in ["", "Main", "notes", "main2", "prompt"] {
            let e = check_caller(label).unwrap_err();
            assert_eq!(e.kind, "forbidden", "{label:?}");
        }
    }
}
