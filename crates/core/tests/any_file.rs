//! 0.2.0: any file type syncs both ways, always-refused names never move, items expire on time,
//! the phone storage guard, the old-protocol refusal, an expired QR, a v1 database that migrates
//! and syncs. Everything binds 127.0.0.1.

mod common;

use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};

use common::*;
use tugboat_core::{
    events::SyncState,
    ops::{AddKind, NewLocalFile},
    store, ActivityAction, BlobHash, CoreError, ItemId, ItemState, LocalFileState, MediaKind, Retention, Role, Settings, TestClock, Thumb,
};

/// Write `bytes` as <dev dir>/src/<name> and add it as a file the way a share or a drop would.
fn try_add(dev: &Dev, name: &str, bytes: &[u8]) -> Result<ItemId, CoreError> {
    let dir = dev.dir.path().join("src").join(format!("f{}", name.len() + bytes.len()));
    std::fs::create_dir_all(&dir).unwrap();
    let src: PathBuf = dir.join(format!("src-{}", blake3::hash(name.as_bytes()).to_hex()));
    std::fs::write(&src, bytes).unwrap();
    dev.core.add_local_file(NewLocalFile { src, name: name.to_string(), folder: None, captured_at: dev.clock_now(), geo: None, thumb: None, how: AddKind::Added, ntfs: None })
}

fn add(dev: &Dev, name: &str, bytes: &[u8]) -> ItemId {
    try_add(dev, name, bytes).unwrap_or_else(|e| panic!("add {name}: {e}"))
}

fn present(d: &Dev, id: ItemId) -> bool {
    d.local(id) == Some(LocalFileState::Present)
}

// A pdf and a file with no extension go phone -> PC, a csv goes PC -> phone. Kind File, the
// extension kept (or empty), bytes equal, no thumbnail, receipts on both sides.
#[tokio::test(flavor = "multi_thread")]
async fn any_file_type_flows_both_ways() {
    let r = Rig::new().await;
    let pdf_bytes = b"%PDF-1.7\nreport body\n%%EOF".to_vec();
    let mk_bytes = b"all:\n\techo hi\n".to_vec();
    let pdf = add(&r.phone, "report.pdf", &pdf_bytes);
    let mk = add(&r.phone, "Makefile", &mk_bytes);
    let csv = add(&r.pc, "data.csv", b"a,b\n1,2\n");
    let empty = add(&r.pc, "empty.txt", b"");
    until("both phone files on the PC and both PC files on the phone", 20, || {
        present(&r.pc, pdf) && present(&r.pc, mk) && present(&r.phone, csv) && present(&r.phone, empty)
    })
    .await;
    for (dev, id, want_kind, ext, bytes) in [
        (&r.pc, pdf, MediaKind::File, "pdf", pdf_bytes.clone()),
        (&r.pc, mk, MediaKind::File, "", mk_bytes.clone()),
        (&r.phone, csv, MediaKind::File, "csv", b"a,b\n1,2\n".to_vec()),
        (&r.phone, empty, MediaKind::File, "txt", Vec::new()),
    ] {
        let v = dev.view(id).unwrap();
        assert_eq!((v.kind, v.ext.as_str()), (want_kind, ext), "{}", dev.name);
        assert_eq!(dev.file_bytes(id), bytes, "{}: bytes equal", dev.name);
        assert!(v.thumb_path.is_none(), "no thumbnail for a File");
        let hash = dev.record(id).unwrap().content.val.hash;
        until("receipts from both devices", 15, || r.pc.has_receipt(hash, r.pc.id) && r.pc.has_receipt(hash, r.phone.id) && r.phone.has_receipt(hash, r.pc.id) && r.phone.has_receipt(hash, r.phone.id)).await;
    }
    // The file with no extension is stored without a dot on the phone's storage.
    let held = r.pc.view(mk).unwrap().path.unwrap();
    assert_eq!(PathBuf::from(&held).file_name().unwrap().to_string_lossy(), mk.0.hyphenated().to_string());
    // Expiry and Keep behave like any item.
    r.pc.core.set_pinned(pdf, true).unwrap();
    until("the keep reaches the phone", 15, || r.phone.view(pdf).is_some_and(|v| v.pinned)).await;
    r.settle().await;
    r.shutdown().await;
}

