//! Scenarios run as two real cores over loopback:
//! both directions, field conflicts, Keep vs expiry while apart, restore vs purge while apart,
//! expiry on time whatever the receipts say (incl. zero paired devices and a re-paired device), a PC edit
//! plus a phone move while apart, live latency. Scenarios S1-S6, S13, S18.

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

// Expiry is on time. The phone's item expires at its due time while the PC
// is away, no "waiting" state ever shows, the Bin and the activity row say it never reached the
// PC, and a PC that comes back after the due time never downloads the expired item.
#[tokio::test(flavor = "multi_thread")]
async fn expiry_is_on_time_while_the_pc_is_away() {
    let r = Rig::unconnected();
    pair(&r.pc, &r.phone).await;
    r.settle().await;
    r.pc.stop().await;
    r.phone.wait_state(SyncState::Offline).await;

    let id = r.phone.add("late.jpg", 40, 6_000);
    let hash = r.phone.record(id).unwrap().content.val.hash;
    r.phone.clock.advance(23 * HOUR);
    r.phone.core.tick().unwrap();
    let v = r.phone.view(id).expect("still in the gallery");
    assert!(matches!(v.state, ItemState::Live { .. }), "before the due time it is live, not waiting: {:?}", v.state);
    assert_eq!(r.phone.core.status().waiting, 1, "the PC has not confirmed it: the status counts it");

    r.phone.clock.advance(HOUR);
    r.phone.core.tick().unwrap();
    assert!(r.phone.items().iter().all(|v| v.id != id), "gone from the gallery at its due time");
    let bin = r.phone.bin();
    assert_eq!(bin.len(), 1);
    assert_eq!(bin[0].state, ItemState::Expired { binned_at: T0 + 24 * HOUR }, "binned at the due time");
    assert!(bin[0].never_reached_peer, "the Bin says the PC never got it");
    assert!(r.phone.activity_text().iter().any(|t| t.starts_with("Expired") && t.contains("never reached TEST-PC")), "{:?}", r.phone.activity_text());
    assert_eq!(r.phone.core.status().waiting, 0, "an expired item is not waiting");

    // The PC comes back after the due time: it gets the record already expired and never downloads it.
    r.pc.clock.advance(25 * HOUR);
    r.pc.start().await;
    until("the PC has the record", 20, || r.pc.has_item(id)).await;
    r.phone.core.reconnect_now();
    r.pc.core.tick().unwrap();
    until("the PC dropped the expired item without downloading it", 20, || r.pc.local(id) == Some(LocalFileState::Purged)).await;
    assert!(!r.pc.has_receipt(hash, r.pc.id), "the PC never held the file");
    assert!(r.pc.bin().is_empty() || r.pc.view(id).is_some_and(|v| v.path.is_none()));
    assert!(r.pc.items_dir().read_dir().map(|mut d| d.next().is_none()).unwrap_or(true), "nothing stored on the PC");
    r.shutdown().await;
}

// S5b (0.2.0): a device that was never paired expires on time too, a day later and a year later.
#[tokio::test(flavor = "multi_thread")]
async fn s05b_never_paired_expires_on_time() {
    let clock = TestClock::new(T0);
    let d = Dev::new(Role::Phone, "alone", &clock);
    let id = d.add("solo.jpg", 41, 3_000);
    clock.advance(24 * HOUR - 1);
    d.core.tick().unwrap();
    assert!(matches!(d.view(id).unwrap().state, ItemState::Live { .. }));
    clock.advance(1);
    d.core.tick().unwrap();
    assert!(d.view(id).is_some_and(|v| matches!(v.state, ItemState::Expired { .. }) && v.local == LocalFileState::Binned));
    assert_eq!(d.core.status().waiting, 0, "nobody is paired: nothing waits on a peer");
    assert!(!d.bin()[0].never_reached_peer, "no paired device, so no 'never reached' claim");
}

