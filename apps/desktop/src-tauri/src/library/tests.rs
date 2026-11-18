//! Library tests against real temp folders and a real Core (SQLite, real files). Scans are
//! driven by hand with a controlled `Instant`, so "stable for 2 seconds" needs no sleeping;
//! one test runs the real watcher thread end to end.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tugboat_core::{
    facade::device_id_from_secret, ActivityAction, Core, CoreConfig, FolderId, FolderKind, ItemId, ItemQuery, ItemState, ItemView, LocalFileState, Retention, Role,
    SystemClock, INBOX_ID,
};

use super::{config::LibraryConfig, placement::PcLibrary, Library, LibraryHealth, LibraryOptions, LibraryStatus};

/// A JPEG-looking file: the magic bytes are all the type check reads, `seed` makes content unique.
fn jpeg(seed: u8, len: usize) -> Vec<u8> {
    let mut v = vec![seed; len.max(8)];
    v[..4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
    v
}

struct Rig {
    _tmp: tempfile::TempDir,
    core: Core,
    lib: Library,
    place: Arc<PcLibrary>,
    root: PathBuf,
    data: PathBuf,
    now: Instant,
    statuses: Arc<Mutex<Vec<LibraryStatus>>>,
}

impl Rig {
    /// Library with the percentage rule switched off (100%), so deleting one file of a small test
    /// library is an ordinary delete. The bulk tests use `strict` (the real defaults).
    fn new() -> Rig {
        let r = Rig::strict();
        let s = tugboat_core::Settings { missing_pause_pct: 100, ..Default::default() };
        r.core.save_settings(&s).unwrap();
        r
    }

    fn strict() -> Rig {
        Rig::with_default_root(|tmp| tmp.join("lib"), |_| {})
    }

    /// Items deleted so far (activity), whatever happened to their files: the Bin screen hides
    /// items whose file is gone, so "nothing was deleted" is asserted on the log.
    fn deleted_count(&self) -> usize {
        self.actions().iter().filter(|a| **a == ActivityAction::Deleted).count()
    }

    fn with_default_root(root: impl Fn(&Path) -> PathBuf, prepare: impl Fn(&Path)) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let root = root(tmp.path());
        prepare(&root);
        let place = Arc::new(PcLibrary::new(None, tmp.path().join("bin"), Default::default()));
        let secret = [7u8; 32];
        let cfg = CoreConfig { data_dir: data.clone(), role: Role::Pc, self_id: device_id_from_secret(&secret), device_name: "PC".into(), app_version: "test".into() };
        let (core, _rx) = Core::open(cfg, Arc::new(SystemClock), place.clone()).unwrap();
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = statuses.clone();
        let lib = Library::open(
            core.clone(),
            place.clone(),
            data.clone(),
            LibraryConfig::default(),
            &root,
            LibraryOptions::default(),
            Box::new(move |s: &LibraryStatus| sink.lock().unwrap().push(s.clone())),
        )
        .unwrap();
        Rig { _tmp: tmp, core, lib, place, root, data, now: Instant::now(), statuses }
    }

    /// One scan `secs` after the previous one.
    fn scan(&mut self, secs: u64) -> super::PassReport {
        self.now += Duration::from_secs(secs);
        self.lib.scan_at(false, self.now)
    }

    /// Two scans 3 s apart: new files become stable and are imported.
    fn settle(&mut self) -> super::PassReport {
        self.scan(0);
        self.scan(3)
    }

    fn write(&self, rel: &str, seed: u8) -> PathBuf {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, jpeg(seed, 64)).unwrap();
        p
    }

    fn items(&self) -> Vec<ItemView> {
        self.core.items(&ItemQuery { limit: 500, ..Default::default() }).unwrap().rows
    }

    fn bin(&self) -> Vec<ItemView> {
        self.core.items(&ItemQuery { limit: 500, bin: true, ..Default::default() }).unwrap().rows
    }

    fn item(&self, name: &str) -> ItemView {
        self.items().into_iter().find(|i| i.name == name).unwrap_or_else(|| panic!("no item named {name}: {:?}", self.items().iter().map(|i| &i.name).collect::<Vec<_>>()))
    }

    fn folder_named(&self, name: &str) -> tugboat_core::FolderView {
        self.core.folders().unwrap().into_iter().find(|f| f.name == name).unwrap_or_else(|| panic!("no folder {name}"))
    }

    fn actions(&self) -> Vec<ActivityAction> {
        self.core.activity(None, 200).unwrap().into_iter().map(|a| a.action).collect()
    }

    fn sync_state(&self) -> tugboat_core::events::SyncState {
        self.core.status().state
    }
}

// ---------- first run ----------

#[test]
fn first_run_makes_the_default_folder_the_library_with_a_marker() {
    let r = Rig::new();
    assert!(r.root.is_dir());
    let id = super::config::read_marker(&r.root).expect("marker file");
    let cfg = LibraryConfig::load(&r.data).unwrap();
    assert_eq!(cfg.path.as_deref(), Some(r.root.as_path()));
    assert_eq!(cfg.id.as_deref(), Some(id.as_str()));
    assert!(r.root.join(".tugboat").is_dir());
    assert_eq!(r.lib.status().health, LibraryHealth::Ready);
}

#[test]
fn a_non_empty_default_folder_is_never_adopted_as_the_library() {
    let r = Rig::with_default_root(|t| t.join("Local Tugboat"), |root| {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("holiday.jpg"), jpeg(1, 64)).unwrap();
    });
    assert_eq!(r.lib.status().health, LibraryHealth::NotConfigured);
    assert!(r.lib.root().is_none());
    assert!(!r.root.join(".tugboat").exists(), "nothing was written into the user's folder");
    let mut r = r;
    r.scan(0);
    assert_eq!(r.sync_state(), tugboat_core::events::SyncState::Unpaired, "no peer: Unpaired, but the core is paused too");
    assert!(r.lib.status().message.unwrap().contains("LIBRARY NOT CHOSEN"));
    assert!(r.items().is_empty(), "the user's photos were not imported");
}

// ---------- new files ----------

#[test]
fn a_dropped_photo_becomes_an_item_in_place_after_it_is_stable() {
    let mut r = Rig::new();
    let p = r.write("IMG_1.jpg", 1);
    let first = r.scan(0);
    assert_eq!(first.imported, 0);
    assert_eq!(first.deferred, 1, "seen once: not trusted yet");
    assert_eq!(first.retry_after, Some(Duration::from_secs(2)), "the worker is told to look again");
    let second = r.scan(1);
    assert_eq!(second.imported, 0, "1 s of stability is not 2 s");
    let third = r.scan(3);
    assert_eq!(third.imported, 1);
    let it = r.item("IMG_1.jpg");
    assert_eq!(it.folder, INBOX_ID, "the library root is the Inbox");
    assert_eq!(it.local, LocalFileState::Present);
    assert_eq!(it.path.as_deref().map(Path::new), Some(p.as_path()), "the user's file stays where it is");
    assert!(p.exists());
    // The NTFS identity was recorded for move matching.
    let held = r.core.held_files().unwrap();
    assert!(held.iter().any(|h| h.item == it.id && h.file_id.is_some() && h.volume_serial.is_some()));
    assert!(r.actions().contains(&ActivityAction::Added));
    // A later scan changes nothing.
    let again = r.scan(5);
    assert_eq!((again.imported, again.moved, again.edited, again.binned), (0, 0, 0, 0));
    assert_eq!(r.items().len(), 1);
}

#[test]
fn a_file_still_being_written_is_not_imported_until_it_stops_changing() {
    let mut r = Rig::new();
    let p = r.root.join("big.jpg");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(&jpeg(2, 1000)).unwrap();
    f.flush().unwrap();
    r.scan(0);
    // The copy goes on: the size moved, so the clock restarts.
    f.write_all(&[3u8; 5000]).unwrap();
    f.flush().unwrap();
    assert_eq!(r.scan(3).imported, 0, "size changed since the last look");
    assert_eq!(r.scan(1).imported, 0, "stable for 1 s only");
    // Stable for 3 s now, but the writer still has the file open: the exclusive-open probe fails.
    drop(f);
    let held_open = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
    assert_eq!(r.scan(3).imported, 0, "a writer still holds the file");
    drop(held_open);
    // Closing a writer's handle can touch the file's time stamp, which restarts the clock.
    assert_eq!(r.scan(0).imported + r.scan(3).imported, 1);
    let it = r.item("big.jpg");
    assert_eq!(it.size, 6000, "the whole file, not the first chunk");
}

#[test]
fn any_file_becomes_an_item_and_only_refused_and_mismatched_files_are_listed() {
    let mut r = Rig::new();
    std::fs::write(r.root.join("notes.txt"), b"hello").unwrap();
    std::fs::write(r.root.join("Makefile"), b"all:").unwrap();
    std::fs::create_dir(r.root.join("Work")).unwrap();
    std::fs::write(r.root.join("Work").join("plan.docx"), b"x").unwrap();
    std::fs::write(r.root.join("fake.jpg"), b"this is not a jpeg at all").unwrap();
    // Shortcut and folder-setting files are never synced, even when the user can see them.
    std::fs::write(r.root.join("link.lnk"), b"x").unwrap();
    std::fs::write(r.root.join("desktop.ini"), b"[x]").unwrap();
    // Windows' own hidden files are not the user's and are not listed.
    let hidden = r.root.join("Work").join("desktop.ini");
    std::fs::write(&hidden, b"[x]").unwrap();
    crate::winfs::hide(&hidden).unwrap();
    r.settle();
    r.settle();
    let s = r.lib.status();
    let reasons: Vec<(String, String)> = s.not_synced.iter().map(|n| (n.name.clone(), n.reason.clone())).collect();
    let by_name = |n: &str| r.items().into_iter().find(|i| i.name == n);
    let notes = by_name("notes.txt").expect("a text file syncs");
    assert_eq!((notes.kind, notes.ext.as_str()), (tugboat_core::MediaKind::File, "txt"));
    let make = by_name("Makefile").expect("a file with no extension syncs");
    assert_eq!((make.kind, make.ext.as_str()), (tugboat_core::MediaKind::File, ""));
    assert_eq!(by_name("plan.docx").expect("files in subfolders too").folder, r.folder_named("Work").id);
    assert!(reasons.iter().any(|(n, why)| n == "fake.jpg" && why == "REFUSED - content does not match its type"), "{reasons:?}");
    assert!(reasons.iter().any(|(n, why)| n == "link.lnk" && why.starts_with("REFUSED")), "{reasons:?}");
    assert!(reasons.iter().any(|(n, why)| n == "desktop.ini" && why.starts_with("REFUSED")), "a visible desktop.ini is listed: {reasons:?}");
    assert_eq!(reasons.iter().filter(|(n, _)| n == "desktop.ini").count(), 1, "the hidden one is not listed");
    assert!(by_name("link.lnk").is_none() && by_name("desktop.ini").is_none() && by_name("fake.jpg").is_none());
    assert_eq!(r.items().len(), 3);
    assert!(s.last_error.is_none(), "a refused file is a listing, not an error: {:?}", s.last_error);
    // Judged once: changing the file's content makes it get judged again.
    std::fs::write(r.root.join("fake.jpg"), jpeg(4, 64)).unwrap();
    r.settle();
    assert_eq!(r.item("fake.jpg").kind, tugboat_core::MediaKind::Image);
}

#[test]
fn downloads_and_editor_lock_files_are_listed_not_synced_yet_and_sync_once_they_are_real_files() {
    let mut r = Rig::new();
    let names = ["a.crdownload", "b.part", "c.partial", "d.download", "e.opdownload", "~$f.docx", ".~lock.g.odt#"];
    for n in names {
        std::fs::write(r.root.join(n), b"x").unwrap();
    }
    r.settle();
    r.settle();
    let s = r.lib.status();
    for n in names {
        assert!(s.not_synced.iter().any(|f| f.name == n && f.reason.starts_with("IN PROGRESS")), "{n} is listed: {:?}", s.not_synced);
    }
    assert!(r.items().is_empty(), "nothing in progress is imported");
    // The download finishes: it is renamed to the real file and syncs.
    std::fs::rename(r.root.join("a.crdownload"), r.root.join("a.zip")).unwrap();
    r.settle();
    assert_eq!(r.item("a.zip").ext, "zip");
}

/// The copy a cross-drive move makes sits in the visible library folder under `<name>.tugboat-move`: a scan must
/// never import it (a crash mid-copy leaves it), and the start-up clean-up removes it from the root and the folders.
#[test]
fn a_move_copy_left_in_the_library_is_in_progress_never_an_item_and_is_removed_at_start() {
    let mut r = Rig::new();
    std::fs::create_dir(r.root.join("Work")).unwrap();
    std::fs::write(r.root.join("a.tugboat-move"), jpeg(1, 64)).unwrap();
    std::fs::write(r.root.join("Work").join("B.TUGBOAT-MOVE"), jpeg(2, 64)).unwrap();
    r.write("keep.jpg", 3);
    r.settle();
    r.settle();
    let s = r.lib.status();
    for n in ["a.tugboat-move", "B.TUGBOAT-MOVE"] {
        assert!(s.not_synced.iter().any(|f| f.name == n && f.reason.starts_with("IN PROGRESS")), "{n} is listed: {:?}", s.not_synced);
    }
    assert_eq!(r.items().iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["keep.jpg"], "no move copy is imported");
    assert_eq!(crate::drop::clean_stale(&r.root), 2);
    assert!(!r.root.join("a.tugboat-move").exists() && !r.root.join("Work").join("B.TUGBOAT-MOVE").exists());
    assert!(r.root.join("keep.jpg").exists(), "nothing else is removed");
    assert_eq!(crate::drop::clean_stale(&r.root), 0);
}

