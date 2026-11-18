//! The PC library: a real folder the user can open in Explorer, kept in step with Core.
//!
//! - `config`: library path, id marker, folder-to-directory map (`library.json`).
//! - `placement`: `PcLibrary`, the Placement Core uses to put files in the folder.
//! - `reconcile`: one scan pass compares the disk with what Core holds and turns Explorer
//!   actions into Core calls (new file, edit, move, rename, delete, new/renamed/deleted
//!   subfolder). The watcher only says "look again"; the pass is the same code the start-up
//!   and 10-minute full rescans run, so a dropped watcher event can never lose a change.
//! - `worker`: the thread that owns the watcher, the debounce and the timers.
//! - `Library::drop_in`: the copy a file dropped on the window gets into the library (drop.rs
//!   does the copying; this does the placing and the Core call under the same lock as a scan).

pub mod config;
pub mod placement;
mod reconcile;
mod worker;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tugboat_core::{Core, ItemId};
use ts_rs::TS;

use crate::winfs::FileIdentity;
use config::LibraryConfig;
use placement::PcLibrary;

pub use reconcile::PassReport;
pub use worker::Worker;

#[cfg(test)]
mod tests;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone)]
pub struct LibraryOptions {
    /// A new file counts after its size has been stable this long (PLAN: 2 seconds).
    pub stable_for: Duration,
    /// Quiet time after the last watcher event before a pass runs.
    pub debounce: Duration,
    /// Longest a stream of events may hold a pass back.
    pub max_debounce: Duration,
    /// Full rescan interval (PLAN: 10 minutes).
    pub rescan_every: Duration,
}

impl Default for LibraryOptions {
    fn default() -> Self {
        LibraryOptions {
            stable_for: Duration::from_secs(2),
            debounce: Duration::from_millis(400),
            max_debounce: Duration::from_secs(5),
            rescan_every: Duration::from_secs(600),
        }
    }
}