// Always-refused names from a peer. The records are accepted (digests stay equal) and never
// downloaded: zero blob and thumbnail requests reach the sender, nothing is stored, a REFUSED
// state, an activity row and an error event on the receiver, and `refused_items` lists them.
#[tokio::test(flavor = "multi_thread")]
async fn refused_names_never_cross_the_wire() {
    let r = Rig::new().await;
    let cases: Vec<(&str, &str)> = vec![
        ("x.lnk", "lnk"),
        ("desktop.ini", "ini"),
        ("AUTORUN.INF. ", "inf"),
        ("y.library-ms", "library-ms"),
        ("z.searchConnector-ms", "searchconnector-ms"),
        ("q.url", "url"),
        ("s.scf", "scf"),
        // Extension refused although the name looks harmless (the disk name would be IMG_<time>.lnk).
        ("a:b", "lnk"),
        ("x", "lnk"),
        ("desktop", "ini"),
        ("Desktop.Ini.", ""),
        ("x.lnk::$DATA", ""),
        ("x.ln\u{212A}", ""),
    ];
    let mut ids = Vec::new();
    for (i, (name, ext)) in cases.iter().enumerate() {
        let thumb = Some(Thumb { hash: BlobHash([i as u8 + 1; 32]), size: 10 });
        ids.push(r.phone.core.debug_add_record(name, ext, MediaKind::File, thumb, LocalFileState::Refused).unwrap());
    }
    until("every refused record on the PC and marked Refused", 20, || ids.iter().all(|id| r.pc.local(*id) == Some(LocalFileState::Refused))).await;
    assert_eq!(r.phone.core.debug_blob_requests(), 0, "the PC asked the phone for no file and no thumbnail");
    assert!(std::fs::read_dir(r.pc.items_dir()).unwrap().next().is_none(), "nothing stored on the PC");
    assert!(std::fs::read_dir(r.pc.parts_dir()).map(|mut d| d.next().is_none()).unwrap_or(true), "no part file");
    assert!(r.pc.has_activity(ActivityAction::Refused));
    assert!(r.pc.error_kinds().iter().any(|k| k == "refused"), "{:?}", r.pc.errors());
    assert!(r.pc.errors().iter().any(|(_, m)| m.contains("REFUSED - shortcut and folder-setting files never sync")), "{:?}", r.pc.errors());
    let listed = r.pc.core.refused_items(100).unwrap();
    assert_eq!(listed.len(), cases.len());
    assert!(listed.iter().all(|v| v.local == LocalFileState::Refused && v.path.is_none()));
    // The records themselves synced: digests stay equal, no reconcile error.
    r.settle().await;
    assert!(!r.pc.error_kinds().iter().any(|k| k == "reconcile"), "{:?}", r.pc.errors());
    assert!(!r.phone.error_kinds().iter().any(|k| k == "reconcile"));
    r.shutdown().await;
}

// ... and a refused name can never be made on the sending side either.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_name_is_refused_when_adding() {
    let clock = TestClock::new(T0);
    let d = Dev::new(Role::Phone, "alone", &clock);
    for name in ["x.lnk", "desktop.ini", "AUTORUN.INF", "a.url"] {
        let e = try_add(&d, name, b"x").unwrap_err();
        assert!(matches!(e, CoreError::Refused(_)), "{name}: {e}");
    }
    assert!(d.items().is_empty());
}

// A program posing as a photo is still refused (S17 covers the sync path); a pdf that holds
// JPEG bytes is just a File, with no thumbnail.
#[tokio::test(flavor = "multi_thread")]
async fn a_pdf_holding_jpeg_bytes_is_a_plain_file() {
    let r = Rig::new().await;
    let id = add(&r.pc, "doc.pdf", &jpeg_bytes(7, 500));
    until("on the phone", 20, || present(&r.phone, id)).await;
    let v = r.phone.view(id).unwrap();
    assert_eq!((v.kind, v.ext.as_str()), (MediaKind::File, "pdf"));
    assert!(v.thumb_path.is_none());
    // And an exe posing as a photo can not even be added.
    let e = try_add(&r.pc, "photo.jpg", b"MZ\x90\0\x03\0\0\0").unwrap_err();
    assert!(matches!(e, CoreError::NotMedia(_)), "{e}");
    r.shutdown().await;
}

