//! One scan pass: compare the disk with what Core holds and turn Explorer actions into Core
//! calls. See the module doc in `mod.rs`.
//!
//! Order inside a pass matters and is the safety design:
//! 1. Health. A missing folder or marker pauses sync before anything is read as "deleted".
//! 2. Directories first (new, renamed, deleted subfolders), so a file moved into a brand-new
//!    subfolder finds its folder.
//! 3. Files: known files (by NTFS id, then by path) -> move/rename/edit; unknown files -> a
//!    move whose id changed (same size + BLAKE3 as a file that vanished), else a new item once
//!    stable; held files seen nowhere -> missing.
//! 4. Missing files go through the bulk guard BEFORE anything is binned.
//!
//! Never treated as missing: a file in a directory that could not be listed, a file the
//! placement itself just moved away, a path that still exists on disk.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use tugboat_core::{
    names,
    ops::{AddKind, NewLocalFile},
    placement::{FileTarget, Placement},
    store::HeldFile,
    CoreError, FolderId, FolderKind, ItemId, Retention, INBOX_ID, UNSORTED_ID,
};

use super::{
    config::{self, DirEntry, HIDDEN_DIR},
    lock,
    placement::{is_inside, norm, same_path},
    unix_ms, Bulk, Inner, LibraryHealth, LibraryStatus, NotSyncedFile, PauseKind, Seen, State,
};
use crate::{
    error::CmdError,
    winfs::{self, FileIdentity},
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    pub imported: u32,
    pub moved: u32,
    pub edited: u32,
    pub binned: u32,
    pub folders_created: u32,
    pub folders_renamed: u32,
    pub folders_deleted: u32,
    /// Files moved or renamed on disk to follow a change made on the other device.
    pub realigned: u32,
    /// Files seen but not stable yet.
    pub deferred: u32,
    /// Look again after this long (files waiting to become stable).
    pub retry_after: Option<Duration>,
}

// ---------- disk walk ----------

struct DiskFile {
    path: PathBuf,
    /// Directory name under the root; None = the root itself (Inbox).
    dir: Option<String>,
    name: String,
    size: u64,
    mtime: i64,
}

struct DiskDir {
    name: String,
    ident: Option<FileIdentity>,
}

#[derive(Default)]
struct Walk {
    /// Files with a photo/video extension.
    files: Vec<DiskFile>,
    dirs: Vec<DiskDir>,
    /// Directories Tugboat does not sync (dot-named or hidden/system). Listed so a folder the
    /// user renamed to ".Work" is recognised by its NTFS id and never mistaken for a deleted one.
    hidden: Vec<DiskDir>,
    not_synced: Vec<NotSyncedFile>,
    /// Lowercase names of directories whose listing failed: their files are unknown, not gone.
    unreadable: HashSet<String>,
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn not_synced(path: &Path, name: &str, reason: &str) -> NotSyncedFile {
    NotSyncedFile { path: path.display().to_string(), name: name.to_string(), reason: reason.to_string() }
}

fn is_media_name(name: &str) -> bool {
    Path::new(name).extension().and_then(|e| e.to_str()).is_some_and(|e| names::ext_family(e).is_some())
}

/// List the root and its subfolders (one level: folders are flat). The root failing is an
/// error; a subfolder failing is recorded so its files are never called missing.
fn walk(root: &Path, with_files: bool) -> std::io::Result<Walk> {
    let mut w = Walk::default();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case(HIDDEN_DIR) {
            continue;
        }
        let meta = entry.metadata()?;
        let hidden = winfs::is_hidden_or_system(&meta);
        let path = entry.path();
        if meta.is_dir() {
            if hidden || name.starts_with('.') {
                w.hidden.push(DiskDir { name, ident: winfs::identity_of(&path).ok() });
                continue;
            }
            w.dirs.push(DiskDir { name: name.clone(), ident: winfs::identity_of(&path).ok() });
            if !with_files {
                continue;
            }
            match std::fs::read_dir(&path) {
                Err(_) => {
                    w.unreadable.insert(name.to_lowercase());
                }
                Ok(rd) => {
                    for sub in rd {
                        let sub = match sub {
                            Ok(s) => s,
                            Err(_) => {
                                w.unreadable.insert(name.to_lowercase());
                                continue;
                            }
                        };
                        let sname = sub.file_name().to_string_lossy().into_owned();
                        let Ok(smeta) = sub.metadata() else {
                            w.unreadable.insert(name.to_lowercase());
                            continue;
                        };
                        if winfs::is_hidden_or_system(&smeta) {
                            continue;
                        }
                        if smeta.is_dir() {
                            w.not_synced.push(not_synced(&sub.path(), &sname, "NESTED FOLDER - folders are one level deep"));
                        } else if is_media_name(&sname) {
                            w.files.push(DiskFile { path: sub.path(), dir: Some(name.clone()), name: sname, size: smeta.len(), mtime: mtime_ms(&smeta) });
                        } else {
                            w.not_synced.push(not_synced(&sub.path(), &sname, "NOT A PHOTO OR VIDEO"));
                        }
                    }
                }
            }
        } else if hidden {
            continue; // desktop.ini, Thumbs.db: Windows' own files, not the user's
        } else if with_files {
            if is_media_name(&name) {
                w.files.push(DiskFile { path, dir: None, name, size: meta.len(), mtime: mtime_ms(&meta) });
            } else {
                w.not_synced.push(not_synced(&path, &name, "NOT A PHOTO OR VIDEO"));
            }
        }
    }
    Ok(w)
}

