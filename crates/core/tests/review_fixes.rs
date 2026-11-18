//! Tests for the verifier's second-round findings: violations and refused dials are surfaced,
//! an unpair made while apart reaches the other device, a missing file is retried quietly.
//! Network tests bind 127.0.0.1 only (common harness).

mod common;

use common::*;
use std::{path::Path, sync::Arc, time::Duration};

use tugboat_core::{
    events::{Event, SyncState},
    ops::{AddKind, NewLocalFile},
    ActivityAction, LocalFileState, Retention, Role, TestClock, INBOX_ID,
};

// PLAN Network "iroh 1.4.1 cannot turn NAT traversal off": the live guard closes a connection
// whose path leaves the LAN. PLAN: errors are surfaced, never swallowed. The enforcing device
// must show an error and an activity row, not only a debug log line.
#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_path_is_shown_to_the_user_and_the_session_recovers() {
    let r = Rig::new().await;
    assert!(r.phone.core.debug_enforce_path("8.8.8.8:47300"), "the guard closed the connection");
    until("the phone reports the blocked path", 10, || r.phone.error_kinds().contains(&"not_local".to_string())).await;
    let text = r.phone.errors().into_iter().find(|e| e.0 == "not_local").unwrap().1;
    assert!(text.contains("8.8.8.8:47300") && text.contains("BLOCKED"), "{text}");
    assert!(r.phone.has_activity(ActivityAction::SyncError), "{:?}", r.phone.activity_text());
    // The far side is told the code, and both come back (a LAN path is still available).
    until("the PC saw the close", 10, || !r.pc.errors().is_empty()).await;
    r.settle().await;
    r.shutdown().await;
}

// A stored PC address that is not on the local network can never be dialed. The phone must say
// so once (not stay silently "Offline"), and not repeat the error on every retry.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_local_pc_address_is_reported_once() {
    let r = Rig::new().await;
    r.apart().await;
    r.phone.core.debug_conn(|c| c.execute("UPDATE device SET addresses = '[\"8.8.8.8:47300\"]' WHERE is_self = 0", []).unwrap());
    r.together().await;
    until("the phone reports the blocked address", 10, || r.phone.error_kinds().contains(&"not_local".to_string())).await;
    tokio::time::sleep(std::time::Duration::from_millis(3_500)).await;
    let n = r.phone.error_kinds().iter().filter(|k| *k == "not_local").count();
    assert_eq!(n, 1, "reported once, not on every retry: {:?}", r.phone.errors());
    assert_eq!(r.phone.state(), SyncState::Offline);
    r.shutdown().await;
}

// A database failure inside the paired-key check must not look like an ordinary refusal.
#[tokio::test(flavor = "multi_thread")]
async fn a_database_error_in_the_pairing_check_is_reported() {
    let r = Rig::new().await;
    r.apart().await;
    r.pc.core.debug_conn(|c| c.execute_batch("ALTER TABLE device RENAME TO device_hidden").unwrap());
    r.together().await;
    until("the PC reports the failed check", 15, || r.pc.errors().iter().any(|e| e.1.contains("could not check the paired device"))).await;
    // The fault is the PC's: the phone must not hear NOT_PAIRED (it answers that by deleting its
    // own pairing) and so stays paired.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    assert!(r.phone.core.paired_device().unwrap().is_some(), "a database fault on the PC did not unpair the healthy phone");
    r.pc.core.debug_conn(|c| c.execute_batch("ALTER TABLE device_hidden RENAME TO device").unwrap());
    // And it recovers by itself once the PC's database is back.
    r.phone.core.reconnect_now();
    r.settle().await;
    r.shutdown().await;
}

// PLAN Security: "Unpair removes the key on both devices". The phone unpairs while the PC is
// out of reach: it must tell the PC as soon as it can, not leave the PC paired with a phone
// that has forgotten it. Before the fix the phone stopped dialing and the PC stayed paired.
#[tokio::test(flavor = "multi_thread")]
async fn unpair_made_while_apart_is_delivered_when_the_pc_is_reachable() {
    let r = Rig::new().await;
    let phone_id = r.phone.id;
    r.apart().await;
    r.phone.core.unpair().await.unwrap();
    assert_eq!(r.phone.state(), SyncState::Unpaired);
    let msg = r.phone.core.status().message.unwrap_or_default();
    assert!(msg.contains("NOT TOLD"), "the phone shows the PC has not been told: {msg:?}");
    assert!(r.pc.core.debug_conn(|c| tugboat_core::store::is_paired(c, phone_id).unwrap()), "the PC still pairs the phone");

    // The phone is back on the network (its network restarts; nothing else is done).
    r.together().await;
    until("the PC unpaired the phone", 15, || !r.pc.core.debug_conn(|c| tugboat_core::store::is_paired(c, phone_id).unwrap())).await;
    until("the notice is cleared", 15, || r.phone.core.status().message.is_none()).await;
    assert_eq!(r.pc.state(), SyncState::Unpaired);
    assert!(r.pc.has_activity(ActivityAction::Unpaired));
    // And the phone does not keep dialing a PC that no longer knows it.
    assert!(r.phone.core.debug_conn(|c| tugboat_core::store::get_pending_unpair(c).unwrap().is_none()));
    r.shutdown().await;
}

