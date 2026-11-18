//! Adversarial tests for the 0.2.0 PC library: any file type, Explorer renames, Mark-of-the-Web
//! placement and drag-and-drop copies. Each one tries to break a rule of the library; a failing
//! test means the rule does not hold.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use tugboat_core::{
    facade::device_id_from_secret, names, BlobHash, Core, CoreConfig, FileTarget, FolderKind, ItemId, ItemQuery, ItemView, Placement, Role, Settings,
    SystemClock, INBOX_ID,
};
use tugboat_desktop_lib::{
    drop::{copy_dropped, DropReport, DropTarget, NativeDrop},
    library::{config::LibraryConfig, placement::PcLibrary, Library, LibraryOptions},
};

struct Rig {
    tmp: tempfile::TempDir,
    core: Core,
    lib: Library,
    root: PathBuf,
    now: Instant,
}

impl Rig {
    fn loose() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let root = tmp.path().join("lib");
        let place = Arc::new(PcLibrary::new(None, data.join("Bin"), Default::default()));
        let secret = [11u8; 32];
        let cfg = CoreConfig { data_dir: data.clone(), role: Role::Pc, self_id: device_id_from_secret(&secret), device_name: "PC".into(), app_version: "test".into() };
        let (core, _rx) = Core::open(cfg, Arc::new(SystemClock), place.clone()).unwrap();
        let lib = Library::open(core.clone(), place, data.clone(), LibraryConfig::default(), &root, LibraryOptions::default(), Box::new(|_| {})).unwrap();
        core.save_settings(&Settings { missing_pause_pct: 100, ..Default::default() }).unwrap();
        Rig { tmp, core, lib, root, now: Instant::now() }
    }

    fn scan(&mut self, secs: u64) {
        self.now += Duration::from_secs(secs);
        self.lib.scan_at(false, self.now);
    }

    fn settle(&mut self) {
        self.scan(0);
        self.scan(3);
        self.scan(3);
    }

    fn items(&self) -> Vec<ItemView> {
        self.core.items(&ItemQuery { limit: 500, ..Default::default() }).unwrap().rows
    }

    fn src(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let d = self.tmp.path().join("usb");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn drop_in_inbox(&self, paths: &[PathBuf]) -> DropReport {
        let t = DropTarget { id: INBOX_ID, kind: FolderKind::Inbox, name: "Inbox".into() };
        copy_dropped(&self.lib, NativeDrop::from_native(paths), &t, &mut |_| {})
    }

    fn files_in_root(&self) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(&self.root).unwrap().flatten().filter(|e| e.path().is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    fn drop_temps(&self) -> usize {
        std::fs::read_dir(self.root.join(".tugboat"))
            .map(|d| d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("drop-")).count())
            .unwrap_or(0)
    }
}

/// A rename in Explorer is a rename. A plain file with no extension by the `ext_of` rule ("Dr. Smith letter":
/// the text after the last dot has a space) is renamed in Explorer to "Dr. Smith letter 2". The
/// record must take the new name and the file must stay where the user put it. `moved()` builds
/// the new name with Rust's `file_stem` ("Dr") instead of the `ext_of` rule, so the record becomes
/// "Dr" and the next pass renames the user's file on disk to "Dr".
#[test]
fn adv020_explorer_rename_of_a_dotted_name_with_no_extension_is_kept() {
    let mut r = Rig::loose();
    std::fs::create_dir_all(&r.root).unwrap();
    std::fs::write(r.root.join("Dr. Smith letter"), b"Dear Dr. Smith").unwrap();
    r.settle();
    let it = r.items();
    assert_eq!(it.len(), 1, "imported: {:?}", it.iter().map(|i| &i.name).collect::<Vec<_>>());
    assert_eq!(it[0].ext, "", "no extension by the ext_of rule");
    std::fs::rename(r.root.join("Dr. Smith letter"), r.root.join("Dr. Smith letter 2")).unwrap();
    r.settle();
    let names: Vec<String> = r.items().into_iter().map(|i| i.name).collect();
    let disk = r.files_in_root();
    assert_eq!(names, ["Dr. Smith letter 2"], "record name after an Explorer rename (disk now {disk:?})");
    assert_eq!(disk, ["Dr. Smith letter 2"], "the user's file keeps the name the user gave it");
}