/// A 0.1.x library: the config has a path but no `legacy_done`. Its non-photo files were listed
/// "not synced" and left alone; importing them now would start an expiry clock on files the user
/// never asked to sync.
#[test]
fn files_that_were_there_before_0_2_0_stay_listed_and_are_never_imported() {
    let mut r = Rig::new();
    let reopen = |r: &Rig, done: bool| {
        let mut cfg = LibraryConfig::load(&r.data).unwrap();
        cfg.legacy_done = done;
        if !done {
            cfg.legacy.clear();
        }
        cfg.save(&r.data).unwrap();
        Library::open(r.core.clone(), r.place.clone(), r.data.clone(), cfg, &r.root, LibraryOptions::default(), Box::new(|_| {})).unwrap()
    };
    std::fs::write(r.root.join("notes.txt"), b"old").unwrap();
    r.write("a.jpg", 1);
    r.lib = reopen(&r, false); // as 0.1.x left it
    r.settle();
    assert_eq!(r.items().iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["a.jpg"], "the photo syncs as before");
    let listed = |r: &Rig| r.lib.status().not_synced.iter().any(|n| n.name == "notes.txt" && n.reason.starts_with("FROM BEFORE 0.2.0"));
    assert!(listed(&r));
    assert!(LibraryConfig::load(&r.data).unwrap().legacy_done, "the first full pass closed the list");
    // A file added AFTER the upgrade syncs like any other.
    std::fs::write(r.root.join("b.txt"), b"new").unwrap();
    r.settle();
    assert_eq!(r.item("b.txt").kind, tugboat_core::MediaKind::File);
    // The list survives a restart (a new Library reads library.json).
    r.lib = reopen(&r, true);
    r.settle();
    assert!(listed(&r));
    assert!(r.items().iter().all(|i| i.name != "notes.txt"));
    // Renamed in Explorer it is the same file (NTFS identity): still listed, never imported.
    std::fs::rename(r.root.join("notes.txt"), r.root.join("notes2.txt")).unwrap();
    r.settle();
    assert!(r.lib.status().not_synced.iter().any(|n| n.name == "notes2.txt" && n.reason.starts_with("FROM BEFORE 0.2.0")));
    // Deleted, its name is free again: a new file with that name syncs.
    std::fs::remove_file(r.root.join("notes2.txt")).unwrap();
    r.settle();
    assert!(r.lib.status().not_synced.iter().all(|n| !n.reason.starts_with("FROM BEFORE")), "{:?}", r.lib.status().not_synced);
    std::fs::write(r.root.join("notes2.txt"), b"fresh").unwrap();
    r.settle();
    assert_eq!(r.item("notes2.txt").kind, tugboat_core::MediaKind::File);
}

/// Closing the pre-0.2.0 list is written to library.json; when that write fails the in-memory config must not
/// say "done" while the file does not, or no later pass would ever write it (nothing differs any more).
#[test]
fn a_failed_save_of_the_closed_pre_0_2_0_list_is_retried_not_believed() {
    let mut r = Rig::new();
    let mut cfg = LibraryConfig::load(&r.data).unwrap();
    cfg.legacy_done = false;
    cfg.save(&r.data).unwrap();
    r.lib = Library::open(r.core.clone(), r.place.clone(), r.data.clone(), cfg, &r.root, LibraryOptions::default(), Box::new(|_| {})).unwrap();
    std::fs::write(r.root.join("notes.txt"), b"old").unwrap();
    let blocker = block_config_save(&r.data);
    r.scan(0);
    let err = r.lib.status().last_error.expect("the failed save is shown");
    assert!(err.contains("CANNOT SAVE LIBRARY CONFIG"), "{err}");
    assert!(!LibraryConfig::load(&r.data).unwrap().legacy_done, "the file was not written");
    std::fs::remove_dir(&blocker).unwrap();
    // Nothing changed on disk since: the retry must still happen.
    r.scan(1);
    assert_eq!(r.lib.status().last_error, None);
    let cfg = LibraryConfig::load(&r.data).unwrap();
    assert!(cfg.legacy_done, "library.json now holds the closed list");
    assert!(cfg.legacy.iter().any(|e| e.path.to_lowercase().ends_with("notes.txt")), "{cfg:?}");
}

/// While library.json cannot be saved, a file added AFTER the first complete pass is not a pre-0.2.0 file: it must
/// sync, and it must never reach the legacy list (in memory or, once the save works, in the file).
#[test]
fn a_file_added_while_the_closed_list_cannot_be_saved_is_not_taken_for_old() {
    let mut r = Rig::new();
    let mut cfg = LibraryConfig::load(&r.data).unwrap();
    cfg.legacy_done = false;
    cfg.save(&r.data).unwrap();
    r.lib = Library::open(r.core.clone(), r.place.clone(), r.data.clone(), cfg, &r.root, LibraryOptions::default(), Box::new(|_| {})).unwrap();
    std::fs::write(r.root.join("notes.txt"), b"old").unwrap();
    let blocker = block_config_save(&r.data);
    r.scan(0);
    assert!(r.lib.status().last_error.is_some(), "the save failed");
    std::fs::write(r.root.join("new.pdf"), b"new").unwrap();
    r.settle();
    assert_eq!(r.item("new.pdf").kind, tugboat_core::MediaKind::File, "the new file syncs although the save still fails");
    assert!(r.lib.status().not_synced.iter().any(|n| n.name == "notes.txt"), "the old file stays listed");
    std::fs::remove_dir(&blocker).unwrap();
    r.settle();
    let cfg = LibraryConfig::load(&r.data).unwrap();
    assert!(cfg.legacy_done);
    assert!(cfg.legacy.iter().any(|e| e.path.to_lowercase().ends_with("notes.txt")), "{cfg:?}");
    assert!(cfg.legacy.iter().all(|e| !e.path.to_lowercase().ends_with("new.pdf")), "{cfg:?}");
    assert_eq!(r.item("new.pdf").kind, tugboat_core::MediaKind::File);
}

/// D6: a received file carries `Zone.Identifier` (ZoneId=3). Choosing a new folder reserves each destination
/// (create_new) and copies over it; the copy must bring the stream along, and Locate must leave it alone.
#[test]
fn a_received_files_mark_of_the_web_survives_choose_and_locate() {
    use crate::winfs::{read_zone_mark, write_zone_mark, ZONE_TEXT};
    let mut r = Rig::new();
    for (rel, seed) in [("a.jpg", 1), ("Work/b.jpg", 2)] {
        let p = r.write(rel, seed);
        write_zone_mark(&p).unwrap();
    }
    r.settle();
    assert_eq!(r.items().len(), 2);
    let new_root = r._tmp.path().join("newlib");
    r.lib.choose(&new_root, false).unwrap();
    for p in [new_root.join("a.jpg"), new_root.join("Work").join("b.jpg")] {
        assert_eq!(read_zone_mark(&p).as_deref(), Some(ZONE_TEXT), "Choose kept the mark on {}", p.display());
    }
    // Locate: the folder is moved in Explorer, the records follow; no file is rewritten, so the mark stays.
    let located = r._tmp.path().join("located");
    std::fs::rename(&new_root, &located).unwrap();
    r.scan(1);
    r.lib.locate(&located).unwrap();
    r.scan(1);
    assert_eq!(r.items().len(), 2);
    for p in [located.join("a.jpg"), located.join("Work").join("b.jpg")] {
        assert_eq!(read_zone_mark(&p).as_deref(), Some(ZONE_TEXT), "Locate kept the mark on {}", p.display());
    }
}

/// Explorer rename to a refused name: the record does not change, the file is listed, Tugboat never
/// renames it back and never bins it.
#[test]
fn a_synced_file_renamed_to_a_refused_name_in_explorer_is_listed_not_binned_or_renamed_back() {
    let mut r = Rig::new();
    std::fs::write(r.root.join("a.txt"), b"hello").unwrap();
    r.settle();
    let id = r.item("a.txt").id;
    std::fs::rename(r.root.join("a.txt"), r.root.join("a.lnk")).unwrap();
    for _ in 0..4 {
        r.scan(3);
    }
    assert!(r.root.join("a.lnk").exists() && !r.root.join("a.txt").exists(), "never renamed back");
    assert_eq!(r.deleted_count(), 0, "never binned");
    assert!(r.bin().is_empty());
    assert_eq!(r.core.item(id).unwrap().unwrap().local, LocalFileState::Present);
    let s = r.lib.status();
    assert!(s.not_synced.iter().any(|n| n.name == "a.lnk" && n.reason.starts_with("REFUSED")), "{:?}", s.not_synced);
    assert!(s.last_error.is_none(), "{:?}", s.last_error);
}

/// The other device renames a File item to a name this PC refuses: the file stays, one REFUSED row,
/// no error and no retry storm.
#[test]
fn a_rename_to_a_refused_name_on_the_other_device_keeps_the_file_and_lists_one_row() {
    let mut r = Rig::new();
    std::fs::write(r.root.join("notes.ini"), b"[x]").unwrap();
    r.settle();
    // Core refuses such a rename on THIS device, so the other device's record is written straight into
    // the database, the way a sync merge would: name "desktop", extension ini stays, disk name desktop.ini.
    let changed = r.core.debug_conn(|c| c.execute("UPDATE item SET name = 'desktop' WHERE name = 'notes.ini'", []).unwrap());
    assert_eq!(changed, 1);
    for _ in 0..4 {
        r.scan(3);
    }
    let s = r.lib.status();
    assert!(r.root.join("notes.ini").exists(), "the file stays where it is");
    assert!(!r.root.join("desktop.ini").exists());
    assert!(s.last_error.is_none(), "no repeated error: {:?}", s.last_error);
    let rows = s.not_synced.iter().filter(|n| n.name == "notes.ini" && n.reason.starts_with("REFUSED")).count();
    assert_eq!(rows, 1, "one REFUSED row: {:?}", s.not_synced);
    assert_eq!(r.deleted_count(), 0);
}

/// Word and Excel save by writing a temp file and swapping it in with ReplaceFileW, which can give the
/// path a new NTFS identity. It must be an edit of the same item, never a delete plus an add.
#[test]
fn an_office_safe_save_with_replacefilew_is_an_edit_of_the_same_item() {
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{ReplaceFileW, REPLACE_FILE_FLAGS},
    };
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect() };
    let mut r = Rig::new();
    let p = r.root.join("plan.docx");
    std::fs::write(&p, b"version one").unwrap();
    r.settle();
    let first = r.item("plan.docx");
    let tmp = r.root.join("~WRL0001.tmp");
    std::fs::write(&tmp, b"version two, a little longer").unwrap();
    let (replaced, replacement) = (wide(&p), wide(&tmp));
    unsafe { ReplaceFileW(PCWSTR(replaced.as_ptr()), PCWSTR(replacement.as_ptr()), PCWSTR::null(), REPLACE_FILE_FLAGS(0), None, None) }.unwrap();
    r.settle();
    r.settle();
    assert_eq!(r.items().len(), 1, "{:?}", r.items().iter().map(|i| &i.name).collect::<Vec<_>>());
    let now = r.item("plan.docx");
    assert_eq!(now.id, first.id, "the same item");
    assert_eq!(now.size, 28, "with the new content");
    assert!(r.actions().contains(&ActivityAction::Edited));
    assert_eq!(r.deleted_count(), 0);
    assert!(r.bin().is_empty());
    assert!(r.lib.status().last_error.is_none());
}

/// Same-size edit while the app was closed: the mtime Core remembers is the only thing that shows it.
#[test]
fn an_edit_made_while_the_app_was_closed_is_seen_at_the_next_start_even_at_the_same_size() {
    let mut r = Rig::new();
    let p = r.write("a.jpg", 1);
    let t = r.root.join("n.txt");
    std::fs::write(&t, b"aaaa").unwrap();
    r.settle();
    let held = r.core.held_files().unwrap();
    assert!(held.iter().all(|h| h.mtime_ms.is_some()), "every held file remembers its mtime: {held:?}");
    // The app is closed; the files change without changing size.
    let later = std::time::SystemTime::now() + Duration::from_secs(30);
    let mut bytes = jpeg(2, 64);
    bytes[10] = 0x77;
    std::fs::write(&p, &bytes).unwrap();
    std::fs::write(&t, b"bbbb").unwrap();
    for f in [&p, &t] {
        std::fs::OpenOptions::new().write(true).open(f).unwrap().set_modified(later).unwrap();
    }
    // A fresh start: a new Library with an empty in-memory cache over the same Core.
    let cfg = LibraryConfig::load(&r.data).unwrap();
    let lib2 = Library::open(r.core.clone(), r.place.clone(), r.data.clone(), cfg, &r.root, LibraryOptions::default(), Box::new(|_| {})).unwrap();
    let now = r.now;
    lib2.scan_at(false, now);
    let rep = lib2.scan_at(false, now + Duration::from_secs(3));
    assert_eq!(rep.edited, 2, "both same-size edits were seen: {rep:?}");
    assert!(r.actions().iter().filter(|a| **a == ActivityAction::Edited).count() >= 2);
    // And it is quiet afterwards.
    let again = lib2.scan_at(false, now + Duration::from_secs(8));
    assert_eq!((again.edited, again.imported, again.binned), (0, 0, 0));
}

// ---------- Explorer actions ----------

#[test]
fn moving_a_file_between_folders_in_explorer_is_the_same_item() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.settle();
    let before = r.item("a.jpg");
    std::fs::create_dir(r.root.join("Work")).unwrap();
    std::fs::rename(r.root.join("a.jpg"), r.root.join("Work").join("a.jpg")).unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.folders_created, 1, "the new subfolder became a folder first");
    assert_eq!(rep.moved, 1);
    assert_eq!(rep.imported, 0, "not a new item");
    assert_eq!(r.deleted_count(), 0, "not a delete");
    let items = r.items();
    assert_eq!(items.len(), 1);
    let after = &items[0];
    assert_eq!(after.id, before.id, "same item: its Keep and history survive");
    assert_eq!(after.folder, r.folder_named("Work").id);
    assert_eq!(after.path.as_deref().map(Path::new), Some(r.root.join("Work").join("a.jpg").as_path()));
    assert!(r.actions().contains(&ActivityAction::Moved));
    // And back to the root = back to the Inbox.
    std::fs::rename(r.root.join("Work").join("a.jpg"), r.root.join("a.jpg")).unwrap();
    r.scan(1);
    assert_eq!(r.items()[0].folder, INBOX_ID);
    assert_eq!(r.items()[0].id, before.id);
}

