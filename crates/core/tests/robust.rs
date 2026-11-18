//! M1 robustness scenarios over loopback: tombstones against a restored backup, new-peer full
//! snapshot, reconcile catching a dropped change, resumable transfers, corrupt and non-media
//! files. Design test ids S7-S10, S16, S17.

mod common;

use std::path::Path;

use common::*;
use tugboat_core::{events::SyncState, ActivityAction, LocalFileState, Retention, Role, TestClock};

fn part_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

// S7: restore an old backup of the PC. Items deleted and purged since must not come back, the
// restored device's lamport catches up in the handshake, and its next edit wins.
#[tokio::test(flavor = "multi_thread")]
async fn s07_restored_backup_cannot_resurrect_deleted_items() {
    let r = Rig::new().await;
    let work = r.pc.core.create_folder("Work", Retention::Keep).unwrap();
    let i1 = r.phone.add("one.jpg", 100, 5_000);
    let i2 = r.phone.add("two.jpg", 101, 5_000);
    until("items on the PC", 15, || [i1, i2].iter().all(|i| r.pc.local(*i) == Some(LocalFileState::Present))).await;
    // In the Keep folder, so the 8 days the clocks jump below do not expire them on their own.
    r.phone.core.move_item(i1, tugboat_core::KEEP_ID).unwrap();
    r.phone.core.move_item(i2, tugboat_core::KEEP_ID).unwrap();
    until("moved on the PC", 15, || [i1, i2].iter().all(|i| r.pc.record(*i).map(|x| x.location.val.folder) == Some(tugboat_core::KEEP_ID))).await;
    until("folder on the phone", 15, || r.phone.core.folders().unwrap().iter().any(|f| f.id == work)).await;
    r.settle().await;

    // Backup of the PC database at this point.
    let backup = r.pc.dir.path().join("backup.db");
    let sql = format!("VACUUM INTO '{}'", backup.display().to_string().replace('\'', "''"));
    r.pc.core.debug_conn(|c| c.execute_batch(&sql)).unwrap();
    // The held files too: a real backup holds the library next to the database.
    let files_backup = r.pc.dir.path().join("storage-backup");
    copy_tree(&r.pc.dir.path().join("storage"), &files_backup);

    // Both items are deleted and purged on both devices; the phone renames the folder many times.
    r.phone.core.delete_item(i1).unwrap();
    r.phone.core.delete_item(i2).unwrap();
    until("PC binned both", 15, || [i1, i2].iter().all(|i| r.pc.local(*i) == Some(LocalFileState::Binned))).await;
    for n in 0..10 {
        r.phone.core.rename_folder(work, &format!("Work v{n}")).unwrap();
    }
    until("rename on the PC", 15, || r.pc.core.folders().unwrap().iter().any(|f| f.name == "Work v9")).await;
    r.settle().await;
    for d in [&r.pc, &r.phone] {
        d.clock.advance(8 * DAY);
        d.core.tick().unwrap();
        assert_eq!(d.local(i1), Some(LocalFileState::Purged), "{}: purged", d.name);
    }
    let phone_lamport = r.phone.max_lamport();
    assert!(phone_lamport >= 10);

    // The PC comes back from the backup: same identity, same address, old database.
    let (secret, port, name) = (r.pc.secret, r.pc.port, r.pc.name.clone());
    r.phone.stop().await;
    r.pc.stop().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    std::fs::copy(&backup, dir.path().join("data").join("tugboat.db")).unwrap();
    copy_tree(&files_backup, &dir.path().join("storage"));
    let mut pc2 = Dev::open_in(dir, Role::Pc, &name, &r.pc.clock, secret);
    pc2.port = port;
    assert!(pc2.max_lamport() < phone_lamport, "the restored database is behind");
    assert_eq!(pc2.local(i1), Some(LocalFileState::Present), "the backup still believes the item is alive");

    pc2.start().await;
    r.phone.start().await;
    pc2.wait_state(SyncState::Synced).await;
    r.phone.wait_state(SyncState::Synced).await;
    until("equal digests", 15, || pc2.digest() == r.phone.digest()).await;

    for i in [i1, i2] {
        assert!(r.phone.record(i).unwrap().deleted.val.is_some(), "phone: still deleted");
        assert!(pc2.record(i).unwrap().deleted.val.is_some(), "restored PC: the delete came back");
        // The Bin clock is local: the restored PC only now learns of the delete, so its file gets
        // the full Bin days from now (it was not binned 8 days ago on THIS device).
        assert_eq!(pc2.local(i), Some(LocalFileState::Binned), "restored PC: binned on arrival, not purged with no Bin time");
        assert_eq!(r.phone.local(i), Some(LocalFileState::Purged));
    }
    assert!(r.phone.items().is_empty(), "nothing resurrected on the phone");
    assert!(pc2.items().is_empty(), "nothing resurrected on the PC");
    pc2.clock.advance(8 * DAY);
    pc2.core.tick().unwrap();
    for i in [i1, i2] {
        assert_eq!(pc2.local(i), Some(LocalFileState::Purged), "restored PC: purged after its own Bin days");
    }
    assert!(pc2.max_lamport() >= phone_lamport, "lamport caught up");
    // The restored PC's first edit gets a version above everything the phone holds: it wins.
    pc2.core.rename_folder(work, "Work after restore").unwrap();
    until("the phone took the restored PC's edit", 15, || r.phone.core.folders().unwrap().iter().any(|f| f.name == "Work after restore")).await;
    assert!(pc2.error_kinds().is_empty(), "{:?}", pc2.errors());
    r.phone.stop().await;
    pc2.stop().await;
}

