//! Adversarial security tests for 0.2.0 on the PC (verifier stage, security lens). They try to break:
//! the always-refused names against what NTFS and Win32 really do with a name (case, trailing dots and
//! spaces, stream syntax, Unicode lookalikes); the Mark-of-the-Web on received files when the other
//! device later renames or moves them; and the drag-drop copy against device paths and NTFS stream
//! paths that a hostile drag source could put in a drop. A FAILING test here is a finding.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use tugboat_core::{
    facade::device_id_from_secret,
    names,
    placement::{FileTarget, Placement},
    BlobHash, Core, CoreConfig, FolderId, FolderKind, ItemId, ItemQuery, ItemView, Role, SystemClock, INBOX_ID,
};
use tugboat_desktop_lib::{
    drop::{copy_dropped, DropReport, DropTarget, NativeDrop},
    library::{config::LibraryConfig, placement::PcLibrary, Library, LibraryOptions},
    winfs::{read_zone_mark, ZONE_TEXT},
};

// ---------- names against the real filesystem ----------

/// Every name in `variants` is written through plain Win32 (std::fs) into an empty folder. Whatever
/// file Windows really creates is read back from the directory listing; if that file is an
/// always-refused one, the name we were given must have been refused too (otherwise a watcher, a drop
/// or a received record could produce it without being stopped).
#[test]
fn adversarial_020_security_a_name_that_windows_turns_into_a_refused_file_is_refused() {
    let variants = [
        "x.lnk",
        "X.LNK",
        "x.lnk.",
        "x.lnk. .",
        "x.lnk   ",
        "x.lnk::$DATA",
        "x.lnk:payload",
        "x.url..",
        "x.scf ",
        "x.Library-ms.",
        "x.searchConnector-ms ",
        "DESKTOP.INI",
        "desktop.ini...",
        "desktop.ini:zone",
        "autorun.inf ",
        "AutoRun.Inf::$DATA",
        // Not refused by Windows itself (the extra character stays in the name): only checked for consistency.
        "x.lnk\u{200B}",
        "x.l\u{0131}nk",
    ];
    for v in variants {
        let dir = tempfile::tempdir().unwrap();
        if std::fs::write(dir.path().join(v), b"x").is_err() {
            continue; // Windows refused the name outright: nothing could be made from it.
        }
        let made: Vec<String> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        for real in &made {
            if names::is_refused_name(real) {
                assert!(names::is_refused_name(v), "{v:?} creates the refused file {real:?} but is not refused itself");
            }
        }
    }
}

/// Unicode lookalikes (dotless i, long s, Kelvin sign, Turkish capital I, fullwidth letters) are NOT
/// folded to ASCII by NTFS name lookup or by the registry's file-type lookup on this PC, so a name like
/// "de\u{17F}ktop.ini" is not desktop.ini to Explorer and need not be refused. This test pins that
/// assumption: if a Windows update ever starts folding them, it fails and the refused list needs a
/// Unicode case fold.
#[test]
fn adversarial_020_security_unicode_lookalikes_do_not_alias_a_refused_file_or_file_type() {
    let dir = tempfile::tempdir().unwrap();
    for (look, real) in [
        ("de\u{017F}ktop.ini", "desktop.ini"),
        ("desktop.\u{0131}ni", "desktop.ini"),
        ("desktop.\u{0130}ni", "desktop.ini"),
        ("autorun.\u{0131}nf", "autorun.inf"),
        ("x.ln\u{212A}", "x.lnk"),
        ("x.\u{FF4C}nk", "x.lnk"),
    ] {
        let p = dir.path().join(look);
        std::fs::write(&p, b"x").unwrap();
        assert!(!dir.path().join(real).exists(), "NTFS opens {look:?} when {real:?} is asked for: the refused list must case-fold Unicode");
        std::fs::remove_file(&p).unwrap();
    }
    // File types: HKCR lookups of the lookalike extensions must find no key (reg.exe exits 1 = not found).
    for ext in [".\u{017F}cf", ".ln\u{212A}", ".\u{0131}ni"] {
        let out = std::process::Command::new("reg").args(["query", &format!(r"HKCR\{ext}")]).output().unwrap();
        assert!(!out.status.success(), "the registry resolves the lookalike type {ext:?} to a real file type");
    }
    // Control: the real types do exist, so the probe itself works.
    let out = std::process::Command::new("reg").args(["query", r"HKCR\.lnk"]).output().unwrap();
    assert!(out.status.success(), "control probe failed: HKCR\\.lnk not found");
}