#[test]
fn a_kept_item_stays_kept_through_an_explorer_move() {
    let mut r = Rig::new();
    r.write("keepme.jpg", 5);
    r.settle();
    let id = r.item("keepme.jpg").id;
    r.core.set_pinned(id, true).unwrap();
    std::fs::create_dir(r.root.join("Site")).unwrap();
    std::fs::rename(r.root.join("keepme.jpg"), r.root.join("Site").join("keepme.jpg")).unwrap();
    r.scan(1);
    let it = r.core.item(id).unwrap().unwrap();
    assert!(it.pinned, "history kept: it was not deleted and re-added");
}

#[test]
fn renaming_a_file_renames_the_item() {
    let mut r = Rig::new();
    r.write("old.jpg", 1);
    r.settle();
    let id = r.item("old.jpg").id;
    std::fs::rename(r.root.join("old.jpg"), r.root.join("new name.jpg")).unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.moved, 1);
    let it = r.item("new name.jpg");
    assert_eq!(it.id, id);
    assert!(r.actions().contains(&ActivityAction::Renamed));
    // A rename that changes the extension spelling keeps the record's extension: no "x.jpeg.jpg".
    std::fs::rename(r.root.join("new name.jpg"), r.root.join("shot.JPEG")).unwrap();
    r.scan(1);
    assert_eq!(r.item("shot.jpg").id, id);
}

#[test]
fn deleting_a_file_moves_the_item_to_the_bin_and_logs_it() {
    let mut r = Rig::new();
    r.write("gone.jpg", 1);
    r.write("stays.jpg", 2);
    r.settle();
    let deleted_id = r.item("gone.jpg").id;
    std::fs::remove_file(r.root.join("gone.jpg")).unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.binned, 1);
    assert_eq!(r.items().iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["stays.jpg"]);
    assert_eq!(r.deleted_count(), 1);
    let gone = r.core.held_files().unwrap();
    assert_eq!(gone.len(), 1, "only the other file is still held");
    assert!(matches!(r.core.item(deleted_id).unwrap().unwrap().state, ItemState::Deleted { .. }));
    assert!(r.actions().contains(&ActivityAction::Deleted));
}

#[test]
fn an_edit_in_place_becomes_new_content_of_the_same_item() {
    let mut r = Rig::new();
    let p = r.write("photo.jpg", 1);
    r.settle();
    let id = r.item("photo.jpg").id;
    let old = r.core.held_files().unwrap().into_iter().find(|h| h.item == id).unwrap().hash;
    std::fs::write(&p, jpeg(9, 500)).unwrap();
    assert_eq!(r.scan(1).edited, 0, "a changed file waits until it is stable");
    let rep = r.scan(3);
    assert_eq!(rep.edited, 1);
    let new = r.core.held_files().unwrap().into_iter().find(|h| h.item == id).unwrap();
    assert_ne!(new.hash, old);
    assert_eq!(new.size, 500);
    assert_eq!(r.items().len(), 1);
    assert!(r.actions().contains(&ActivityAction::Edited));
}

#[test]
fn an_editor_that_saves_by_replacing_the_file_is_an_edit_not_a_delete_and_add() {
    let mut r = Rig::new();
    let p = r.write("photo.jpg", 1);
    r.settle();
    let id = r.item("photo.jpg").id;
    // Write a temp file and rename it over the original: a new NTFS id at the same path.
    let tmp = r.root.join("photo.jpg.tmp-save");
    std::fs::write(&tmp, jpeg(8, 300)).unwrap();
    std::fs::rename(&tmp, &p).unwrap();
    r.scan(1);
    r.scan(3);
    assert_eq!(r.items().len(), 1, "{:?}", r.items().iter().map(|i| &i.name).collect::<Vec<_>>());
    assert_eq!(r.items()[0].id, id);
    assert_eq!(r.deleted_count(), 0);
    assert_eq!(r.core.held_files().unwrap()[0].size, 300);
    // The new identity was stored, so a later move is still matched by id.
    std::fs::create_dir(r.root.join("Moved")).unwrap();
    std::fs::rename(&p, r.root.join("Moved").join("photo.jpg")).unwrap();
    r.scan(1);
    assert_eq!(r.items().len(), 1);
    assert_eq!(r.items()[0].id, id);
}

#[test]
fn a_file_that_comes_back_with_a_new_identity_is_matched_by_its_bytes() {
    let mut r = Rig::new();
    let p = r.write("a.jpg", 1);
    r.settle();
    let id = r.item("a.jpg").id;
    // A copy + delete (another drive, a tool that rewrites files): new id, same bytes, new place.
    std::fs::create_dir(r.root.join("Copied")).unwrap();
    std::fs::copy(&p, r.root.join("Copied").join("a.jpg")).unwrap();
    std::fs::remove_file(&p).unwrap();
    let rep = r.scan(1);
    assert_eq!((rep.moved, rep.imported, rep.binned), (1, 0, 0), "{rep:?}");
    assert_eq!(r.items()[0].id, id);
}

// ---------- folders ----------

#[test]
fn a_new_subfolder_is_a_keep_folder_and_files_dropped_in_it_land_there() {
    let mut r = Rig::new();
    std::fs::create_dir(r.root.join("Receipts")).unwrap();
    r.write("Receipts/r1.jpg", 1);
    let rep = r.settle();
    assert!(rep.imported == 1);
    let f = r.folder_named("Receipts");
    assert_eq!(f.kind, FolderKind::User);
    assert_eq!(f.retention, Retention::Keep, "nothing new is deleted by surprise");
    assert_eq!(r.item("r1.jpg").folder, f.id);
    assert!(r.actions().contains(&ActivityAction::FolderCreated));
    // A second scan does not create it again.
    r.scan(1);
    assert_eq!(r.core.folders().unwrap().iter().filter(|f| f.name == "Receipts").count(), 1);
}

#[test]
fn renaming_a_subfolder_in_explorer_renames_the_folder_and_keeps_its_items() {
    let mut r = Rig::new();
    r.write("Work/a.jpg", 1);
    r.settle();
    let work = r.folder_named("Work");
    let item = r.item("a.jpg");
    std::fs::rename(r.root.join("Work"), r.root.join("Site")).unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.folders_renamed, 1);
    assert_eq!((rep.folders_created, rep.folders_deleted, rep.binned), (0, 0, 0), "{rep:?}");
    let site = r.folder_named("Site");
    assert_eq!(site.id, work.id, "the same folder, renamed (its retention and items follow)");
    assert!(r.core.folders().unwrap().iter().all(|f| f.name != "Work"));
    let after = r.item("a.jpg");
    assert_eq!(after.id, item.id);
    assert_eq!(after.folder, work.id);
    assert_eq!(after.path.as_deref().map(Path::new), Some(r.root.join("Site").join("a.jpg").as_path()));
    assert!(r.actions().contains(&ActivityAction::FolderRenamed));
}

#[test]
fn a_folder_renamed_on_the_other_device_gets_its_directory_renamed() {
    let mut r = Rig::new();
    r.write("Work/a.jpg", 1);
    r.settle();
    let work = r.folder_named("Work");
    r.core.rename_folder(work.id, "Plans").unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.folders_created, 0, "{rep:?}");
    assert!(r.root.join("Plans").join("a.jpg").exists(), "the directory followed the record");
    assert!(!r.root.join("Work").exists());
    let it = r.item("a.jpg");
    assert_eq!(it.path.as_deref().map(Path::new), Some(r.root.join("Plans").join("a.jpg").as_path()));
    // Nothing was binned or re-imported, and a second scan is quiet.
    let again = r.scan(5);
    assert_eq!((again.imported, again.moved, again.binned, again.folders_created), (0, 0, 0, 0));
    assert_eq!(r.items().len(), 1);
}

#[test]
fn deleting_a_subfolder_sends_its_items_to_the_bin_and_removes_the_folder() {
    let mut r = Rig::new();
    r.write("Work/a.jpg", 1);
    r.write("Work/b.jpg", 2);
    r.write("c.jpg", 3);
    r.settle();
    let work = r.folder_named("Work");
    std::fs::remove_dir_all(r.root.join("Work")).unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.binned, 2);
    assert_eq!(rep.folders_deleted, 1);
    assert!(r.core.folders().unwrap().iter().all(|f| f.id != work.id), "the folder is gone");
    assert_eq!(r.items().iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["c.jpg"]);
    assert_eq!(r.deleted_count(), 2);
}

#[test]
fn an_empty_subfolder_deleted_in_explorer_deletes_the_folder() {
    let mut r = Rig::new();
    std::fs::create_dir(r.root.join("Empty")).unwrap();
    r.scan(0);
    assert!(r.core.folders().unwrap().iter().any(|f| f.name == "Empty"));
    std::fs::remove_dir(r.root.join("Empty")).unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.folders_deleted, 1);
    assert!(r.core.folders().unwrap().iter().all(|f| f.name != "Empty"));
}

#[test]
fn a_default_folders_directory_vanishing_does_not_delete_the_folder() {
    let mut r = Rig::new();
    r.write("Keep/k.jpg", 1);
    r.settle();
    assert!(r.item("k.jpg").folder == tugboat_core::KEEP_ID);
    std::fs::remove_dir_all(r.root.join("Keep")).unwrap();
    r.scan(1);
    assert!(r.core.folders().unwrap().iter().any(|f| f.id == tugboat_core::KEEP_ID), "defaults are never deleted");
}

// ---------- pauses ----------

#[test]
fn a_missing_library_pauses_sync_and_bins_nothing() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.write("b.jpg", 2);
    r.settle();
    let moved_away = r.root.with_file_name("lib-moved");
    std::fs::rename(&r.root, &moved_away).unwrap();
    r.scan(1);
    let s = r.lib.status();
    assert!(matches!(s.health, LibraryHealth::Missing { .. }), "{:?}", s.health);
    assert!(s.message.unwrap().contains("LIBRARY FOLDER NOT FOUND"));
    assert_eq!(r.items().len(), 2, "nothing was binned because the folder is not there");
    assert_eq!(r.deleted_count(), 0);
    // The folder comes back: sync resumes by itself, still two items, none lost.
    std::fs::rename(&moved_away, &r.root).unwrap();
    r.scan(1);
    assert_eq!(r.lib.status().health, LibraryHealth::Ready);
    assert_eq!(r.items().len(), 2);
    assert_eq!(r.deleted_count(), 0);
    assert!(r.actions().contains(&ActivityAction::Paused) && r.actions().contains(&ActivityAction::Resumed));
}

#[test]
fn a_deleted_marker_pauses_like_a_missing_folder() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.settle();
    std::fs::remove_file(r.root.join(".tugboat").join("library-id")).unwrap();
    r.scan(1);
    assert!(matches!(r.lib.status().health, LibraryHealth::Missing { .. }));
    // A different folder with the same path but another marker (a fresh library) is also not ours.
    std::fs::write(r.root.join(".tugboat").join("library-id"), "someone-else").unwrap();
    r.scan(1);
    assert!(matches!(r.lib.status().health, LibraryHealth::Missing { .. }));
    assert_eq!(r.items().len(), 1);
}

#[test]
fn locate_accepts_only_the_folder_that_holds_this_librarys_marker() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.settle();
    let id = r.item("a.jpg").id;
    let new_root = r.root.with_file_name("moved-library");
    std::fs::rename(&r.root, &new_root).unwrap();
    r.scan(1);
    assert!(matches!(r.lib.status().health, LibraryHealth::Missing { .. }));
    let elsewhere = r.root.with_file_name("not-it");
    std::fs::create_dir(&elsewhere).unwrap();
    assert_eq!(r.lib.locate(&elsewhere).unwrap_err().kind, "not_library");
    r.lib.locate(&new_root).unwrap();
    r.scan(1);
    assert_eq!(r.lib.status().health, LibraryHealth::Ready);
    let it = r.core.item(id).unwrap().unwrap();
    assert_eq!(it.path.as_deref().map(Path::new), Some(new_root.join("a.jpg").as_path()), "paths follow by NTFS id");
    assert_eq!(r.deleted_count(), 0);
}

fn fill(r: &mut Rig, n: u8) {
    for i in 0..n {
        r.write(&format!("p{i:02}.jpg"), i + 1);
    }
    r.settle();
    assert_eq!(r.items().len(), n as usize);
}

#[test]
fn more_than_twenty_missing_files_pause_and_ask_instead_of_binning() {
    let mut r = Rig::strict();
    fill(&mut r, 25);
    for i in 0..25 {
        std::fs::remove_file(r.root.join(format!("p{i:02}.jpg"))).unwrap();
    }
    let rep = r.scan(1);
    assert_eq!(rep.binned, 0);
    assert_eq!(r.lib.status().health, LibraryHealth::BulkMissing { missing: 25, total: 25 });
    assert!(r.lib.status().message.unwrap().contains("25 FILES MISSING"));
    assert_eq!(r.items().len(), 25, "still listed: nothing was binned without asking");
    assert_eq!(r.deleted_count(), 0);
    // New drops wait while paused, and are imported after the decision.
    r.write("new.jpg", 99);
    r.settle();
    assert!(r.items().iter().all(|i| i.name != "new.jpg"));
    // [Move them to the Bin]
    r.lib.resolve_missing(true).unwrap();
    assert_eq!(r.deleted_count(), 25);
    assert_eq!(r.lib.status().health, LibraryHealth::Ready);
    r.settle();
    assert_eq!(r.items().iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["new.jpg"], "resumed: the waiting file is imported");
}