// Same, but the PC had already unpaired the phone: the notice is still "delivered" (the PC
// refuses the phone as unknown), so the phone stops owing it.
#[tokio::test(flavor = "multi_thread")]
async fn an_unpair_notice_to_a_pc_that_already_forgot_us_is_cleared() {
    let r = Rig::new().await;
    let phone_id = r.phone.id;
    r.apart().await;
    r.pc.core.unpair().await.unwrap();
    r.phone.core.unpair().await.unwrap();
    assert!(r.phone.core.debug_conn(|c| tugboat_core::store::get_pending_unpair(c).unwrap().is_some()));
    r.together().await;
    until("the notice is cleared", 15, || r.phone.core.debug_conn(|c| tugboat_core::store::get_pending_unpair(c).unwrap().is_none())).await;
    assert!(!r.pc.core.debug_conn(|c| tugboat_core::store::is_paired(c, phone_id).unwrap()));
    r.shutdown().await;
}

// Silent failure guard for the Lost fix: a file that stays missing on the PC is asked for again
// on every connect, but the user hears about it once (one error, one activity row), not on every retry.
#[tokio::test(flavor = "multi_thread")]
async fn a_still_missing_file_is_retried_but_reported_once() {
    let r = Rig::new().await;
    r.apart().await;
    let id = r.pc.add("gone.jpg", 7, 6_000);
    let held = std::path::PathBuf::from(r.pc.view(id).unwrap().path.expect("PC holds the file"));
    let hidden = held.with_extension("away");
    std::fs::rename(&held, &hidden).unwrap();

    for round in 0..3 {
        r.together().await;
        until("the phone knows the item", 15, || r.phone.has_item(id)).await;
        until("lost on the phone", 15, || r.phone.local(id) == Some(LocalFileState::Lost)).await;
        // Let the retry for this connection happen before leaving again.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let lost_rows = r.phone.activity_text().iter().filter(|t| t.starts_with("Lost")).count();
        assert_eq!(lost_rows, 1, "round {round}: one Lost row, {:?}", r.phone.activity_text());
        let errs = r.phone.error_kinds().iter().filter(|k| *k == "file_gone").count();
        assert_eq!(errs, 1, "round {round}: one error, {:?}", r.phone.errors());
        r.apart().await;
    }
    // The file comes back: the next connect fetches it.
    std::fs::rename(&hidden, &held).unwrap();
    r.together().await;
    until("fetched after all", 15, || r.phone.local(id) == Some(LocalFileState::Present)).await;
    r.shutdown().await;
}

// An unpair that fails on this device must tell nobody: the peer would drop the pairing while this
// device kept it, and the caller would see success.
#[tokio::test(flavor = "multi_thread")]
async fn an_unpair_that_fails_locally_is_an_error_and_the_phone_stays_paired() {
    let r = Rig::new().await;
    r.pc.core.debug_conn(|c| {
        c.execute_batch("CREATE TEMP TRIGGER no_unpair BEFORE UPDATE OF unpaired_at ON main.device BEGIN SELECT RAISE(ABORT, 'test: unpair blocked'); END").unwrap()
    });
    assert!(r.pc.core.unpair().await.is_err(), "the caller sees the failure");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(r.phone.core.paired_device().unwrap().is_some(), "the phone was not told");
    assert!(r.pc.core.paired_device().unwrap().is_some(), "the PC is still paired");
    // With the fault gone the same call works and both sides end unpaired.
    r.pc.core.debug_conn(|c| c.execute_batch("DROP TRIGGER temp.no_unpair").unwrap());
    r.pc.core.unpair().await.unwrap();
    until("the phone unpaired", 10, || r.phone.core.paired_device().unwrap().is_none()).await;
    r.shutdown().await;
}

// The phone owes an unpair notice to an old PC that is away, and its network restarts meanwhile:
// the old PC is told once it is back. (The unit test `stopping_the_network_ends_the_notice_task`
// in sync/net.rs pins the mechanism: the notice task goes with the network it was started for.)
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_unpair_notice_survives_a_network_restart() {
    let r = Rig::new().await;
    let phone_id = r.phone.id;
    r.apart().await;
    r.phone.core.unpair().await.unwrap();
    r.pc.stop().await; // the old PC is away

    // The phone pairs with a second PC while the first is unreachable: its notice stays owed.
    let pc2 = Dev::new(Role::Pc, "SECOND-PC", &TestClock::new(T0));
    pair(&pc2, &r.phone).await;
    assert!(r.phone.core.debug_conn(|c| tugboat_core::store::get_pending_unpair(c).unwrap().is_some()), "the first PC has not been told");

    // The notice task is dialing or in back-off; restart the phone's network, and only then bring
    // the old PC back.
    tokio::time::sleep(Duration::from_secs(7)).await;
    r.phone.stop().await;
    r.phone.start().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    r.pc.start().await;
    until("the old PC was told", 60, || !r.pc.core.debug_conn(|c| tugboat_core::store::is_paired(c, phone_id).unwrap())).await;
    until("the notice is cleared", 20, || r.phone.core.debug_conn(|c| tugboat_core::store::get_pending_unpair(c).unwrap().is_none())).await;
    pc2.stop().await;
    r.shutdown().await;
}

