//! Drag files onto the Tugboat window: copy them into the folder being viewed.
//!
//! Safety rules (the reason this module looks the way it does):
//! - The paths come ONLY from the native window event (`NativeDrop::from_native`, called by
//!   `shell.rs`). No Tauri command takes a path to copy, so a script running in the page cannot make
//!   the app copy an arbitrary file; a test reads the sources to keep it that way.
//! - The source is only ever read (CopyFileExW): a USB stick can be pulled right after "copied".
//!   Its own Mark-of-the-Web, if it has one, comes along; none is added (the user's own file).
//! - The copy is made in the hidden `.tugboat` folder under a temp name and only then placed in the
//!   library with a free name (hard link or checked rename), so the library never lists a half file.
//! - A failure is always a line in the report (refused or failed), never silent.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tugboat_core::{names, CoreError, FolderId, FolderKind};
use ts_rs::TS;

use crate::{library::Library, winfs};

/// Room kept free on the library drive besides the files themselves.
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;
/// Progress is reported at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

pub const FOLDERS_TEXT: &str = "FOLDERS NOT COPIED - drop the files inside it";
pub const NOTHING_TEXT: &str = "NOTHING TO COPY - drop files from Explorer";

/// The folder a drop lands in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTarget {
    pub id: FolderId,
    pub kind: FolderKind,
    pub name: String,
}

/// Paths of a drop as the window's native event gave them. Only `shell.rs` (and the tests) build one.
#[derive(Debug, Clone)]
pub struct NativeDrop {
    paths: Vec<PathBuf>,
}