#[test]
fn keep_waiting_then_the_files_return_means_nothing_is_lost() {
    let mut r = Rig::strict();
    fill(&mut r, 25);
    let backup: Vec<(PathBuf, Vec<u8>)> = (0..25).map(|i| { let p = r.root.join(format!("p{i:02}.jpg")); (p.clone(), std::fs::read(&p).unwrap()) }).collect();
    for (p, _) in &backup {
        std::fs::remove_file(p).unwrap();
    }
    r.scan(1);
    assert!(matches!(r.lib.status().health, LibraryHealth::BulkMissing { .. }));
    r.lib.resolve_missing(false).unwrap(); // [Keep waiting]
    assert!(matches!(r.lib.status().health, LibraryHealth::BulkMissing { .. }), "still paused");
    for (p, bytes) in &backup {
        std::fs::write(p, bytes).unwrap(); // the drive came back / restored from backup
    }
    r.scan(10);
    assert_eq!(r.lib.status().health, LibraryHealth::Ready);
    assert_eq!(r.items().len(), 25);
    assert_eq!(r.deleted_count(), 0);
}

#[test]
fn the_ten_percent_rule_applies_even_below_twenty_files() {
    let mut r = Rig::strict();
    fill(&mut r, 30);
    // 3 of 30 = exactly 10%: not "more than 10%", so they go to the Bin.
    for i in 0..3 {
        std::fs::remove_file(r.root.join(format!("p{i:02}.jpg"))).unwrap();
    }
    let rep = r.scan(1);
    assert_eq!(rep.binned, 3, "{rep:?}");
    assert_eq!(r.lib.status().health, LibraryHealth::Ready);
    // 4 of the remaining 27 = 14.8%: pause.
    for i in 3..7 {
        std::fs::remove_file(r.root.join(format!("p{i:02}.jpg"))).unwrap();
    }
    let rep = r.scan(1);
    assert_eq!(rep.binned, 0);
    assert_eq!(r.lib.status().health, LibraryHealth::BulkMissing { missing: 4, total: 27 });
}

#[test]
fn a_tiny_library_pauses_on_one_missing_file_under_the_literal_ten_percent_rule() {
    // PLAN: "more than 20 files or 10% of the library missing". In a 2-file library one delete
    // is 50%. Pinned here so the behaviour is visible; see the report's open issues.
    let mut r = Rig::strict();
    r.write("a.jpg", 1);
    r.write("b.jpg", 2);
    r.settle();
    std::fs::remove_file(r.root.join("a.jpg")).unwrap();
    assert_eq!(r.scan(1).binned, 0);
    assert_eq!(r.lib.status().health, LibraryHealth::BulkMissing { missing: 1, total: 2 });
    r.lib.resolve_missing(true).unwrap();
    assert_eq!(r.deleted_count(), 1);
}

#[test]
fn exactly_twenty_missing_of_a_big_library_is_binned_and_twenty_one_pauses() {
    let mut r = Rig::strict();
    fill(&mut r, 250);
    for i in 0..20 {
        std::fs::remove_file(r.root.join(format!("p{i:02}.jpg"))).unwrap();
    }
    assert_eq!(r.scan(1).binned, 20, "20 of 250 is 8%: under both limits");
    std::fs::remove_file(r.root.join("p20.jpg")).unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.binned, 1, "1 file is under both limits");
    for i in 21..42 {
        std::fs::remove_file(r.root.join(format!("p{i:02}.jpg"))).unwrap();
    }
    assert_eq!(r.scan(1).binned, 0, "21 files: over the file limit");
    assert!(matches!(r.lib.status().health, LibraryHealth::BulkMissing { missing: 21, .. }));
}

// ---------- choosing a library ----------

#[test]
fn a_library_must_be_new_or_empty_when_chosen() {
    let mut r = Rig::new();
    let full = r._tmp.path().join("photos");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::write(full.join("holiday.jpg"), jpeg(1, 64)).unwrap();
    let e = r.lib.choose(&full, false).unwrap_err();
    assert_eq!(e.kind, "not_empty");
    assert!(e.message.contains("MUST BE EMPTY"), "{}", e.message);
    assert_eq!(r.lib.root().as_deref(), Some(r.root.as_path()), "the library did not change");
    r.scan(0);
    // Not a folder, relative, and overlapping paths are refused too.
    let file = r._tmp.path().join("afile");
    std::fs::write(&file, b"x").unwrap();
    assert!(r.lib.choose(&file, false).is_err());
    assert!(r.lib.choose(Path::new("relative"), false).is_err());
    assert!(r.lib.choose(&r.root.join("inside"), false).is_err());
    assert!(r.lib.choose(&r.data.join("lib"), false).is_err(), "not inside the app's own data");
    // An empty folder (and one holding only Windows' hidden files) is fine.
    let empty = r._tmp.path().join("empty-ok");
    std::fs::create_dir_all(&empty).unwrap();
    let hidden = empty.join("desktop.ini");
    std::fs::write(&hidden, b"[x]").unwrap();
    let _ = crate::winfs::hide(&hidden);
    r.lib.choose(&empty, false).unwrap();
}

#[test]
fn choosing_a_new_library_copies_held_files_first_then_switches() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.write("Work/b.jpg", 2);
    r.settle();
    let (a, b) = (r.item("a.jpg").id, r.item("b.jpg").id);
    let new_root = r._tmp.path().join("newlib");
    r.lib.choose(&new_root, false).unwrap();
    assert_eq!(r.lib.root().as_deref(), Some(new_root.as_path()));
    assert!(new_root.join("a.jpg").exists() && new_root.join("Work").join("b.jpg").exists());
    assert!(!r.root.join("a.jpg").exists(), "the old copies are removed after the switch");
    assert_eq!(r.core.item(a).unwrap().unwrap().path.as_deref().map(Path::new), Some(new_root.join("a.jpg").as_path()));
    assert_eq!(r.core.item(b).unwrap().unwrap().path.as_deref().map(Path::new), Some(new_root.join("Work").join("b.jpg").as_path()));
    // The config on disk and the placement follow, and a scan of the new folder is quiet.
    assert_eq!(LibraryConfig::load(&r.data).unwrap().path.as_deref(), Some(new_root.as_path()));
    assert_eq!(r.place.root().as_deref(), Some(new_root.as_path()));
    let rep = r.scan(1);
    assert_eq!((rep.imported, rep.binned, rep.moved), (0, 0, 0), "{rep:?}");
    assert_eq!(r.items().len(), 2);
    assert_eq!(r.lib.status().health, LibraryHealth::Ready);
}

/// R5: a hidden folder passes the "must be empty" check, so its files can sit exactly where a
/// copy is about to go. The copy must refuse, never overwrite, and clean-up must not delete it.
#[test]
fn a_file_hidden_in_the_new_folder_is_never_overwritten_by_the_move() {
    let mut r = Rig::new();
    r.write("Work/a.jpg", 1);
    r.settle();
    let before = held_paths(&r);
    let new_root = r._tmp.path().join("newlib");
    let work = new_root.join("Work");
    std::fs::create_dir_all(&work).unwrap();
    let theirs = jpeg(9, 80);
    std::fs::write(work.join("a.jpg"), &theirs).unwrap();
    crate::winfs::hide(&work).unwrap();
    let e = r.lib.choose(&new_root, false).unwrap_err();
    assert!(e.message.contains("already exists"), "{}", e.message);
    assert_eq!(std::fs::read(work.join("a.jpg")).unwrap(), theirs, "the user's own file is untouched");
    assert_eq!(held_paths(&r), before, "every record still points at the old library");
    assert!(r.root.join("Work").join("a.jpg").exists());
    assert_eq!(r.lib.root().as_deref(), Some(r.root.as_path()));
    assert_eq!(r.lib.status().health, LibraryHealth::Ready, "sync is not left paused");
}

/// R6: the old file goes only while it is still what was copied.
#[test]
fn an_old_file_is_deleted_only_while_it_is_what_was_copied() {
    use super::reconcile::remove_if_unchanged;
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("src.jpg"), tmp.path().join("dst.jpg"));
    // Untouched: the copy keeps the source's time and length, so it is deleted.
    std::fs::write(&src, jpeg(1, 64)).unwrap();
    std::fs::copy(&src, &dst).unwrap();
    assert!(remove_if_unchanged(&src, &dst));
    assert!(!src.exists());
    // Edited to a different length after the copy: kept.
    std::fs::write(&src, jpeg(1, 64)).unwrap();
    std::fs::copy(&src, &dst).unwrap();
    std::fs::write(&src, jpeg(2, 99)).unwrap();
    assert!(!remove_if_unchanged(&src, &dst));
    assert_eq!(std::fs::read(&src).unwrap(), jpeg(2, 99));
    // Same length, newer time (an in-place edit): kept.
    std::fs::write(&src, jpeg(1, 64)).unwrap();
    std::fs::copy(&src, &dst).unwrap();
    std::fs::write(&src, jpeg(3, 64)).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&src).unwrap();
    f.set_modified(std::time::SystemTime::now() + Duration::from_secs(60)).unwrap();
    drop(f);
    assert!(!remove_if_unchanged(&src, &dst));
    assert_eq!(std::fs::read(&src).unwrap(), jpeg(3, 64));
    // Source or copy missing: nothing to delete, reported as kept.
    assert!(!remove_if_unchanged(&tmp.path().join("gone.jpg"), &dst));
    std::fs::remove_file(&dst).unwrap();
    assert!(!remove_if_unchanged(&src, &dst));
    assert!(src.exists());
}

#[test]
fn a_cloud_synced_location_is_refused_unless_the_user_insists() {
    let r = Rig::new();
    let dropbox = r._tmp.path().join("Dropbox").join("lib");
    let e = r.lib.choose(&dropbox, false).unwrap_err();
    assert_eq!(e.kind, "cloud_folder");
    assert!(e.message.starts_with("CLOUD FOLDER"));
    assert_eq!(r.lib.root().as_deref(), Some(r.root.as_path()));
    r.lib.choose(&dropbox, true).unwrap();
    assert!(r.lib.status().cloud_warning.is_some(), "the banner stays while it is in use");
}

#[test]
fn a_failed_copy_leaves_the_old_library_untouched() {
    let mut r = Rig::new();
    let p = r.write("a.jpg", 1);
    r.settle();
    // Make the destination impossible: a file where the folder "newlib2" must be created... the
    // parent of the new library is a file, so creating it fails before anything is copied.
    let blocker = r._tmp.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    assert!(r.lib.choose(&blocker.join("lib"), false).is_err());
    assert_eq!(r.lib.root().as_deref(), Some(r.root.as_path()));
    assert!(p.exists());
    assert_eq!(r.items().len(), 1);
}

/// Make `Locate` / `Choose` fail at the config write: a directory where the temp file goes.
fn block_config_save(data: &Path) -> PathBuf {
    let blocker = data.join("library.json.tmp");
    std::fs::create_dir_all(&blocker).unwrap();
    blocker
}

fn held_paths(r: &Rig) -> Vec<(ItemId, String)> {
    let mut v: Vec<_> = r.core.held_files().unwrap().into_iter().map(|h| (h.item, h.path)).collect();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

#[test]
fn a_failed_config_save_is_retried_and_the_directory_map_reaches_the_disk() {
    let mut r = Rig::new();
    let blocker = block_config_save(&r.data);
    std::fs::create_dir(r.root.join("Work")).unwrap();
    r.scan(0);
    let err = r.lib.status().last_error.expect("the failed save is shown");
    assert!(err.contains("CANNOT SAVE LIBRARY CONFIG"), "{err}");
    std::fs::remove_dir(&blocker).unwrap();
    // Nothing changed on disk since: the retry must still happen.
    r.scan(1);
    assert_eq!(r.lib.status().last_error, None);
    let cfg = LibraryConfig::load(&r.data).unwrap();
    assert!(cfg.dirs.values().any(|e| e.name == "Work"), "library.json holds the new folder: {cfg:?}");
}

#[test]
fn a_failed_choose_after_the_copies_rolls_everything_back_and_can_be_retried() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.write("Work/b.jpg", 2);
    r.settle();
    let before = held_paths(&r);
    assert_eq!(before.len(), 2);
    let blocker = block_config_save(&r.data);
    let new_root = r._tmp.path().join("newlib");
    assert!(r.lib.choose(&new_root, false).is_err(), "the config cannot be saved");
    // The records, the old files, the config and the placement are all still the old library.
    assert_eq!(held_paths(&r), before, "every record points at the old root again");
    assert!(r.root.join("a.jpg").exists() && r.root.join("Work").join("b.jpg").exists());
    assert!(!new_root.join("a.jpg").exists() && !new_root.join("Work").join("b.jpg").exists(), "the copies are gone");
    assert!(!new_root.join(".tugboat").join("library-id").exists(), "the new marker is gone");
    assert_eq!(r.lib.root().as_deref(), Some(r.root.as_path()));
    assert_eq!(r.place.root().as_deref(), Some(r.root.as_path()));
    assert_eq!(LibraryConfig::load(&r.data).unwrap().path.as_deref(), Some(r.root.as_path()));
    assert_eq!(r.lib.status().health, LibraryHealth::Ready, "sync is not left paused");
    let rep = r.scan(1);
    assert_eq!((rep.imported, rep.binned, rep.moved), (0, 0, 0), "{rep:?}");
    // The blocker gone, the same choice works.
    std::fs::remove_dir(&blocker).unwrap();
    r.lib.choose(&new_root, false).unwrap();
    assert_eq!(r.lib.root().as_deref(), Some(new_root.as_path()));
    assert!(new_root.join("a.jpg").exists() && new_root.join("Work").join("b.jpg").exists());
    assert_eq!(LibraryConfig::load(&r.data).unwrap().path.as_deref(), Some(new_root.as_path()));
}