/// Ok when `path` may become a library: it does not exist, or holds nothing but `.tugboat`
/// and Windows' hidden files. Pointing the library at a folder of 10,000 photos would sync
/// them all and start expiring them (PLAN).
pub(super) fn check_new_library(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if !path.is_dir() {
        return Err(format!("NOT A FOLDER - {}", path.display()));
    }
    let rd = std::fs::read_dir(path).map_err(|e| format!("CANNOT READ - {}: {e}", path.display()))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("CANNOT READ - {}: {e}", path.display()))?;
        if entry.file_name().to_string_lossy().eq_ignore_ascii_case(HIDDEN_DIR) {
            continue;
        }
        if entry.metadata().map(|m| winfs::is_hidden_or_system(&m)).unwrap_or(false) {
            continue;
        }
        return Err(format!("LIBRARY MUST BE EMPTY - {} already has files", path.display()));
    }
    Ok(())
}

pub(super) fn build_status(st: &State) -> LibraryStatus {
    let path = st.cfg.path.as_ref().map(|p| p.display().to_string());
    let (health, message) = match (&st.cfg.path, &st.pause) {
        (None, _) | (_, Some(PauseKind::NotConfigured)) => (LibraryHealth::NotConfigured, st.pause_reason.clone()),
        (Some(p), Some(PauseKind::Missing)) => (LibraryHealth::Missing { path: p.display().to_string() }, st.pause_reason.clone()),
        (_, Some(PauseKind::Bulk)) => {
            let b = st.bulk.as_ref();
            (LibraryHealth::BulkMissing { missing: b.map_or(0, |b| b.missing), total: b.map_or(0, |b| b.total) }, st.pause_reason.clone())
        }
        _ => (LibraryHealth::Ready, None),
    };
    LibraryStatus {
        cloud_warning: st.cfg.path.as_deref().and_then(winfs::cloud_warning),
        path,
        health,
        message,
        not_synced: st.not_synced.clone(),
        last_scan_at: st.last_scan_at,
        last_error: st.last_error.clone(),
    }
}

/// The old-library warning from choose(), for as long as any of those files still exists.
fn leftover_message(st: &mut State) -> Option<String> {
    st.leftovers.retain(|p| p.exists());
    (!st.leftovers.is_empty()).then(|| format!("{} OLD FILES NOT DELETED in the previous library folder", st.leftovers.len()))
}

/// Does the file name on disk belong to this record name? Disk names add " (2)" on a clash and
/// are cleaned, so compare the stem with a trailing " (n)" removed, ignoring case. A mismatch
/// means the record was renamed elsewhere.
pub(super) fn name_matches(record_name: &str, ext: &str, disk_name: &str) -> bool {
    let want = names::clean_stem(names::strip_ext(record_name, ext)).unwrap_or_default().to_lowercase();
    let stem = Path::new(disk_name).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    // IMG_1 (2) -> IMG_1
    let base = match stem.rfind(" (") {
        Some(i) if stem.ends_with(')') && stem[i + 2..stem.len() - 1].chars().all(|c| c.is_ascii_digit()) && stem.len() > i + 3 => &stem[..i],
        _ => stem.as_str(),
    };
    // A record name that could not be cleaned got a generated IMG_ name on disk: not a mismatch.
    want.is_empty() || stem == want || base == want
}

fn parse_folder(key: &str) -> Option<FolderId> {
    uuid::Uuid::parse_str(key).ok().map(FolderId)
}

/// Directory-name (lowercase) -> folder, from the directory map and the live folders.
fn dir_index(dirs: &BTreeMap<String, DirEntry>, live: &HashMap<FolderId, (FolderKind, String)>) -> HashMap<String, FolderId> {
    dirs.iter()
        .filter_map(|(k, e)| parse_folder(k).filter(|f| live.contains_key(f)).map(|f| (e.name.to_lowercase(), f)))
        .collect()
}

struct DirPhase {
    dirs: BTreeMap<String, DirEntry>,
    changes: Vec<(String, Option<DirEntry>)>,
    /// Lowercase names of directories that belong to a folder deleted on the other device.
    dead: HashSet<String>,
    /// User folders whose directory vanished (their items are counted as missing).
    gone: Vec<(FolderId, String)>,
    /// Lowercase (old) names of directories that are still there but now hidden or dot-named:
    /// their files are unknown, not gone.
    hidden: HashSet<String>,
    notes: Vec<NotSyncedFile>,
}

impl DirPhase {
    fn set(&mut self, key: &str, e: DirEntry) {
        self.dirs.insert(key.to_string(), e.clone());
        self.changes.push((key.to_string(), Some(e)));
    }
    fn remove(&mut self, key: &str) {
        self.dirs.remove(key);
        self.changes.push((key.to_string(), None));
    }
}

impl Inner {
    pub(crate) fn pass(&self, full: bool, now: Instant) -> super::PassReport {
        self.pass_with(full, now, false)
    }

    fn pass_with(&self, full: bool, now: Instant, force_bin: bool) -> PassReport {
        let mut st = lock(&self.st);
        let mut rep = PassReport::default();
        let mut errors: Vec<String> = Vec::new();
        self.pass_inner(&mut st, full, now, force_bin, &mut rep, &mut errors);
        // A failed step (a folder not known yet, a locked file) is tried again soon: the watcher
        // sends no event when nothing on disk changes.
        if !errors.is_empty() && rep.retry_after.is_none() {
            rep.retry_after = Some(self.opts.stable_for.max(Duration::from_secs(5)));
        }
        st.last_error = errors.into_iter().next().or_else(|| leftover_message(&mut st));
        st.last_scan_at = Some(unix_ms());
        self.persist(&mut st);
        self.publish(&mut st);
        rep
    }