// ---------- Mark-of-the-Web when the other device renames or moves a received file ----------

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

/// A received file keeps its mark when the other device renames it, moves it to another folder, and
/// when it goes to the Bin and back: every one of those is the placement moving the same file, so the
/// "downloaded from elsewhere" warning must never quietly disappear.
#[test]
fn adversarial_020_security_a_received_file_stays_marked_through_rename_move_bin_and_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    std::fs::create_dir_all(&root).unwrap();
    let p = PcLibrary::new(Some(root.clone()), tmp.path().join("bin"), Default::default());
    let mut t = target(7, "setup.bat", "bat");
    let part = p.part_path(&t.hash).unwrap();
    std::fs::write(&part, b"@echo hi").unwrap();
    let placed = p.commit(&part, &t).unwrap();
    assert_eq!(read_zone_mark(&placed).as_deref(), Some(ZONE_TEXT));

    // Renamed by the phone.
    t.current = Some(placed);
    t.name = "install.bat".into();
    let renamed = p.relocate(&t).unwrap();
    assert_eq!(renamed.file_name().unwrap(), "install.bat");
    assert_eq!(read_zone_mark(&renamed).as_deref(), Some(ZONE_TEXT), "mark lost on a rename by the other device");

    // Moved to another folder by the phone.
    t.current = Some(renamed);
    t.folder = FolderId(uuid::Uuid::from_u128(99));
    t.folder_kind = FolderKind::User;
    t.folder_name = "Work".into();
    let moved = p.relocate(&t).unwrap();
    assert!(moved.starts_with(root.join("Work")), "{}", moved.display());
    assert_eq!(read_zone_mark(&moved).as_deref(), Some(ZONE_TEXT), "mark lost on a move to another folder");

    // Bin and back.
    t.current = Some(moved);
    let binned = p.to_bin(&t).unwrap();
    assert_eq!(read_zone_mark(&binned).as_deref(), Some(ZONE_TEXT), "mark lost in the Bin");
    t.current = Some(binned);
    let back = p.from_bin(&t).unwrap();
    assert_eq!(read_zone_mark(&back).as_deref(), Some(ZONE_TEXT), "mark lost on restore");
}

// ---------- drag and drop: hostile drop paths ----------

struct Rig {
    _tmp: tempfile::TempDir,
    core: Core,
    lib: Library,
    root: PathBuf,
    src: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let root = tmp.path().join("lib");
        let src = tmp.path().join("usb");
        std::fs::create_dir_all(&src).unwrap();
        let place = Arc::new(PcLibrary::new(None, data.join("Bin"), Default::default()));
        let secret = [7u8; 32];
        let cfg = CoreConfig { data_dir: data.clone(), role: Role::Pc, self_id: device_id_from_secret(&secret), device_name: "PC".into(), app_version: "test".into() };
        let (core, _rx) = Core::open(cfg, Arc::new(SystemClock), place.clone()).unwrap();
        let lib = Library::open(core.clone(), place, data, LibraryConfig::default(), &root, LibraryOptions::default(), Box::new(|_| {})).unwrap();
        let now = Instant::now();
        lib.scan_at(false, now);
        lib.scan_at(false, now + Duration::from_secs(3));
        Rig { _tmp: tmp, core, lib, root, src }
    }

    fn inbox(&self) -> DropTarget {
        let (id, kind, name) = self.core.live_folders().unwrap().into_iter().find(|(_, k, _)| *k == FolderKind::Inbox).unwrap();
        DropTarget { id, kind, name }
    }

    fn items(&self) -> Vec<ItemView> {
        self.core.items(&ItemQuery { limit: 500, ..Default::default() }).unwrap().rows
    }

    /// Run a drop on its own thread; a drop that never returns (a device that blocks a read) is a finding.
    fn drop_paths(&self, paths: Vec<PathBuf>) -> DropReport {
        let lib = self.lib.clone();
        let t = self.inbox();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rep = copy_dropped(&lib, NativeDrop::from_native(&paths), &t, &mut |_| {});
            let _ = tx.send(rep);
        });
        rx.recv_timeout(Duration::from_secs(30)).expect("the drop did not return within 30 s (a device path blocked the copy thread)")
    }
}

