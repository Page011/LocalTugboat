//! Drag files and folders onto the Tugboat window: copy them into the folder being viewed. A folder is
//! zipped first (`zipdrop.rs`) and goes in as one "<folder name>.zip".
//!
//! Safety rules (the reason this module looks the way it does):
//! - The paths come ONLY from the native window event (`NativeDrop::from_native`, called by
//!   `shell.rs`). No Tauri command takes a path to copy, so a script running in the page cannot make
//!   the app copy an arbitrary file; a test reads the sources to keep it that way.
//! - The source is only ever read (CopyFileExW): a USB stick can be pulled right after "copied".
//!   Its own Mark-of-the-Web, if it has one, comes along; none is added (the user's own file). A zip is
//!   marked when any file inside it is, because Explorer passes a zip's mark on to what it unpacks.
//! - The copy (or the zip) is made in the hidden `.tugboat` folder under a temp name and only then placed in
//!   the library with a free name (hard link or checked rename), so the library never lists a half file.
//! - A failure is always a line in the report (refused or failed), never silent.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tugboat_core::{names, CoreError, FolderId, FolderKind};
use ts_rs::TS;

use crate::{
    library::Library,
    winfs,
    zipdrop::{self, Plan},
};

/// Room kept free on the library drive besides the files themselves.
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;
/// Progress is reported at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

