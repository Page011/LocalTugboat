//! `PcLibrary`: the Placement the desktop gives Core. Files live in the library folder the
//! user can see: root = Inbox, one subfolder per other folder, `.tugboat\<hash>.part` for
//! in-progress downloads (same drive as the library, so finishing a download is a rename), and
//! the Bin under `%LOCALAPPDATA%\Local Tugboat\Bin\<item id>.<ext>`.
//!
//! Contract C1-C6 (crates/core/src/placement.rs) as kept here:
//! - C2 never overwrite another item's file: a new name is made free with `names::disambiguate`
//!   and the file is put there with a hard link (fails if the name appeared meanwhile) or, where
//!   hard links do not exist (FAT, exFAT), a checked rename. Only the SAME item's own old file
//!   is replaced.
//! - C3 every method returns the path it ended up at.
//! - C4 a failure leaves the file where it was.
//! - C5 no database access. What the watcher must not mistake for a user action is remembered
//!   in two small time-limited sets (files just put somewhere, files just taken away), which the
//!   rescan reads before it imports "new" files or bins "missing" ones.
//! - C6 relocate, to_bin, from_bin, purge are idempotent.
//! - C7 (0.2.0) `commit` marks every file RECEIVED from the other device as downloaded
//!   (Mark-of-the-Web, `winfs::write_zone_mark`) on the part BEFORE the hard link / rename, so
//!   there is no moment the file is visible in the library unmarked, and fails closed when the
//!   mark cannot be written. `adopt` (the user's own files) never adds a mark.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard, RwLock,
    },
    time::{Duration, Instant},
};

use tugboat_core::{
    names::{self, SNIFF_LEN},
    placement::{FileTarget, Placement},
    CoreError, FolderKind, Result,
};

use super::config::{DirEntry, HIDDEN_DIR};
use crate::winfs;

/// How long a path stays in the recent sets. The rescan only consults them for the few
/// seconds between a placement move and Core writing the new path to the database.
const RECENT_TTL: Duration = Duration::from_secs(60);
const ERROR_NOT_SAME_DEVICE: i32 = 17;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Windows paths compare case-insensitively, accept both separators and may spell one folder
/// with "." and ".." segments (library paths come from the webview). Those are collapsed here,
/// so `is_inside` cannot be fooled by `elsewhere\..\data`.
pub fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    let s = s.strip_prefix("\\\\?\\").unwrap_or(&s);
    let mut parts: Vec<&str> = Vec::new();
    for seg in s.split('\\') {
        match seg {
            "." => {}
            // Never above the drive or share root.
            ".." => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            _ => parts.push(seg),
        }
    }
    parts.join("\\")
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

/// True when `p` is `root` or below it.
pub fn is_inside(root: &Path, p: &Path) -> bool {
    let (r, p) = (norm(root), norm(p));
    let r = r.trim_end_matches('\\');
    p == r || p.starts_with(&format!("{r}\\"))
}

fn not_found(path: &Path) -> CoreError {
    CoreError::io(path, io::Error::new(io::ErrorKind::NotFound, "file not found"))
}

fn read_head(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut buf = vec![0u8; SNIFF_LEN];
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    buf.truncate(n);
    Ok(buf)
}

fn remove_if_exists(p: &Path) -> Result<()> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CoreError::io(p, e)),
    }
}

/// Extension of the copy `move_across` makes next to its destination. It sits in the VISIBLE library folder, so the
/// scan treats it as in progress (never imported) and the start-up clean-up removes one a crash left.
pub const MOVE_EXT: &str = "tugboat-move";
const MOVE_PREFIX: &str = ".tugboat-";

/// A fresh name for that copy: `.tugboat-<32 hex>.tugboat-move`. A file the user or the phone brought in
/// ("Notes.tugboat-move") never has this shape, so it is never taken for a leftover.
fn move_temp_name() -> String {
    format!("{MOVE_PREFIX}{}.{MOVE_EXT}", uuid::Uuid::now_v7().simple())
}