// S5c (0.2.0): a re-paired device. Both items expire on time. The Bin says which one the CURRENT
// phone never confirmed: the old item reached phone two through the snapshot, the fresh one did not.
#[tokio::test(flavor = "multi_thread")]
async fn s05c_repaired_device_expires_on_time_and_says_what_never_reached_the_new_phone() {
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
    // Synced does not mean the snapshot's file is downloaded: wait for phone two's receipt of the old item,
    // or under load the PC still lists the old item as never reaching phone two.
    let old_hash = pc.record(old).unwrap().content.val.hash;
    until("phone two confirmed the old item", 20, || pc.has_receipt(old_hash, phone2.id)).await;
    // Phone two is away; the PC adds a new item that only phone two has not confirmed.
    phone2.stop().await;
    pc.wait_state(SyncState::Offline).await;
    let fresh = pc.add("fresh.jpg", 51, 4_000);
    pc_clock.advance(25 * HOUR);
    pc.core.tick().unwrap();

    let bin = pc.bin();
    let find = |id| bin.iter().find(|v| v.id == id).unwrap_or_else(|| panic!("{id:?} is not in the PC's Bin"));
    assert!(matches!(find(old).state, ItemState::Expired { .. }));
    assert!(matches!(find(fresh).state, ItemState::Expired { .. }), "nothing waits for the new phone");
    assert!(!find(old).never_reached_peer, "phone two got the old item in the snapshot");
    assert!(find(fresh).never_reached_peer, "phone two never confirmed the fresh item");
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
    assert_eq!(s.devices.len(), 1, "one linked device");
    assert_eq!(s.devices[0].name, "TEST-PC");
    assert_eq!(s.devices[0].last_synced_at, Some(synced_at), "the aggregate is the one device's time");

    r.phone.clock.advance(HOUR);
    r.pc.stop().await;
    r.phone.wait_state(SyncState::Offline).await;
    let s = r.phone.core.status();
    assert_eq!(s.last_synced_at, Some(synced_at), "kept while Offline");
    assert_eq!(s.devices[0].state, SyncState::Offline, "the device row says the PC is away");
    assert!(r.phone.has_event(|e| matches!(e, tugboat_core::Event::Sync(x) if x.state == SyncState::Offline)));

    // It survives a restart (stored on the PC's device row, not only in memory).
    let pc_id = r.pc.id;
    assert_eq!(r.phone.core.debug_conn(|c| tugboat_core::store::get_device(c, pc_id).unwrap().unwrap().last_synced_at), Some(synced_at));
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

// ---------- Delete forever ----------

fn tombstone(d: &Dev, id: tugboat_core::ItemId) -> bool {
    d.local(id) == Some(LocalFileState::Purged) && d.record(id).is_some_and(|r| r.purged.is_some()) && d.view(id).is_none()
}

// With no other device linked the same actions work: Delete forever and Empty Bin remove the file
// and the thumbnail here, and refuse an item that is not in the Bin.
#[tokio::test(flavor = "multi_thread")]
async fn delete_forever_with_nothing_linked() {
    let d = Dev::new(Role::Phone, "Alone", &TestClock::new(T0));
    let ids: Vec<_> = (0..3u64).map(|n| d.add_with_thumb(&format!("solo{n}.jpg"), 60 + n, 5_000)).collect();
    let live = d.add("live.jpg", 70, 5_000);
    assert_eq!((d.stored_files(), d.thumb_files()), (4, 3));
    // Only an item in the Bin can be deleted forever.
    assert!(matches!(d.core.purge_item(live), Err(tugboat_core::CoreError::Invalid(_))));
    for id in &ids {
        d.core.delete_item(*id).unwrap();
    }
    let asked = d.core.bin_count().unwrap();
    assert_eq!(asked.count, 3);
    d.core.purge_item(ids[0]).unwrap();
    assert!(tombstone(&d, ids[0]));
    assert_eq!((d.stored_files(), d.thumb_files()), (3, 2), "one file and one thumbnail gone");
    assert_eq!(d.core.bin_count().unwrap().count, 2);
    // The Bin changed since the question (it holds 2, the user was told 3): nothing is deleted.
    assert!(matches!(d.core.empty_bin(asked.count, &asked.token), Err(tugboat_core::CoreError::Invalid(_))));
    let asked = d.core.bin_count().unwrap();
    assert_eq!(asked.count, 2);
    assert_eq!(d.core.empty_bin(asked.count, &asked.token).unwrap(), 2);
    assert!(ids.iter().all(|id| tombstone(&d, *id)));
    assert_eq!((d.stored_files(), d.thumb_files()), (1, 0), "only the live item's file is left");
    assert!(d.bin().is_empty() && d.items().len() == 1);
    let none = d.core.bin_count().unwrap();
    assert_eq!(d.core.empty_bin(none.count, &none.token).unwrap(), 0);
    assert!(matches!(d.core.purge_item(ids[1]), Ok(())), "deleting forever twice is harmless");
    assert!(matches!(d.core.restore_item(ids[1]), Err(tugboat_core::CoreError::NotFound { .. })));
    assert!(d.has_activity(ActivityAction::DeletedForever));
    assert_eq!(d.core.folders().unwrap().iter().find(|f| f.id == INBOX_ID).unwrap().count, 1, "the folder count leaves it out");
}

// Empty Bin deletes the items the question named, not "as many items as the question named": when one
// item left the Bin and another arrived meanwhile the count is the same, and nothing may be deleted.
#[tokio::test(flavor = "multi_thread")]
async fn empty_bin_refuses_when_another_item_took_the_place_of_one_shown() {
    let d = Dev::new(Role::Phone, "Alone", &TestClock::new(T0));
    let (a, b, c, other) = (d.add("a.jpg", 81, 5_000), d.add("b.jpg", 82, 5_000), d.add("c.jpg", 83, 5_000), d.add("d.jpg", 84, 5_000));
    for id in [a, b, c] {
        d.core.delete_item(id).unwrap();
    }
    let asked = d.core.bin_count().unwrap();
    assert_eq!(asked.count, 3);
    // Meanwhile c is restored and d is deleted: the Bin lists three items again, but not the same three.
    d.core.restore_item(c).unwrap();
    d.core.delete_item(other).unwrap();
    let now = d.core.bin_count().unwrap();
    assert_eq!(now.count, asked.count, "the number alone cannot tell the difference");
    assert_ne!(now.token, asked.token);
    assert!(matches!(d.core.empty_bin(asked.count, &asked.token), Err(tugboat_core::CoreError::Invalid(m)) if m.contains("BIN CHANGED")));
    for id in [a, b, other] {
        assert!(d.record(id).is_some_and(|r| r.purged.is_none()), "nothing was deleted");
    }
    assert_eq!(d.stored_files(), 4, "every file is still there");
    assert_eq!(d.bin().len(), 3);
    // Asked again, the question names the items the Bin holds now, and that goes through.
    assert_eq!(d.core.empty_bin(now.count, &now.token).unwrap(), 3);
    assert!(d.record(c).is_some_and(|r| r.purged.is_none()), "the restored item was never part of it");
    assert_eq!(d.stored_files(), 1);
}

// A thumbnail download that was interrupted keeps a partial file to resume; deleting the item forever
// removes that too.
#[tokio::test(flavor = "multi_thread")]
async fn delete_forever_removes_a_partial_thumbnail_download() {
    let d = Dev::new(Role::Phone, "Alone", &TestClock::new(T0));
    let id = d.add_with_thumb("half.jpg", 91, 5_000);
    let hash = d.record(id).unwrap().content.val.thumb.expect("a thumbnail").hash;
    let thumbs = d.dir.path().join("data").join("thumbs");
    let part = thumbs.join(format!("{}.jpg.part", hex::encode(hash.0)));
    std::fs::write(&part, b"half").unwrap();
    d.core.delete_item(id).unwrap();
    d.core.purge_item(id).unwrap();
    assert!(!part.exists(), "the partial thumbnail is gone");
    assert_eq!(d.thumb_files(), 0);
    assert!(std::fs::read_dir(&thumbs).unwrap().all(|e| e.unwrap().path().extension().is_none_or(|x| x != "part")));
}

// An item added with the same thumbnail as one deleted forever a moment ago: if the clean-up removes the
// thumbnail file after the add decided to reuse it and before the record exists, the new item must still
// end up with its thumbnail.
#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_removed_while_an_add_waits_is_put_back() {
    let d = Dev::new(Role::Phone, "Alone", &TestClock::new(T0));
    let first = d.add_with_thumb("first.jpg", 93, 5_000);
    let hash = d.record(first).unwrap().content.val.thumb.expect("a thumbnail").hash;
    let path = d.dir.path().join("data").join("thumbs").join(format!("{}.jpg", hex::encode(hash.0)));
    assert!(path.exists());
    let second = std::thread::scope(|s| {
        // Holds the database for a while, then removes the thumbnail file, as a clean-up would.
        let hold = s.spawn(|| {
            d.core.debug_conn(|_| {
                std::thread::sleep(std::time::Duration::from_millis(600));
                std::fs::remove_file(&path).unwrap();
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        // Decides the thumbnail is there, then waits for the database.
        let add = s.spawn(|| d.add_with_thumb("second.jpg", 93, 5_000));
        hold.join().unwrap();
        add.join().unwrap()
    });
    assert_eq!(d.record(second).unwrap().content.val.thumb.map(|t| t.hash), Some(hash), "the same thumbnail");
    assert!(path.exists(), "the new item's thumbnail file is there");
    assert_eq!(d.thumb_files(), 1);
}

// Thumbnails are shared by content: the file goes only when the last item using it is deleted forever.
#[tokio::test(flavor = "multi_thread")]
async fn a_thumbnail_shared_by_two_items_goes_with_the_last_one() {
    let d = Dev::new(Role::Phone, "Alone", &TestClock::new(T0));
    let shared = d.jpeg("shared-thumb.jpg", 99, 600);
    let add = |name: &str, seed: u64| {
        d.core
            .add_local_file(tugboat_core::ops::NewLocalFile {
                src: d.jpeg(name, seed, 5_000),
                name: name.to_string(),
                folder: None,
                captured_at: d.clock_now(),
                geo: None,
                thumb: Some(shared.clone()),
                how: tugboat_core::ops::AddKind::Captured,
                ntfs: None,
            })
            .unwrap()
    };
    let (a, b) = (add("one.jpg", 1), add("two.jpg", 2));
    assert_eq!(d.thumb_files(), 1, "one thumbnail for both");
    d.core.delete_item(a).unwrap();
    d.core.delete_item(b).unwrap();
    d.core.purge_item(a).unwrap();
    assert_eq!(d.thumb_files(), 1, "item two still shows it");
    assert!(d.view(b).and_then(|v| v.thumb_path).is_some());
    d.core.purge_item(b).unwrap();
    assert_eq!(d.thumb_files(), 0);
}

// A purge made while the phone is away waits in the PC's log and arrives when the phone is back.
#[tokio::test(flavor = "multi_thread")]
async fn delete_forever_on_the_pc_while_the_phone_is_away() {
    let r = Rig::new().await;
    let id = r.phone.add_with_thumb("later.jpg", 61, 30_000);
    until("the item and thumbnail on the PC", 20, || r.pc.local(id) == Some(LocalFileState::Present) && r.pc.thumb_files() == 1).await;
    r.settle().await;
    r.pc.core.delete_item(id).unwrap();
    until("in the Bin on the phone", 20, || r.phone.local(id) == Some(LocalFileState::Binned)).await;
    r.settle().await;

    r.apart().await;
    r.pc.core.purge_item(id).unwrap();
    assert!(tombstone(&r.pc, id));
    assert_eq!(r.phone.local(id), Some(LocalFileState::Binned), "the phone has not heard yet");
    r.together().await;
    until("the phone deleted its copy too", 30, || tombstone(&r.phone, id)).await;
    until("no file and no thumbnail on either device", 20, || r.phone.stored_files() == 0 && r.phone.thumb_files() == 0 && r.pc.stored_files() == 0 && r.pc.thumb_files() == 0).await;
    r.settle().await;
    assert!(r.phone.error_kinds().is_empty() && r.pc.error_kinds().is_empty(), "{:?} {:?}", r.phone.errors(), r.pc.errors());
    r.shutdown().await;
}

// The tombstone a purge leaves keeps no personal data on either device: no name, no saved location, no
// thumbnail, the same record on both, equal digests.
#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_forever_item_keeps_no_name_or_location_on_either_device() {
    let r = Rig::new().await;
    let id = r.phone.add_with_geo("Patient_ward_3.jpg", 63, 20_000);
    until("the item, its location and thumbnail on the PC", 20, || {
        r.pc.local(id) == Some(LocalFileState::Present) && r.pc.thumb_files() == 1 && r.pc.record(id).is_some_and(|x| x.geo.is_some())
    })
    .await;
    r.settle().await;
    assert!(r.phone.record(id).unwrap().geo.is_some() && r.phone.record(id).unwrap().name.val == "Patient_ward_3.jpg");
    r.pc.core.delete_item(id).unwrap();
    until("in the Bin on the phone", 20, || r.phone.local(id) == Some(LocalFileState::Binned)).await;
    r.settle().await;

    r.phone.core.purge_item(id).unwrap();
    until("deleted forever on the PC", 20, || tombstone(&r.pc, id)).await;
    r.settle().await;
    assert_eq!(r.pc.digest(), r.phone.digest(), "equal digests after the purge");
    for d in [&r.phone, &r.pc] {
        let rec = d.record(id).expect("the tombstone stays");
        assert!(rec.name.val.is_empty(), "{}: no name", d.name);
        assert!(rec.geo.is_none(), "{}: no location", d.name);
        assert!(rec.content.val.thumb.is_none() && d.thumb_files() == 0, "{}: no thumbnail", d.name);
        assert!(rec.purged.is_some());
    }
    assert_eq!(r.phone.record(id), r.pc.record(id), "the same record on both devices");
    // The Activity rows no longer name it.
    for d in [&r.phone, &r.pc] {
        assert!(d.activity_text().iter().all(|t| !t.contains("Patient_ward_3")), "{}: {:?}", d.name, d.activity_text());
    }
    r.shutdown().await;
}

// The timed clean-up of the Bin stays what it was: each device on its own clock, deleting only its
// own copy. It is not a purge and does not travel.
#[tokio::test(flavor = "multi_thread")]
async fn the_timed_bin_clean_up_is_not_a_purge() {
    let r = Rig::new().await;
    let id = r.phone.add("timed.jpg", 62, 5_000);
    until("the item on the PC", 20, || r.pc.local(id) == Some(LocalFileState::Present)).await;
    r.settle().await;
    r.phone.core.delete_item(id).unwrap();
    until("in the Bin on the PC", 20, || r.pc.local(id) == Some(LocalFileState::Binned)).await;
    r.settle().await;

    r.phone.clock.advance(8 * DAY);
    r.phone.core.tick().unwrap();
    assert_eq!(r.phone.local(id), Some(LocalFileState::Purged), "the phone's Bin days passed: its copy is gone");
    assert!(r.phone.record(id).unwrap().purged.is_none(), "but the record is not deleted forever");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(r.pc.local(id), Some(LocalFileState::Binned), "the PC keeps its copy for its own Bin days");
    assert!(r.pc.bin().iter().any(|v| v.id == id));
    r.settle().await;
    r.shutdown().await;
}