// An old (0.1.x) phone. Sync: both devices say "Update Tugboat on the phone". Pairing: the
// phone is told while pairing, with the same text; a phone newer than the PC says "on the PC".
#[tokio::test(flavor = "multi_thread")]
async fn an_old_peer_is_told_to_update_at_sync_and_at_pairing() {
    // Sync, phone speaks protocol 1.
    let r = Rig::unconnected();
    r.phone.core.debug_set_proto(1);
    pair(&r.pc, &r.phone).await;
    r.phone.wait_state(SyncState::Refused).await;
    r.pc.wait_state(SyncState::Refused).await;
    for d in [&r.pc, &r.phone] {
        assert_eq!(d.core.status().message.as_deref(), Some("Update Tugboat on the phone"), "{}", d.name);
        assert!(d.has_activity(ActivityAction::SyncError), "{}", d.name);
    }
    r.shutdown().await;

    // Pairing, the phone speaks pairing protocol 1 (a 0.1.x phone).
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;
    r.phone.core.debug_set_pair_proto(1);
    let payload = r.pc.core.begin_pairing(false).unwrap();
    match r.phone.core.pair_with(&payload).await {
        Err(CoreError::Pairing { code, msg }) => assert_eq!((code.as_str(), msg.as_str()), ("version_mismatch", "Update Tugboat on the phone")),
        other => panic!("expected a version mismatch, got {:?}", other.map(|r| r.name)),
    }
    assert!(r.pc.core.paired_device().unwrap().is_none() && r.phone.core.paired_device().unwrap().is_none());
    // A phone from the future: the PC is the one to update.
    r.phone.core.debug_set_pair_proto(tugboat_net::pairing::PAIR_PROTO + 1);
    match r.phone.core.pair_with(&payload).await {
        Err(CoreError::Pairing { msg, .. }) => assert_eq!(msg, "Update Tugboat on the PC"),
        other => panic!("expected a version mismatch, got {:?}", other.map(|r| r.name)),
    }
    r.shutdown().await;
}