pub const DRIVES_TEXT: &str = "DRIVES NOT COPIED - drop a folder or files";
pub const LINK_TEXT: &str = "FOLDER LINK NOT COPIED - drop the real folder";
pub const EMPTY_TEXT: &str = "FOLDER IS EMPTY - nothing to zip";
pub const OWN_TEXT: &str = "TUGBOAT DATA NOT COPIED - drop your own files";
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
    /// The file being copied, or the folder being zipped.
    pub file: String,
    /// 1-based position among the items being copied (files and folders).
    pub index: u32,
    pub count: u32,
    #[ts(type = "number")]
    pub done_bytes: u64,
    #[ts(type = "number")]
    pub total_bytes: u64,
    /// A folder being zipped (the bytes are those of the files read so far), not a file being copied.
    pub zip: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DropLine {
    /// Empty when the line is about the whole drop (a refusal), or is a folder that could not be zipped (a failure:
    /// the reason names the folder).
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

/// Count files and folders in a drag (the overlay's sub-line). Only a folder that would be zipped counts as
/// one: a link to a folder and a drive are refused on the drop, so they are not promised a zip here.
pub fn count_kinds(paths: &[PathBuf]) -> (u32, u32) {
    let (mut files, mut folders) = (0, 0);
    for p in paths {
        let zippable = p.parent().is_some() && std::fs::symlink_metadata(p).map(|m| m.is_dir() && !winfs::is_link(p, &m)).unwrap_or(false);
        if zippable {
            folders += 1;
        } else {
            files += 1;
        }
    }
    (files, folders)
}

/// One thing to bring in, judged before any copying starts.
enum Job {
    File { src: PathBuf, name: String, size: u64, mtime: i64 },
    Folder { name: String, plan: Plan },
}

impl Job {
    fn bytes(&self) -> u64 {
        match self {
            Job::File { size, .. } => *size,
            // What the zip will take on the drive: the file bytes and its own records.
            Job::Folder { plan, .. } => plan.bytes.saturating_add(plan.overhead),
        }
    }
}

/// Judge a dropped folder: its plan, or the line saying why it is not zipped (`true` = refused, `false` = failed).
/// `hidden` is the library's own hidden folder (canonical): it is never zipped and never dropped itself.
fn judge_folder(p: &Path, name: &str, hidden: Option<&Path>) -> Result<Plan, (bool, DropLine)> {
    let refused = |reason: &str| Err((true, DropLine { name: name.to_string(), reason: reason.to_string() }));
    if p.parent().is_none() {
        return refused(DRIVES_TEXT);
    }
    if std::fs::symlink_metadata(p).map(|m| winfs::is_link(p, &m)).unwrap_or(false) {
        return refused(LINK_TEXT);
    }
    if names::is_refused_name(&format!("{name}.zip")) {
        return refused(names::REFUSED_TEXT);
    }
    if hidden.is_some_and(|h| std::fs::canonicalize(p).is_ok_and(|c| c.starts_with(h))) {
        return refused(OWN_TEXT);
    }
    match zipdrop::plan_folder(p, hidden) {
        Ok(plan) if plan.files == 0 => refused(EMPTY_TEXT),
        Ok(plan) => Ok(plan),
        // The whole line is the reason ("ZIP FAILED - Trip: IMG_1.jpg ..."): it names the folder itself.
        Err(e) => Err((false, DropLine { name: String::new(), reason: e.text(name) })),
    }
}

/// Copy the dropped files into `target`; a dropped folder goes in as one zip. Runs on the calling thread;
/// `progress` is told how far the current item is. Everything that was not copied is in the report with its reason.
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

    // Judge every path first: refused names, vanished files and folders that cannot be zipped never reach the copy.
    let hidden = std::fs::canonicalize(root.join(crate::library::config::HIDDEN_DIR)).ok();
    let mut todo: Vec<Job> = Vec::new();
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
            // Listing a big tree on a slow drive takes a while: say so (no size yet) before it starts.
            progress(DropProgress { file: name.clone(), index: 1, count: 1, done_bytes: 0, total_bytes: 0, zip: true });
            match judge_folder(p, &name, hidden.as_deref()) {
                Ok(plan) => todo.push(Job::Folder { name, plan }),
                Err((true, line)) => rep.refused.push(line),
                Err((false, line)) => rep.failed.push(line),
            }
        } else if names::is_refused_name(&name) {
            rep.refused.push(DropLine { name, reason: names::REFUSED_TEXT.into() });
        } else {
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
            todo.push(Job::File { src: p.clone(), name, size: meta.len(), mtime });
        }
    }
    if todo.is_empty() {
        return rep;
    }

    let total: u64 = todo.iter().map(Job::bytes).sum();
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
    for (i, job) in todo.into_iter().enumerate() {
        let tmp = staging.join(format!("drop-{}.tmp", uuid::Uuid::now_v7()));
        let mut last = Instant::now().checked_sub(PROGRESS_EVERY).unwrap_or_else(Instant::now);
        let index = i as u32 + 1;
        let (name, mtime) = match job {
            Job::File { src, name, size, mtime } => {
                let mut on_bytes = |done: u64, all: u64| {
                    if last.elapsed() >= PROGRESS_EVERY || (all > 0 && done >= all) {
                        last = Instant::now();
                        progress(DropProgress { file: name.clone(), index, count, done_bytes: done, total_bytes: all.max(size), zip: false });
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
                (name, mtime)
            }
            Job::Folder { name, plan } => {
                let all = plan.bytes;
                // Also the only event a folder of empty files ever gets.
                progress(DropProgress { file: name.clone(), index, count, done_bytes: 0, total_bytes: all, zip: true });
                let mut on_bytes = |done: u64| {
                    if last.elapsed() >= PROGRESS_EVERY || done >= all {
                        last = Instant::now();
                        progress(DropProgress { file: name.clone(), index, count, done_bytes: done, total_bytes: all, zip: true });
                    }
                };
                if let Err(e) = zipdrop::write_zip(&tmp, &plan.entries, &mut on_bytes) {
                    let _ = std::fs::remove_file(&tmp);
                    rep.failed.push(DropLine { name: String::new(), reason: e.text(&name) });
                    continue;
                }
                // Unzipping hands the zip's mark to every file, so a marked file inside must mark the zip: a
                // folder keeps what a single dropped file keeps. A drive that cannot hold it gets no zip.
                if plan.entries.iter().filter_map(|e| e.source.as_deref()).any(|p| winfs::read_zone_mark(p).is_some()) && lib.inner.place.mark_downloaded(&tmp).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    rep.failed.push(DropLine { name: String::new(), reason: format!("ZIP FAILED - {name}: this drive cannot keep the downloaded mark") });
                    continue;
                }
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
                (format!("{name}.zip"), now)
            }
        };
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
/// (`.tugboat-<id>.tugboat-move` in the library root or one of its folders, see `placement::is_move_temp`: a
/// file of the user's own that merely ends in `.tugboat-move` is left alone). Returns how many.
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
    let is_move_copy = |p: &Path| p.file_name().is_some_and(|n| crate::library::placement::is_move_temp(&n.to_string_lossy()));
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