// S8: a new peer gets a full snapshot both ways: more than two pages, tombstones included, and
// no file is ever downloaded for a purged tombstone.
#[tokio::test(flavor = "multi_thread")]
async fn s08_new_peer_full_snapshot_both_ways() {
    let pc_clock = TestClock::new(T0);
    let pc = Dev::new(Role::Pc, "TEST-PC", &pc_clock);
    let phone = Dev::new(Role::Phone, "Test phone", &TestClock::new(T0));
    // PC history before any pairing: 600 items, 150 of them deleted and purged (tombstones).
    let mut pc_ids = Vec::new();
    for n in 0..600u64 {
        pc_ids.push(pc.add(&format!("pc_{n}.jpg"), 1000 + n, 64));
    }
    for id in &pc_ids[..150] {
        pc.core.delete_item(*id).unwrap();
    }
    pc_clock.advance(8 * DAY);
    pc.core.tick().unwrap();
    assert!(pc_ids[..150].iter().all(|i| pc.local(*i) == Some(LocalFileState::Purged)));
    // Back to "now": the phone's items must not look 8 days old to the PC (they would expire on arrival).
    pc_clock.set(T0);
    // Phone history: 100 items.
    let phone_ids: Vec<_> = (0..100u64).map(|n| phone.add(&format!("ph_{n}.jpg"), 5000 + n, 64)).collect();

    let t0 = std::time::Instant::now();
    pair(&pc, &phone).await;
    until("equal digests", 60, || pc.digest() == phone.digest()).await;
    println!("S8 records equal on both devices after {} ms", t0.elapsed().as_millis());
    let (_, records) = pc.digest();
    assert!(records > 1000, "more than two 500-record pages: {records}");

    for id in &pc_ids[..150] {
        assert_eq!(phone.local(*id), Some(LocalFileState::Purged), "tombstone: record only, never downloaded");
        assert!(phone.record(*id).unwrap().deleted.val.is_some());
    }
    until("every live item on both devices", 90, || {
        pc_ids[150..].iter().all(|i| phone.local(*i) == Some(LocalFileState::Present)) && phone_ids.iter().all(|i| pc.local(*i) == Some(LocalFileState::Present))
    })
    .await;
    println!("S8 all {} files transferred after {} ms", 450 + 100, t0.elapsed().as_millis());
    pc.wait_state(SyncState::Synced).await;
    phone.wait_state(SyncState::Synced).await;
    let held = std::fs::read_dir(phone.items_dir()).unwrap().count();
    assert_eq!(held, 450 + 100, "no file for a purged tombstone");
    assert!(phone.error_kinds().is_empty() && pc.error_kinds().is_empty(), "{:?} {:?}", phone.errors(), pc.errors());
    phone.stop().await;
    pc.stop().await;
}

