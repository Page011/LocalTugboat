//! Adversarial tests for the PC library (verifier stage). Each test states a rule from PLAN.md or
//! from the module's own doc comments and tries to break it with a real temp folder and a real
//! Core. A failing test here is a finding, not a broken test.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use tugboat_core::{facade::device_id_from_secret, ActivityAction, Core, CoreConfig, ItemQuery, ItemView, Role, Settings, SystemClock};
use tugboat_desktop_lib::library::{config::LibraryConfig, placement::PcLibrary, Library, LibraryHealth, LibraryOptions};

fn jpeg(seed: u8, len: usize) -> Vec<u8> {
    let mut v = vec![seed; len.max(8)];
    v[..4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
    v
}

struct Rig {
    core: Core,
    lib: Library,
    root: PathBuf,
    data: PathBuf,
    now: Instant,
    /// Last on purpose: fields drop in declaration order, and the temp folder must be removed
    /// after Core's SQLite handle and the library are closed (Windows cannot delete open files).
    tmp: tempfile::TempDir,
}

impl Rig {
    /// Real defaults (20 files / 10 percent), data and Bin laid out like the real app
    /// (Bin under the data folder, as paths.rs does).
    fn strict() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let root = tmp.path().join("lib");
        let place = Arc::new(PcLibrary::new(None, data.join("Bin"), Default::default()));
        let secret = [9u8; 32];
        let cfg = CoreConfig { data_dir: data.clone(), role: Role::Pc, self_id: device_id_from_secret(&secret), device_name: "PC".into(), app_version: "test".into() };
        let (core, _rx) = Core::open(cfg, Arc::new(SystemClock), place.clone()).unwrap();
        let lib = Library::open(core.clone(), place, data.clone(), LibraryConfig::default(), &root, LibraryOptions::default(), Box::new(|_| {})).unwrap();
        Rig { tmp, core, lib, root, data, now: Instant::now() }
    }

    /// Percent rule off, so single deletes in a small library are ordinary deletes.
    fn loose() -> Rig {
        let r = Rig::strict();
        r.core.save_settings(&Settings { missing_pause_pct: 100, ..Default::default() }).unwrap();
        r
    }

    fn scan(&mut self, secs: u64) -> tugboat_desktop_lib::library::PassReport {
        self.now += Duration::from_secs(secs);
        self.lib.scan_at(false, self.now)
    }

    fn settle(&mut self) {
        self.scan(0);
        self.scan(3);
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

    fn deleted_count(&self) -> usize {
        self.core.activity(None, 500).unwrap().into_iter().filter(|a| a.action == ActivityAction::Deleted).count()
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

/// PLAN: "Library root or its identity marker missing (... folder moved or renamed): sync
/// pauses ... [Locate]". A library moved to another drive in Explorer is a copy + delete, so
/// every file has a new NTFS id. After Locate the same items must be found again, not imported
/// a second time (the phone would get every photo twice and the old records would be held
/// forever at paths that do not exist).
#[test]
fn locate_after_the_library_was_copied_to_a_new_place_does_not_duplicate_every_item() {
    let mut r = Rig::loose();
    r.write("a.jpg", 1);
    r.write("b.jpg", 2);
    r.write("Work/c.jpg", 3);
    r.settle();
    assert_eq!(r.items().len(), 3);

    let moved = r.tmp.path().join("other-drive").join("lib");
    copy_tree(&r.root, &moved);
    std::fs::remove_dir_all(&r.root).unwrap();
    r.scan(1);
    assert!(matches!(r.lib.status().health, LibraryHealth::Missing { .. }));

    r.lib.locate(&moved).unwrap();
    r.settle();
    r.settle();
    let names: Vec<String> = r.items().into_iter().map(|i| i.name).collect();
    assert_eq!(names.len(), 3, "items after Locate: {names:?}");
    assert_eq!(r.deleted_count(), 0);
    for h in r.core.held_files().unwrap() {
        assert!(Path::new(&h.path).exists(), "held file points at a path that does not exist: {}", h.path);
    }
}

/// The bulk prompt names a count ("25 FILES MISSING - move them to the Bin or keep waiting").
/// [Move them to the Bin] must bin what the user saw, not whatever is missing by the time the
/// button is pressed: here the rest of the library vanishes between the prompt and the click
/// (a failing drive, a sync tool), and the click bins all of it without asking again.
#[test]
fn move_to_bin_bins_only_the_count_the_user_confirmed() {
    let mut r = Rig::strict();
    for i in 0..30u8 {
        r.write(&format!("p{i:02}.jpg"), i);
    }
    r.settle();
    assert_eq!(r.items().len(), 30);
    for i in 0..25 {
        std::fs::remove_file(r.root.join(format!("p{i:02}.jpg"))).unwrap();
    }
    r.scan(1);
    assert_eq!(r.lib.status().health, LibraryHealth::BulkMissing { missing: 25, total: 30 });
    // Before the click: the other 5 vanish too.
    for i in 25..30 {
        std::fs::remove_file(r.root.join(format!("p{i:02}.jpg"))).unwrap();
    }
    let _ = r.lib.resolve_missing(true);
    assert!(r.deleted_count() <= 25, "the user confirmed 25 files, {} were deleted", r.deleted_count());
}

/// choose() refuses a library inside the app's data folder ("NOT HERE - the library cannot hold
/// the app's own data"). The check compares strings, so the same folder spelled with ".." gets
/// through. The path comes from the webview (library_choose).
#[test]
fn choose_refuses_the_data_folder_even_when_spelled_with_dot_dot() {
    let r = Rig::loose();
    // Not through the library folder itself (that trips the overlap rule by accident).
    let sneaky = r.tmp.path().join("elsewhere").join("..").join("data").join("Bin");
    assert!(!sneaky.exists() || std::fs::read_dir(&sneaky).unwrap().next().is_none(), "precondition: Bin missing or empty");
    let res = r.lib.choose(&sneaky, false);
    assert!(res.is_err(), "the Bin folder was accepted as the library: {:?}", r.lib.root());
}

/// Same string-compare gap for the overlap rule: a new library nested inside the current one.
#[test]
fn choose_refuses_a_folder_inside_the_current_library_even_when_spelled_with_dot_dot() {
    let r = Rig::loose();
    let nested = r.tmp.path().join("elsewhere").join("..").join("lib").join("Nested");
    let res = r.lib.choose(&nested, false);
    assert!(res.is_err(), "a library nested in the current one was accepted: {:?}", r.lib.root());
}

/// PLAN: "Library root ... missing (USB drive unplugged ...)". If that drive is gone for good,
/// choosing a new empty library is the only way out (Locate needs the old marker). choose()
/// copies every held file first and fails on the first one it cannot read, so it can never
/// succeed: the app stays paused forever.
#[test]
fn a_new_library_can_be_chosen_after_the_old_drive_is_gone_for_good() {
    let mut r = Rig::loose();
    r.write("a.jpg", 1);
    r.settle();
    std::fs::remove_dir_all(&r.root).unwrap(); // the USB stick died
    r.scan(1);
    assert!(matches!(r.lib.status().health, LibraryHealth::Missing { .. }));
    let fresh = r.tmp.path().join("fresh");
    let res = r.lib.choose(&fresh, false);
    assert!(res.is_ok(), "no way out of a dead library drive: {res:?}");
}

/// PLAN: "rename = rename". A case-only rename in Explorer (IMG_1.jpg -> img_1.jpg) is a rename.
#[test]
fn a_case_only_rename_in_explorer_renames_the_item() {
    let mut r = Rig::loose();
    r.write("IMG_1.jpg", 1);
    r.settle();
    std::fs::rename(r.root.join("IMG_1.jpg"), r.root.join("img_1.jpg")).unwrap();
    r.scan(1);
    let names: Vec<String> = r.items().into_iter().map(|i| i.name).collect();
    assert_eq!(names, ["img_1.jpg"]);
}

/// PLAN: "delete = Bin" and "Restoring from the Bin ... If only the other device still has the
/// file, it is fetched from there". A file deleted in Explorer must be listed in the Bin so it
/// can be restored; the Bin screen otherwise says "Bin is empty." while an item was deleted.
#[test]
fn a_file_deleted_in_explorer_is_listed_in_the_bin() {
    let mut r = Rig::loose();
    r.write("gone.jpg", 1);
    r.write("stays.jpg", 2);
    r.settle();
    std::fs::remove_file(r.root.join("gone.jpg")).unwrap();
    assert_eq!(r.scan(1).binned, 1);
    assert_eq!(r.deleted_count(), 1);
    let bin: Vec<String> = r.bin().into_iter().map(|i| i.name).collect();
    assert_eq!(bin, ["gone.jpg"], "Bin page after an Explorer delete");
}

/// Explorer allows renaming a subfolder to ".Work". The walk skips dot-named directories
/// silently, so the folder looks deleted: its items go to the Bin on both devices and the folder
/// is deleted, while the files sit untouched (and unlisted) in ".Work". A rename must never
/// become a delete.
#[test]
fn renaming_a_subfolder_to_a_dot_name_does_not_delete_its_items() {
    let mut r = Rig::loose();
    r.write("Work/a.jpg", 1);
    r.write("c.jpg", 3);
    r.settle();
    assert_eq!(r.items().len(), 2);
    std::fs::rename(r.root.join("Work"), r.root.join(".Work")).unwrap();
    r.scan(1);
    r.scan(1);
    assert!(r.root.join(".Work").join("a.jpg").exists(), "the file is still on disk");
    assert_eq!(r.deleted_count(), 0, "a rename was turned into a delete");
}

/// choose() reports old files it could not delete ("1 OLD FILES NOT DELETED in the previous
/// library folder"), but Backend::library_choose restarts the worker straight away and its first
/// pass overwrites last_error with None, so the user never sees it.
#[test]
fn the_old_files_not_deleted_warning_survives_the_next_scan() {
    use std::os::windows::fs::OpenOptionsExt;
    let mut r = Rig::loose();
    let p = r.write("a.jpg", 1);
    r.settle();
    // Another program holds the old file without delete sharing (an image viewer).
    let _holder = std::fs::OpenOptions::new().read(true).share_mode(1).open(&p).unwrap();
    let fresh = r.tmp.path().join("fresh");
    r.lib.choose(&fresh, false).unwrap();
    let before = r.lib.status().last_error;
    assert!(before.as_deref().is_some_and(|e| e.contains("OLD FILES NOT DELETED")), "{before:?}");
    r.scan(1); // what the restarted worker does at once
    let after = r.lib.status().last_error;
    assert!(after.as_deref().is_some_and(|e| e.contains("OLD FILES NOT DELETED")), "warning lost after one scan: {after:?}");
}

/// Sanity: the rig itself behaves like the real defaults (guards the other tests' setup).
#[test]
fn rig_sanity_the_data_dir_is_refused_when_spelled_plainly() {
    let r = Rig::loose();
    assert!(r.lib.choose(&r.data.join("Bin"), false).is_err());
}
