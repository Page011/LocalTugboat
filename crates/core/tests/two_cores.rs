//! M1 scenarios from PLAN "Milestones / M1 Core", run as two real cores over loopback:
//! both directions, field conflicts, Keep vs expiry while apart, restore vs purge while apart,
//! expiry blocked until a receipt (incl. zero paired devices and a re-paired device), a PC edit
//! plus a phone move while apart, live latency. Design test ids S1-S6, S13, S18.

mod common;

use std::time::Instant;

use common::*;
use tugboat_core::{
    events::{Event, PairingEvent, SyncState}, ActivityAction, FolderId, ItemState, LocalFileState, Retention, Role, TestClock, INBOX_ID,
};

fn live(v: &Option<tugboat_core::ItemView>) -> bool {
    matches!(v.as_ref().map(|v| v.state), Some(ItemState::Live { .. }))
}

// S1: a phone item reaches the PC and a PC item reaches the phone; bytes identical; receipts
// on both sides; both Live once the receipts are in.
#[tokio::test(flavor = "multi_thread")]
async fn s01_items_flow_both_ways_with_receipts() {
    let r = Rig::new().await;
    let from_phone = r.phone.add("phone1.jpg", 1, 200_000);
    let from_pc = r.pc.add("pc1.jpg", 2, 150_000);

    until("both items on both devices, Present", 15, || {
        [(&r.pc, from_phone), (&r.phone, from_pc)].iter().all(|(d, id)| d.local(*id) == Some(LocalFileState::Present))
    })
    .await;
    assert_eq!(r.pc.file_bytes(from_phone), r.phone.file_bytes(from_phone), "phone item: bytes identical");
    assert_eq!(r.phone.file_bytes(from_pc), r.pc.file_bytes(from_pc), "PC item: bytes identical");
    assert_eq!(r.pc.file_bytes(from_phone), tugboat_core_jpeg(1, 200_000));

    // Receipts: each device holds both its own and the other's receipt for each file.
    for id in [from_phone, from_pc] {
        let hash = r.pc.record(id).unwrap().content.val.hash;
        until("receipts from both devices on both devices", 15, || {
            [&r.pc, &r.phone].iter().all(|d| d.has_receipt(hash, r.pc.id) && d.has_receipt(hash, r.phone.id))
        })
        .await;
    }
    until("both items Live on both devices", 15, || {
        [from_phone, from_pc].iter().all(|id| live(&r.pc.view(*id)) && live(&r.phone.view(*id)))
    })
    .await;
    r.settle().await;
    assert_eq!(r.pc.items().len(), 2);
    assert_eq!(r.phone.items().len(), 2);
    r.shutdown().await;
}

fn tugboat_core_jpeg(seed: u64, len: usize) -> Vec<u8> {
    common::jpeg_bytes(seed, len)
}

// S2: the same field renamed on both devices while apart: one winner, the same on both, and a
// Conflict row on both. A rename on one side and a pin on the other both survive.
#[tokio::test(flavor = "multi_thread")]
async fn s02_conflicts_and_independent_groups() {
    let r = Rig::new().await;
    let a = r.phone.add("a.jpg", 10, 5_000);
    let b = r.phone.add("b.jpg", 11, 5_000);
    until("items on the PC", 15, || r.pc.local(a) == Some(LocalFileState::Present) && r.pc.local(b) == Some(LocalFileState::Present)).await;
    r.settle().await;

    r.apart().await;
    r.pc.core.rename_item(a, "from_pc.jpg").unwrap();
    r.pc.core.rename_item(a, "from_pc_2.jpg").unwrap(); // PC edits twice: higher lamport
    r.phone.core.rename_item(a, "from_phone.jpg").unwrap();
    // Different groups on one item: rename on the PC, pin on the phone.
    r.pc.core.rename_item(b, "renamed_on_pc.jpg").unwrap();
    r.phone.core.set_pinned(b, true).unwrap();
    r.together().await;
    r.settle().await;

    let (pa, fa) = (r.pc.view(a).unwrap(), r.phone.view(a).unwrap());
    assert_eq!(pa.name, fa.name, "same winner on both devices");
    assert_eq!(pa.name, "from_pc_2.jpg", "the PC's later (higher lamport) edit wins");
    for d in [&r.pc, &r.phone] {
        assert!(d.has_activity(ActivityAction::Conflict), "{} logged the conflict: {:?}", d.name, d.activity_text());
    }
    // Rename vs pin: both survive on both devices.
    for d in [&r.pc, &r.phone] {
        let v = d.view(b).unwrap();
        assert_eq!(v.name, "renamed_on_pc.jpg", "{}: rename survives", d.name);
        assert!(v.pinned && v.state == ItemState::Kept, "{}: pin survives", d.name);
    }
    r.shutdown().await;
}