    /// Tell the UI when the status changed.
    pub(super) fn publish(&self, st: &mut State) {
        let s = build_status(st);
        // The scan time moves on every pass: it is sent with the next real change, not alone.
        let same = st.published.as_ref().is_some_and(|p| LibraryStatus { last_scan_at: s.last_scan_at, ..p.clone() } == s);
        if !same {
            st.published = Some(s.clone());
            (self.notify)(&s);
        }
    }

    /// Write the directory map to library.json when it changed.
    fn persist(&self, st: &mut State) {
        let dirs = self.place.dirs();
        self.place.take_dirty();
        if st.cfg.dirs != dirs {
            st.cfg.dirs = dirs;
            if let Err(e) = st.cfg.save(&self.data_dir) {
                st.last_error = Some(format!("CANNOT SAVE LIBRARY CONFIG - {e}"));
            }
        }
    }

    fn set_pause(&self, st: &mut State, kind: Option<PauseKind>, reason: Option<String>) {
        if st.pause == kind && st.pause_reason == reason {
            return;
        }
        st.pause = kind;
        st.pause_reason = reason.clone();
        self.core.set_paused(reason);
    }

    fn pass_inner(&self, st: &mut State, full: bool, now: Instant, force_bin: bool, rep: &mut PassReport, errors: &mut Vec<String>) {
        // 1. Health.
        let Some(root) = st.cfg.path.clone() else {
            self.set_pause(st, Some(PauseKind::NotConfigured), Some("LIBRARY NOT CHOSEN - choose a folder in Settings".into()));
            return;
        };
        let marker = config::read_marker(&root);
        if !root.is_dir() || marker.is_none() || marker != st.cfg.id {
            self.set_pause(st, Some(PauseKind::Missing), Some(format!("LIBRARY FOLDER NOT FOUND - {}", root.display())));
            return;
        }
        if matches!(st.pause, Some(PauseKind::NotConfigured | PauseKind::Missing)) {
            self.set_pause(st, None, None);
            st.rewatch = true;
        }

        // 2. Directories. The map is read BEFORE the walk: an entry the placement adds after
        // this point has its directory on disk before it could be read here, never the reverse
        // (a directory that is in the map but not on disk would look deleted).
        let live: HashMap<FolderId, (FolderKind, String)> = match self.core.live_folders() {
            Ok(v) => v.into_iter().map(|(id, k, n)| (id, (k, n))).collect(),
            Err(e) => {
                errors.push(format!("FOLDER LIST FAILED - {e}"));
                return;
            }
        };
        let dirs_snapshot = self.place.dirs();
        let top = match walk(&root, false) {
            Ok(w) => w,
            Err(e) => {
                self.set_pause(st, Some(PauseKind::Missing), Some(format!("LIBRARY FOLDER NOT READABLE - {}: {e}", root.display())));
                return;
            }
        };
        let phase = self.sync_dirs(&root, &live, dirs_snapshot, &top, rep, errors);
        self.place.apply_dir_changes(&phase.changes);
        // Folders made just now (a new subfolder) must be visible to the file phase below.
        let live: HashMap<FolderId, (FolderKind, String)> = match self.core.live_folders() {
            Ok(v) => v.into_iter().map(|(id, k, n)| (id, (k, n))).collect(),
            Err(e) => {
                errors.push(format!("FOLDER LIST FAILED - {e}"));
                return;
            }
        };

        // 3. Files.
        let w = match walk(&root, true) {
            Ok(w) => w,
            Err(e) => {
                errors.push(format!("SCAN FAILED - {e}"));
                return;
            }
        };
        let index = dir_index(&phase.dirs, &live);
        let held = match self.core.held_files() {
            Ok(h) => h,
            Err(e) => {
                errors.push(format!("HELD FILES FAILED - {e}"));
                return;
            }
        };
        let mut by_path: HashMap<String, usize> = HashMap::new();
        let mut by_ident: HashMap<(u64, [u8; 16]), usize> = HashMap::new();
        for (i, h) in held.iter().enumerate() {
            by_path.insert(norm(Path::new(&h.path)), i);
            if let (Some(v), Some(f)) = (h.volume_serial, h.file_id) {
                by_ident.insert((v, f), i);
            }
        }
        let mut matched = vec![false; held.len()];
        let mut ns: Vec<NotSyncedFile> = w.not_synced.clone();
        ns.extend(phase.notes.iter().cloned());
        let mut unmatched: Vec<(&DiskFile, Option<FileIdentity>)> = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::new();

        for f in &w.files {
            let key = norm(&f.path);
            seen_paths.insert(key.clone());
            if let Some(d) = &f.dir {
                if phase.dead.contains(&d.to_lowercase()) {
                    ns.push(not_synced(&f.path, &f.name, "FOLDER WAS DELETED on the other device"));
                    continue;
                }
            }
            let ident = self.identity(st, f, &key, full);
            let idx = ident.and_then(|i| by_ident.get(&(i.volume_serial, i.file_id)).copied()).or_else(|| by_path.get(&key).copied());
            match idx {
                Some(i) if !matched[i] => {
                    matched[i] = true;
                    self.known(st, &held[i], f, ident, &index, &live, now, rep, errors);
                }
                // A second disk file claiming an already-matched item (a hard link): ignore.
                Some(_) => {}
                None => unmatched.push((f, ident)),
            }
        }

        // Held files that are on neither side's list: candidates for "missing".
        let in_root = |p: &Path| is_inside(&root, p);
        let unreadable = &w.unreadable;
        let hidden = &phase.hidden;
        let mut missing: Vec<usize> = (0..held.len())
            .filter(|&i| !matched[i])
            .filter(|&i| {
                let p = Path::new(&held[i].path);
                in_root(p)
                    && !p.exists()
                    && !self.place.recently_departed(p)
                    && !p.parent().and_then(|d| d.file_name()).is_some_and(|d| {
                        let d = d.to_string_lossy().to_lowercase();
                        unreadable.contains(&d) || hidden.contains(&d)
                    })
            })
            .collect();

        for (f, ident) in unmatched {
            if self.place.recently_arrived(&f.path) {
                continue; // the placement just put it there; Core is about to record the path
            }
            // A move whose NTFS id changed (another drive, a program that rewrites files):
            // the same bytes as a file that vanished.
            let cands: Vec<usize> = missing.iter().copied().filter(|&i| held[i].size == f.size).collect();
            if !cands.is_empty() {
                if let Ok((hash, _)) = tugboat_core::ops::hash_file(&f.path) {
                    if let Some(&i) = cands.iter().find(|&&i| held[i].hash == hash) {
                        missing.retain(|&m| m != i);
                        self.moved(&held[i], f, ident, &index, rep, errors);
                        continue;
                    }
                }
            }
            let key = norm(&f.path);
            if st.rejected.get(&key) == Some(&(f.size, f.mtime)) {
                ns.push(not_synced(&f.path, &f.name, "NOT A PHOTO OR VIDEO - content does not match its type"));
                continue;
            }
            if st.pause.is_some() {
                continue; // paused: nothing new is imported until the user decides
            }
            let Some(folder) = (match &f.dir {
                None => Some(None),
                Some(d) => index.get(&d.to_lowercase()).map(|id| Some(*id)),
            }) else {
                ns.push(not_synced(&f.path, &f.name, "FOLDER NOT KNOWN yet"));
                continue;
            };
            if !self.stable(st, f, now) {
                rep.deferred += 1;
                continue;
            }
            self.import(st, f, folder, ident, &mut ns, rep, errors);
        }

        // Entries that no longer exist leave the caches (they only ever shrink to what is on disk).
        st.pending.retain(|k, _| seen_paths.contains(k));
        st.ids.retain(|k, _| seen_paths.contains(k));
        st.mtimes.retain(|k, _| seen_paths.contains(k));
        st.rejected.retain(|k, _| seen_paths.contains(k));
        for f in &w.files {
            if st.rejected.contains_key(&norm(&f.path)) && !ns.iter().any(|n| n.path == f.path.display().to_string()) {
                ns.push(not_synced(&f.path, &f.name, "NOT A PHOTO OR VIDEO - content does not match its type"));
            }
        }
        st.not_synced = ns;
        if rep.deferred > 0 {
            rep.retry_after = Some(self.opts.stable_for);
        }

        // 4. Missing files and deleted subfolders, through the bulk guard.
        let ids: Vec<ItemId> = missing.iter().map(|&i| held[i].item).collect();
        self.settle_missing(st, &root, &ids, &phase.gone, held.len() as u32, force_bin, rep, errors);
    }