// A committed retention change is announced even if the sweep after it fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sweep_after_a_retention_change_still_announces_the_change() {
    let pc = Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0));
    pc.core.debug_conn(|c| c.execute_batch("ALTER TABLE item RENAME TO item_hidden").unwrap());
    let r = pc.core.set_retention(INBOX_ID, Retention::Secs { secs: 3_600 });
    assert!(r.is_err(), "the sweep could not run");
    until("FoldersChanged", 5, || pc.has_event(|e| matches!(e, Event::FoldersChanged))).await;
    pc.core.debug_conn(|c| c.execute_batch("ALTER TABLE item_hidden RENAME TO item").unwrap());
    let inbox = pc.core.folders().unwrap().into_iter().find(|f| f.id == INBOX_ID).unwrap();
    assert_eq!(inbox.retention, Retention::Secs { secs: 3_600 }, "the change itself is committed");
}

// Items already moved to the Bin by a batch that fails part-way still get their event and wake the
// sync session.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_files_batch_that_fails_late_still_reports_the_ones_it_did() {
    let pc = Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0));
    let id = pc.add("a.jpg", 1, 2_000);
    let unknown = tugboat_core::ItemId(uuid::Uuid::from_u128(0xDEAD));
    let n = pc.count_events(|e| matches!(e, Event::ItemsChanged { .. }));
    assert!(pc.core.local_files_missing(&[id, unknown]).is_err());
    until("ItemsChanged for the first id", 5, || {
        pc.events().iter().any(|e| matches!(e, Event::ItemsChanged { ids } if ids == &vec![id])) && pc.count_events(|e| matches!(e, Event::ItemsChanged { .. })) > n
    })
    .await;
    assert!(pc.bin().iter().any(|v| v.id == id), "the first item is in the Bin");
}

fn thumb_file(dir: &Path, name: &str, seed: u64) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, jpeg_bytes(seed, 600)).unwrap();
    p
}

fn thumbs_of(pc: &Dev) -> Vec<String> {
    let dir = pc.dir.path().join("data").join("thumbs");
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

// Two imports of the same thumbnail bytes at once: both succeed, nothing is left half-written.
#[test]
fn concurrent_adds_with_the_same_thumbnail_both_succeed() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let pc = {
        let _g = rt.enter();
        Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0))
    };
    for round in 0..15u64 {
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let thumb = thumb_file(pc.dir.path(), &format!("thumb{round}.jpg"), 1_000 + round);
        let handles: Vec<_> = (0..4u64)
            .map(|i| {
                let (core, barrier, thumb) = (pc.core.clone(), barrier.clone(), thumb.clone());
                let src = pc.jpeg(&format!("r{round}_{i}.jpg"), round * 10 + i, 3_000);
                std::thread::spawn(move || {
                    barrier.wait();
                    core.add_local_file(NewLocalFile {
                        src,
                        name: format!("r{round}_{i}.jpg"),
                        folder: None,
                        captured_at: T0 + round as i64 * 10 + i as i64,
                        geo: None,
                        thumb: Some(thumb),
                        how: AddKind::Captured,
                        ntfs: None,
                    })
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("every add succeeds");
        }
    }
    let names = thumbs_of(&pc);
    assert_eq!(names.len(), 15, "one thumbnail per distinct content: {names:?}");
    assert!(names.iter().all(|n| n.ends_with(".jpg")), "no temp file left behind: {names:?}");
}

// A failed add leaves the thumbnail it published: thumbnails are shared by content and another
// import may already point at it.
#[test]
fn a_failed_add_does_not_delete_the_shared_thumbnail() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let pc = {
        let _g = rt.enter();
        Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0))
    };
    let thumb = thumb_file(pc.dir.path(), "t.jpg", 77);
    let hash = blake3::hash(&std::fs::read(&thumb).unwrap()).to_hex().to_string();
    // Not a picture: the add fails after the thumbnail was published.
    let bad = pc.dir.path().join("bad.jpg");
    std::fs::write(&bad, b"this is not an image").unwrap();
    let r = pc.core.add_local_file(NewLocalFile { src: bad, name: "bad.jpg".into(), folder: None, captured_at: T0, geo: None, thumb: Some(thumb.clone()), how: AddKind::Captured, ntfs: None });
    assert!(r.is_err());
    assert_eq!(thumbs_of(&pc), vec![format!("{hash}.jpg")], "the thumbnail stays, no temp file");
    // A good import of the same thumbnail afterwards simply reuses it.
    let ok = pc.core.add_local_file(NewLocalFile { src: pc.jpeg("good.jpg", 5, 2_000), name: "good.jpg".into(), folder: None, captured_at: T0, geo: None, thumb: Some(thumb), how: AddKind::Captured, ntfs: None });
    assert!(ok.is_ok());
    assert_eq!(thumbs_of(&pc), vec![format!("{hash}.jpg")]);
}