// The phone storage guard. Below the guard nothing downloads, the status says so, one
// activity row is written; with space again everything arrives. A file that would eat the margin
// is skipped on its own; two files that do not both fit above the guard: the second waits;
// an unreadable free space blocks and is shown, never read as "enough".
#[tokio::test(flavor = "multi_thread")]
async fn the_storage_guard_holds_downloads_back_and_says_so() {
    let pc = Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0));
    let (phone, free, fail) = Dev::phone_with_space("Test phone", &TestClock::new(T0), 1_000);
    // A tiny guard so the test needs no big files: the guard is 1000 bytes.
    phone.core.save_settings(&Settings { storage_guard_bytes: 1_000, ..Settings::default() }).unwrap();
    phone.core.debug_set_guard_recheck_ms(100);
    phone.core.debug_set_retry_base_ms(50);
    pair(&pc, &phone).await;
    let rig = Rig { pc, phone };
    rig.settle().await;
    assert!(!rig.phone.core.status().storage_low, "off by default");

    // Free space 1000 = the guard (not below) but there is no room above it: a 400-byte file waits.
    rig.phone.core.debug_set_guard_recheck_ms(100);
    rig.apart().await;
    let a = add(&rig.pc, "a.bin", &[1u8; 400]);
    let b = add(&rig.pc, "b.bin", &[2u8; 400]);
    let c = add(&rig.pc, "c.bin", &[3u8; 400]);
    free.store(900, Ordering::SeqCst); // below the guard
    rig.together().await;
    until("the guard is on", 20, || rig.phone.core.status().storage_low).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    for id in [a, b, c] {
        assert_eq!(rig.phone.local(id), Some(LocalFileState::Pending), "nothing downloads below the guard");
    }
    let st = rig.phone.core.status();
    assert_eq!(st.downloading, 3);
    assert!(st.storage_low);
    let rows = |d: &Dev| d.activity_text().iter().filter(|t| t.contains("PHONE STORAGE LOW")).count();
    assert_eq!(rows(&rig.phone), 1, "one activity row when the guard turns on, not one per re-check: {:?}", rig.phone.activity_text());
    assert!(rig.phone.error_kinds().iter().any(|k| k == "storage_low"));

    // Free space returns: everything arrives (the queue looks again by itself).
    free.store(1_000_000, Ordering::SeqCst);
    until("all three downloaded", 35, || [a, b, c].iter().all(|id| present(&rig.phone, *id))).await;
    until("the guard is off", 10, || !rig.phone.core.status().storage_low).await;

    // Per-item rule: 400 bytes above the guard fit once (spare 500), the second waits.
    let low_events = |d: &Dev| d.count_events(|e| matches!(e, tugboat_core::Event::Sync(s) if s.storage_low));
    let before = low_events(&rig.phone);
    rig.apart().await;
    let d = add(&rig.pc, "d.bin", &[4u8; 400]);
    let e = add(&rig.pc, "e.bin", &[5u8; 400]);
    free.store(1_500, Ordering::SeqCst);
    rig.together().await;
    until("one of the two arrived", 20, || present(&rig.phone, d) || present(&rig.phone, e)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The test's free space does not shrink as files arrive, so the second one is taken once the
    // first has finished; but while both were wanted the guard held the second back and said so.
    assert!(low_events(&rig.phone) > before, "the guard turned on again for the second file");
    until("both arrive in turn", 35, || present(&rig.phone, d) && present(&rig.phone, e)).await;

    // Unreadable free space blocks and is shown.
    rig.apart().await;
    let f = add(&rig.pc, "f.bin", &[6u8; 100]);
    free.store(1_000_000, Ordering::SeqCst);
    fail.store(true, Ordering::SeqCst);
    rig.together().await;
    until("blocked because the free space cannot be read", 20, || rig.phone.core.status().storage_low).await;
    assert_ne!(rig.phone.local(f), Some(LocalFileState::Present), "an unknown amount of space is never enough");
    fail.store(false, Ordering::SeqCst);
    until("it downloads once the space is known", 20, || present(&rig.phone, f)).await;
    rig.shutdown().await;
}

// The waiting count = held here, not in the Bin, no receipt from the paired device.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_counts_items_the_peer_has_not_confirmed() {
    let r = Rig::new().await;
    assert_eq!(r.pc.core.status().waiting, 0);
    r.apart().await;
    let id = add(&r.pc, "later.pdf", b"%PDF later");
    assert_eq!(r.pc.core.status().waiting, 1, "the phone has not confirmed it");
    r.together().await;
    until("the phone confirmed it", 20, || r.pc.core.status().waiting == 0 && present(&r.phone, id)).await;
    // Unpaired: nothing to wait for.
    let alone = Dev::new(Role::Pc, "alone", &TestClock::new(T0));
    add(&alone, "x.pdf", b"x");
    assert_eq!(alone.core.status().waiting, 0);
    r.shutdown().await;
}

// "Last seen" is written when the session ends, not only at the hello.
#[tokio::test(flavor = "multi_thread")]
async fn last_seen_follows_the_session_to_its_end() {
    let r = Rig::new().await;
    let hello_time = r.pc.core.paired_device().unwrap().unwrap().last_seen_at.expect("set at the hello");
    r.pc.clock.advance(10 * 60_000);
    let phone_id = r.phone.id;
    r.phone.stop().await;
    r.pc.wait_state(SyncState::Offline).await;
    let seen = r.pc.core.paired_device().unwrap().unwrap().last_seen_at.unwrap();
    assert_eq!(seen, r.pc.clock_now(), "the end of the session is the last contact");
    assert!(seen >= hello_time + 10 * 60_000);
    assert_eq!(r.pc.core.paired_device().unwrap().unwrap().id, phone_id);
    r.pc.stop().await;
}