    /// NTFS identity of a file: cached while size and mtime are unchanged (a full rescan
    /// re-reads every id, so a stale cache cannot outlive 10 minutes).
    fn identity(&self, st: &mut State, f: &DiskFile, key: &str, full: bool) -> Option<FileIdentity> {
        if !full {
            if let Some((size, mtime, id)) = st.ids.get(key) {
                if *size == f.size && *mtime == f.mtime {
                    return Some(*id);
                }
            }
        }
        match winfs::identity_of(&f.path) {
            Ok(id) => {
                st.ids.insert(key.to_string(), (f.size, f.mtime, id));
                Some(id)
            }
            Err(_) => None,
        }
    }

    /// "Copy finished": the same size and mtime on two looks at least `stable_for` apart, and
    /// the file opens exclusively (a writer still holding it fails that).
    fn stable(&self, st: &mut State, f: &DiskFile, now: Instant) -> bool {
        let key = norm(&f.path);
        match st.pending.get(&key) {
            Some(s) if s.size == f.size && s.mtime == f.mtime => {
                if now.duration_since(s.since) >= self.opts.stable_for && winfs::open_exclusive(&f.path).is_ok() {
                    st.pending.remove(&key);
                    true
                } else {
                    false
                }
            }
            _ => {
                st.pending.insert(key, Seen { size: f.size, mtime: f.mtime, since: now });
                false
            }
        }
    }

    // ----- directories -----