/// A file in the library folder that Tugboat does not sync, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct NotSyncedFile {
    pub path: String,
    pub name: String,
    /// One short line, uppercase key word ("REFUSED - ...", "IN PROGRESS - ...").
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum LibraryHealth {
    /// No library chosen yet (first run with a non-empty default folder).
    NotConfigured,
    Ready,
    /// Folder or its identity marker is gone: sync is paused.
    Missing { path: String },
    /// Too many files vanished at once: sync is paused until the user decides.
    BulkMissing { missing: u32, total: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LibraryStatus {
    pub path: Option<String>,
    pub health: LibraryHealth,
    /// One short line for the banner when health is not Ready.
    pub message: Option<String>,
    /// Set when the library folder looks cloud-synced.
    pub cloud_warning: Option<String>,
    pub not_synced: Vec<NotSyncedFile>,
    /// The library drive cannot keep the Mark-of-the-Web (FAT, exFAT): files from the phone wait
    /// instead of being saved unmarked.
    pub cannot_mark: bool,
    #[ts(type = "number | null")]
    pub last_scan_at: Option<i64>,
    /// The first error of the last scan (an unreadable folder, a locked file), if any.
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PauseKind {
    NotConfigured,
    Missing,
    Bulk,
}

/// What the last scan saw of one file; stable_for is measured from `since`.
pub(crate) struct Seen {
    pub size: u64,
    pub mtime: i64,
    pub since: std::time::Instant,
}

pub(crate) struct Bulk {
    pub missing: u32,
    pub total: u32,
    /// The files the user was shown: [Move them to the Bin] bins these and no others.
    pub ids: HashSet<ItemId>,
}

pub(crate) struct State {
    pub cfg: LibraryConfig,
    pub pause: Option<PauseKind>,
    pub pause_reason: Option<String>,
    pub bulk: Option<Bulk>,
    pub not_synced: Vec<NotSyncedFile>,
    /// Files seen but not yet stable (new or being edited), by normalised path.
    pub pending: HashMap<String, Seen>,
    /// NTFS identity per path, valid while size and mtime are unchanged.
    pub ids: HashMap<String, (u64, i64, FileIdentity)>,
    /// mtime last handled per path: a later mtime means the file was edited.
    pub mtimes: HashMap<String, i64>,
    /// Files judged not importable (content does not match a photo/video type, too large):
    /// (size, mtime, reason line) when judged, so they are listed and not retried until they change.
    pub rejected: HashMap<String, (u64, i64, String)>,
    /// The library drive cannot hold named streams (no Mark-of-the-Web possible).
    pub cannot_mark: bool,
    pub last_scan_at: Option<i64>,
    pub last_error: Option<String>,
    /// Old files choose() could not delete: kept in the status until they are gone.
    pub leftovers: Vec<PathBuf>,
    pub published: Option<LibraryStatus>,
    /// Set when a missing library came back: the worker makes a fresh watcher.
    pub rewatch: bool,
    /// Set while the file watcher is not running; shown in `last_error` on every pass, because
    /// a pass rewrites `last_error` and would otherwise hide it at once.
    pub watcher_error: Option<String>,
}

pub(crate) struct Inner {
    pub core: Core,
    pub place: Arc<PcLibrary>,
    pub data_dir: PathBuf,
    pub opts: LibraryOptions,
    pub st: Mutex<State>,
    pub notify: Box<dyn Fn(&LibraryStatus) + Send + Sync>,
}

/// Handle to the library: scans, status and the user's library actions.
#[derive(Clone)]
pub struct Library {
    pub(crate) inner: Arc<Inner>,
}

/// Unix milliseconds of the system clock (file times are real time, not the injected one).
pub(crate) fn unix_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl Library {
    /// Build the handle. First run: a missing or empty default folder becomes the library
    /// (marker written); a non-empty one is left alone and the status says NotConfigured.
    pub fn open(
        core: Core,
        place: Arc<PcLibrary>,
        data_dir: PathBuf,
        cfg: LibraryConfig,
        default_root: &Path,
        opts: LibraryOptions,
        notify: Box<dyn Fn(&LibraryStatus) + Send + Sync>,
    ) -> std::io::Result<Library> {
        let mut cfg = cfg;
        if cfg.path.is_none() {
            match reconcile::check_new_library(default_root) {
                Ok(()) => {
                    std::fs::create_dir_all(default_root)?;
                    cfg.id = Some(config::write_new_marker(default_root)?);
                    cfg.path = Some(default_root.to_path_buf());
                    cfg.legacy_done = true; // a library made now was empty: nothing pre-dates 0.2.0
                    cfg.save(&data_dir)?;
                    place.set_root(cfg.path.clone());
                }
                Err(why) => tracing::info!("default library folder not used: {why}"),
            }
        }
        let st = State {
            cfg,
            pause: None,
            pause_reason: None,
            bulk: None,
            not_synced: Vec::new(),
            pending: HashMap::new(),
            ids: HashMap::new(),
            mtimes: HashMap::new(),
            rejected: HashMap::new(),
            cannot_mark: false,
            last_scan_at: None,
            last_error: None,
            leftovers: Vec::new(),
            published: None,
            rewatch: false,
            watcher_error: None,
        };
        Ok(Library { inner: Arc::new(Inner { core, place, data_dir, opts, st: Mutex::new(st), notify }) })
    }

    pub fn status(&self) -> LibraryStatus {
        self.inner.status_of(&lock(&self.inner.st))
    }

    /// One scan pass now, on the calling thread (the worker and the tests both use this).
    pub fn scan(&self, full: bool) -> PassReport {
        self.inner.pass(full, std::time::Instant::now())
    }

    /// Same, with the clock the stability rule reads (tests move it without sleeping).
    pub fn scan_at(&self, full: bool, now: std::time::Instant) -> PassReport {
        self.inner.pass(full, now)
    }

    /// Choose another library folder. Must be new or empty. Held files are copied over first;
    /// the switch happens only when every copy succeeded.
    pub fn choose(&self, path: &Path, accept_cloud: bool) -> Result<(), crate::error::CmdError> {
        self.inner.choose(path, accept_cloud)
    }

    /// The missing-library [Locate]: point at the folder that holds the marker of this library.
    pub fn locate(&self, path: &Path) -> Result<(), crate::error::CmdError> {
        self.inner.locate(path)
    }

    /// The bulk-missing choice: `bin` = move the missing files to the Bin now; false = keep waiting.
    pub fn resolve_missing(&self, bin: bool) -> Result<(), crate::error::CmdError> {
        self.inner.resolve_missing(bin)
    }

    pub fn root(&self) -> Option<PathBuf> {
        lock(&self.inner.st).cfg.path.clone()
    }
}