#[test]
fn a_failed_locate_leaves_the_records_the_config_and_the_placement_on_the_old_root() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.write("Work/b.jpg", 2);
    r.settle();
    let before = held_paths(&r);
    // The same library, copied elsewhere (marker included): the folder to Locate.
    let copy = r._tmp.path().join("copy");
    copy_dir(&r.root, &copy);
    let blocker = block_config_save(&r.data);
    assert!(r.lib.locate(&copy).is_err());
    assert_eq!(held_paths(&r), before, "the scan and the placement use one root");
    assert_eq!(r.lib.root().as_deref(), Some(r.root.as_path()));
    assert_eq!(r.place.root().as_deref(), Some(r.root.as_path()));
    assert_eq!(LibraryConfig::load(&r.data).unwrap().path.as_deref(), Some(r.root.as_path()), "library.json still names the old root");
    let rep = r.scan(1);
    assert_eq!((rep.imported, rep.binned, rep.moved), (0, 0, 0), "{rep:?}");
    std::fs::remove_dir(&blocker).unwrap();
    r.lib.locate(&copy).unwrap();
    assert_eq!(r.lib.root().as_deref(), Some(copy.as_path()));
    assert!(held_paths(&r).iter().all(|(_, p)| p.starts_with(copy.to_str().unwrap())));
}

#[test]
fn a_dead_file_watcher_stays_on_screen_after_a_scan() {
    let mut r = Rig::new();
    super::lock(&r.lib.inner.st).watcher_error = Some("FILE WATCHER NOT RUNNING - test".into());
    r.scan(0);
    assert_eq!(r.lib.status().last_error.as_deref(), Some("FILE WATCHER NOT RUNNING - test"));
    r.scan(1);
    assert_eq!(r.lib.status().last_error.as_deref(), Some("FILE WATCHER NOT RUNNING - test"), "a clean pass does not hide it");
    super::lock(&r.lib.inner.st).watcher_error = None;
    r.scan(1);
    assert_eq!(r.lib.status().last_error, None);
}

#[test]
fn a_recovered_file_watcher_clears_its_error_without_waiting_for_the_next_event() {
    let r = Rig::new();
    let lib = &r.lib;
    let worker = lib.start();
    let wait = |what: &str, f: &dyn Fn() -> bool| {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(15), "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait("the start-up scan ran", &|| lib.status().last_scan_at.is_some());
    // The watcher died earlier (error on screen) and the next pass makes a new one that works.
    {
        let mut st = super::lock(&lib.inner.st);
        st.watcher_error = Some("FILE WATCHER NOT RUNNING - test".into());
        st.rewatch = true;
    }
    worker.request_scan(false);
    // Rescan is every 600 s and no file changes: only the second pass can clear the line.
    wait("the stale watcher error is gone", &|| lib.status().last_error.is_none() && super::lock(&lib.inner.st).watcher_error.is_none());
    drop(worker);
}

#[test]
fn a_choose_that_fails_leaves_no_folder_made_for_a_file_that_was_already_gone() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.write("Sub/c.jpg", 3);
    r.settle();
    // Edited away since the last scan: its record is still held, its source is gone.
    std::fs::remove_file(r.root.join("Sub").join("c.jpg")).unwrap();
    let blocker = block_config_save(&r.data);
    let new_root = r._tmp.path().join("newlib");
    assert!(r.lib.choose(&new_root, false).is_err());
    assert!(!new_root.join("Sub").exists(), "the empty subfolder made for the missing file is gone");
    assert!(!new_root.join("a.jpg").exists());
    std::fs::remove_dir(&blocker).unwrap();
    r.lib.choose(&new_root, false).expect("the retry is not refused with LIBRARY MUST BE EMPTY");
    assert!(new_root.join("a.jpg").exists());
}

#[test]
fn the_watcher_moves_to_a_library_chosen_after_start() {
    // No library yet (the default folder holds the user's own photo): the worker starts with no
    // watcher. Rescan every 10 minutes, so only a watcher event can import the drop below.
    let r = Rig::with_default_root(|t| t.join("Local Tugboat"), |root| {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("holiday.jpg"), jpeg(1, 64)).unwrap();
    });
    let opts = LibraryOptions { stable_for: Duration::from_millis(150), debounce: Duration::from_millis(100), max_debounce: Duration::from_secs(1), rescan_every: Duration::from_secs(600) };
    let lib = Library::open(r.core.clone(), r.place.clone(), r.data.clone(), LibraryConfig::default(), &r.root, opts, Box::new(|_| {})).unwrap();
    assert!(lib.root().is_none());
    let worker = lib.start();
    let wait = |what: &str, f: &dyn Fn() -> bool| {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(15), "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait("the start-up scan ran", &|| lib.status().last_scan_at.is_some());
    let new_root = r._tmp.path().join("chosen");
    lib.choose(&new_root, false).unwrap();
    let scanned = lib.status().last_scan_at;
    worker.request_scan(true);
    wait("the scan after Choose ran", &|| lib.status().last_scan_at != scanned);
    std::thread::sleep(Duration::from_millis(500)); // the new watcher is made right after that scan
    std::fs::write(new_root.join("fresh.jpg"), jpeg(2, 64)).unwrap();
    wait("a file dropped in the chosen folder is imported by the watcher", &|| r.items().iter().any(|i| i.name == "fresh.jpg"));
    drop(worker);
}

// ---------- the placement and the scan must not fight ----------

#[test]
fn files_moved_by_core_itself_are_not_mistaken_for_user_actions() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.settle();
    let id = r.item("a.jpg").id;
    // Core deletes (placement moves the file to the Bin) and restores it.
    r.core.delete_item(id).unwrap();
    let rep = r.scan(1);
    assert_eq!((rep.binned, rep.imported, rep.moved), (0, 0, 0), "{rep:?}");
    assert_eq!(r.bin().len(), 1);
    r.core.restore_item(id).unwrap();
    let rep = r.scan(1);
    r.scan(3);
    assert_eq!((rep.imported, rep.binned), (0, 0), "{rep:?}");
    assert_eq!(r.items().len(), 1);
    assert_eq!(r.items()[0].id, id);
    assert_eq!(r.bin().len(), 0);
    // Folder move through Core relocates the file; the scan agrees with it.
    let work = r.core.create_folder("Work", Retention::Keep).unwrap();
    r.core.move_item(id, work).unwrap();
    assert!(r.root.join("Work").join("a.jpg").exists());
    let rep = r.scan(1);
    assert_eq!((rep.imported, rep.binned, rep.folders_created), (0, 0, 0), "{rep:?}");
    assert_eq!(r.items()[0].folder, work);
}

// ---------- status events and the real watcher ----------

#[test]
fn the_ui_is_told_when_the_library_status_changes_and_only_then() {
    let mut r = Rig::new();
    r.scan(0);
    let n = r.statuses.lock().unwrap().len();
    assert!(n >= 1, "the first scan published a status");
    r.scan(1);
    assert_eq!(r.statuses.lock().unwrap().len(), n, "an unchanged status is not re-sent");
    std::fs::write(r.root.join("x.lnk"), b"1").unwrap();
    r.scan(1);
    let all = r.statuses.lock().unwrap();
    assert_eq!(all.len(), n + 1);
    assert_eq!(all.last().unwrap().not_synced.len(), 1);
}