// Keep, Move, Rename, Delete and Restore on a File item reach the other device.
#[tokio::test(flavor = "multi_thread")]
async fn item_actions_work_on_files() {
    let r = Rig::new().await;
    let id = add(&r.phone, "plan.docx", b"PK\x03\x04 pretend docx");
    until("on the PC", 20, || present(&r.pc, id)).await;
    let work = r.pc.core.create_folder("Work", Retention::Keep).unwrap();
    until("the folder on the phone", 15, || r.phone.core.folders().unwrap().iter().any(|f| f.id == work)).await;
    r.pc.core.move_item(id, work).unwrap();
    until("moved on the phone", 15, || r.phone.view(id).is_some_and(|v| v.folder == work)).await;
    r.phone.core.rename_item(id, "Plan v2").unwrap();
    until("renamed on the PC, extension kept", 15, || r.pc.view(id).is_some_and(|v| v.name == "Plan v2.docx")).await;
    r.pc.core.set_pinned(id, true).unwrap();
    until("kept on the phone", 15, || r.phone.view(id).is_some_and(|v| v.pinned)).await;
    r.pc.core.delete_item(id).unwrap();
    until("in the phone's Bin", 15, || r.phone.bin().iter().any(|v| v.id == id)).await;
    assert!(matches!(r.phone.bin()[0].state, ItemState::Deleted { .. }));
    r.pc.core.restore_item(id).unwrap();
    until("back on the phone", 15, || r.phone.view(id).is_some_and(|v| matches!(v.state, ItemState::Live { .. } | ItemState::FolderKeep | ItemState::Kept))).await;
    r.settle().await;
    r.shutdown().await;
}

// A QR older than its expiry. Reproduced first: the PC's window closed, the phone's dial
// failed and the old code answered "not_open" ("Pairing is not open on the PC"), which the phone
// app then showed as "That is not a Tugboat code". Now the phone says the code expired.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_qr_says_expired() {
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;

    // 1. The window closed (the PC app ticked): the PC no longer offers the pairing protocol.
    let payload = r.pc.core.begin_pairing(false).unwrap();
    r.pc.clock.advance(3 * 60_000);
    r.pc.core.tick().unwrap();
    r.phone.clock.advance(3 * 60_000);
    match r.phone.core.pair_with(&payload).await {
        Err(CoreError::Pairing { code, msg }) => assert_eq!((code.as_str(), msg.as_str()), ("expired", "PAIRING CODE EXPIRED - show a new QR")),
        other => panic!("expected an expiry, got {:?}", other.map(|r| r.name)),
    }
    until("the phone's cancelled event", 5, || r.phone.cancelled().is_some()).await;
    assert_eq!(r.phone.cancelled().map(|c| c.0).as_deref(), Some("expired"));

    // 2. The PC has not ticked yet (window still offered but past its time): the PC itself refuses.
    let payload = r.pc.core.begin_pairing(false).unwrap();
    r.pc.clock.advance(3 * 60_000);
    r.phone.clock.advance(3 * 60_000);
    match r.phone.core.pair_with(&payload).await {
        Err(CoreError::Pairing { code, .. }) => assert_eq!(code, "expired"),
        other => panic!("expected an expiry, got {:?}", other.map(|r| r.name)),
    }

    // 3. A code that has NOT expired by the phone's clock, but the PC closed its window (the user
    //    cancelled): that stays "not open", not "expired".
    let payload = r.pc.core.begin_pairing(false).unwrap();
    r.pc.core.cancel_pairing();
    match r.phone.core.pair_with(&payload).await {
        Err(CoreError::Pairing { code, .. }) => assert_eq!(code, "not_open"),
        other => panic!("expected not_open, got {:?}", other.map(|r| r.name)),
    }
    r.shutdown().await;
}