    fn sync_dirs(
        &self,
        root: &Path,
        live: &HashMap<FolderId, (FolderKind, String)>,
        dirs: BTreeMap<String, DirEntry>,
        top: &Walk,
        rep: &mut PassReport,
        errors: &mut Vec<String>,
    ) -> DirPhase {
        let mut p = DirPhase { dirs, changes: Vec::new(), dead: HashSet::new(), gone: Vec::new(), hidden: HashSet::new(), notes: Vec::new() };
        let mut claimed: HashSet<String> = HashSet::new();

        for d in &top.dirs {
            let by_id = d.ident.and_then(|i| {
                p.dirs.iter().find(|(_, e)| e.volume == Some(i.volume_serial) && e.id.as_deref() == Some(&i.id_hex())).map(|(k, _)| k.clone())
            });
            let key = by_id.clone().or_else(|| p.dirs.iter().find(|(_, e)| e.name.eq_ignore_ascii_case(&d.name)).map(|(k, _)| k.clone()));
            let (volume, id) = (d.ident.map(|i| i.volume_serial), d.ident.map(|i| i.id_hex()));
            match key {
                Some(k) => {
                    let Some(fid) = parse_folder(&k) else { continue };
                    if !live.contains_key(&fid) {
                        p.dead.insert(d.name.to_lowercase());
                        continue;
                    }
                    let old = p.dirs[&k].clone();
                    if by_id.is_some() && old.name != d.name {
                        // The user renamed the directory in Explorer: the folder follows.
                        match self.core.rename_folder(fid, &d.name) {
                            Ok(()) => rep.folders_renamed += 1,
                            Err(e) => errors.push(format!("FOLDER RENAME FAILED - {}: {e}", d.name)),
                        }
                        self.repoint_files(fid, &root.join(&old.name), &root.join(&d.name), errors);
                    }
                    claimed.insert(k.clone());
                    p.set(&k, DirEntry { name: d.name.clone(), volume, id });
                }
                None => {
                    // A folder that has no directory yet and carries this name (a default
                    // folder, or one made on the phone) takes the directory the user made.
                    let by_name = live.iter().find(|(fid, (kind, name))| {
                        *kind != FolderKind::Inbox
                            && !p.dirs.contains_key(&fid.0.to_string())
                            && names::clean_folder_name(name).is_ok_and(|n| n.eq_ignore_ascii_case(&d.name))
                    });
                    if let Some((fid, _)) = by_name {
                        let k = fid.0.to_string();
                        claimed.insert(k.clone());
                        p.set(&k, DirEntry { name: d.name.clone(), volume, id });
                    } else {
                        // New subfolder = new folder, Keep until its retention is set (PLAN).
                        match self.core.create_folder(&d.name, Retention::Keep) {
                            Ok(fid) => {
                                rep.folders_created += 1;
                                let k = fid.0.to_string();
                                claimed.insert(k.clone());
                                p.set(&k, DirEntry { name: d.name.clone(), volume, id });
                            }
                            Err(e) => errors.push(format!("NEW FOLDER FAILED - {}: {e}", d.name)),
                        }
                    }
                }
            }
        }

        // A directory renamed to a dot name (or made hidden) is the same directory under a name
        // Tugboat does not sync: its folder stays as it is and the user is told. A rename must
        // never become a delete.
        for d in &top.hidden {
            let Some(i) = d.ident else { continue };
            let key = p.dirs.iter().find(|(_, e)| e.volume == Some(i.volume_serial) && e.id.as_deref() == Some(&i.id_hex())).map(|(k, _)| k.clone());
            if let Some(k) = key {
                p.hidden.insert(p.dirs[&k].name.to_lowercase());
                p.notes.push(not_synced(&root.join(&d.name), &d.name, "HIDDEN FOLDER - not synced while its name starts with a dot"));
                claimed.insert(k);
            }
        }

        // Folders whose directory is not on disk any more.
        let keys: Vec<String> = p.dirs.keys().cloned().collect();
        for k in keys {
            if claimed.contains(&k) {
                continue;
            }
            let Some(fid) = parse_folder(&k) else { continue };
            let e = p.dirs[&k].clone();
            match live.get(&fid) {
                // Deleted on the other device: forget its directory once it is gone or empty.
                None => {
                    let path = root.join(&e.name);
                    if !path.exists() || std::fs::remove_dir(&path).is_ok() {
                        p.remove(&k);
                    }
                }
                // A user folder we have seen on disk and now cannot find: deleted in Explorer.
                Some((FolderKind::User, _)) if e.id.is_some() => p.gone.push((fid, e.name.clone())),
                // Default folders and folders never seen on disk keep their entry.
                Some(_) => {}
            }
        }

        // Record -> disk: a folder renamed on the other device gets its directory renamed. The
        // names are read again here: a rename made from the disk side just above must not be
        // undone by the stale name from the start of the pass.
        let live_now: HashMap<FolderId, (FolderKind, String)> = match self.core.live_folders() {
            Ok(v) => v.into_iter().map(|(id, k, n)| (id, (k, n))).collect(),
            Err(e) => {
                errors.push(format!("FOLDER LIST FAILED - {e}"));
                return p;
            }
        };
        for (k, e) in p.dirs.clone() {
            let Some(fid) = parse_folder(&k) else { continue };
            let Some((kind, name)) = live_now.get(&fid) else { continue };
            if !claimed.contains(&k) || *kind == FolderKind::Inbox {
                continue;
            }
            let Ok(wanted) = names::clean_folder_name(name) else { continue };
            if wanted == e.name {
                continue;
            }
            let taken = |n: &str| {
                let other = p.dirs.iter().any(|(k2, e2)| *k2 != k && e2.name.eq_ignore_ascii_case(n));
                other || (!n.eq_ignore_ascii_case(&e.name) && root.join(n).exists())
            };
            let target = names::disambiguate(&wanted, &taken);
            if target == e.name {
                continue;
            }
            let (from, to) = (root.join(&e.name), root.join(&target));
            match std::fs::rename(&from, &to) {
                Ok(()) => {
                    self.repoint_files(fid, &from, &to, errors);
                    let ident = winfs::identity_of(&to).ok();
                    p.set(&k, DirEntry { name: target, volume: ident.map(|i| i.volume_serial), id: ident.map(|i| i.id_hex()) });
                }
                // Open in another program, or a permissions problem: try again next pass.
                Err(err) => errors.push(format!("FOLDER RENAME ON DISK FAILED - {} -> {target}: {err}", e.name)),
            }
        }
        p
    }

    /// After a directory was renamed, point every held file of that folder at its new path.
    fn repoint_files(&self, fid: FolderId, from: &Path, to: &Path, errors: &mut Vec<String>) {
        let held = match self.core.held_files() {
            Ok(h) => h,
            Err(e) => {
                errors.push(format!("HELD FILES FAILED - {e}"));
                return;
            }
        };
        for h in held.iter().filter(|h| h.folder == fid && is_inside(from, Path::new(&h.path))) {
            let rel = Path::new(&h.path).strip_prefix(from).map(Path::to_path_buf).unwrap_or_else(|_| PathBuf::from(Path::new(&h.path).file_name().unwrap_or_default()));
            if let Err(e) = self.core.local_file_moved(h.item, to.join(rel), fid, &h.name) {
                errors.push(format!("FILE PATH UPDATE FAILED - {}: {e}", h.name));
            }
        }
    }