#[test]
fn the_watcher_thread_imports_a_drop_and_follows_an_explorer_move() {
    let r = Rig::new();
    // Real timers, short: stable after 150 ms, scan 100 ms after the last event.
    let opts = LibraryOptions { stable_for: Duration::from_millis(150), debounce: Duration::from_millis(100), max_debounce: Duration::from_secs(1), rescan_every: Duration::from_secs(600) };
    let lib = Library::open(
        r.core.clone(),
        r.place.clone(),
        r.data.clone(),
        LibraryConfig::load(&r.data).unwrap(),
        &r.root,
        opts,
        Box::new(|_| {}),
    )
    .unwrap();
    let worker = lib.start();
    let wait = |what: &str, f: &dyn Fn() -> bool| {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    std::thread::sleep(Duration::from_millis(500)); // the start-up scan and the watcher are running
    r.write("live.jpg", 1);
    wait("the dropped file becomes an item", &|| r.items().iter().any(|i| i.name == "live.jpg"));
    let id = r.item("live.jpg").id;
    std::fs::create_dir(r.root.join("Moved")).unwrap();
    std::fs::rename(r.root.join("live.jpg"), r.root.join("Moved").join("live.jpg")).unwrap();
    wait("the Explorer move is followed", &|| r.core.item(id).unwrap().is_some_and(|i| i.folder != INBOX_ID));
    assert_eq!(r.items().len(), 1);
    assert_eq!(r.deleted_count(), 0);
    // A manual rescan request works too: a text file is a file like any other now.
    std::fs::write(r.root.join("note.txt"), b"x").unwrap();
    worker.request_scan(true);
    wait("the rescan imports the text file", &|| r.items().iter().any(|i| i.name == "note.txt"));
    std::fs::write(r.root.join("link.lnk"), b"x").unwrap();
    worker.request_scan(true);
    wait("the rescan lists the shortcut as refused", &|| lib.status().not_synced.iter().any(|n| n.name == "link.lnk"));
    drop(worker); // stops the thread and the watcher
}

// ---------- the placement ----------

mod placement {
    use super::*;
    use tugboat_core::{placement::FileTarget, BlobHash, Placement};

    fn rig() -> (tempfile::TempDir, PcLibrary, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        std::fs::create_dir_all(&root).unwrap();
        let p = PcLibrary::new(Some(root.clone()), tmp.path().join("bin"), Default::default());
        (tmp, p, root)
    }

    fn target(n: u128, name: &str, folder: FolderId, kind: FolderKind, folder_name: &str) -> FileTarget {
        FileTarget {
            item: ItemId(uuid::Uuid::from_u128(n)),
            hash: BlobHash([n as u8; 32]),
            name: name.into(),
            ext: "jpg".into(),
            captured_at: 1_790_000_000_000,
            folder,
            folder_kind: kind,
            folder_name: folder_name.into(),
            current: None,
        }
    }

    fn inbox(n: u128, name: &str) -> FileTarget {
        target(n, name, INBOX_ID, FolderKind::Inbox, "Inbox")
    }

    fn part(p: &PcLibrary, t: &FileTarget, bytes: &[u8]) -> PathBuf {
        let path = p.part_path(&t.hash).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn downloads_are_staged_in_the_hidden_folder_and_finish_with_a_rename() {
        let (_t, p, root) = rig();
        let t = inbox(1, "x.jpg");
        let pp = p.part_path(&t.hash).unwrap();
        assert_eq!(pp.parent().unwrap(), root.join(".tugboat"));
        assert!(pp.to_string_lossy().ends_with(".part"));
        let src = part(&p, &t, &jpeg(1, 64));
        let placed = p.commit(&src, &t).unwrap();
        assert_eq!(placed, root.join("x.jpg"));
        assert!(!src.exists(), "the part was consumed");
        assert!(crate::winfs::is_hidden_or_system(&std::fs::metadata(root.join(".tugboat")).unwrap()));
    }

    #[test]
    fn nothing_is_placed_while_the_library_is_being_switched() {
        let (_t, p, root) = rig();
        let t = inbox(1, "x.jpg");
        let src = part(&p, &t, &jpeg(1, 64));
        p.set_switching(true);
        assert!(p.commit(&src, &t).is_err(), "a download finishing now is refused (and fetched again)");
        assert!(p.adopt(&src, &t).is_err());
        assert!(!root.join("x.jpg").exists(), "no file under the old root");
        assert!(src.exists(), "the part is kept for the retry");
        p.set_switching(false);
        assert_eq!(p.commit(&src, &t).unwrap(), root.join("x.jpg"));
    }

    #[test]
    fn a_name_that_is_taken_gets_a_suffix_and_nobody_is_overwritten() {
        let (_t, p, root) = rig();
        // A file the user already has with that name.
        std::fs::write(root.join("x.jpg"), b"users own file").unwrap();
        let t1 = inbox(1, "x.jpg");
        let a = p.commit(&part(&p, &t1, &jpeg(1, 64)), &t1).unwrap();
        assert_eq!(a, root.join("x (2).jpg"));
        let t2 = inbox(2, "X.JPG");
        let b = p.commit(&part(&p, &t2, &jpeg(2, 64)), &t2).unwrap();
        assert_eq!(b.file_name().unwrap(), "X (3).jpg", "case-insensitive: x.jpg and x (2).jpg are both taken");
        assert_eq!(std::fs::read(root.join("x.jpg")).unwrap(), b"users own file");
        assert_eq!(std::fs::read(&a).unwrap(), jpeg(1, 64));
    }

    #[test]
    fn new_content_for_the_same_item_replaces_its_own_file() {
        let (_t, p, root) = rig();
        let mut t = inbox(1, "x.jpg");
        let first = p.commit(&part(&p, &t, &jpeg(1, 64)), &t).unwrap();
        t.current = Some(first.clone());
        t.hash = BlobHash([9; 32]);
        let second = p.commit(&part(&p, &t, &jpeg(2, 80)), &t).unwrap();
        assert_eq!(second, first, "same place");
        assert_eq!(std::fs::read(&second).unwrap(), jpeg(2, 80));
        assert_eq!(std::fs::read_dir(&root).unwrap().filter(|e| e.as_ref().unwrap().file_name() != ".tugboat").count(), 1);
    }

    #[test]
    fn other_folders_get_a_subfolder_and_two_folders_with_one_name_get_two() {
        let (_t, p, root) = rig();
        let f1 = FolderId(uuid::Uuid::from_u128(0x100));
        let f2 = FolderId(uuid::Uuid::from_u128(0x200));
        let a = target(1, "a.jpg", f1, FolderKind::User, "Work");
        let b = target(2, "b.jpg", f2, FolderKind::User, "work");
        let pa = p.commit(&part(&p, &a, &jpeg(1, 64)), &a).unwrap();
        let pb = p.commit(&part(&p, &b, &jpeg(2, 64)), &b).unwrap();
        assert_eq!(pa.parent().unwrap(), root.join("Work"));
        assert_eq!(pb.parent().unwrap(), root.join("work (2)"), "a second folder of the same name must not share a directory");
        // The mapping is remembered and flagged for saving.
        assert!(p.take_dirty());
        assert_eq!(p.dirs().len(), 2);
        // A folder name that is unusable on disk still gets a directory.
        let weird = target(3, "c.jpg", FolderId(uuid::Uuid::from_u128(0x300)), FolderKind::User, "CON");
        let pc = p.commit(&part(&p, &weird, &jpeg(3, 64)), &weird).unwrap();
        assert_eq!(pc.parent().unwrap(), root.join("Folder"));
    }

    #[test]
    fn a_record_name_with_unsafe_characters_never_escapes_the_library() {
        let (_t, p, root) = rig();
        for name in ["..\\..\\evil.jpg", "a:b.jpg", "con.jpg", "x/../../y.jpg", "trail. "] {
            let t = inbox(7, name);
            let path = p.commit(&part(&p, &t, &jpeg(1, 64)), &t).unwrap();
            assert_eq!(path.parent().unwrap(), root, "{name:?} -> {}", path.display());
            assert!(path.exists());
            std::fs::remove_file(path).unwrap();
        }
        // The extension comes from a synced record: only a safe one may become part of a path.
        for bad in ["lnk", "a/b", "..", "x y", "EXE"] {
            let mut t = inbox(8, "x.jpg");
            t.ext = bad.into();
            assert!(p.to_bin(&t).is_err(), "{bad:?}");
            assert!(p.commit(&part(&p, &inbox(8, "x.jpg"), &jpeg(1, 64)), &t).is_err(), "{bad:?}");
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().filter(|e| e.as_ref().unwrap().file_name() != ".tugboat").count(), 0, "nothing was placed");
    }

    #[test]
    fn a_file_that_is_not_the_type_it_claims_is_refused_and_left_alone() {
        let (_t, p, root) = rig();
        let t = inbox(1, "x.jpg");
        let src = part(&p, &t, b"MZ this is an exe");
        assert!(p.commit(&src, &t).is_err());
        assert!(src.exists(), "a failed commit leaves the part where it was");
        assert!(!root.join("x.jpg").exists());
    }

    #[test]
    fn bin_round_trip_relocate_and_purge_are_idempotent() {
        let (tmp, p, root) = rig();
        let mut t = inbox(1, "x.jpg");
        let placed = p.commit(&part(&p, &t, &jpeg(1, 64)), &t).unwrap();
        t.current = Some(placed.clone());
        // To the Bin.
        let binned = p.to_bin(&t).unwrap();
        assert_eq!(binned, tmp.path().join("bin").join("00000000-0000-0000-0000-000000000001.jpg"));
        assert!(binned.exists() && !placed.exists());
        let mut tb = t.clone();
        tb.current = Some(binned.clone());
        assert_eq!(p.to_bin(&tb).unwrap(), binned, "to_bin twice is fine");
        // Back out of the Bin, under a free name when the old one is taken.
        std::fs::write(root.join("x.jpg"), b"someone put a new file there").unwrap();
        let back = p.from_bin(&tb).unwrap();
        assert_eq!(back, root.join("x (2).jpg"));
        assert!(!binned.exists());
        let mut tl = t.clone();
        tl.current = Some(back.clone());
        assert_eq!(p.from_bin(&tl).unwrap(), back, "from_bin twice is fine");
        // Folder change: relocate moves the file into the folder's directory.
        let work = FolderId(uuid::Uuid::from_u128(0x100));
        tl.folder = work;
        tl.folder_kind = FolderKind::User;
        tl.folder_name = "Work".into();
        let moved = p.relocate(&tl).unwrap();
        assert_eq!(moved, root.join("Work").join("x.jpg"));
        assert_eq!(p.relocate(&FileTarget { current: Some(moved.clone()), ..tl.clone() }).unwrap(), moved, "relocate twice is fine");
        // Purge removes the file and is idempotent.
        let tm = FileTarget { current: Some(moved.clone()), ..tl };
        p.purge(&tm).unwrap();
        assert!(!moved.exists());
        p.purge(&tm).unwrap();
        // A file that is nowhere is an error, never silent.
        assert!(p.to_bin(&inbox(5, "nothing.jpg")).is_err());
        assert!(p.from_bin(&inbox(5, "nothing.jpg")).is_err());
    }

    #[test]
    fn a_failed_add_never_deletes_the_users_own_file() {
        let (_t, p, root) = rig();
        let own = root.join("mine.jpg");
        std::fs::write(&own, jpeg(1, 64)).unwrap();
        let mut t = inbox(1, "mine.jpg");
        // adopt() of a file already in the library returns it unchanged ...
        assert_eq!(p.adopt(&own, &t).unwrap(), own);
        // ... and Core's cleanup after a failed record write calls purge on it: it must survive.
        t.current = Some(own.clone());
        p.purge(&t).unwrap();
        assert!(own.exists(), "the user's file was not deleted");
    }

    #[test]
    fn adopting_a_file_from_outside_copies_it_in_and_leaves_the_original() {
        let (tmp, p, root) = rig();
        let src = tmp.path().join("camera.jpg");
        std::fs::write(&src, jpeg(5, 64)).unwrap();
        let t = inbox(1, "camera.jpg");
        let dest = p.adopt(&src, &t).unwrap();
        assert_eq!(dest, root.join("camera.jpg"));
        assert!(src.exists() && dest.exists());
        assert!(std::fs::read_dir(root.join(".tugboat")).unwrap().next().is_none(), "no staging file left behind");
    }

    #[test]
    fn free_space_is_a_real_number_and_no_root_is_an_error() {
        let (_t, p, _root) = rig();
        assert!(p.available_space().unwrap() > 0);
        let none = PcLibrary::new(None, PathBuf::from("bin"), Default::default());
        assert!(none.available_space().is_err());
        assert!(none.part_path(&BlobHash([1; 32])).is_err());
    }

    #[test]
    fn move_across_replaces_only_when_told_to() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.jpg"), d.path().join("b.jpg"));
        std::fs::write(&a, b"AAA").unwrap();
        std::fs::write(&b, b"BBB").unwrap();
        let e =crate::library::placement::move_across(&a, &b, false).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e}");
        assert_eq!(std::fs::read(&a).unwrap(), b"AAA");
        assert_eq!(std::fs::read(&b).unwrap(), b"BBB", "someone else's file survives");
       crate::library::placement::move_across(&a, &b, true).unwrap();
        assert_eq!(std::fs::read(&b).unwrap(), b"AAA");
        assert!(!a.exists());
    }

    #[test]
    fn a_missing_file_is_found_at_its_new_place_only_when_the_bytes_are_its_own() {
        let (_t, p, root) = rig();
        let work = FolderId(uuid::Uuid::from_u128(0x100));
        let mut t = target(1, "x.jpg", work, FolderKind::User, "Work");
        t.current = Some(root.join("gone.jpg"));
        let dir = root.join("Work");
        std::fs::create_dir_all(&dir).unwrap();
        // Another item's (or the user's) file with the wanted name: never recorded for this item.
        std::fs::write(dir.join("x.jpg"), jpeg(9, 64)).unwrap();
        assert!(p.relocate(&t).is_err());
        assert_eq!(std::fs::read(dir.join("x.jpg")).unwrap(), jpeg(9, 64), "the other file is untouched");
        // The same bytes: the watcher moved it already (C6).
        t.hash = tugboat_core::ops::hash_file(&dir.join("x.jpg")).unwrap().0;
        assert_eq!(p.relocate(&t).unwrap(), dir.join("x.jpg"));
    }

    #[test]
    fn a_locked_bin_file_makes_from_bin_fail_cleanly_and_retry_without_copies() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_t, p, root) = rig();
        let t = inbox(1, "x.jpg");
        let placed = p.commit(&part(&p, &t, &jpeg(1, 64)), &t).unwrap();
        let binned = p.to_bin(&FileTarget { current: Some(placed), ..t.clone() }).unwrap();
        let tb = FileTarget { current: Some(binned.clone()), ..t.clone() };
        // Readers may copy it, nobody may delete it.
        let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&binned).unwrap();
        assert!(p.from_bin(&tb).is_err());
        assert!(binned.exists(), "the Bin copy is intact");
        assert!(!root.join("x.jpg").exists() && !root.join("x (2).jpg").exists(), "no restored copy is left in the library");
        assert!(std::fs::read_dir(root.join(".tugboat")).unwrap().next().is_none(), "no staging file left");
        drop(held);
        assert_eq!(p.from_bin(&tb).unwrap(), root.join("x.jpg"), "the retry gets the plain name");
        assert!(!binned.exists());
    }

    #[test]
    fn a_locked_old_file_makes_commit_fail_with_the_part_and_the_old_file_as_they_were() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_t, p, root) = rig();
        let first = inbox(1, "old.jpg");
        let old = p.commit(&part(&p, &first, &jpeg(1, 64)), &first).unwrap();
        // The same item, new content under a new name: the old file has to go.
        let renamed = FileTarget { current: Some(old.clone()), ..inbox(1, "new.jpg") };
        let pt = part(&p, &renamed, &jpeg(2, 64));
        let held = std::fs::OpenOptions::new().read(true).share_mode(1).open(&old).unwrap();
        assert!(p.commit(&pt, &renamed).is_err());
        assert!(pt.exists(), "the part is back where it was");
        assert!(!root.join("new.jpg").exists(), "no unrecorded copy in the library");
        assert_eq!(std::fs::read(&old).unwrap(), jpeg(1, 64));
        drop(held);
        assert_eq!(p.commit(&pt, &renamed).unwrap(), root.join("new.jpg"));
        assert!(!old.exists(), "the old file went with the successful commit");
    }

    #[test]
    fn recent_sets_expire_by_path_ignoring_case() {
        let (_t, p, root) = rig();
        let t = inbox(1, "x.jpg");
        let placed = p.commit(&part(&p, &t, &jpeg(1, 64)), &t).unwrap();
        assert!(p.recently_arrived(&placed));
        assert!(p.recently_arrived(Path::new(&placed.to_string_lossy().to_uppercase())), "Windows paths ignore case");
        assert!(!p.recently_arrived(&root.join("other.jpg")));
        assert!(!p.recently_departed(&placed));
    }
}

// ---------- changes made on the other device reach the disk ----------

#[test]
fn a_move_made_on_the_other_device_moves_the_file_in_the_library_folder() {
    let mut r = Rig::new();
    r.write("a.jpg", 1);
    r.settle();
    let id = r.item("a.jpg").id;
    // The phone moves the item to a folder named Work (remote changes arrive as record changes only).
    let work = r.core.create_folder("Work", Retention::Keep).unwrap();
    r.core.move_item(id, work).unwrap(); // Core's own relocate already moved it ...
    std::fs::rename(r.root.join("Work").join("a.jpg"), r.root.join("a.jpg")).unwrap(); // ... undo it on disk to mimic a record-only (remote) change
    r.core.local_file_moved(id, r.root.join("a.jpg"), work, "a.jpg").unwrap(); // file_ref back at the root, record says Work
    let rep = r.scan(1);
    assert_eq!(rep.realigned, 1, "{rep:?}");
    assert_eq!(rep.moved, 0, "not a user move");
    assert!(r.root.join("Work").join("a.jpg").exists() && !r.root.join("a.jpg").exists());
    let it = r.item("a.jpg");
    assert_eq!(it.id, id);
    assert_eq!(it.folder, work, "the record kept its folder");
    assert_eq!(it.path.as_deref().map(Path::new), Some(r.root.join("Work").join("a.jpg").as_path()));
    // Quiet afterwards, and the move did not reset anything on the item.
    let again = r.scan(5);
    assert_eq!((again.realigned, again.moved, again.imported, again.binned), (0, 0, 0, 0));
}

#[test]
fn a_rename_made_on_the_other_device_renames_the_file_and_a_clash_suffix_is_not_a_rename() {
    use super::reconcile::name_matches;
    assert!(name_matches("IMG_1.jpg", "jpg", "IMG_1.jpg"));
    assert!(name_matches("IMG_1.jpg", "jpg", "img_1 (2).JPG"), "disambiguation suffix and case");
    assert!(!name_matches("IMG_2.jpg", "jpg", "IMG_1.jpg"), "renamed elsewhere");
    assert!(!name_matches("Trip (2).jpg", "jpg", "Trip.jpg"));
    assert!(name_matches("Trip (2).jpg", "jpg", "Trip (2).jpg"));
    assert!(name_matches("a:b.jpg", "jpg", "IMG_20261117_140233.jpg"), "an uncleanable record name got a generated disk name");

    let mut r = Rig::new();
    r.write("old.jpg", 1);
    r.settle();
    let id = r.item("old.jpg").id;
    // Record-only rename (what a remote change does): bypass the placement by renaming then pointing back.
    r.core.rename_item(id, "fresh.jpg").unwrap();
    std::fs::rename(r.root.join("fresh.jpg"), r.root.join("old.jpg")).unwrap();
    r.core.local_file_moved(id, r.root.join("old.jpg"), INBOX_ID, "fresh.jpg").unwrap();
    let rep = r.scan(1);
    assert_eq!(rep.realigned, 1, "{rep:?}");
    assert!(r.root.join("fresh.jpg").exists() && !r.root.join("old.jpg").exists());
    assert_eq!(r.item("fresh.jpg").id, id);
}