/// R1 "a renamed program cannot pose as a photo". A received File record with an EMPTY extension
/// whose name ends in ".jpg" (a phone share whose bytes are not a photo, or a hostile peer) is
/// placed on the PC under the record name, so an EXE lands in the library as "holiday.jpg" although
/// the core's own rule says a File never carries a photo extension (verify_received KindMismatch).
#[test]
fn adv020_a_received_file_with_no_extension_never_lands_under_a_photo_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    std::fs::create_dir_all(&root).unwrap();
    let p = PcLibrary::new(Some(root.clone()), tmp.path().join("bin"), Default::default());
    let mut mz = b"MZ\x90\x00\x03\x00\x00\x00".to_vec();
    mz.resize(64, 0);
    let mut placed = Vec::new();
    for (n, name) in [(1u128, "holiday.jpg"), (2, "clip.mp4"), (3, "scan.png")] {
        let t = FileTarget {
            item: ItemId(uuid::Uuid::from_u128(n)),
            hash: BlobHash([n as u8; 32]),
            name: name.into(),
            ext: String::new(),
            captured_at: 1_790_000_000_000,
            folder: INBOX_ID,
            folder_kind: FolderKind::Inbox,
            folder_name: "Inbox".into(),
            current: None,
        };
        let part = p.part_path(&t.hash).unwrap();
        std::fs::write(&part, &mz).unwrap();
        if let Ok(dest) = p.commit(&part, &t) {
            placed.push(dest.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    let posing: Vec<&String> = placed.iter().filter(|n| names::media_family_of_name(n).is_some()).collect();
    assert!(posing.is_empty(), "program bytes placed under a photo/video name: {posing:?}");
}

/// R2: a zero-byte file dropped on the window is copied and becomes an item (any file type).
#[test]
fn adv020_a_dropped_zero_byte_file_is_copied() {
    let mut r = Rig::loose();
    r.settle();
    let src = r.src("empty.txt", b"");
    let rep = r.drop_in_inbox(std::slice::from_ref(&src));
    assert_eq!(rep.copied, ["empty.txt"], "{rep:?}");
    assert!(src.exists(), "source untouched");
    let it = r.items();
    assert_eq!(it.len(), 1);
    assert_eq!(it[0].size, 0);
    assert_eq!(r.drop_temps(), 0, "no temp copy left");
}

/// R1/R2: a refused name with a trailing dot (Windows treats "evil.lnk." as "evil.lnk"; it can be
/// made through a \\?\ path, e.g. on a USB stick) is refused by a drop and nothing is copied.
#[test]
fn adv020_a_dropped_shortcut_with_a_trailing_dot_is_refused() {
    let mut r = Rig::loose();
    r.settle();
    let d = r.tmp.path().join("usb2");
    std::fs::create_dir_all(&d).unwrap();
    let verbatim = PathBuf::from(format!(r"\\?\{}\evil.lnk.", d.display()));
    std::fs::write(&verbatim, b"L\x00\x00\x00").unwrap();
    let listed: Vec<PathBuf> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.path()).collect();
    assert_eq!(listed.len(), 1);
    let rep = r.drop_in_inbox(&listed);
    assert!(rep.copied.is_empty(), "a shortcut must never be copied in: {rep:?}");
    assert_eq!(rep.refused.len(), 1, "{rep:?}");
    assert!(r.files_in_root().is_empty(), "nothing in the library: {:?}", r.files_in_root());
    let _ = std::fs::remove_file(&verbatim);
}

/// R1/R2: a program named like a photo is dropped. Core refuses it (content does not match); the
/// drop must say so and leave neither the placed copy nor a temp copy behind.
#[test]
fn adv020_a_dropped_program_named_like_a_photo_leaves_nothing_behind() {
    let mut r = Rig::loose();
    r.settle();
    let mut mz = b"MZ\x90\x00".to_vec();
    mz.resize(4096, 1);
    let src = r.src("photo.jpg", &mz);
    let rep = r.drop_in_inbox(&[src]);
    assert!(rep.copied.is_empty(), "{rep:?}");
    assert_eq!(rep.refused.len() + rep.failed.len(), 1, "visible: {rep:?}");
    assert!(r.files_in_root().is_empty(), "no copy left in the library: {:?}", r.files_in_root());
    assert_eq!(r.drop_temps(), 0, "no temp copy left in .tugboat");
    r.scan(3);
    assert!(r.items().is_empty(), "no item made by a later scan");
}

/// R2 unique names: two different files with the same name dropped in one go plus an existing
/// library file of that name: all three survive, nothing is overwritten.
#[test]
fn adv020_dropping_two_same_named_files_overwrites_nothing() {
    let mut r = Rig::loose();
    std::fs::create_dir_all(&r.root).unwrap();
    std::fs::write(r.root.join("notes.txt"), b"library copy").unwrap();
    r.settle();
    let a = r.src("notes.txt", b"from usb A");
    let d = r.tmp.path().join("usbB");
    std::fs::create_dir_all(&d).unwrap();
    let b = d.join("notes.txt");
    std::fs::write(&b, b"from usb B").unwrap();
    let rep = r.drop_in_inbox(&[a, b]);
    assert_eq!(rep.copied.len(), 2, "{rep:?}");
    let mut bodies: Vec<Vec<u8>> = r.files_in_root().iter().map(|n| std::fs::read(r.root.join(n)).unwrap()).collect();
    bodies.sort();
    assert_eq!(bodies, [b"from usb A".to_vec(), b"from usb B".to_vec(), b"library copy".to_vec()]);
}