    // ----- files -----

    #[allow(clippy::too_many_arguments)]
    fn known(
        &self,
        st: &mut State,
        h: &HeldFile,
        f: &DiskFile,
        ident: Option<FileIdentity>,
        index: &HashMap<String, FolderId>,
        live: &HashMap<FolderId, (FolderKind, String)>,
        now: Instant,
        rep: &mut PassReport,
        errors: &mut Vec<String>,
    ) {
        // Windows paths ignore case, so a case-only rename (IMG_1.jpg -> img_1.jpg) is the same
        // path to same_path: compare the file name exactly as well.
        let renamed = Path::new(&h.path).file_name().is_some_and(|n| n.to_string_lossy() != f.name.as_str());
        if !same_path(Path::new(&h.path), &f.path) || renamed {
            self.moved(h, f, ident, index, rep, errors);
        } else {
            if let Some(i) = ident {
                if h.volume_serial != Some(i.volume_serial) || h.file_id != Some(i.file_id) {
                    if let Err(e) = self.core.set_file_identity(h.item, i.volume_serial, i.file_id) {
                        errors.push(format!("FILE IDENTITY FAILED - {}: {e}", f.name));
                    }
                }
            }
            // The disk agrees with what Core recorded, so a folder or name that differs from the
            // record was changed on the OTHER device: the file follows.
            self.realign(h, f, index, live, rep, errors);
        }
        // An edit: size differs from the record, or mtime moved since the last pass. Waits for
        // the same "copy finished" check as a new file, so a half-saved edit is not synced.
        let key = norm(&f.path);
        let last = st.mtimes.get(&key).copied();
        let changed = f.size != h.size || last.is_some_and(|m| m != f.mtime);
        if changed {
            if self.stable(st, f, now) {
                match self.core.local_content_changed(h.item, f.path.clone()) {
                    Ok(()) => {
                        rep.edited += 1;
                        st.mtimes.insert(key, f.mtime);
                    }
                    Err(e) => errors.push(format!("EDIT NOT SYNCED - {}: {e}", f.name)),
                }
            } else {
                rep.deferred += 1;
            }
        } else {
            st.mtimes.insert(key, f.mtime);
        }
    }

    /// The record's folder or name changed on the other device: move the file on disk to match.
    fn realign(&self, h: &HeldFile, f: &DiskFile, index: &HashMap<String, FolderId>, live: &HashMap<FolderId, (FolderKind, String)>, rep: &mut PassReport, errors: &mut Vec<String>) {
        // A deleted or unknown folder shows its items under Unsorted (Core's effective folder).
        let want = if live.contains_key(&h.folder) { h.folder } else { UNSORTED_ID };
        let Some((kind, folder_name)) = live.get(&want).cloned() else { return };
        let on_disk = match &f.dir {
            None => Some(INBOX_ID),
            Some(d) => index.get(&d.to_lowercase()).copied(),
        };
        if on_disk == Some(want) && name_matches(&h.name, &h.ext, &f.name) {
            return;
        }
        let t = FileTarget {
            item: h.item,
            hash: h.hash,
            name: h.name.clone(),
            ext: h.ext.clone(),
            captured_at: h.captured_at,
            folder: want,
            folder_kind: kind,
            folder_name,
            current: Some(f.path.clone()),
        };
        match self.place.relocate(&t) {
            // Same folder and name as the record: this only points file_ref at the new path.
            Ok(new_path) if !same_path(&new_path, &f.path) => match self.core.local_file_moved(h.item, new_path, h.folder, &h.name) {
                Ok(()) => rep.realigned += 1,
                Err(e) => errors.push(format!("FILE PATH UPDATE FAILED - {}: {e}", h.name)),
            },
            Ok(_) => {}
            Err(e) => errors.push(format!("FILE NOT MOVED - {}: {e}", h.name)),
        }
    }