#[test]
fn the_watcher_keeps_working_after_the_library_folder_goes_away_and_returns() {
    let r = Rig::new();
    let opts = LibraryOptions { stable_for: Duration::from_millis(150), debounce: Duration::from_millis(100), max_debounce: Duration::from_secs(1), rescan_every: Duration::from_secs(600) };
    let lib = Library::open(r.core.clone(), r.place.clone(), r.data.clone(), LibraryConfig::load(&r.data).unwrap(), &r.root, opts, Box::new(|_| {})).unwrap();
    let worker = lib.start();
    let wait = |what: &str, f: &dyn Fn() -> bool| {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    std::thread::sleep(Duration::from_millis(400));
    let away = r.root.with_file_name("lib-away");
    std::fs::rename(&r.root, &away).unwrap();
    worker.request_scan(false);
    wait("paused", &|| matches!(lib.status().health, LibraryHealth::Missing { .. }));
    std::fs::rename(&away, &r.root).unwrap();
    worker.request_scan(false);
    wait("resumed", &|| lib.status().health == LibraryHealth::Ready);
    // A drop after the return is picked up by events, with the 10-minute rescan far away.
    r.write("after.jpg", 3);
    wait("the file dropped after the return is imported", &|| r.items().iter().any(|i| i.name == "after.jpg"));
    drop(worker);
}

/// Measurement, not a pass/fail rule: `cargo test -p tugboat-desktop --lib measure -- --ignored --nocapture`.
#[test]
#[ignore = "measurement: prints timings"]
fn measure_import_and_steady_state_scan_of_1000_files() {
    let mut r = Rig::new();
    for i in 0..1000u32 {
        let p = r.root.join(format!("p{i:04}.jpg"));
        std::fs::write(&p, jpeg((i % 250) as u8, 2000 + i as usize)).unwrap();
    }
    r.scan(0);
    let t = Instant::now();
    let rep = r.scan(3);
    eprintln!("MEASURE import of {} files: {:?} ({:?} per file)", rep.imported, t.elapsed(), t.elapsed() / rep.imported.max(1));
    let t = Instant::now();
    let rep = r.scan(5);
    eprintln!("MEASURE steady-state scan (cached ids), 1000 files: {:?} {rep:?}", t.elapsed());
    let t = Instant::now();
    let rep = r.lib.scan_at(true, r.now + Duration::from_secs(9));
    eprintln!("MEASURE full rescan (re-reads every file id), 1000 files: {:?} {rep:?}", t.elapsed());
}

/// Library paths come from the webview: ".." must not hide that a folder is inside another.
#[test]
fn path_compare_collapses_dot_and_dot_dot() {
    use super::placement::{is_inside, norm, same_path};
    assert_eq!(norm(Path::new(r"C:\a\.\b\..\c")), r"c:\a\c");
    assert!(same_path(Path::new(r"C:\x\..\Data"), Path::new(r"c:/data")));
    assert!(is_inside(Path::new(r"C:\Data"), Path::new(r"C:\Other\..\Data\Bin")));
    assert!(!is_inside(Path::new(r"C:\Data"), Path::new(r"C:\Data\..\Datum")));
    // Never above the drive root.
    assert_eq!(norm(Path::new(r"C:\..\..\a")), r"c:\a");
}

// ---------- any file type and the Mark-of-the-Web (placement) ----------

mod any_file_placement {
    use super::*;
    use crate::winfs::{read_zone_mark, write_zone_mark, ZONE_TEXT};
    use tugboat_core::{placement::FileTarget, BlobHash, CoreError, Placement};

    fn rig() -> (tempfile::TempDir, PcLibrary, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        std::fs::create_dir_all(&root).unwrap();
        let p = PcLibrary::new(Some(root.clone()), tmp.path().join("bin"), Default::default());
        (tmp, p, root)
    }

    fn target(n: u128, name: &str, ext: &str) -> FileTarget {
        FileTarget {
            item: ItemId(uuid::Uuid::from_u128(n)),
            hash: BlobHash([n as u8; 32]),
            name: name.into(),
            ext: ext.into(),
            captured_at: 1_790_000_000_000,
            folder: INBOX_ID,
            folder_kind: FolderKind::Inbox,
            folder_name: "Inbox".into(),
            current: None,
        }
    }

    fn part(p: &PcLibrary, t: &FileTarget, bytes: &[u8]) -> PathBuf {
        let path = p.part_path(&t.hash).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_received_file_of_any_type_is_placed_under_its_own_name_and_marked_as_downloaded() {
        let (_t, p, root) = rig();
        for (n, name, ext) in [(1, "report.pdf", "pdf"), (2, "Makefile", ""), (3, "data.sqlite3", "sqlite3"), (4, "run.bat", "bat")] {
            let t = target(n, name, ext);
            let dest = p.commit(&part(&p, &t, b"some bytes"), &t).unwrap();
            assert_eq!(dest, root.join(name), "{name}");
            assert_eq!(read_zone_mark(&dest).as_deref(), Some(ZONE_TEXT), "{name} carries the mark");
            assert_eq!(std::fs::read(format!("{}:Zone.Identifier", dest.display())).unwrap(), b"[ZoneTransfer]\r\nZoneId=3\r\n", "byte exact");
        }
    }

    #[test]
    fn the_mark_goes_on_the_part_first_and_travels_with_the_file_however_it_is_put_down() {
        let (tmp, _p, root) = rig();
        let dir = root.clone();
        // Hard link (the normal way), the same-item replace (a rename over the old file) and the
        // copy that a cross-drive move falls back to: all keep the stream, so the file is never visible unmarked.
        let a = tmp.path().join("a.part");
        std::fs::write(&a, b"1").unwrap();
        write_zone_mark(&a).unwrap();
        let linked = super::super::placement::place_new(&a, &dir, "a.txt", None).unwrap();
        assert!(read_zone_mark(&linked).is_some(), "hard link");
        let b = tmp.path().join("b.part");
        std::fs::write(&b, b"2").unwrap();
        write_zone_mark(&b).unwrap();
        let replaced = super::super::placement::place_new(&b, &dir, "a.txt", Some(&linked)).unwrap();
        assert_eq!(std::fs::read(&replaced).unwrap(), b"2");
        assert!(read_zone_mark(&replaced).is_some(), "rename over the old file");
        let copy = tmp.path().join("c.copy");
        std::fs::copy(&replaced, &copy).unwrap();
        assert!(read_zone_mark(&copy).is_some(), "std::fs::copy (what move_across and the Bin restore use) carries the stream");
    }

    #[test]
    fn replacing_an_items_file_keeps_it_marked_and_the_mark_stays_through_the_bin_and_back() {
        let (_t, p, root) = rig();
        let mut t = target(1, "doc.docx", "docx");
        let first = p.commit(&part(&p, &t, b"v1"), &t).unwrap();
        t.current = Some(first.clone());
        t.hash = BlobHash([9; 32]);
        let second = p.commit(&part(&p, &t, b"v2 longer"), &t).unwrap();
        assert_eq!(second, first);
        assert_eq!(std::fs::read(&second).unwrap(), b"v2 longer");
        assert!(read_zone_mark(&second).is_some(), "the new content is marked too");
        let binned = p.to_bin(&t).unwrap();
        assert!(read_zone_mark(&binned).is_some(), "in the Bin");
        let mut tb = t.clone();
        tb.current = Some(binned);
        let back = p.from_bin(&tb).unwrap();
        assert_eq!(back, root.join("doc.docx"));
        assert!(read_zone_mark(&back).is_some(), "restored from the Bin");
    }

    #[test]
    fn a_users_own_file_adopted_in_place_or_copied_in_gets_no_mark() {
        let (tmp, p, root) = rig();
        let own = root.join("mine.txt");
        std::fs::write(&own, b"mine").unwrap();
        let t = target(1, "mine.txt", "txt");
        assert_eq!(p.adopt(&own, &t).unwrap(), own);
        assert!(read_zone_mark(&own).is_none());
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"x").unwrap();
        let t2 = target(2, "outside.txt", "txt");
        let dest = p.adopt(&outside, &t2).unwrap();
        assert!(read_zone_mark(&dest).is_none(), "copied in by the user: no mark");
        // A source that already had one (downloaded from the internet) keeps it through the copy.
        let marked = tmp.path().join("marked.txt");
        std::fs::write(&marked, b"x").unwrap();
        write_zone_mark(&marked).unwrap();
        let t3 = target(3, "marked.txt", "txt");
        assert!(read_zone_mark(&p.adopt(&marked, &t3).unwrap()).is_some());
    }

    #[test]
    fn a_drive_that_cannot_hold_the_mark_gets_nothing_in_the_library_and_the_part_stays() {
        let (_t, p, root) = rig();
        let t = target(1, "report.pdf", "pdf");
        let src = part(&p, &t, b"bytes");
        p.fail_marks.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = p.commit(&src, &t).unwrap_err();
        assert!(matches!(err, CoreError::Io { .. }), "{err:?}");
        assert!(src.exists(), "the part is kept for the next try");
        assert!(!root.join("report.pdf").exists(), "nothing appears unmarked");
        p.fail_marks.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(p.commit(&src, &t).unwrap().exists(), "and the retry works once the drive can mark");
    }

    #[test]
    fn a_rename_to_a_refused_name_is_refused_by_relocate_and_the_file_stays() {
        let (_t, p, root) = rig();
        let mut t = target(1, "notes.ini", "ini");
        let placed = p.commit(&part(&p, &t, b"[x]"), &t).unwrap();
        t.current = Some(placed.clone());
        t.name = "desktop".into();
        assert!(matches!(p.relocate(&t), Err(CoreError::Refused(_))));
        assert!(placed.exists() && !root.join("desktop.ini").exists());
    }

    #[test]
    fn this_drive_supports_named_streams_and_a_bad_path_is_an_error_not_a_yes() {
        let (_t, _p, root) = rig();
        assert!(crate::winfs::supports_named_streams(&root).unwrap());
        assert!(crate::winfs::supports_named_streams(Path::new(r"Z:\definitely\not\a\drive")).is_err());
    }

    #[test]
    fn a_name_the_pc_refuses_is_refused_visibly_and_left_alone() {
        let (_t, p, root) = rig();
        // The record name is harmless, the final disk name is not: desktop + ini.
        for (name, ext) in [("desktop", "ini"), ("x", "lnk"), ("a:b", "lnk"), ("x.url", "")] {
            let t = target(5, name, ext);
            let src = part(&p, &t, b"x");
            let err = p.commit(&src, &t).unwrap_err();
            assert!(matches!(err, CoreError::Refused(_)) || matches!(err, CoreError::Invalid(_)), "{name}.{ext}: {err:?}");
            assert!(src.exists(), "the part is left alone");
            std::fs::remove_file(&src).unwrap();
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().filter(|e| e.as_ref().unwrap().file_name() != ".tugboat").count(), 0);
    }
}

// ---------- drag and drop ----------

mod dropping {
    use super::*;
    use crate::{
        drop::{clean_stale, copy_dropped, DropReport, DropTarget, NativeDrop, FOLDERS_TEXT, NOTHING_TEXT},
        winfs::{read_zone_mark, write_zone_mark},
    };

    fn target_of(r: &Rig, name: &str) -> DropTarget {
        let (id, kind, name) = r.core.live_folders().unwrap().into_iter().find(|(_, _, n)| n == name).unwrap_or_else(|| panic!("no folder {name}"));
        DropTarget { id, kind, name }
    }

    fn inbox(r: &Rig) -> DropTarget {
        target_of(r, "Inbox")
    }

    fn src_dir(r: &Rig) -> PathBuf {
        let d = r._tmp.path().join("usb");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn src(r: &Rig, name: &str, bytes: &[u8]) -> PathBuf {
        let p = src_dir(r).join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn copy(r: &Rig, paths: &[PathBuf], t: &DropTarget) -> DropReport {
        copy_dropped(&r.lib, NativeDrop::from_native(paths), t, &mut |_| {})
    }

    fn mtime(p: &Path) -> std::time::SystemTime {
        std::fs::metadata(p).unwrap().modified().unwrap()
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn files_are_copied_into_the_inbox_and_into_a_viewed_folder_and_become_items() {
        let r = Rig::new();
        r.core.create_folder("Work", Retention::Keep).unwrap();
        let paths = [src(&r, "photo.jpg", &jpeg(1, 200)), src(&r, "report.pdf", b"%PDF-1.4 hello"), src(&r, "Makefile", b"all:")];
        let rep = copy(&r, &paths, &inbox(&r));
        assert_eq!(rep.folder_name, "Inbox");
        assert_eq!(rep.copied, ["photo.jpg", "report.pdf", "Makefile"], "{rep:?}");
        assert!(rep.refused.is_empty() && rep.failed.is_empty());
        assert_eq!(files_in(&r.root), ["Makefile", "photo.jpg", "report.pdf"]);
        let kinds: Vec<_> = r.items().into_iter().map(|i| (i.name, i.kind, i.folder)).collect();
        assert_eq!(kinds.len(), 3);
        assert!(kinds.iter().all(|(_, _, f)| *f == INBOX_ID));
        assert!(kinds.iter().any(|(n, k, _)| n == "photo.jpg" && *k == tugboat_core::MediaKind::Image));
        assert!(kinds.iter().any(|(n, k, _)| n == "report.pdf" && *k == tugboat_core::MediaKind::File));
        // Into Work: its own directory, its own folder id.
        let work = target_of(&r, "Work");
        let more = [src(&r, "two.pdf", b"%PDF-1.4 two")];
        let rep = copy(&r, &more, &work);
        assert_eq!((rep.folder_name.as_str(), rep.copied.as_slice()), ("Work", ["two.pdf".to_string()].as_slice()));
        assert!(r.root.join("Work").join("two.pdf").is_file());
        assert_eq!(r.item("two.pdf").folder, work.id);
        // The watcher does not import any of it a second time, nor bin it.
        r.lib.scan(true);
        let mut r = r;
        r.settle();
        assert_eq!(r.items().len(), 4);
        assert_eq!(r.deleted_count(), 0);
        assert!(r.lib.status().last_error.is_none());
    }

    #[test]
    fn a_name_that_exists_gets_a_unique_name_and_the_original_is_not_touched() {
        let r = Rig::new();
        let a = src(&r, "a.txt", b"first");
        assert_eq!(copy(&r, std::slice::from_ref(&a), &inbox(&r)).copied, ["a.txt"]);
        std::fs::write(&a, b"second").unwrap();
        assert_eq!(copy(&r, std::slice::from_ref(&a), &inbox(&r)).copied, ["a (2).txt"]);
        assert_eq!(copy(&r, &[a], &inbox(&r)).copied, ["a (3).txt"]);
        assert_eq!(std::fs::read(r.root.join("a.txt")).unwrap(), b"first");
        assert_eq!(std::fs::read(r.root.join("a (2).txt")).unwrap(), b"second");
        assert_eq!(r.items().len(), 3);
    }

    #[test]
    fn a_dropped_folder_copies_nothing_and_says_so_and_a_junction_counts_as_a_folder() {
        let r = Rig::new();
        let folder = src_dir(&r).join("Holiday");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("inside.jpg"), jpeg(1, 100)).unwrap();
        let rep = copy(&r, std::slice::from_ref(&folder), &inbox(&r));
        assert!(rep.copied.is_empty());
        assert_eq!(rep.refused.len(), 1);
        assert_eq!((rep.refused[0].name.as_str(), rep.refused[0].reason.as_str()), ("Holiday", FOLDERS_TEXT));
        assert!(files_in(&r.root).is_empty() && r.items().is_empty(), "nothing from inside it either");
        // A junction to a folder is a folder (mklink /J needs no privilege).
        let link = src_dir(&r).join("Link");
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&link).arg(&folder).output().map(|o| o.status.success()).unwrap_or(false);
        if made {
            let rep = copy(&r, std::slice::from_ref(&link), &inbox(&r));
            assert_eq!(rep.refused[0].reason, FOLDERS_TEXT);
            assert!(r.items().is_empty());
            let _ = std::fs::remove_dir(&link); // removes the junction, not its target
        }
        assert!(folder.join("inside.jpg").exists());
    }

    #[test]
    fn refused_names_virtual_drops_and_vanished_files_are_refused_with_a_reason_each() {
        let r = Rig::new();
        let ok = src(&r, "ok.txt", b"x");
        let lnk = src(&r, "shortcut.lnk", b"x");
        let ini = src(&r, "desktop.ini", b"x");
        let gone = src_dir(&r).join("gone.txt");
        let rep = copy(&r, &[lnk, ok, ini, gone], &inbox(&r));
        assert_eq!(rep.copied, ["ok.txt"]);
        let names: Vec<_> = rep.refused.iter().map(|l| (l.name.as_str(), l.reason.as_str())).collect();
        assert!(names.iter().any(|(n, why)| *n == "shortcut.lnk" && why.starts_with("REFUSED")), "{names:?}");
        assert!(names.iter().any(|(n, why)| *n == "desktop.ini" && why.starts_with("REFUSED")), "{names:?}");
        assert!(names.iter().any(|(n, why)| *n == "gone.txt" && why.starts_with("NOT FOUND")), "{names:?}");
        assert_eq!(files_in(&r.root), ["ok.txt"]);
        // An Outlook attachment or a zip entry has no path: nothing to copy, and it says so.
        let rep = copy(&r, &[], &inbox(&r));
        assert_eq!(rep.refused[0].reason, NOTHING_TEXT);
    }

    #[test]
    fn the_source_is_untouched_a_marked_source_keeps_its_mark_and_an_unmarked_one_gets_none() {
        let r = Rig::new();
        let plain = src(&r, "plain.txt", b"plain bytes");
        let marked = src(&r, "marked.txt", b"marked bytes");
        write_zone_mark(&marked).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(86_400 * 30);
        for f in [&plain, &marked] {
            std::fs::OpenOptions::new().write(true).open(f).unwrap().set_modified(old).unwrap();
        }
        let before: Vec<_> = [&plain, &marked].iter().map(|p| (std::fs::read(p).unwrap(), mtime(p))).collect();
        let rep = copy(&r, &[plain.clone(), marked.clone()], &inbox(&r));
        assert_eq!(rep.copied.len(), 2, "{rep:?}");
        let after: Vec<_> = [&plain, &marked].iter().map(|p| (std::fs::read(p).unwrap(), mtime(p))).collect();
        assert_eq!(before, after, "the source bytes and mtime did not change");
        assert!(read_zone_mark(&marked).is_some() && read_zone_mark(&plain).is_none(), "the sources' own marks are as they were");
        assert!(read_zone_mark(&r.root.join("plain.txt")).is_none(), "a copy the user made gets no mark");
        assert!(read_zone_mark(&r.root.join("marked.txt")).is_some(), "an existing mark comes along");
        assert_eq!(mtime(&r.root.join("plain.txt")), old, "the copy keeps the source's modified time");
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)] // Windows only: this clears the read-only attribute for the temp-dir cleanup
    fn a_hidden_read_only_source_gives_an_ordinary_copy_that_stays_an_item() {
        let mut r = Rig::new();
        let s = src(&r, "secret.txt", b"hidden one");
        crate::winfs::hide(&s).unwrap();
        let mut perm = std::fs::metadata(&s).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&s, perm).unwrap();
        let rep = copy(&r, std::slice::from_ref(&s), &inbox(&r));
        assert_eq!(rep.copied, ["secret.txt"], "{rep:?}");
        let meta = std::fs::metadata(r.root.join("secret.txt")).unwrap();
        assert!(!crate::winfs::is_hidden_or_system(&meta) && !meta.permissions().readonly(), "the copy is an ordinary file");
        // The scan must see it (a hidden file would be skipped and then counted as missing).
        r.settle();
        r.settle();
        assert_eq!(r.deleted_count(), 0);
        assert_eq!(r.item("secret.txt").local, LocalFileState::Present);
        let mut perm = std::fs::metadata(&s).unwrap().permissions();
        perm.set_readonly(false);
        std::fs::set_permissions(&s, perm).unwrap();
    }

    #[test]
    fn a_big_copy_never_shows_a_partial_file_in_the_library_and_reports_progress() {
        let r = Rig::new();
        const SIZE: usize = 96 * 1024 * 1024;
        let big = src_dir(&r).join("big.mp4");
        {
            let mut v = vec![0u8; SIZE];
            v[4..8].copy_from_slice(b"ftyp");
            v[8..12].copy_from_slice(b"isom");
            std::fs::write(&big, &v).unwrap();
        }
        let lib = r.lib.clone();
        let t = inbox(&r);
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let paths = vec![big.clone()];
        let worker = std::thread::spawn(move || copy_dropped(&lib, NativeDrop::from_native(&paths), &t, &mut |p| ev.lock().unwrap().push(p)));
        let mut saw_tmp = false;
        while !worker.is_finished() {
            for e in std::fs::read_dir(&r.root).unwrap().flatten() {
                let p = e.path();
                if p.is_file() {
                    assert_eq!(std::fs::metadata(&p).map(|m| m.len()).unwrap_or(SIZE as u64), SIZE as u64, "a partial file is visible: {}", p.display());
                }
            }
            if let Ok(rd) = std::fs::read_dir(r.root.join(".tugboat")) {
                saw_tmp |= rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with("drop-"));
            }
        }
        let rep = worker.join().unwrap();
        assert_eq!(rep.copied, ["big.mp4"], "{rep:?}");
        assert!(saw_tmp, "the copy was staged in the hidden folder");
        let ev = events.lock().unwrap();
        assert!(!ev.is_empty(), "progress was reported");
        assert!(ev.iter().all(|p| p.index == 1 && p.count == 1 && p.done_bytes <= p.total_bytes && p.total_bytes == SIZE as u64), "{:?}", ev.last());
        assert_eq!(ev.last().unwrap().done_bytes, SIZE as u64, "the last event is the whole file");
        assert!(std::fs::read_dir(r.root.join(".tugboat")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("drop-")), "no temp copy left");
    }

    #[test]
    fn a_missing_or_paused_library_takes_nothing() {
        let mut r = Rig::new();
        let f = src(&r, "a.txt", b"x");
        // Paused: more than ten percent of the files gone.
        let mut strict = Rig::strict();
        fill(&mut strict, 25);
        for i in 0..25 {
            std::fs::remove_file(strict.root.join(format!("p{i:02}.jpg"))).unwrap();
        }
        strict.scan(1);
        let f2 = src(&strict, "b.txt", b"x");
        let rep = copy(&strict, &[f2], &inbox(&strict));
        assert!(rep.copied.is_empty());
        assert_eq!(rep.refused[0].reason, "PAUSED - nothing copied");
        // Missing: the folder is gone.
        let away = r.root.with_file_name("lib-away");
        std::fs::rename(&r.root, &away).unwrap();
        let rep = copy(&r, std::slice::from_ref(&f), &inbox(&r));
        assert!(rep.copied.is_empty());
        assert_eq!(rep.refused[0].reason, "LIBRARY NOT FOUND - nothing copied");
        assert!(f.exists());
        std::fs::rename(&away, &r.root).unwrap();
        r.settle();
        assert_eq!(copy(&r, &[f], &inbox(&r)).copied, ["a.txt"], "and it works again once the library is back");
    }

    #[test]
    fn a_failed_add_removes_the_copy_and_lists_the_file_as_failed() {
        let r = Rig::new();
        let fake = src(&r, "fake.jpg", b"MZ this is a program, not a photo");
        let rep = copy(&r, &[fake], &inbox(&r));
        assert!(rep.copied.is_empty(), "{rep:?}");
        assert_eq!(rep.failed.len(), 1, "{rep:?}");
        assert!(rep.failed[0].reason.starts_with("COPY FAILED"), "{:?}", rep.failed);
        assert!(files_in(&r.root).is_empty(), "the copy was removed again: nothing the next scan could adopt");
        assert!(std::fs::read_dir(r.root.join(".tugboat")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("drop-")));
        assert!(r.items().is_empty());
    }

    #[test]
    fn unfinished_copies_left_by_a_quit_are_removed_at_start_and_nothing_else_is() {
        let r = Rig::new();
        let hidden = r.root.join(".tugboat");
        std::fs::write(hidden.join("drop-0123.tmp"), b"half").unwrap();
        std::fs::write(hidden.join("DROP-ABCD.TMP"), b"half").unwrap();
        std::fs::write(hidden.join("0123.part"), b"a download").unwrap();
        assert_eq!(clean_stale(&r.root), 2);
        assert!(hidden.join("0123.part").exists() && hidden.join("library-id").exists());
        assert_eq!(clean_stale(&r.root), 0);
    }

    #[test]
    fn a_drop_during_a_scan_makes_exactly_one_item() {
        let r = Rig::new();
        let lib = r.lib.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let scanner = std::thread::spawn(move || {
            let mut now = Instant::now();
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                now += Duration::from_secs(3);
                lib.scan_at(false, now);
            }
        });
        for i in 0..8 {
            let rep = copy(&r, &[src(&r, &format!("f{i}.txt"), format!("file {i}").as_bytes())], &inbox(&r));
            assert_eq!(rep.copied.len(), 1, "{rep:?}");
        }
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        scanner.join().unwrap();
        assert_eq!(r.items().len(), 8, "one item per file, none twice: {:?}", r.items().iter().map(|i| &i.name).collect::<Vec<_>>());
        assert_eq!(r.deleted_count(), 0);
    }

    #[test]
    fn a_source_inside_the_library_is_copied_under_a_new_name_and_left_alone() {
        let mut r = Rig::new();
        let own = r.root.join("a.txt");
        std::fs::write(&own, b"mine").unwrap();
        let rep = copy(&r, std::slice::from_ref(&own), &inbox(&r));
        assert_eq!(rep.copied, ["a (2).txt"], "{rep:?}");
        assert_eq!(std::fs::read(&own).unwrap(), b"mine");
        r.settle();
        r.settle();
        assert_eq!(r.items().len(), 2, "the original is imported by the scan, the copy by the drop");
    }

    #[test]
    fn expiry_runs_from_the_copy_not_from_the_sources_age() {
        let r = Rig::new();
        let old = src(&r, "old.txt", b"two years old");
        std::fs::OpenOptions::new().write(true).open(&old).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(86_400 * 730)).unwrap();
        assert_eq!(copy(&r, &[old], &inbox(&r)).copied, ["old.txt"]);
        let it = r.item("old.txt");
        assert!(matches!(it.state, ItemState::Live { .. }), "a year-old source is not already expired: {:?}", it.state);
        assert!(r.bin().is_empty());
    }

    /// The "no path from the page" guarantee: the window's own event is the only source of copy paths.
    #[test]
    fn only_the_shell_and_the_drop_module_build_a_native_drop() {
        for (name, text) in [
            ("commands.rs", include_str!("../commands.rs")),
            ("backend.rs", include_str!("../backend.rs")),
            ("lib.rs", include_str!("../lib.rs")),
            ("library/mod.rs", include_str!("mod.rs")),
            ("library/reconcile.rs", include_str!("reconcile.rs")),
            ("library/placement.rs", include_str!("placement.rs")),
            ("library/worker.rs", include_str!("worker.rs")),
        ] {
            assert!(!text.contains("from_native("), "{name} builds a NativeDrop");
        }
        assert!(include_str!("../shell.rs").contains("NativeDrop::from_native(paths)"));
    }
}