// S3a: the PC expires an item (clock past due) while the phone pins it apart. On reconnect the
// pin undoes the expiry: the file comes back out of the Bin, Kept on both.
#[tokio::test(flavor = "multi_thread")]
async fn s03a_keep_on_one_side_undoes_expiry_on_the_other() {
    let r = Rig::new().await;
    let id = r.phone.add("keep.jpg", 20, 8_000);
    until("item Live on the PC", 15, || live(&r.pc.view(id))).await;
    r.settle().await;

    r.apart().await;
    r.pc.clock.advance(25 * HOUR);
    r.pc.core.tick().unwrap();
    assert!(r.pc.items().iter().all(|v| v.id != id), "expired: not in the gallery");
    assert!(matches!(r.pc.bin().first().map(|v| v.state), Some(ItemState::Expired { .. })));
    assert_eq!(r.pc.local(id), Some(LocalFileState::Binned), "file moved to the Bin area");
    r.phone.core.set_pinned(id, true).unwrap();

    r.together().await;
    until("the PC returned the file from its Bin", 15, || r.pc.local(id) == Some(LocalFileState::Present)).await;
    for d in [&r.pc, &r.phone] {
        assert_eq!(d.view(id).unwrap().state, ItemState::Kept, "{} Kept", d.name);
    }
    assert!(r.pc.has_activity(ActivityAction::Returned), "{:?}", r.pc.activity_text());
    r.shutdown().await;
}

// S3b: same, but the PC already purged its copy (Bin days passed). The pin brings the item back
// and the PC fetches the file again from the phone.
#[tokio::test(flavor = "multi_thread")]
async fn s03b_keep_after_purge_refetches_the_file() {
    let r = Rig::new().await;
    let id = r.phone.add("keep2.jpg", 21, 9_000);
    until("item Live on the PC", 15, || live(&r.pc.view(id))).await;
    r.settle().await;
    let bytes = r.phone.file_bytes(id);

    r.apart().await;
    // Two steps, as in real life where the sweep runs at each due time: the file enters the Bin
    // at expiry, and is purged Bin days later (the Bin clock is local, so one jump bins only).
    r.pc.clock.advance(25 * HOUR);
    r.pc.core.tick().unwrap();
    assert_eq!(r.pc.local(id), Some(LocalFileState::Binned));
    r.pc.clock.advance(8 * DAY);
    r.pc.core.tick().unwrap();
    assert_eq!(r.pc.local(id), Some(LocalFileState::Purged), "past Bin days: file deleted");
    assert!(r.pc.has_item(id), "the record stays as a tombstone");
    r.phone.core.set_pinned(id, true).unwrap();

    r.together().await;
    until("the PC fetched the file again", 15, || r.pc.local(id) == Some(LocalFileState::Present)).await;
    assert_eq!(r.pc.file_bytes(id), bytes);
    assert_eq!(r.pc.view(id).unwrap().state, ItemState::Kept);
    r.shutdown().await;
}

// S4: the phone purges (Bin days passed) while the PC restores apart. On reconnect the phone
// fetches the file from the PC and the item has a fresh full retention.
#[tokio::test(flavor = "multi_thread")]
async fn s04_restore_beats_purge() {
    let r = Rig::new().await;
    let id = r.phone.add("restore.jpg", 30, 7_000);
    until("item on the PC", 15, || r.pc.local(id) == Some(LocalFileState::Present)).await;
    r.settle().await;
    let bytes = r.phone.file_bytes(id);

    // PC deletes it: Bin on both devices.
    r.pc.core.delete_item(id).unwrap();
    until("phone binned it", 15, || r.phone.local(id) == Some(LocalFileState::Binned)).await;
    r.settle().await;

    r.apart().await;
    r.phone.clock.advance(8 * DAY);
    r.phone.core.tick().unwrap();
    assert_eq!(r.phone.local(id), Some(LocalFileState::Purged));
    // The PC restores at its own (later) time: a fresh retention from that moment.
    r.pc.clock.advance(8 * DAY);
    r.pc.core.restore_item(id).unwrap();
    assert!(matches!(r.pc.view(id).map(|v| v.state), Some(ItemState::Live { .. })));

    r.together().await;
    until("phone fetched the restored file", 15, || r.phone.local(id) == Some(LocalFileState::Present)).await;
    assert_eq!(r.phone.file_bytes(id), bytes);
    let v = r.phone.view(id).expect("visible again");
    assert!(matches!(v.state, ItemState::Live { expires_at } if expires_at == T0 + 8 * DAY + DAY), "fresh full retention: {:?}", v.state);
    r.settle().await;
    r.shutdown().await;
}