// A database written by 0.1.x (schema v1, items held on disk) opens as v2, keeps its rows,
// leaves a .bak-1, and syncs with a fresh peer to equal digests.
#[tokio::test(flavor = "multi_thread")]
async fn a_v1_database_migrates_and_syncs() {
    let clock = TestClock::new(T0);
    let secret = random_secret();
    let id_dev = tugboat_core::facade::device_id_from_secret(&secret);
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let items = dir.path().join("storage").join("items");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&items).unwrap();
    let bytes = jpeg_bytes(77, 3_000);
    let hash = BlobHash(*blake3::hash(&bytes).as_bytes());
    let item_id = ItemId::new(T0);
    {
        // Exactly what 0.1.x wrote: the v1 schema only, user_version 1.
        let conn = rusqlite::Connection::open(data.join("tugboat.db")).unwrap();
        conn.execute_batch(tugboat_core::db::MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        // Default folders as 0.1.x created them: the open below runs ensure_defaults on the migrated file.
        tugboat_core::folders::ensure_defaults(&conn).unwrap();
        let v = tugboat_core::Version { lamport: 1, device: id_dev };
        let rec = tugboat_core::ItemRecord {
            id: item_id,
            captured_at: T0,
            origin: id_dev,
            geo: None,
            name: tugboat_core::Versioned { v, val: "old.jpg".into() },
            content: tugboat_core::Versioned { v, val: tugboat_core::Content { hash, size: bytes.len() as u64, kind: MediaKind::Image, ext: "jpg".into(), thumb: None } },
            location: tugboat_core::Versioned { v, val: tugboat_core::Location { folder: tugboat_core::KEEP_ID, entered_at: T0 } },
            pinned: tugboat_core::Versioned { v, val: false },
            deleted: tugboat_core::Versioned { v, val: None },
        };
        store::insert_item(&conn, &rec, LocalFileState::Present, T0).unwrap();
        let path = items.join(format!("{}.jpg", item_id.0.hyphenated()));
        std::fs::write(&path, &bytes).unwrap();
        conn.execute("INSERT INTO file_ref (item_id, path) VALUES (?1, ?2)", rusqlite::params![item_id.0.as_bytes().as_slice(), path.to_string_lossy().into_owned()]).unwrap();
    }
    let phone = Dev::open_in(dir, Role::Phone, "Old phone", &clock, secret);
    assert!(data.join("tugboat.db.bak-1").exists(), "the v1 file was copied first");
    assert_eq!(phone.core.debug_conn(|c| c.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0)).unwrap()), 2);
    assert_eq!(phone.view(item_id).unwrap().name, "old.jpg");
    let pc = Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0));
    pair(&pc, &phone).await;
    let r = Rig { pc, phone };
    r.settle().await;
    until("the old photo reached the PC", 20, || present(&r.pc, item_id)).await;
    assert_eq!(r.pc.file_bytes(item_id), bytes);
    // And a File works on the migrated database (the CHECK accepts 'file').
    let f = add(&r.phone, "after.pdf", b"%PDF after");
    until("the new file on the PC", 20, || present(&r.pc, f)).await;
    r.shutdown().await;
}

// A record that declares a file over Settings.max_blob_bytes is refused once and visibly, not retried
// for ever; nothing is requested from the sender.
#[tokio::test(flavor = "multi_thread")]
async fn a_record_declaring_a_huge_file_is_refused_once() {
    let r = Rig::new().await;
    r.apart().await;
    let id = r.phone.core.debug_add_record("huge.iso", "iso", MediaKind::File, None, LocalFileState::Refused).unwrap();
    let sql = format!("UPDATE item SET size = {} WHERE id = x'{}'", 20u64 << 30, hex::encode(id.0.as_bytes()));
    r.phone.core.debug_conn(|c| c.execute_batch(&sql)).unwrap();
    r.together().await;
    until("refused on the PC", 20, || r.pc.local(id) == Some(LocalFileState::Refused)).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let n = r.pc.errors().iter().filter(|(_, m)| m == "TOO LARGE - over 16 GB").count();
    assert_eq!(n, 1, "refused once, not retried: {:?}", r.pc.errors());
    assert_eq!(r.phone.core.debug_blob_requests(), 0);
    r.shutdown().await;
}