fn streams_of(p: &Path) -> Option<String> {
    std::fs::read_to_string(format!("{}:payload", p.display())).ok()
}

/// A drag source is another program: it can put any path in a CF_HDROP drop, including NTFS stream
/// paths and device names. None of them may create a stream on a library file, put a name with ':'
/// into the library or a record, or vanish without a line in the report.
#[test]
fn adversarial_020_security_stream_and_device_paths_in_a_drop_never_touch_the_library() {
    let r = Rig::new();
    // A user's own file already in the library, and a source file that has a hidden stream.
    let own = r.root.join("a.txt");
    std::fs::write(&own, b"mine").unwrap();
    let src = r.src.join("a.txt");
    std::fs::write(&src, b"outer").unwrap();
    std::fs::write(format!("{}:payload", src.display()), b"hidden stream bytes").unwrap();
    let ini = r.src.join("desktop.ini");
    std::fs::write(&ini, b"[.ShellClassInfo]").unwrap();
    std::fs::write(format!("{}:x", ini.display()), b"y").unwrap();

    let paths = vec![
        PathBuf::from(format!("{}:payload", src.display())),
        PathBuf::from(format!("{}:payload:$DATA", src.display())),
        PathBuf::from(format!("{}:x", ini.display())),
        PathBuf::from(r"\\.\NUL"),
        r.src.join("NUL"),
        r.src.join("CON.txt"),
    ];
    let n = paths.len();
    let rep = r.drop_paths(paths);
    eprintln!("drop report: {rep:?}");
    // Silent-failure rule: every dropped path is accounted for.
    assert_eq!(rep.copied.len() + rep.refused.len() + rep.failed.len(), n, "a dropped path vanished from the report: {rep:?}");
    // Nothing was hung onto the user's own file as a stream, and its bytes are unchanged.
    assert_eq!(std::fs::read(&own).unwrap(), b"mine");
    assert!(streams_of(&own).is_none(), "the drop wrote a stream onto the library's a.txt");
    // No name with ':' anywhere: not on disk, not in a record, not in the report's copied list.
    for e in std::fs::read_dir(&r.root).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        assert!(!name.contains(':') && !names::is_refused_name(&name), "the drop made {name:?} in the library");
    }
    for it in r.items() {
        assert!(!it.name.contains(':'), "a record got the name {:?}", it.name);
        assert!(!names::is_refused_name(&it.name), "a record got the refused name {:?}", it.name);
    }
    for c in &rep.copied {
        assert!(!c.contains(':'), "copied as {c:?}");
    }
    // The desktop.ini stream path is refused by name, not copied.
    assert!(!rep.copied.iter().any(|c| c.to_ascii_lowercase().starts_with("desktop")), "{rep:?}");
    // Device paths never become library files named after a device.
    for dev in ["NUL", "CON.txt"] {
        assert!(!rep.copied.iter().any(|c| c.eq_ignore_ascii_case(dev)), "a device was copied in as {dev}: {rep:?}");
    }
}