// S5a: nothing expires until the other device confirmed the file. The phone's item stays
// Waiting past 24 h while the PC is away; the PC's receipt then expires it, and the Bin clock
// starts at the receipt, not at the due time.
#[tokio::test(flavor = "multi_thread")]
async fn s05a_waiting_until_the_peer_confirms() {
    let r = Rig::unconnected();
    pair(&r.pc, &r.phone).await;
    r.settle().await;
    r.pc.stop().await;
    r.phone.wait_state(SyncState::Offline).await;

    let id = r.phone.add("late.jpg", 40, 6_000);
    r.phone.clock.advance(30 * HOUR);
    r.phone.core.tick().unwrap();
    let v = r.phone.view(id).expect("still in the gallery");
    assert_eq!(v.state, ItemState::Waiting, "no receipt: never expires");
    assert_eq!(v.local, LocalFileState::Present);
    assert_eq!(r.phone.core.status().waiting, 1, "the status counts it");

    r.pc.start().await;
    until("the PC's receipt expired it on the phone", 20, || r.phone.items().iter().all(|v| v.id != id)).await;
    let bin = r.phone.bin();
    let ItemState::Expired { binned_at } = bin[0].state else { panic!("not expired: {:?}", bin[0].state) };
    assert_eq!(binned_at, T0 + 30 * HOUR, "binned when the receipt arrived (late receipt), not at the due time");
    assert_eq!(r.phone.core.status().waiting, 0);
    r.shutdown().await;
}

// S5b: a device that was never paired: nothing expires, even a year later.
#[tokio::test(flavor = "multi_thread")]
async fn s05b_never_paired_nothing_expires() {
    let clock = TestClock::new(T0);
    let d = Dev::new(Role::Phone, "alone", &clock);
    let id = d.add("solo.jpg", 41, 3_000);
    clock.advance(365 * DAY);
    d.core.tick().unwrap();
    let v = d.view(id).expect("still in the gallery");
    assert_eq!(v.state, ItemState::Waiting);
    assert_eq!(v.local, LocalFileState::Present);
    assert!(d.bin().is_empty());
}

// S5c: a re-paired device. The old phone's receipt still counts ("is or was paired"): its item
// expires. An item only the NEW phone has not confirmed waits for the new key's receipt.
#[tokio::test(flavor = "multi_thread")]
async fn s05c_repaired_device_old_receipt_counts_new_item_waits() {
    let pc_clock = TestClock::new(T0);
    let pc = Dev::new(Role::Pc, "TEST-PC", &pc_clock);
    let phone1 = Dev::new(Role::Phone, "phone one", &TestClock::new(T0));
    pair(&pc, &phone1).await;
    let old = phone1.add("old.jpg", 50, 4_000);
    until("old item on the PC", 15, || pc.local(old) == Some(LocalFileState::Present)).await;

    pc.core.unpair().await.unwrap();
    until("phone one unpaired too", 15, || phone1.core.paired_device().unwrap().is_none()).await;
    phone1.stop().await;

    let phone2 = Dev::new(Role::Phone, "phone two", &TestClock::new(T0));
    phone2.start().await;
    // The PC still holds phone one's Code event: wait for a NEW one, or the confirm can land
    // before the PC has read phone two's hello (the window is still Open: not_open).
    let is_code = |e: &Event| matches!(e, Event::Pairing(PairingEvent::Code { .. }));
    let pc_codes = pc.count_events(is_code);
    let payload = pc.core.begin_pairing(false).unwrap();
    let p2 = phone2.core.clone();
    let task = tokio::spawn(async move { p2.pair_with(&payload).await });
    until("code", 10, || pc.count_events(is_code) > pc_codes && phone2.code().is_some()).await;
    pc.core.confirm_pairing(true).await.unwrap();
    task.await.unwrap().unwrap();
    phone2.wait_state(SyncState::Synced).await;
    // Phone two is away; the PC adds a new item that only phone two has not confirmed.
    phone2.stop().await;
    pc.wait_state(SyncState::Offline).await;
    let fresh = pc.add("fresh.jpg", 51, 4_000);
    pc_clock.advance(25 * HOUR);
    pc.core.tick().unwrap();

    assert!(matches!(pc.view(old).unwrap().state, ItemState::Expired { .. }), "phone one's receipt still counts: expired");
    assert_eq!(pc.view(fresh).unwrap().state, ItemState::Waiting, "the new phone has not confirmed it");
    pc.stop().await;
}