// A File record's thumbnail field is stored unchanged but never fetched, served or shown.
#[tokio::test(flavor = "multi_thread")]
async fn a_files_thumbnail_is_never_fetched_or_shown() {
    let r = Rig::new().await;
    let thumb = Thumb { hash: BlobHash([5; 32]), size: 10 };
    let id = r.phone.core.debug_add_record("scan.pdf", "pdf", MediaKind::File, Some(thumb), LocalFileState::Present).unwrap();
    until("the record on the PC, and its file asked for (there is none: lost)", 20, || r.pc.local(id) == Some(LocalFileState::Lost)).await;
    let file_hash = r.phone.record(id).unwrap().content.val.hash;
    assert!(r.phone.core.debug_blob_requested(file_hash), "the file was asked for");
    assert!(!r.phone.core.debug_blob_requested(BlobHash([5; 32])), "the thumbnail never was");
    assert!(r.pc.view(id).unwrap().thumb_path.is_none());
    assert_eq!(r.pc.record(id).unwrap().content.val.thumb, Some(Thumb { hash: BlobHash([5; 32]), size: 10 }), "stored unchanged: digests stay equal");
    r.shutdown().await;
}

// An edit that leaves a file over max_blob_bytes is refused from its size, before the file is
// read: the file here is locked against reading, so a read would fail with an I/O error instead.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_edit_is_refused_without_reading_the_file() {
    use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};
    let r = Rig::new().await;
    let id = add(&r.pc, "log.txt", b"short");
    r.pc.core.save_settings(&Settings { max_blob_bytes: 2_000, ..Settings::default() }).unwrap();
    let big = r.pc.dir.path().join("grown.txt");
    std::fs::write(&big, vec![b'x'; 3_000]).unwrap();
    let _lock = OpenOptions::new().read(true).share_mode(0).open(&big).unwrap();
    let e = r.pc.core.local_content_changed(id, big).unwrap_err();
    assert!(matches!(&e, CoreError::Refused(_)), "{e:?}");
    r.shutdown().await;
}

// A refused rename leaves the record and its file path as they were: nothing is half done that the
// next pass would read as a change from the other device and undo.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_rename_does_not_move_the_file_path() {
    let r = Rig::new().await;
    let id = add(&r.pc, "Makefile", b"all:\n");
    let folder = r.pc.record(id).unwrap().location.val.folder;
    let path_of = |d: &Dev| d.core.debug_conn(|c| store::get_file_ref(c, id).unwrap().map(|f| f.path));
    let before = path_of(&r.pc);
    let moved = r.pc.dir.path().join("holiday.jpg");
    let e = r.pc.core.local_file_moved(id, moved, folder, "holiday.jpg").unwrap_err();
    assert!(matches!(e, CoreError::NotMedia(_)), "{e:?}");
    assert_eq!(path_of(&r.pc), before, "the path was not rewritten");
    assert_eq!(r.pc.record(id).unwrap().name.val, "Makefile");
    r.shutdown().await;
}

// The storage-low banner ends with the session: with the PC away nothing downloads, so a banner
// left on would outlive the user freeing the space.
#[tokio::test(flavor = "multi_thread")]
async fn the_storage_low_flag_clears_when_the_session_ends() {
    let pc = Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0));
    let (phone, free, _fail) = Dev::phone_with_space("Test phone", &TestClock::new(T0), 1_000);
    phone.core.save_settings(&Settings { storage_guard_bytes: 1_000, ..Settings::default() }).unwrap();
    pair(&pc, &phone).await;
    let rig = Rig { pc, phone };
    rig.settle().await;
    rig.apart().await;
    let a = add(&rig.pc, "a.bin", &[1u8; 400]);
    free.store(900, Ordering::SeqCst);
    rig.together().await;
    until("the guard is on", 20, || rig.phone.core.status().storage_low).await;
    rig.pc.stop().await;
    until("the flag clears with the session", 20, || !rig.phone.core.status().storage_low).await;
    assert_ne!(rig.phone.local(a), Some(LocalFileState::Present));
    rig.shutdown().await;
}