impl NativeDrop {
    /// Only `shell.rs` calls this in the app (a test reads the sources to keep it so); the tests call it
    /// to play a drop. The webview can never reach it: no command takes a path.
    pub fn from_native(paths: &[PathBuf]) -> NativeDrop {
        NativeDrop { paths: paths.to_vec() }
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// Sent while files are dragged over the window: the overlay names the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DropState {
    Enter { files: u32, folders: u32, target_name: String },
    Leave,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DropProgress {
    pub file: String,
    /// 1-based position among the files being copied.
    pub index: u32,
    pub count: u32,
    #[ts(type = "number")]
    pub done_bytes: u64,
    #[ts(type = "number")]
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DropLine {
    /// Empty when the line is about the whole drop.
    pub name: String,
    pub reason: String,
}

/// What a drop did. `refused` = Tugboat will not take it; `failed` = it tried and could not.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
pub struct DropReport {
    pub folder_name: String,
    /// Names the files have in the library now.
    pub copied: Vec<String>,
    pub refused: Vec<DropLine>,
    pub failed: Vec<DropLine>,
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// Drive text for the "not enough space" line: "D:" for `D:\...`.
fn drive_of(p: &Path) -> String {
    match p.components().next() {
        Some(std::path::Component::Prefix(pre)) => pre.as_os_str().to_string_lossy().into_owned(),
        _ => "the library drive".to_string(),
    }
}

fn io_text(e: &std::io::Error) -> String {
    let t = e.to_string();
    t.split(" (os error").next().unwrap_or(&t).to_string()
}

/// Count files and folders in a drag (the overlay's sub-line). A link to a folder counts as a folder.
pub fn count_kinds(paths: &[PathBuf]) -> (u32, u32) {
    let (mut files, mut folders) = (0, 0);
    for p in paths {
        if std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false) {
            folders += 1;
        } else {
            files += 1;
        }
    }
    (files, folders)
}

/// Copy the dropped files into `target`. Runs on the calling thread; `progress` is told how far the
/// current file is. Everything that was not copied is in the report with its reason.
pub fn copy_dropped(lib: &Library, drop: NativeDrop, target: &DropTarget, progress: &mut dyn FnMut(DropProgress)) -> DropReport {
    let mut rep = DropReport { folder_name: target.name.clone(), ..Default::default() };
    let whole = |rep: &mut DropReport, reason: String| rep.refused.push(DropLine { name: String::new(), reason });

    if drop.paths.is_empty() {
        // A virtual drop (an Outlook attachment, a zip entry): there is no file on disk to copy.
        whole(&mut rep, NOTHING_TEXT.into());
        return rep;
    }
    let root = match lib.inner.drop_gate() {
        Ok(r) => r,
        Err(why) => {
            whole(&mut rep, why);
            return rep;
        }
    };

    // Judge every path first: folders, refused names and vanished files never reach the copy.
    let mut todo: Vec<(PathBuf, String, u64, i64)> = Vec::new();
    for p in &drop.paths {
        let name = file_name(p);
        let meta = match std::fs::metadata(p) {
            Ok(m) => m,
            Err(e) => {
                rep.refused.push(DropLine { name, reason: format!("NOT FOUND - {}", io_text(&e)) });
                continue;
            }
        };
        if meta.is_dir() {
            rep.refused.push(DropLine { name, reason: FOLDERS_TEXT.into() });
        } else if names::is_refused_name(&name) {
            rep.refused.push(DropLine { name, reason: names::REFUSED_TEXT.into() });
        } else {
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
            todo.push((p.clone(), name, meta.len(), mtime));
        }
    }
    if todo.is_empty() {
        return rep;
    }

    let total: u64 = todo.iter().map(|t| t.2).sum();
    match fs4::available_space(&root) {
        Ok(free) if free < total.saturating_add(SPACE_MARGIN) => {
            whole(&mut rep, format!("NOT ENOUGH SPACE on {} - nothing copied", drive_of(&root)));
            return rep;
        }
        Ok(_) => {}
        Err(e) => {
            whole(&mut rep, format!("LIBRARY DRIVE NOT READABLE - nothing copied ({})", io_text(&e)));
            return rep;
        }
    }
    let staging = match lib.inner.place.staging_dir() {
        Ok(d) => d,
        Err(e) => {
            whole(&mut rep, format!("COPY FAILED - {e}"));
            return rep;
        }
    };

    let count = todo.len() as u32;
    for (i, (src, name, size, mtime)) in todo.into_iter().enumerate() {
        let tmp = staging.join(format!("drop-{}.tmp", uuid::Uuid::now_v7()));
        let mut last = Instant::now().checked_sub(PROGRESS_EVERY).unwrap_or_else(Instant::now);
        let mut on_bytes = |done: u64, all: u64| {
            if last.elapsed() >= PROGRESS_EVERY || (all > 0 && done >= all) {
                last = Instant::now();
                progress(DropProgress { file: name.clone(), index: i as u32 + 1, count, done_bytes: done, total_bytes: all.max(size) });
            }
        };
        if let Err(e) = winfs::copy_file(&src, &tmp, &mut on_bytes) {
            let _ = std::fs::remove_file(&tmp);
            rep.failed.push(DropLine { name, reason: format!("COPY FAILED - {}", io_text(&e)) });
            continue;
        }
        // A copy of a hidden or read-only file must be an ordinary file: the library scan skips hidden
        // ones and would then call the item missing.
        if let Err(e) = winfs::clear_attributes(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            rep.failed.push(DropLine { name, reason: format!("COPY FAILED - {}", io_text(&e)) });
            continue;
        }
        match lib.inner.place_dropped(&tmp, &name, target, mtime) {
            Ok(final_name) => rep.copied.push(final_name),
            Err(CoreError::Refused(why)) => {
                let _ = std::fs::remove_file(&tmp);
                rep.refused.push(DropLine { name, reason: why });
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                rep.failed.push(DropLine { name, reason: format!("COPY FAILED - {e}") });
            }
        }
    }
    rep
}

/// Remove copies left behind by a quit or crash: a drop's (`.tugboat\drop-*.tmp`) and a cross-drive move's
/// (`*.tugboat-move` in the library root or one of its folders, see `placement::MOVE_EXT`). Returns how many.
pub fn clean_stale(root: &Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(root.join(crate::library::config::HIDDEN_DIR)) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with("drop-") && name.ends_with(".tmp") && std::fs::remove_file(e.path()).is_ok() {
                n += 1;
            }
        }
    }
    let is_move_copy = |p: &Path| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(crate::library::placement::MOVE_EXT));
    let mut remove_in = |dir: &Path| {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if e.file_type().is_ok_and(|t| t.is_file()) && is_move_copy(&p) && std::fs::remove_file(&p).is_ok() {
                    n += 1;
                }
            }
        }
    };
    remove_in(root);
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with('.') {
                remove_in(&e.path());
            }
        }
    }
    n
}