/// Is this file name one of this app's own move copies (see `move_temp_name`)?
pub fn is_move_temp(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower
        .strip_prefix(MOVE_PREFIX)
        .and_then(|rest| rest.strip_suffix(MOVE_EXT))
        .and_then(|rest| rest.strip_suffix('.'))
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Move a file; across drives (the Bin is under %LOCALAPPDATA%, the library may be anywhere)
/// copy next to the destination, then rename, then delete the source. A failure at any step
/// leaves the source in place (C4). `replace` = an existing `to` may be overwritten (the Bin
/// holds only this item's own file); false = `ErrorKind::AlreadyExists` instead, so a file that
/// appeared at `to` meanwhile is never lost.
pub fn move_across(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    let rename = |a: &Path, b: &Path| if replace { std::fs::rename(a, b) } else { winfs::rename_no_replace(a, b) };
    match rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE) => {
            let tmp = to.with_file_name(move_temp_name());
            if let Err(e) = std::fs::copy(from, &tmp) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            if let Err(e) = rename(&tmp, to) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            std::fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

/// Put `src` into `dir` under `name` without ever overwriting someone else's file. `own` is the
/// item's own current file: it may be replaced. Returns the final path; `src` is consumed.
pub(crate) fn place_new(src: &Path, dir: &Path, name: &str, own: Option<&Path>) -> io::Result<PathBuf> {
    for _ in 0..64 {
        let taken = |n: &str| {
            let p = dir.join(n);
            p.exists() && !own.is_some_and(|o| same_path(o, &p))
        };
        let candidate = dir.join(names::disambiguate(name, &taken));
        if own.is_some_and(|o| same_path(o, &candidate)) {
            // Same item, new content: replace in place (rename replaces on Windows).
            std::fs::rename(src, &candidate)?;
            return Ok(candidate);
        }
        match std::fs::hard_link(src, &candidate) {
            Ok(()) => {
                std::fs::remove_file(src)?;
                return Ok(candidate);
            }
            // The name was taken between the check and the link: pick the next free one.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // No hard links on this volume (FAT/exFAT), or src is on another drive.
                // A move that never replaces: a file created under `candidate` since the check
                // (std::fs::rename would overwrite it on Windows) sends us to the next free name.
                match move_across(src, &candidate, false) {
                    Ok(()) => return Ok(candidate),
                    // Appeared after the check above: the rename refused, pick the next name.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("no free name for {name} in {}", dir.display())))
}

pub struct PcLibrary {
    root: RwLock<Option<PathBuf>>,
    /// Set while Choose copies the library to a new folder: every placement that would write
    /// or rename a file under the old root is refused (retryable), so nothing can land there
    /// after the list of files to copy was taken.
    switching: AtomicBool,
    bin: PathBuf,
    dirs: Mutex<BTreeMap<String, DirEntry>>,
    dirty: AtomicBool,
    /// Paths this placement created or moved a file to (rescan: not a user drop).
    arrived: Mutex<HashMap<String, Instant>>,
    /// Paths this placement moved a file away from (rescan: not "missing").
    departed: Mutex<HashMap<String, Instant>>,
    /// Test seam: make the Mark-of-the-Web write fail (a drive that cannot hold it).
    #[cfg(test)]
    pub(crate) fail_marks: AtomicBool,
    /// Test seam: `copy_marked` loses the mark, as a copy onto FAT/exFAT does.
    #[cfg(test)]
    pub(crate) strip_copy_marks: AtomicBool,
}

impl PcLibrary {
    pub fn new(root: Option<PathBuf>, bin: PathBuf, dirs: BTreeMap<String, DirEntry>) -> PcLibrary {
        PcLibrary {
            root: RwLock::new(root),
            switching: AtomicBool::new(false),
            bin,
            dirs: Mutex::new(dirs),
            dirty: AtomicBool::new(false),
            arrived: Mutex::new(HashMap::new()),
            departed: Mutex::new(HashMap::new()),
            #[cfg(test)]
            fail_marks: AtomicBool::new(false),
            #[cfg(test)]
            strip_copy_marks: AtomicBool::new(false),
        }
    }

    pub fn root(&self) -> Option<PathBuf> {
        self.root.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_root(&self, root: Option<PathBuf>) {
        *self.root.write().unwrap_or_else(|e| e.into_inner()) = root;
    }

    /// Start or end the refusal of placements (see `switching`).
    pub fn set_switching(&self, on: bool) {
        self.switching.store(on, Ordering::SeqCst);
    }

    fn not_switching(&self) -> Result<()> {
        if self.switching.load(Ordering::SeqCst) {
            return Err(CoreError::Paused("MOVING LIBRARY - please wait".into()));
        }
        Ok(())
    }

    pub fn bin_dir(&self) -> &Path {
        &self.bin
    }

    /// The placement just put a file here, or Core is about to record one the placement put here.
    pub(crate) fn mark_arrived(&self, p: &Path) {
        Self::touch(&self.arrived, p);
    }

    pub fn dirs(&self) -> BTreeMap<String, DirEntry> {
        lock(&self.dirs).clone()
    }

    pub fn replace_dirs(&self, dirs: BTreeMap<String, DirEntry>) {
        *lock(&self.dirs) = dirs;
    }

    /// Apply a scan's directory-map changes on top of whatever the placement added meanwhile
    /// (a snapshot-and-replace would lose entries made between the two).
    pub fn apply_dir_changes(&self, changes: &[(String, Option<DirEntry>)]) {
        if changes.is_empty() {
            return;
        }
        let mut dirs = lock(&self.dirs);
        for (k, e) in changes {
            match e {
                Some(e) => {
                    dirs.insert(k.clone(), e.clone());
                }
                None => {
                    dirs.remove(k);
                }
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// True once since the last call when a placement changed the directory map.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }

    fn touch(map: &Mutex<HashMap<String, Instant>>, p: &Path) {
        let mut m = lock(map);
        m.retain(|_, t| t.elapsed() < RECENT_TTL);
        m.insert(norm(p), Instant::now());
    }

    fn recent(map: &Mutex<HashMap<String, Instant>>, p: &Path) -> bool {
        lock(map).get(&norm(p)).is_some_and(|t| t.elapsed() < RECENT_TTL)
    }

    /// The placement just put a file here: a rescan must not import it as a user drop.
    pub fn recently_arrived(&self, p: &Path) -> bool {
        Self::recent(&self.arrived, p)
    }

    /// The placement just took a file from here: a rescan must not call it missing.
    pub fn recently_departed(&self, p: &Path) -> bool {
        Self::recent(&self.departed, p)
    }

    fn need_root(&self) -> Result<PathBuf> {
        self.root().ok_or_else(|| CoreError::Invalid("LIBRARY NOT CHOSEN - choose a library folder first".into()))
    }

    /// The hidden `.tugboat` folder (part files, drop copies), made when missing.
    pub(crate) fn staging_dir(&self) -> Result<PathBuf> {
        self.part_dir()
    }

    fn part_dir(&self) -> Result<PathBuf> {
        let dir = self.need_root()?.join(HIDDEN_DIR);
        if !dir.exists() {
            std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(&dir, e))?;
            if let Err(e) = winfs::hide(&dir) {
                tracing::warn!("{e}");
            }
        }
        Ok(dir)
    }

    fn bin_path(&self, t: &FileTarget) -> Result<PathBuf> {
        // The extension came from a synced record: only safe ones (never a refused type) become part of a path.
        if !names::is_safe_ext(&t.ext) || names::is_refused_ext(&t.ext) {
            return Err(CoreError::Invalid(format!("extension '{}' is not allowed", t.ext)));
        }
        Ok(self.bin.join(names::join_ext(&t.item.0.hyphenated().to_string(), &t.ext.to_ascii_lowercase())))
    }

    /// The directory for `t`'s folder (created when missing). Inbox = the root. A folder that
    /// has no directory yet gets the next free name and the entry is remembered.
    fn dir_for(&self, t: &FileTarget) -> Result<PathBuf> {
        self.dir_for_folder(t.folder, t.folder_kind, &t.folder_name)
    }

    /// Same, from the folder alone (a drag-and-drop copy has no item yet).
    pub(crate) fn dir_for_folder(&self, folder: tugboat_core::FolderId, kind: FolderKind, folder_name: &str) -> Result<PathBuf> {
        let root = self.need_root()?;
        if kind == FolderKind::Inbox {
            return Ok(root);
        }
        let key = folder.0.to_string();
        let mut dirs = lock(&self.dirs);
        let name = match dirs.get(&key) {
            Some(e) => e.name.clone(),
            None => {
                let base = names::clean_folder_name(folder_name).unwrap_or_else(|_| "Folder".to_string());
                // Claimed by another folder = taken. A directory that exists on disk but nobody
                // claimed is the user's own of that name: this folder adopts it (the rescan
                // would match it by name too).
                let taken = |n: &str| dirs.iter().any(|(k, e)| *k != key && e.name.eq_ignore_ascii_case(n));
                let name = names::disambiguate(&base, &taken);
                dirs.insert(key.clone(), DirEntry { name: name.clone(), volume: None, id: None });
                self.dirty.store(true, Ordering::SeqCst);
                name
            }
        };
        let path = root.join(&name);
        std::fs::create_dir_all(&path).map_err(|e| CoreError::io(&path, e))?;
        if let Some(e) = dirs.get_mut(&key) {
            if e.id.is_none() {
                if let Ok(i) = winfs::identity_of(&path) {
                    e.volume = Some(i.volume_serial);
                    e.id = Some(i.id_hex());
                    self.dirty.store(true, Ordering::SeqCst);
                }
            }
        }
        Ok(path)
    }

    /// The on-disk name for `t` read from the bytes of `file` (extension and magic checked).
    fn disk_name(&self, t: &FileTarget, file: &Path) -> Result<String> {
        let head = read_head(file)?;
        names::disk_file_name(&t.name, &t.ext, &head, t.captured_at).map(|(n, _)| n).map_err(|e| match e {
            // Visible, never retried in a loop: the file stays where it is and the caller lists it.
            names::NameError::Refused(_) => CoreError::Refused(names::REFUSED_TEXT.to_string()),
            other => CoreError::NotMedia(other.to_string()),
        })
    }

    /// `std::fs::copy` that fails when the source has a Mark-of-the-Web and the copy did not get it: copying onto
    /// FAT/exFAT drops the stream and reports success. The caller keeps its original (C4).
    pub(crate) fn copy_marked(&self, from: &Path, to: &Path) -> io::Result<u64> {
        let n = std::fs::copy(from, to)?;
        #[cfg(test)]
        if self.strip_copy_marks.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(format!("{}:Zone.Identifier", to.display()));
        }
        match winfs::read_zone_mark(from) {
            Some(mark) if winfs::read_zone_mark(to).as_deref() != Some(mark.as_str()) => {
                Err(io::Error::other("NOT MARKED AS DOWNLOADED - the new drive cannot hold the mark"))
            }
            _ => Ok(n),
        }
    }

    /// Mark `part` as downloaded (C7). Fails closed: Err = nothing may appear in the library.
    pub(crate) fn mark_downloaded(&self, part: &Path) -> Result<()> {
        #[cfg(test)]
        if self.fail_marks.load(Ordering::SeqCst) {
            return Err(CoreError::io(part, io::Error::other("test: marks are failing")));
        }
        winfs::write_zone_mark(part).map_err(|e| {
            CoreError::io(part, io::Error::other(format!("NOT MARKED AS DOWNLOADED - this drive cannot hold the mark, so the file was not saved ({e})")))
        })
    }
}

impl Placement for PcLibrary {
    fn part_path(&self, hash: &tugboat_core::BlobHash) -> Result<PathBuf> {
        Ok(self.part_dir()?.join(format!("{}.part", hex::encode(hash.0))))
    }

    fn commit(&self, part: &Path, t: &FileTarget) -> Result<PathBuf> {
        self.not_switching()?;
        // A record extension that is not safe fails here, before anything is placed (C4).
        let bin = self.bin_path(t)?;
        let dir = self.dir_for(t)?;
        let name = self.disk_name(t, part)?;
        // C7: the mark goes on the part first; the hard link / rename below carries the stream with the file.
        self.mark_downloaded(part)?;
        let dest = place_new(part, &dir, &name, t.current.as_deref()).map_err(|e| CoreError::io(part, e))?;
        // Same item, new content that landed under another name or folder: its old file goes.
        if let Some(old) = &t.current {
            if !same_path(old, &dest) {
                if let Err(e) = remove_if_exists(old) {
                    // The old file is locked: undo the placement (C4) so the part and the old
                    // file are as they were, and no unrecorded copy is left in the library for
                    // a rescan to import as a duplicate. The next attempt starts clean.
                    if let Err(back) = winfs::rename_no_replace(&dest, part) {
                        tracing::warn!("could not put {} back to {}: {back}", dest.display(), part.display());
                    }
                    return Err(e);
                }
                Self::touch(&self.departed, old);
            }
        }
        Self::touch(&self.arrived, &dest);
        // A stale Bin copy is this item's own: to_bin and purge replace or remove it later, so a
        // failure here must not undo a commit that worked.
        if let Err(e) = remove_if_exists(&bin) {
            tracing::warn!("stale Bin copy not removed: {e}");
        }
        Ok(dest)
    }

    fn adopt(&self, src: &Path, t: &FileTarget) -> Result<PathBuf> {
        self.not_switching()?;
        let root = self.need_root()?;
        if is_inside(&root, src) && !is_inside(&root.join(HIDDEN_DIR), src) {
            // The user's own file, already in the library: nothing to move. Core leaves a file adopted
            // in place alone when the add fails; once the item exists it owns this file.
            return Ok(src.to_path_buf());
        }
        // From outside (an "add file" action): copy in through a staging file, never move the
        // user's original.
        let dir = self.dir_for(t)?;
        let name = self.disk_name(t, src)?;
        let stage = self.part_dir()?.join(format!("{}.adopt", t.item.0.hyphenated()));
        if let Err(e) = std::fs::copy(src, &stage) {
            // A partial staging file must not stay behind.
            let _ = std::fs::remove_file(&stage);
            return Err(CoreError::io(src, e));
        }
        match place_new(&stage, &dir, &name, None).map_err(|e| CoreError::io(&stage, e)) {
            Ok(dest) => {
                Self::touch(&self.arrived, &dest);
                Ok(dest)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&stage);
                Err(e)
            }
        }
    }

    fn relocate(&self, t: &FileTarget) -> Result<PathBuf> {
        self.not_switching()?;
        let cur = t.current.clone().ok_or_else(|| CoreError::Invalid("relocate: the file is not held".into()))?;
        let dir = self.dir_for(t)?;
        if !cur.exists() {
            // C6: the watcher may have moved it already; if the wanted place has it, done.
            // Only when the file there is this item's own bytes: another item's or the user's
            // file of the same name would be recorded for this item and later binned or purged.
            let want = dir.join(stem_name(t));
            return match tugboat_core::ops::hash_file(&want) {
                Ok((h, _)) if h == t.hash => Ok(want),
                _ => Err(not_found(&cur)),
            };
        }
        let name = self.disk_name(t, &cur)?;
        let taken = |n: &str| {
            let p = dir.join(n);
            p.exists() && !same_path(&p, &cur)
        };
        let wanted = dir.join(names::disambiguate(&name, &taken));
        if same_path(&wanted, &cur) {
            return Ok(cur);
        }
        // A case-only rename (work -> Work) is a rename of the same file: allowed. A file that
        // appeared at `wanted` after the check above is never overwritten: the error leaves the
        // file where it was (C4) and the next pass tries again.
        winfs::rename_no_replace(&cur, &wanted).map_err(|e| CoreError::io(&cur, e))?;
        Self::touch(&self.arrived, &wanted);
        Self::touch(&self.departed, &cur);
        Ok(wanted)
    }

    fn to_bin(&self, t: &FileTarget) -> Result<PathBuf> {
        let bin_path = self.bin_path(t)?;
        if let Some(cur) = t.current.as_ref().filter(|c| c.exists() && !same_path(c, &bin_path)) {
            std::fs::create_dir_all(&self.bin).map_err(|e| CoreError::io(&self.bin, e))?;
            move_across(cur, &bin_path, true).map_err(|e| CoreError::io(cur, e))?;
            Self::touch(&self.departed, cur);
            return Ok(bin_path);
        }
        if bin_path.exists() {
            return Ok(bin_path); // already there (C6)
        }
        Err(not_found(t.current.as_deref().unwrap_or(&bin_path)))
    }

    fn from_bin(&self, t: &FileTarget) -> Result<PathBuf> {
        self.not_switching()?;
        let bin_path = self.bin_path(t)?;
        if !bin_path.exists() {
            // Already back (C6) when the held path exists.
            return match &t.current {
                Some(c) if c.exists() && !is_inside(&self.bin, c) => Ok(c.clone()),
                _ => Err(not_found(&bin_path)),
            };
        }
        let dir = self.dir_for(t)?;
        let name = self.disk_name(t, &bin_path)?;
        // Stage through `.tugboat` so a cross-drive restore is copy + rename on the library drive.
        let stage = self.part_dir()?.join(format!("{}.restore", t.item.0.hyphenated()));
        // A marked file that would come out unmarked (a library on FAT/exFAT) stays in the Bin, like a download
        // that cannot be marked stays out of the library.
        if let Err(e) = self.copy_marked(&bin_path, &stage) {
            let _ = std::fs::remove_file(&stage);
            return Err(CoreError::io(&bin_path, e));
        }
        match place_new(&stage, &dir, &name, None) {
            Ok(dest) => {
                if let Err(e) = remove_if_exists(&bin_path) {
                    // The Bin copy is locked: take the restored file back out (the Bin copy is
                    // still whole), or every retry would publish another "x (2).jpg" (C4).
                    let _ = std::fs::remove_file(&dest);
                    return Err(e);
                }
                Self::touch(&self.arrived, &dest);
                Ok(dest)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&stage);
                Err(CoreError::io(&bin_path, e))
            }
        }
    }

    fn purge(&self, t: &FileTarget) -> Result<()> {
        remove_if_exists(&self.bin_path(t)?)?;
        if let Some(cur) = &t.current {
            // The item's file in the library goes, even one taken in place a moment ago: a purge arriving
            // with the delete must not leave a file for the next rescan to import as a new item. A path
            // in neither the library nor the Bin is not ours to remove.
            let in_library = self.root().is_some_and(|r| is_inside(&r, cur));
            if in_library {
                remove_if_exists(cur)?;
                Self::touch(&self.departed, cur);
            } else if is_inside(&self.bin, cur) {
                remove_if_exists(cur)?;
            }
        }
        Ok(())
    }

    fn available_space(&self) -> Result<u64> {
        let dir = self.part_dir()?;
        fs4::available_space(&dir).map_err(|e| CoreError::io(&dir, e))
    }
}

/// The on-disk name a file of `t` would have, from the record alone (no bytes to check). Only
/// used to find a file the watcher already moved, so a type failure does not matter here.
fn stem_name(t: &FileTarget) -> String {
    let ext = t.ext.to_ascii_lowercase();
    match names::clean_stem(names::strip_ext(&t.name, &t.ext)) {
        Ok(stem) => names::join_ext(&stem, &ext),
        Err(_) => names::join_ext(&names::fallback_stem(t.captured_at), &ext),
    }
}
