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
fn non_media_and_mismatched_files_are_listed_never_ignored() {
    let mut r = Rig::new();
    std::fs::write(r.root.join("notes.txt"), b"hello").unwrap();
    std::fs::create_dir(r.root.join("Work")).unwrap();
    std::fs::write(r.root.join("Work").join("plan.docx"), b"x").unwrap();
    std::fs::write(r.root.join("fake.jpg"), b"this is not a jpeg at all").unwrap();
    // Windows' own files are not the user's.
    let hidden = r.root.join("desktop.ini");
    std::fs::write(&hidden, b"[x]").unwrap();
    let _ = crate::winfs::hide(&hidden);
    r.settle();
    r.settle();
    let s = r.lib.status();
    let reasons: Vec<(String, String)> = s.not_synced.iter().map(|n| (n.name.clone(), n.reason.clone())).collect();
    assert!(reasons.iter().any(|(n, why)| n == "notes.txt" && why.starts_with("NOT A PHOTO OR VIDEO")), "{reasons:?}");
    assert!(reasons.iter().any(|(n, _)| n == "plan.docx"), "files in subfolders too: {reasons:?}");
    assert!(reasons.iter().any(|(n, why)| n == "fake.jpg" && why.contains("does not match")), "{reasons:?}");
    assert!(!reasons.iter().any(|(n, _)| n == "desktop.ini"), "hidden files are not listed");
    assert!(r.items().is_empty());
    assert!(s.last_error.is_none(), "a rejected file is a listing, not an error: {:?}", s.last_error);
    // Judged once: changing the file's content makes it get judged again.
    std::fs::write(r.root.join("fake.jpg"), jpeg(4, 64)).unwrap();
    r.settle();
    assert_eq!(r.item("fake.jpg").kind, tugboat_core::MediaKind::Image);
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
    std::fs::write(r.root.join("x.txt"), b"1").unwrap();
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
    // A manual rescan request works too.
    std::fs::write(r.root.join("note.txt"), b"x").unwrap();
    worker.request_scan(true);
    wait("the rescan lists the text file", &|| lib.status().not_synced.iter().any(|n| n.name == "note.txt"));
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
        // The extension comes from a synced record: only the allowlist may become part of a path.
        let mut t = inbox(8, "x.jpg");
        t.ext = "exe".into();
        assert!(p.to_bin(&t).is_err());
        assert!(p.commit(&part(&p, &inbox(8, "x.jpg"), &jpeg(1, 64)), &t).is_err());
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