    /// A held file now lives at `f.path`: folder and name follow where the user put it.
    fn moved(&self, h: &HeldFile, f: &DiskFile, ident: Option<FileIdentity>, index: &HashMap<String, FolderId>, rep: &mut PassReport, errors: &mut Vec<String>) {
        let folder = match &f.dir {
            None => INBOX_ID,
            Some(d) => match index.get(&d.to_lowercase()) {
                Some(id) => *id,
                None => {
                    errors.push(format!("MOVE NOT SYNCED - {}: its folder is not known yet", f.name));
                    return;
                }
            },
        };
        // The name changes only when the file's name on disk changed (disambiguation suffixes
        // and cleaning make a record name differ from its disk name without any user rename).
        let old_name = Path::new(&h.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = if old_name == f.name {
            h.name.clone()
        } else {
            let stem = Path::new(&f.name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{stem}.{}", h.ext)
        };
        match self.core.local_file_moved(h.item, f.path.clone(), folder, &name) {
            Ok(()) => rep.moved += 1,
            Err(e) => {
                errors.push(format!("MOVE NOT SYNCED - {}: {e}", f.name));
                return;
            }
        }
        if let Some(i) = ident {
            if let Err(e) = self.core.set_file_identity(h.item, i.volume_serial, i.file_id) {
                errors.push(format!("FILE IDENTITY FAILED - {}: {e}", f.name));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn import(
        &self,
        st: &mut State,
        f: &DiskFile,
        folder: Option<FolderId>,
        ident: Option<FileIdentity>,
        ns: &mut Vec<NotSyncedFile>,
        rep: &mut PassReport,
        errors: &mut Vec<String>,
    ) {
        let tmp_dir = self.data_dir.join("tmp");
        let thumb = std::fs::create_dir_all(&tmp_dir).ok().and_then(|()| {
            let out = tmp_dir.join(format!("{}.jpg", uuid::Uuid::now_v7()));
            match crate::thumbs::make_thumbnail(&f.path, &out) {
                Ok(()) => Some(out),
                Err(e) => {
                    // Best effort: no Shell thumbnail (e.g. a codec is missing) means no thumbnail.
                    tracing::debug!("no thumbnail for {}: {e}", f.path.display());
                    None
                }
            }
        });
        let new = NewLocalFile {
            src: f.path.clone(),
            name: f.name.clone(),
            folder,
            captured_at: f.mtime.min(unix_ms()),
            geo: None,
            thumb: thumb.clone(),
            how: AddKind::Added,
            ntfs: ident.map(|i| (i.volume_serial, i.file_id)),
        };
        let r = self.core.add_local_file(new);
        if let Some(t) = thumb {
            let _ = std::fs::remove_file(t);
        }
        match r {
            Ok(_) => {
                rep.imported += 1;
                st.mtimes.insert(norm(&f.path), f.mtime);
            }
            Err(CoreError::NotMedia(why)) => {
                // Judged once per (size, mtime): listed, never retried until the file changes.
                st.rejected.insert(norm(&f.path), (f.size, f.mtime));
                ns.push(not_synced(&f.path, &f.name, &format!("NOT A PHOTO OR VIDEO - {why}")));
            }
            Err(e) => errors.push(format!("NOT SYNCED YET - {}: {e}", f.name)),
        }
    }

    // ----- missing files -----

    #[allow(clippy::too_many_arguments)]
    fn settle_missing(
        &self,
        st: &mut State,
        root: &Path,
        ids: &[ItemId],
        gone: &[(FolderId, String)],
        total: u32,
        force: bool,
        rep: &mut PassReport,
        errors: &mut Vec<String>,
    ) {
        if ids.is_empty() && gone.is_empty() {
            st.bulk = None;
            if st.pause == Some(PauseKind::Bulk) {
                self.set_pause(st, None, None);
            }
            return;
        }
        let settings = match self.core.settings() {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("SETTINGS FAILED - {e}"));
                return;
            }
        };
        let n = ids.len() as u32;
        let over = n > settings.missing_pause_files || (n as u64) * 100 > settings.missing_pause_pct as u64 * total as u64;
        if over && !force {
            st.bulk = Some(Bulk { missing: n, total, ids: ids.iter().copied().collect() });
            self.set_pause(st, Some(PauseKind::Bulk), Some(format!("{n} FILES MISSING - move them to the Bin or keep waiting")));
            return;
        }
        // The user confirmed the files the prompt named. Files that went missing after the
        // prompt are not part of that decision: they are counted again by the next pass, which
        // asks again.
        let confirmed = if force { st.bulk.take().map(|b| b.ids) } else { None };
        st.bulk = None;
        if st.pause == Some(PauseKind::Bulk) {
            self.set_pause(st, None, None);
        }
        // Re-read what Core holds right now: an item that changed hands since the scan began
        // (a sync moved its file) must not be binned on stale information.
        let fresh: HashSet<ItemId> = match self.core.held_files() {
            Ok(h) => h.into_iter().filter(|h| !Path::new(&h.path).exists()).map(|h| h.item).collect(),
            Err(e) => {
                errors.push(format!("HELD FILES FAILED - {e}"));
                return;
            }
        };
        let ids: Vec<ItemId> = ids.iter().copied().filter(|i| fresh.contains(i) && confirmed.as_ref().is_none_or(|c| c.contains(i))).collect();
        if !ids.is_empty() {
            match self.core.local_files_missing(&ids) {
                Ok(()) => rep.binned += ids.len() as u32,
                Err(e) => {
                    errors.push(format!("MISSING FILES NOT BINNED - {e}"));
                    return;
                }
            }
        }
        for (fid, name) in gone {
            // The folder goes only if its directory is still gone right now.
            if root.join(name).exists() {
                continue;
            }
            match self.core.delete_folder(*fid) {
                Ok(()) => {
                    rep.folders_deleted += 1;
                    self.place.apply_dir_changes(&[(fid.0.to_string(), None)]);
                }
                Err(e) => errors.push(format!("FOLDER DELETE FAILED - {name}: {e}")),
            }
        }
    }

    // ----- user actions -----

    pub(super) fn resolve_missing(&self, bin: bool) -> Result<(), CmdError> {
        if !bin {
            return Ok(()); // keep waiting: sync stays paused; the next scan re-counts
        }
        let mut st = lock(&self.st);
        if st.bulk.is_none() {
            return Err(CmdError::new("invalid", "NOTHING TO DECIDE - no files are missing"));
        }
        let mut rep = PassReport::default();
        let mut errors = Vec::new();
        self.pass_inner(&mut st, true, Instant::now(), true, &mut rep, &mut errors);
        st.last_error = errors.first().cloned().or_else(|| leftover_message(&mut st));
        self.persist(&mut st);
        self.publish(&mut st);
        match errors.into_iter().next() {
            Some(e) => Err(CmdError::new("scan", e)),
            None => Ok(()),
        }
    }

    pub(super) fn locate(&self, path: &Path) -> Result<(), CmdError> {
        let mut st = lock(&self.st);
        let want = st.cfg.id.clone().ok_or_else(|| CmdError::new("invalid", "NO LIBRARY TO LOCATE"))?;
        if config::read_marker(path).as_deref() != Some(want.as_str()) {
            return Err(CmdError::new("not_library", format!("NOT YOUR LIBRARY - {} has no matching Tugboat marker", path.display())));
        }
        // Held files point into the old folder. Move every path over to the same place under the
        // new root, or the scan would find the files there as new ones and import each a second
        // time (a library copied to another drive has new NTFS ids, so only the path can match).
        if let Some(old) = st.cfg.path.clone().filter(|o| !same_path(o, path)) {
            for h in self.core.held_files()?.iter().filter(|h| is_inside(&old, Path::new(&h.path))) {
                let rel = Path::new(&h.path).strip_prefix(&old).map(Path::to_path_buf).unwrap_or_else(|_| PathBuf::from(Path::new(&h.path).file_name().unwrap_or_default()));
                self.core.local_file_moved(h.item, path.join(rel), h.folder, &h.name)?;
            }
        }
        // Directory ids belong to the old copy: relearn them by name (as choose does).
        let mut dirs = self.place.dirs();
        for e in dirs.values_mut() {
            e.volume = None;
            e.id = None;
        }
        self.place.replace_dirs(dirs.clone());
        st.cfg.dirs = dirs;
        st.cfg.path = Some(path.to_path_buf());
        st.cfg.save(&self.data_dir)?;
        self.place.set_root(Some(path.to_path_buf()));
        st.ids.clear();
        st.mtimes.clear();
        st.pending.clear();
        Ok(())
    }

    pub(super) fn choose(&self, path: &Path, accept_cloud: bool) -> Result<(), CmdError> {
        let mut st = lock(&self.st);
        if !path.is_absolute() {
            return Err(CmdError::new("invalid", "NEEDS A FULL PATH"));
        }
        if is_inside(&self.data_dir, path) || is_inside(path, &self.data_dir) {
            return Err(CmdError::new("invalid", "NOT HERE - the library cannot hold the app's own data"));
        }
        let old_root = st.cfg.path.clone();
        if let Some(old) = &old_root {
            if is_inside(old, path) || is_inside(path, old) {
                return Err(CmdError::new("invalid", "NOT HERE - overlaps the current library"));
            }
        }
        check_new_library(path).map_err(|why| CmdError::new("not_empty", why))?;
        if let Some(why) = winfs::cloud_warning(path) {
            if !accept_cloud {
                return Err(CmdError::new("cloud_folder", format!("CLOUD FOLDER - {why}. Choose another location")));
            }
        }
        std::fs::create_dir_all(path)?;

        // Copy every held file first; switch only if all copies worked.
        let held = self.core.held_files()?;
        let mut copies: Vec<(HeldFile, PathBuf)> = Vec::new();
        // Files whose source is already gone (the old drive died, or one was deleted since the
        // last scan). Nothing to copy: their record moves to the new place and the next scan
        // finds them missing, where the bulk guard asks the user before anything is binned.
        let mut lost: Vec<(HeldFile, PathBuf)> = Vec::new();
        if let Some(old) = &old_root {
            self.core.set_paused(Some("MOVING LIBRARY - please wait".into()));
            for h in held.iter().filter(|h| is_inside(old, Path::new(&h.path))) {
                let rel = Path::new(&h.path).strip_prefix(old).map(Path::to_path_buf).unwrap_or_default();
                let dest = path.join(&rel);
                let r = dest
                    .parent()
                    .map(std::fs::create_dir_all)
                    .transpose()
                    .and_then(|_| std::fs::copy(&h.path, &dest))
                    .and_then(|n| if n == h.size { Ok(()) } else { Err(std::io::Error::other("copy is shorter than the original")) });
                match r {
                    Ok(()) => copies.push((h.clone(), dest)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && !Path::new(&h.path).exists() => lost.push((h.clone(), dest)),
                    Err(e) => {
                        for (_, c) in &copies {
                            let _ = std::fs::remove_file(c);
                        }
                        self.core.set_paused(st.pause_reason.clone());
                        return Err(CmdError::new("io", format!("MOVE FAILED - {}: {e}", h.name)));
                    }
                }
            }
        }
        let id = config::write_new_marker(path)?;
        for (h, dest) in &copies {
            self.core.local_file_moved(h.item, dest.clone(), h.folder, &h.name)?;
            if let Ok(i) = winfs::identity_of(dest) {
                let _ = self.core.set_file_identity(h.item, i.volume_serial, i.file_id);
            }
        }
        for (h, dest) in &lost {
            self.core.local_file_moved(h.item, dest.clone(), h.folder, &h.name)?;
        }
        // Directory identities belong to the old drive location: relearn them by name.
        let mut dirs = self.place.dirs();
        for e in dirs.values_mut() {
            e.volume = None;
            e.id = None;
        }
        self.place.replace_dirs(dirs.clone());
        st.cfg = config::LibraryConfig { path: Some(path.to_path_buf()), id: Some(id), dirs };
        st.cfg.save(&self.data_dir)?;
        self.place.set_root(Some(path.to_path_buf()));
        st.ids.clear();
        st.mtimes.clear();
        st.pending.clear();
        st.rejected.clear();
        st.bulk = None;
        st.pause = None;
        st.pause_reason = None;
        self.core.set_paused(None);
        // The old files go last; failures are reported, never fatal (the new copies are live).
        st.leftovers = copies.iter().map(|(h, _)| PathBuf::from(&h.path)).filter(|p| std::fs::remove_file(p).is_err()).collect();
        st.last_error = leftover_message(&mut st);
        self.publish(&mut st);
        Ok(())
    }
}