// S6: a PC content edit and a phone move made while apart: both survive on both devices and
// the phone downloads the new content.
#[tokio::test(flavor = "multi_thread")]
async fn s06_pc_edit_and_phone_move_both_survive() {
    let r = Rig::new().await;
    let id = r.phone.add("edit.jpg", 60, 20_000);
    until("item on the PC", 15, || r.pc.local(id) == Some(LocalFileState::Present)).await;
    r.settle().await;
    let work: FolderId = r.phone.core.create_folder("Work", Retention::Keep).unwrap();
    until("folder on the PC", 15, || r.pc.core.folders().unwrap().iter().any(|f| f.id == work)).await;
    let old_hash = r.pc.record(id).unwrap().content.val.hash;

    r.apart().await;
    // The PC "edits" the file: new bytes, new hash.
    let edited = r.pc.jpeg("edited.jpg", 61, 25_000);
    r.pc.core.local_content_changed(id, edited).unwrap();
    r.phone.core.move_item(id, work).unwrap();
    r.together().await;

    let want = common::jpeg_bytes(61, 25_000);
    until("phone has the new content", 20, || r.phone.local(id) == Some(LocalFileState::Present) && r.phone.record(id).map(|x| x.content.val.hash) != Some(old_hash)).await;
    r.settle().await;
    for d in [&r.pc, &r.phone] {
        let rec = d.record(id).unwrap();
        assert_ne!(rec.content.val.hash, old_hash, "{}: the PC edit survived", d.name);
        assert_eq!(rec.location.val.folder, work, "{}: the phone move survived", d.name);
    }
    assert_eq!(r.phone.file_bytes(id), want, "phone downloaded the edited bytes");
    r.shutdown().await;
}

// S13: live sync. A 5 MB item added on the PC while connected is on the phone within 2 s.
#[tokio::test(flavor = "multi_thread")]
async fn s13_live_photo_within_two_seconds() {
    let r = Rig::new().await;
    let t0 = Instant::now();
    let id = r.pc.add("live.jpg", 70, 5 * 1024 * 1024);
    until("5 MB item Present on the phone", 10, || r.phone.local(id) == Some(LocalFileState::Present)).await;
    let ms = t0.elapsed().as_millis();
    println!("S13 live latency (debug build, loopback, 5 MB): {ms} ms");
    assert!(ms < 2000, "took {ms} ms");
    assert_eq!(r.phone.file_bytes(id).len(), 5 * 1024 * 1024 + 22);
    r.shutdown().await;
}

// S18: "last synced" is set on an equal digest and kept when Offline; Offline shows at once when
// the peer goes away; the waiting count is in the status.
#[tokio::test(flavor = "multi_thread")]
async fn s18_status_last_synced_and_offline() {
    let r = Rig::new().await;
    let s = r.phone.core.status();
    assert_eq!(s.state, SyncState::Synced);
    let synced_at = s.last_synced_at.expect("last synced time set on an equal digest");
    assert_eq!(synced_at, T0);
    assert_eq!(s.peer_name.as_deref(), Some("TEST-PC"));

    r.phone.clock.advance(HOUR);
    r.pc.stop().await;
    r.phone.wait_state(SyncState::Offline).await;
    let s = r.phone.core.status();
    assert_eq!(s.last_synced_at, Some(synced_at), "kept while Offline");
    assert!(r.phone.has_event(|e| matches!(e, tugboat_core::Event::Sync(x) if x.state == SyncState::Offline)));

    // It survives a restart (stored, not only in memory).
    assert_eq!(r.phone.core.get_value("last_synced_at").unwrap(), Some(synced_at.to_string()));
    r.phone.stop().await;
    let _ = INBOX_ID;
}

// S10 (thumbnails): the device that made the item syncs its thumbnail as a small blob before the
// file; the other device shows it (thumb_path) and a non-JPEG "thumbnail" is refused.
#[tokio::test(flavor = "multi_thread")]
async fn s10c_thumbnail_is_synced_before_the_file() {
    let r = Rig::new().await;
    let src = r.phone.jpeg("with_thumb.jpg", 80, 30_000);
    let thumb = r.phone.jpeg("thumb_src.jpg", 81, 2_000);
    let id = r
        .phone
        .core
        .add_local_file(tugboat_core::ops::NewLocalFile {
            src,
            name: "with_thumb.jpg".into(),
            folder: None,
            captured_at: T0,
            geo: None,
            thumb: Some(thumb),
            how: tugboat_core::ops::AddKind::Captured,
            ntfs: None,
        })
        .unwrap();
    assert!(r.phone.view(id).unwrap().thumb_path.is_some(), "the phone has its own thumbnail");
    until("file and thumbnail on the PC", 15, || r.pc.local(id) == Some(LocalFileState::Present) && r.pc.view(id).is_some_and(|v| v.thumb_path.is_some())).await;
    let path = r.pc.view(id).unwrap().thumb_path.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), common::jpeg_bytes(81, 2_000));
    assert!(r.pc.error_kinds().is_empty(), "{:?}", r.pc.errors());
    r.shutdown().await;
}