// S9a: a change lost from the PC's log (here: the whole log dropped) is healed by the digest
// compare and a list exchange on the next connect.
#[tokio::test(flavor = "multi_thread")]
async fn s09a_reconcile_heals_a_dropped_change() {
    let r = Rig::new().await;
    r.apart().await;
    let id = r.pc.add("lost_change.jpg", 200, 30_000);
    r.pc.core.debug_conn(|c| c.execute_batch("DELETE FROM change_log")).unwrap();
    r.together().await;

    until("the phone healed the missing item", 20, || r.phone.local(id) == Some(LocalFileState::Present)).await;
    r.settle().await;
    assert_eq!(r.phone.file_bytes(id), common::jpeg_bytes(200, 30_000));
    assert!(!r.phone.error_kinds().contains(&"reconcile".to_string()), "healed, so no error: {:?}", r.phone.errors());
    assert!(!r.pc.error_kinds().contains(&"reconcile".to_string()));
    r.shutdown().await;
}

// S9b: a mismatch that cannot be healed (the PC holds a record the phone must refuse) is
// reported as Event::Error{reconcile} plus an activity row, on both devices, not hidden.
#[tokio::test(flavor = "multi_thread")]
async fn s09b_unhealable_mismatch_is_reported() {
    let r = Rig::new().await;
    r.apart().await;
    let id = r.pc.add("bad_ext.jpg", 201, 3_000);
    let sql = format!("UPDATE item SET ext = 'exe' WHERE id = x'{}'", hex::encode(id.0.as_bytes()));
    r.pc.core.debug_conn(|c| c.execute_batch(&sql)).unwrap();
    r.together().await;

    until("reconcile error on both devices", 20, || r.pc.error_kinds().contains(&"reconcile".to_string()) && r.phone.error_kinds().contains(&"reconcile".to_string())).await;
    assert!(!r.phone.has_item(id), "the phone never stored the refused record");
    assert!(r.phone.error_kinds().contains(&"protocol".to_string()), "the skipped record was reported: {:?}", r.phone.errors());
    for d in [&r.pc, &r.phone] {
        assert!(d.has_activity(ActivityAction::SyncError), "{}: {:?}", d.name, d.activity_text());
    }
    assert_ne!(r.phone.state(), SyncState::Synced, "never shows Synced with a known difference");
    r.shutdown().await;
}

// S10a: a transfer that was cut earlier (a .part file with the first 10 MB) resumes from there:
// only the rest is downloaded, the hash is verified and no .part is left.
#[tokio::test(flavor = "multi_thread")]
async fn s10a_resumes_from_an_existing_part_file() {
    let r = Rig::new().await;
    r.apart().await;
    const SIZE: usize = 24 * 1024 * 1024;
    const HEAD: usize = 10 * 1024 * 1024;
    let id = r.pc.add("big.jpg", 300, SIZE);
    let hash = r.pc.record(id).unwrap().content.val.hash;
    let all = common::jpeg_bytes(300, SIZE);
    std::fs::write(r.phone.parts_dir().join(format!("{}.part", hex::encode(hash.0))), &all[..HEAD]).unwrap();
    r.together().await;

    until("the big file arrived", 60, || r.phone.local(id) == Some(LocalFileState::Present)).await;
    assert_eq!(r.phone.core.debug_resumed_bytes(), HEAD as u64, "continued after the existing 10 MB");
    assert_eq!(r.phone.file_bytes(id), all, "whole file, hash verified");
    assert!(part_files(&r.phone.parts_dir()).is_empty(), "no .part left: {:?}", part_files(&r.phone.parts_dir()));
    assert!(r.phone.has_receipt(hash, r.phone.id), "receipt only after verification");
    r.shutdown().await;
}

