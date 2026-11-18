//! Adversarial security tests for 0.2.0. Each test tries to break the rule for any file type and
//! always-refused names from the side of a hostile name: Windows spelling
//! tricks, NTFS stream syntax, a hostile peer's record name, a folder name Explorer treats as a shell
//! junction. A FAILING test here means a hole is open. Everything binds 127.0.0.1.

mod common;

use common::*;
use tugboat_core::{names, LocalFileState, MediaKind};

/// Every spelling Windows resolves to an always-refused file (trailing dots and spaces are dropped by
/// Win32, an NTFS stream suffix opens or creates the main file, case is ignored) must be refused, from
/// whichever side the name arrives (a local add, the PC watcher, a drop, a received record).
#[test]
fn adversarial_020_security_every_windows_spelling_of_a_refused_name_is_refused() {
    let refused = [
        "x.lnk",
        "X.LNK",
        "x.Lnk.",
        "x.lnk. . .",
        "x.lnk   ",
        "x.lnk::$DATA",
        "x.lnk:payload",
        "x.lnk:payload:$DATA",
        "x.url",
        "x.URL.",
        "x.scf",
        "x.SCF ",
        "x.library-ms",
        "x.LIBRARY-MS",
        "x.searchConnector-ms",
        "x.SEARCHCONNECTOR-MS..",
        "desktop.ini",
        "DESKTOP.INI",
        "Desktop.ini...",
        "desktop.ini::$DATA",
        "autorun.inf",
        "AUTORUN.INF ",
        "autorun.inf:x",
        // A path segment (a hostile display name with a path in it).
        r"sub\desktop.ini",
        "a/b/x.lnk",
        // Bidi and control characters are dropped before matching (over-refusal is fine).
        "x.l\u{200E}nk",
        "desk\u{202E}top.ini",
        // A name that is ONLY the extension.
        ".lnk",
        ".url",
    ];
    for n in refused {
        assert!(names::is_refused_name(n), "{n:?} must be refused");
    }
    // A record whose extension or built disk name is refused, whatever its name looks like.
    for (name, ext) in [("x", "lnk"), ("photo", "url"), ("a:b", "scf"), ("desktop", "ini"), ("autorun", "inf"), ("DESKTOP", "ini"), ("x.lnk", ""), ("q", "searchconnector-ms")] {
        assert!(names::is_refused_record(name, ext), "record {name:?} + {ext:?} must be refused");
    }
    // The type check refuses them before any byte is read as well.
    for ext in ["lnk", "LNK", ".lnk", "url", "scf", "library-ms", "searchconnector-ms"] {
        assert!(matches!(names::classify(ext, b"anything"), Err(names::NameError::Refused(_))), "classify {ext:?}");
    }
}

/// A received file never gets a disk name Windows would treat as an always-refused one, whatever the
/// record says: a hostile record name with stream syntax, trailing dots, a refused extension hidden
/// behind a harmless one.
#[test]
fn adversarial_020_security_no_received_record_builds_a_refused_disk_name() {
    let head = b"plain bytes, not a photo";
    for (name, ext) in [
        ("desktop.ini", ""),
        ("desktop", "ini"),
        ("Desktop.INI. ", ""),
        ("x", "lnk"),
        ("a:b", "lnk"),
        ("x.lnk::$DATA", ""),
        ("x.lnk:zone", "txt"),
        ("autorun.inf....", ""),
        ("..\\..\\desktop.ini", ""),
        ("x.url ", ""),
    ] {
        match names::disk_file_name(name, ext, head, 1_790_000_000_000) {
            Err(_) => {}
            Ok((disk, _)) => {
                assert!(!names::is_refused_name(&disk), "record {name:?} + {ext:?} became the refused disk name {disk:?}");
                assert!(!disk.contains(['\\', '/', ':']), "record {name:?} + {ext:?} became the path-like disk name {disk:?}");
            }
        }
    }
}

/// HOSTILE PEER: a record name carrying a bidi override (U+202E). Local adds strip bidi characters
/// (ops::add_local_file uses names::prep) and the disk name is cleaned, but the record NAME is shown in
/// both galleries (desktop ItemTile shows `item.name`, Android shows ItemInfo.name and hands it to other
/// apps as DISPLAY_NAME). "invoice<RLO>fdp.exe" reads "invoiceexe.pdf". validate_remote only checks the
/// length. Fails while the hole is open.
#[tokio::test(flavor = "multi_thread")]
async fn adversarial_020_security_a_peer_record_name_with_a_bidi_override_is_shown_as_sent() {
    let r = Rig::new().await;
    let id = r.phone.core.debug_add_record("invoice\u{202E}fdp.exe", "exe", MediaKind::File, None, LocalFileState::Refused).unwrap();
    until("the hostile record reaches the PC", 20, || r.pc.has_item(id)).await;
    let shown = r.pc.view(id).unwrap().name;
    r.shutdown().await;
    let bidi = |c: char| matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    assert!(
        !shown.chars().any(|c| bidi(c) || c.is_control()),
        "the PC shows the peer's name with bidi/control characters: {shown:?} (renders as a .pdf name for an .exe)"
    );
}

/// HOSTILE PEER: a record whose NAME claims one type and whose extension is another ("invoice.pdf" +
/// ext "exe"). Every honest path builds name = stem + "." + ext (add_local_file, rename_item), so a
/// mismatch only comes from a hostile or broken peer. The PC writes "invoice.pdf.exe" on disk (honest),
/// but the gallery tile shows "invoice.pdf". Fails while validate_remote accepts the mismatch.
#[tokio::test(flavor = "multi_thread")]
async fn adversarial_020_security_a_peer_record_name_cannot_hide_its_real_extension() {
    let r = Rig::new().await;
    let id = r.phone.core.debug_add_record("invoice.pdf", "exe", MediaKind::File, None, LocalFileState::Refused).unwrap();
    until("the hostile record reaches the PC", 20, || r.pc.has_item(id)).await;
    let v = r.pc.view(id).unwrap();
    r.shutdown().await;
    assert!(
        v.ext.is_empty() || v.name.to_ascii_lowercase().ends_with(&format!(".{}", v.ext)),
        "the PC shows {:?} for a file of type .{} (the name hides the real extension)",
        v.name,
        v.ext
    );
}

/// A folder name of the form "Name.{CLSID}" is a shell junction in Explorer: measured on this PC
/// (Shell.Application, 2026-11-18) "Photos.{20D04FE0-3AEA-1069-A2D8-08002B30309D}" is shown as
/// "Photos" and lists the drives of This PC, not the received files inside it. A folder name from the
/// other device must not be able to hide its files from Explorer this way. Fails while
/// clean_folder_name accepts it.
#[test]
fn adversarial_020_security_a_folder_name_cannot_be_an_explorer_shell_junction() {
    for name in ["Photos.{20D04FE0-3AEA-1069-A2D8-08002B30309D}", "GodMode.{ED7BA470-8E54-465E-825C-99712043E01C}", "x.{645FF040-5081-101B-9F08-00AA002F954E}"] {
        assert!(names::clean_folder_name(name).is_err(), "{name:?} is accepted as a folder name (Explorer shows it as a shell junction)");
    }
}