// S10b: the connection is killed while a 64 MB file downloads; after reconnect the download
// resumes from the kept part (resumed bytes > 0), the hash is right and no .part is left.
#[tokio::test(flavor = "multi_thread")]
async fn s10b_connection_killed_mid_transfer_then_resumes() {
    let r = Rig::new().await;
    r.phone.core.debug_set_retry_base_ms(100);
    const SIZE: usize = 64 * 1024 * 1024;
    let id = r.pc.add("huge.jpg", 301, SIZE);
    let hash = r.pc.record(id).unwrap().content.val.hash;
    let part = r.phone.parts_dir().join(format!("{}.part", hex::encode(hash.0)));
    let t0 = std::time::Instant::now();
    let mut cut_at = 0u64;
    while t0.elapsed().as_secs() < 30 {
        cut_at = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if cut_at >= 10 * 1024 * 1024 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(cut_at >= 10 * 1024 * 1024, "the download never reached 10 MB");
    r.phone.core.debug_kill_connection();
    assert!(r.phone.local(id) != Some(LocalFileState::Present) || cut_at as usize >= SIZE, "the file had not finished when the connection was killed (cut at {cut_at})");

    until("the file arrived after reconnect", 60, || r.phone.local(id) == Some(LocalFileState::Present)).await;
    println!("S10b cut at {cut_at} bytes, resumed from {}", r.phone.core.debug_resumed_bytes());
    assert!(r.phone.core.debug_resumed_bytes() > 0, "resumed from the kept part, not from zero");
    assert_eq!(r.phone.file_bytes(id), common::jpeg_bytes(301, SIZE));
    assert!(part_files(&r.phone.parts_dir()).is_empty());
    r.shutdown().await;
}

// S16: the sender serves wrong bytes of the right size. The phone's hash check fails: no
// receipt, not Present, an error event; when the sender's bytes are right again it succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn s16_wrong_bytes_never_become_a_file_or_a_receipt() {
    let r = Rig::new().await;
    r.phone.core.debug_set_retry_base_ms(100);
    r.apart().await;
    let id = r.pc.add("corrupt.jpg", 400, 40_000);
    let hash = r.pc.record(id).unwrap().content.val.hash;
    let path = r.pc.view(id).unwrap().path.unwrap();
    let good = std::fs::read(&path).unwrap();
    let mut bad = good.clone();
    bad[20_000] ^= 0xFF;
    std::fs::write(&path, &bad).unwrap();
    r.together().await;

    until("a hash mismatch error", 20, || r.phone.error_kinds().iter().any(|k| k == "hash_mismatch")).await;
    assert_ne!(r.phone.local(id), Some(LocalFileState::Present));
    assert!(!r.phone.has_receipt(hash, r.phone.id), "no receipt for a file that failed its hash");
    assert!(part_files(&r.phone.parts_dir()).is_empty(), "the corrupt part is deleted");
    assert!(part_files(&r.phone.items_dir()).is_empty(), "nothing stored");

    std::fs::write(&path, &good).unwrap();
    until("the file arrives once the bytes are right", 20, || r.phone.local(id) == Some(LocalFileState::Present)).await;
    assert_eq!(r.phone.file_bytes(id), good);
    assert!(r.phone.has_receipt(hash, r.phone.id));
    r.shutdown().await;
}

// S17: the record says .jpg, the bytes (with a valid hash) are a Windows executable. The phone
// refuses it: nothing is stored, no receipt, so the PC never expires the item.
#[tokio::test(flavor = "multi_thread")]
async fn s17_executable_named_jpg_is_refused() {
    let r = Rig::new().await;
    r.apart().await;
    let id = r.pc.add("innocent.jpg", 500, 2_000);
    let path = r.pc.view(id).unwrap().path.unwrap();
    let mut exe = b"MZ\x90\x00\x03\x00\x00\x00".to_vec();
    exe.extend((0..3000u32).map(|n| (n % 251) as u8));
    std::fs::write(&path, &exe).unwrap();
    let h = blake3::hash(&exe);
    let sql = format!("UPDATE item SET blob_hash = x'{}', size = {} WHERE id = x'{}'", h.to_hex(), exe.len(), hex::encode(id.0.as_bytes()));
    r.pc.core.debug_conn(|c| c.execute_batch(&sql)).unwrap();
    r.together().await;

    until("the phone refused the file", 20, || r.phone.local(id) == Some(LocalFileState::Refused)).await;
    assert!(r.phone.error_kinds().iter().any(|k| k == "not_media"), "{:?}", r.phone.errors());
    assert!(part_files(&r.phone.items_dir()).is_empty(), "nothing stored");
    assert!(part_files(&r.phone.parts_dir()).is_empty(), "no part left");
    assert!(!r.pc.has_receipt(tugboat_core::BlobHash(*h.as_bytes()), r.phone.id), "no receipt: the sender never expires it");
    assert!(r.phone.has_activity(ActivityAction::Refused));
    r.pc.clock.advance(30 * HOUR);
    r.pc.core.tick().unwrap();
    assert_eq!(r.pc.view(id).unwrap().state, tugboat_core::ItemState::Waiting, "the PC keeps it");
    r.shutdown().await;
}
