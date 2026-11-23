//! One phone with several PCs linked: records and files flow between the PCs through the phone,
//! nothing echoes, a device that is away never holds the others back, and linking, unlinking and
//! logging out each touch only the device named. Real cores over loopback, as in two_cores.rs.

mod common;

use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use common::*;
use tugboat_core::{
    events::{Event, PairingEvent, SyncState},
    store, ActivityAction, CoreError, DeviceId, DeviceRow, ItemId, ItemState, LocalFileState, Retention, Role, TestClock,
};

fn present_on(devs: &[&Dev], id: ItemId) -> bool {
    devs.iter().all(|d| d.local(id) == Some(LocalFileState::Present))
}

fn count_activity(d: &Dev, action: ActivityAction) -> usize {
    d.core.activity(None, 500).expect("activity").iter().filter(|a| a.action == action).count()
}

fn name_of(d: &Dev, id: ItemId) -> String {
    d.view(id).unwrap_or_else(|| panic!("{} has no item", d.name)).name
}

fn log_floor(d: &Dev) -> u64 {
    d.core.debug_conn(|c| store::log_floor(c).expect("log floor"))
}

fn head(d: &Dev) -> u64 {
    d.core.debug_conn(|c| store::head_seq(c).expect("head"))
}

/// What `owner` has acked of `peer`'s log, and what it pulled of the peer's.
fn cursor(owner: &Dev, peer: &Dev) -> Option<(u64, u64)> {
    owner.core.debug_conn(|c| store::get_cursor(c, peer.id).expect("cursor"))
}

fn log_rows_above(d: &Dev, seq: u64) -> i64 {
    d.core.debug_conn(|c| c.query_row("SELECT count(*) FROM change_log WHERE seq > ?1", [seq as i64], |r| r.get(0)).expect("count"))
}

fn unpaired_event(d: &Dev, peer: DeviceId) -> Option<bool> {
    d.events().into_iter().find_map(|e| match e {
        Event::Pairing(PairingEvent::Unpaired { peer: p, by_peer }) if p == peer => Some(by_peer),
        _ => None,
    })
}

/// Whether the Unpaired event for `peer` says the other device did it. Events reach the log a moment after the change.
async fn unpaired_by(d: &Dev, peer: DeviceId) -> bool {
    until("an Unpaired event", 10, || unpaired_event(d, peer).is_some()).await;
    unpaired_event(d, peer).unwrap()
}

fn owed_notices(d: &Dev) -> usize {
    d.core.debug_conn(|c| store::pending_unpairs(c).expect("notices").len())
}

fn no_errors(devs: &[&Dev]) {
    for d in devs {
        assert!(d.error_kinds().is_empty(), "{}: {:?}", d.name, d.errors());
    }
}

// An item made on PC A reaches PC B through the phone: the same bytes on all three, every device's
// receipt on every device, equal digests.
#[tokio::test(flavor = "multi_thread")]
async fn star_item_from_pc_a_reaches_pc_b() {
    let s = StarRig::new(2).await;
    let (a, b) = (&s.pcs[0], &s.pcs[1]);
    let id = a.add("from_a.jpg", 1, 120_000);
    until("the item Present on all three", 20, || present_on(&[a, b, &s.phone], id)).await;
    s.settle().await;

    let want = a.file_bytes(id);
    for d in [b, &s.phone] {
        assert_eq!(d.file_bytes(id), want, "{}: same bytes", d.name);
    }
    let hash = a.record(id).unwrap().content.val.hash;
    for d in [a, b, &s.phone] {
        for who in [a.id, b.id, s.phone.id] {
            assert!(d.has_receipt(hash, who), "{} lacks a receipt", d.name);
        }
    }
    let dg = s.phone.digest();
    assert!(s.pcs.iter().all(|p| p.digest() == dg));
    no_errors(&[a, b, &s.phone]);
    s.shutdown().await;
}

// A phone item reaches every PC within 2 s: a change on the phone wakes every session, not one.
#[tokio::test(flavor = "multi_thread")]
async fn star_phone_item_reaches_all_pcs_within_2s() {
    let s = StarRig::new(3).await;
    let id = s.phone.add("from_phone.jpg", 2, 200_000);
    until("the item Present on every PC", 2, || s.pcs.iter().all(|p| p.local(id) == Some(LocalFileState::Present))).await;
    s.settle().await;
    s.shutdown().await;
}

// Once settled nothing moves: the heads and digests stay put and nothing fails, so no record
// bounces between the phone and the PCs.
#[tokio::test(flavor = "multi_thread")]
async fn star_no_echo_after_settle() {
    let s = StarRig::new(2).await;
    let id = s.pcs[0].add("quiet.jpg", 3, 50_000);
    until("the item on all three", 20, || present_on(&[&s.pcs[0], &s.pcs[1], &s.phone], id)).await;
    s.pcs[1].core.rename_item(id, "renamed_on_b.jpg").unwrap();
    until("the rename on every device", 20, || [&s.pcs[0], &s.phone].iter().all(|d| d.view(id).is_some_and(|v| v.name == "renamed_on_b.jpg"))).await;
    s.settle().await;

    let heads: Vec<u64> = std::iter::once(&s.phone).chain(s.pcs.iter()).map(head).collect();
    let digests: Vec<_> = std::iter::once(&s.phone).chain(s.pcs.iter()).map(|d| d.digest()).collect();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let heads_later: Vec<u64> = std::iter::once(&s.phone).chain(s.pcs.iter()).map(head).collect();
    let digests_later: Vec<_> = std::iter::once(&s.phone).chain(s.pcs.iter()).map(|d| d.digest()).collect();
    assert_eq!(heads, heads_later, "no change log row was written while idle");
    assert_eq!(digests, digests_later);
    no_errors(&[&s.pcs[0], &s.pcs[1], &s.phone]);
    s.shutdown().await;
}

/// Both PCs rename the same item while away; they come back in the given order.
async fn concurrent_rename(first: usize) {
    let s = StarRig::new(2).await;
    let id = s.phone.add("a.jpg", 10, 5_000);
    until("the item on both PCs", 20, || present_on(&[&s.pcs[0], &s.pcs[1]], id)).await;
    s.settle().await;

    s.apart(0).await;
    s.apart(1).await;
    s.pcs[0].core.rename_item(id, "from_a_1.jpg").unwrap();
    s.pcs[0].core.rename_item(id, "from_a_2.jpg").unwrap(); // A edits twice: the higher version
    s.pcs[1].core.rename_item(id, "from_b.jpg").unwrap();
    let want_first = if first == 0 { "from_a_2.jpg" } else { "from_b.jpg" };
    s.together(first).await;
    until("the phone took the first PC's name", 20, || name_of(&s.phone, id) == want_first).await;
    s.together(1 - first).await;
    s.settle().await;

    for d in [&s.pcs[0], &s.pcs[1], &s.phone] {
        assert_eq!(name_of(d, id), "from_a_2.jpg", "{}: the higher version wins whichever PC came back first", d.name);
    }
    assert_eq!(count_activity(&s.phone, ActivityAction::Conflict), 1, "the phone logged one conflict: {:?}", s.phone.activity_text());
    s.shutdown().await;
}

// Last writer by version wins across three devices, in both arrival orders, with one Conflict row on the phone.
#[tokio::test(flavor = "multi_thread")]
async fn star_concurrent_rename_both_orders() {
    concurrent_rename(0).await;
    concurrent_rename(1).await;
}

// A rename on one PC and a move on the other, made while both are away: both survive on all three.
#[tokio::test(flavor = "multi_thread")]
async fn star_rename_on_a_move_on_b_both_survive() {
    let s = StarRig::new(2).await;
    let id = s.phone.add("both.jpg", 11, 5_000);
    let work = s.phone.core.create_folder("Work", Retention::Keep).unwrap();
    until("the item and folder on both PCs", 20, || {
        present_on(&[&s.pcs[0], &s.pcs[1]], id) && s.pcs.iter().all(|p| p.core.folders().unwrap().iter().any(|f| f.id == work))
    })
    .await;
    s.settle().await;

    s.apart(0).await;
    s.apart(1).await;
    s.pcs[0].core.rename_item(id, "renamed_on_a.jpg").unwrap();
    s.pcs[1].core.move_item(id, work).unwrap();
    s.together(0).await;
    s.together(1).await;
    s.settle().await;

    for d in [&s.pcs[0], &s.pcs[1], &s.phone] {
        assert_eq!(name_of(d, id), "renamed_on_a.jpg", "{}: the rename survives", d.name);
        assert_eq!(d.record(id).unwrap().location.val.folder, work, "{}: the move survives", d.name);
    }
    s.shutdown().await;
}

/// PC A keeps an item and PC B deletes it while both are away. The device with the later action
/// (more edits first, so a higher version) decides.
async fn keep_against_delete(keep_is_later: bool) {
    let s = StarRig::new(2).await;
    let id = s.phone.add("contested.jpg", 12, 5_000);
    let other = s.phone.add("other.jpg", 13, 5_000);
    until("both items on both PCs", 20, || present_on(&[&s.pcs[0], &s.pcs[1]], id) && present_on(&[&s.pcs[0], &s.pcs[1]], other)).await;
    s.settle().await;

    s.apart(0).await;
    s.apart(1).await;
    let (later, later_name) = if keep_is_later { (0, "bump_a") } else { (1, "bump_b") };
    s.pcs[later].core.rename_item(other, &format!("{later_name}_1.jpg")).unwrap();
    s.pcs[later].core.rename_item(other, &format!("{later_name}_2.jpg")).unwrap();
    s.pcs[0].core.set_pinned(id, true).unwrap();
    s.pcs[1].core.delete_item(id).unwrap();
    s.together(0).await;
    s.together(1).await;
    s.settle().await;

    for d in [&s.pcs[0], &s.pcs[1], &s.phone] {
        let rec = d.record(id).unwrap();
        if keep_is_later {
            assert!(rec.deleted.val.is_none() && rec.pinned.val, "{}: Keep won", d.name);
            assert_eq!(d.local(id), Some(LocalFileState::Present), "{}", d.name);
        } else {
            assert!(rec.deleted.val.is_some() && !rec.pinned.val, "{}: the delete won", d.name);
            assert_eq!(d.local(id), Some(LocalFileState::Binned), "{}", d.name);
        }
    }
    s.shutdown().await;
}

// Keep against delete across three devices: the same winner everywhere, whichever came later.
#[tokio::test(flavor = "multi_thread")]
async fn star_keep_on_a_delete_on_b_converges() {
    keep_against_delete(true).await;
    keep_against_delete(false).await;
}

// A delete on B puts the item in the Bin on all three; a restore on A brings it back on all three.
#[tokio::test(flavor = "multi_thread")]
async fn star_delete_on_b_bins_everywhere_restore_on_a() {
    let s = StarRig::new(2).await;
    let id = s.phone.add("bin_me.jpg", 14, 5_000);
    until("the item on both PCs", 20, || present_on(&[&s.pcs[0], &s.pcs[1]], id)).await;
    s.settle().await;

    s.pcs[1].core.delete_item(id).unwrap();
    until("the item in the Bin on all three", 20, || {
        [&s.pcs[0], &s.pcs[1], &s.phone].iter().all(|d| d.local(id) == Some(LocalFileState::Binned) && matches!(d.view(id).map(|v| v.state), Some(ItemState::Deleted { .. })))
    })
    .await;
    s.settle().await;

    s.pcs[0].core.restore_item(id).unwrap();
    until("the item restored on all three", 20, || {
        [&s.pcs[0], &s.pcs[1], &s.phone].iter().all(|d| d.local(id) == Some(LocalFileState::Present) && d.view(id).is_some_and(|v| matches!(v.state, ItemState::Live { .. })))
    })
    .await;
    s.settle().await;
    s.shutdown().await;
}

// The expiry time is the same on all three devices, and the item is in the Bin on all three once it passes.
#[tokio::test(flavor = "multi_thread")]
async fn star_expiry_linked_on_three() {
    let s = StarRig::new(2).await;
    let id = s.phone.add("expires.jpg", 15, 5_000);
    until("the item on both PCs", 20, || present_on(&[&s.pcs[0], &s.pcs[1]], id)).await;
    s.settle().await;

    let expiry = |d: &Dev| match d.view(id).map(|v| v.state) {
        Some(ItemState::Live { expires_at }) => expires_at,
        other => panic!("{}: not Live: {other:?}", d.name),
    };
    let at = expiry(&s.phone);
    assert_eq!(at, T0 + DAY);
    assert_eq!(expiry(&s.pcs[0]), at);
    assert_eq!(expiry(&s.pcs[1]), at);

    for d in [&s.phone, &s.pcs[0], &s.pcs[1]] {
        d.clock.advance(2 * DAY);
        d.core.tick().unwrap();
    }
    for d in [&s.phone, &s.pcs[0], &s.pcs[1]] {
        assert!(matches!(d.view(id).map(|v| v.state), Some(ItemState::Expired { .. })), "{}: expired", d.name);
        assert_eq!(d.local(id), Some(LocalFileState::Binned), "{}", d.name);
    }
    s.shutdown().await;
}

// An item that expires before any PC had it names every PC it never reached, in the Bin and in the activity row.
#[tokio::test(flavor = "multi_thread")]
async fn star_expired_item_names_the_pcs_it_never_reached() {
    let s = StarRig::new(2).await;
    s.apart(0).await;
    s.apart(1).await;
    let id = s.phone.add("never_arrived.jpg", 31, 5_000);
    s.phone.clock.advance(2 * DAY);
    s.phone.core.tick().unwrap();
    let v = s.phone.view(id).unwrap();
    assert!(matches!(v.state, ItemState::Expired { .. }));
    assert!(v.never_reached_peer);
    assert_eq!(v.missing_on, vec!["PC-1".to_string(), "PC-2".to_string()]);
    assert!(s.phone.activity_text().iter().any(|t| t.contains("never reached PC-1, PC-2")), "{:?}", s.phone.activity_text());
    s.shutdown().await;
}

// PC B is asked for a file only after the phone holds it: one request to the phone, no early one
// that would come back "not found" and leave the item Lost.
#[tokio::test(flavor = "multi_thread")]
async fn star_holder_test_no_early_blob_request() {
    let s = StarRig::new(2).await;
    let (a, b) = (&s.pcs[0], &s.pcs[1]);
    let id = a.add("big.jpg", 16, 2_000_000);
    until("the file Present on B", 30, || b.local(id) == Some(LocalFileState::Present)).await;
    s.settle().await;

    let hash = a.record(id).unwrap().content.val.hash;
    assert!(s.phone.has_receipt(hash, s.phone.id), "the phone held the file before B got it");
    assert!(s.phone.core.debug_blob_requested(hash));
    assert_eq!(s.phone.core.debug_blob_requests(), 1, "B asked the phone once, and only when the phone held the file");
    assert!(b.error_kinds().is_empty(), "{:?}", b.errors());
    assert_eq!(b.file_bytes(id), a.file_bytes(id));
    s.shutdown().await;
}

// Five PCs adding files at once: the phone never runs more than three downloads, and everything arrives.
#[tokio::test(flavor = "multi_thread")]
async fn star_global_fetch_cap() {
    let s = StarRig::new(5).await;
    let mut ids = Vec::new();
    for (i, pc) in s.pcs.iter().enumerate() {
        for k in 0..2u64 {
            ids.push(pc.add(&format!("f{i}_{k}.jpg"), 100 + i as u64 * 10 + k, 3_000_000));
        }
    }
    until("every file on every device", 180, || {
        ids.iter().all(|id| s.pcs.iter().chain(std::iter::once(&s.phone)).all(|d| d.local(*id) == Some(LocalFileState::Present)))
    })
    .await;
    let peak = s.phone.core.debug_fetch_peak();
    println!("star_global_fetch_cap: most downloads at once on the phone: {peak}");
    assert!(peak <= 3, "the phone ran {peak} downloads at once");
    assert!(peak >= 2, "the downloads never overlapped, so the limit was not tested (peak {peak})");
    s.settle().await;
    s.shutdown().await;
}

// A PC that is away holds the phone's log back: its changes stay, it catches up by sequence number.
#[tokio::test(flavor = "multi_thread")]
async fn star_b_offline_catches_up_by_seq() {
    let s = StarRig::new(2).await;
    let id = s.phone.add("seq.jpg", 17, 5_000);
    until("the item on both PCs", 20, || present_on(&[&s.pcs[0], &s.pcs[1]], id)).await;
    s.settle().await;

    let acked_by_b = cursor(&s.phone, &s.pcs[1]).expect("B has a cursor").1;
    s.apart(1).await;
    for i in 1..=3 {
        s.pcs[0].core.rename_item(id, &format!("r{i}.jpg")).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    until("the phone took the last name", 20, || name_of(&s.phone, id) == "r3.jpg").await;
    s.settle().await;
    assert!(log_floor(&s.phone) <= acked_by_b, "the log was trimmed past what B has");
    assert!(log_rows_above(&s.phone, acked_by_b) >= 1, "the changes B has not seen are still in the log");

    s.together(1).await;
    until("B has the last name", 20, || name_of(&s.pcs[1], id) == "r3.jpg").await;
    s.settle().await;
    no_errors(&[&s.pcs[1]]);
    s.shutdown().await;
}

// A PC away longer than the phone keeps its log for starts again from a snapshot, and misses nothing.
#[tokio::test(flavor = "multi_thread")]
async fn star_b_behind_floor_resnapshots() {
    let s = StarRig::new(2).await;
    let docs = s.pcs[0].core.create_folder("Docs", Retention::Keep).unwrap();
    until("the folder on every device", 20, || [&s.phone, &s.pcs[1]].iter().all(|d| d.core.folders().unwrap().iter().any(|f| f.id == docs))).await;
    s.settle().await;

    let acked_by_b = cursor(&s.phone, &s.pcs[1]).expect("B has a cursor").1;
    s.apart(1).await;
    s.pcs[0].core.rename_folder(docs, "Docs v2").unwrap();
    until("the phone has v2", 20, || s.phone.core.folders().unwrap().iter().any(|f| f.name == "Docs v2")).await;
    s.phone.clock.advance(61 * DAY);
    s.pcs[0].core.rename_folder(docs, "Docs v3").unwrap();
    until("the phone has v3", 20, || s.phone.core.folders().unwrap().iter().any(|f| f.name == "Docs v3")).await;
    until("the phone dropped the rows B was holding", 20, || log_floor(&s.phone) > acked_by_b).await;

    s.together(1).await;
    until("B has v3", 30, || s.pcs[1].core.folders().unwrap().iter().any(|f| f.name == "Docs v3")).await;
    s.settle().await;
    assert!(s.pcs[1].core.folders().unwrap().iter().all(|f| f.name != "Docs v2"));
    no_errors(&[&s.pcs[1]]);
    s.shutdown().await;
}

// A PC that has just been linked has no cursor yet: the phone keeps every row from there on, so its
// snapshot is followed by exactly the changes made meanwhile, however much traffic there is.
#[tokio::test(flavor = "multi_thread")]
async fn star_new_pc_snapshot_not_trimmed_under_traffic() {
    let mut s = StarRig::new(2).await;
    let id = s.pcs[0].add("traffic.jpg", 18, 20_000);
    until("the item on both PCs", 20, || present_on(&[&s.pcs[0], &s.pcs[1]], id)).await;
    s.settle().await;

    // C is linked and has not synced yet: stop it before its first snapshot counts, wipe both cursors.
    let c = s.add_pc().await;
    s.pcs[c].stop().await;
    let phone_id = s.phone.id;
    s.phone.core.debug_conn(|conn| store::delete_cursor(conn, s.pcs[c].id).unwrap());
    s.pcs[c].core.debug_conn(|conn| store::delete_cursor(conn, phone_id).unwrap());

    let floor_before = log_floor(&s.phone);
    for i in 0..6 {
        s.pcs[0].core.rename_item(id, &format!("t{i}.jpg")).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    until("the phone has the last name", 20, || name_of(&s.phone, id) == "t5.jpg").await;
    s.pcs[1].core.rename_item(id, "t_b.jpg").unwrap();
    until("the phone has B's name", 20, || name_of(&s.phone, id) == "t_b.jpg").await;
    let (floor, head) = (log_floor(&s.phone), head(&s.phone));
    assert_eq!(floor, floor_before, "a PC with no cursor holds the log where it is");
    assert_eq!(log_rows_above(&s.phone, floor), (head - floor) as i64, "no row above the floor is gone");

    // C comes up while changes keep arriving.
    let traffic = async {
        for i in 0..6 {
            s.pcs[0].core.rename_item(id, &format!("late{i}.jpg")).unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    };
    tokio::join!(s.together(c), traffic);
    until("C has the last name", 30, || s.pcs[c].view(id).is_some_and(|v| v.name == "late5.jpg")).await;
    s.settle().await;
    no_errors(&[&s.pcs[c]]);
    s.shutdown().await;
}

// The phone goes down after it took a change from A and before B ever heard of it: B still gets it
// when both are back, because the change is in the phone's log, not only in a wake-up.
#[tokio::test(flavor = "multi_thread")]
async fn star_crash_after_relay_commit() {
    let mut s = StarRig::new(2).await;
    let id = s.pcs[0].add("crash.jpg", 19, 5_000);
    until("the item on both PCs", 20, || present_on(&[&s.pcs[0], &s.pcs[1]], id)).await;
    s.settle().await;

    s.apart(1).await;
    s.pcs[0].core.rename_item(id, "after_crash.jpg").unwrap();
    until("the phone took the name", 20, || name_of(&s.phone, id) == "after_crash.jpg").await;
    s.restart_phone().await;
    s.together(1).await;
    until("B has the name", 30, || name_of(&s.pcs[1], id) == "after_crash.jpg").await;
    s.settle().await;
    no_errors(&[&s.pcs[0], &s.pcs[1]]);
    s.shutdown().await;
}

// Unlinking A does not empty the log: B, behind, still gets the change A made.
#[tokio::test(flavor = "multi_thread")]
async fn star_unlink_does_not_clear_log_other_pc_needs() {
    let s = StarRig::new(2).await;
    let id = s.phone.add("unlink.jpg", 20, 5_000);
    until("the item on both PCs", 20, || present_on(&[&s.pcs[0], &s.pcs[1]], id)).await;
    s.settle().await;

    let acked_by_b = cursor(&s.phone, &s.pcs[1]).expect("B has a cursor").1;
    s.apart(1).await;
    s.pcs[0].core.rename_item(id, "from_a_before_unlink.jpg").unwrap();
    until("the phone took the name", 20, || name_of(&s.phone, id) == "from_a_before_unlink.jpg").await;
    s.phone.core.unpair_device(s.pcs[0].id).await.unwrap();
    assert!(log_floor(&s.phone) <= acked_by_b, "unlinking A trimmed rows B has not seen");

    s.together(1).await;
    until("B has the name", 30, || name_of(&s.pcs[1], id) == "from_a_before_unlink.jpg").await;
    s.shutdown().await;
}

// At five linked PCs a sixth is turned away before it shows a code, and the list stays five.
#[tokio::test(flavor = "multi_thread")]
async fn star_sixth_pc_refused_before_dial() {
    let s = StarRig::new(5).await;
    let pc6 = Dev::new(Role::Pc, "PC-6", &TestClock::new(T0));
    pc6.start().await;
    let payload = pc6.core.begin_pairing(false).unwrap();
    let err = s.phone.core.pair_with(&payload).await.unwrap_err();
    assert!(matches!(err, CoreError::TooManyDevices), "{err:?}");
    assert_eq!(err.to_string(), "LIMIT 5 DEVICES - unlink one first");
    assert!(pc6.code().is_none(), "PC-6 never showed a code");
    until("the Cancelled event", 5, || s.phone.cancelled().is_some()).await;
    assert_eq!(s.phone.cancelled().map(|c| c.0), Some("too_many_devices".to_string()));
    assert_eq!(s.phone.core.linked_devices().len(), 5);
    assert!(pc6.core.paired_device().unwrap().is_none());
    pc6.core.cancel_pairing();
    pc6.stop().await;
    s.shutdown().await;
}

// A PC that is already linked scans again at the limit: it is not a sixth, and it syncs afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn star_relink_same_pc_not_counted() {
    let s = StarRig::new(5).await;
    link(&s.pcs[2], &s.phone, true).await;
    assert_eq!(s.phone.core.linked_devices().len(), 5, "still five");
    s.settle().await;
    let id = s.phone.add("after_relink.jpg", 21, 5_000);
    until("the item on every PC", 30, || s.pcs.iter().all(|p| p.local(id) == Some(LocalFileState::Present))).await;
    s.settle().await;
    s.shutdown().await;
}

// The phone unlinks B while B is connected: B hears it at once and A keeps syncing.
#[tokio::test(flavor = "multi_thread")]
async fn star_unlink_b_connected() {
    let s = StarRig::new(2).await;
    let (a, b) = (&s.pcs[0], &s.pcs[1]);
    s.phone.core.unpair_device(b.id).await.unwrap();
    until("B unlinked itself", 15, || b.core.paired_device().unwrap().is_none()).await;
    assert!(unpaired_by(b, s.phone.id).await, "B was told by the phone");
    assert!(!unpaired_by(&s.phone, b.id).await, "the phone did it itself");
    assert_eq!(owed_notices(&s.phone), 0);
    let devices = s.phone.core.status().devices;
    assert_eq!(devices.len(), 1, "{devices:?}");
    assert_eq!(devices[0].name, "PC-1");

    let id = s.phone.add("after_unlink.jpg", 22, 5_000);
    until("A got the item", 20, || a.local(id) == Some(LocalFileState::Present)).await;
    s.settle().await;
    assert!(b.local(id).is_none(), "B is no longer linked");
    s.shutdown().await;
}

// The phone unlinks B and C while both are away: each is owed a notice and shown as unlinking, A
// keeps syncing, and both notices are delivered when the PCs come back.
#[tokio::test(flavor = "multi_thread")]
async fn star_unlink_b_away_notice() {
    let s = StarRig::new(3).await;
    s.apart(1).await;
    s.apart(2).await;
    s.phone.core.unpair_device(s.pcs[1].id).await.unwrap();
    s.phone.core.unpair_device(s.pcs[2].id).await.unwrap();
    assert_eq!(owed_notices(&s.phone), 2);
    let devices = s.phone.core.status().devices;
    assert_eq!(devices.iter().filter(|d| d.unlinking).count(), 2, "{devices:?}");
    assert_eq!(devices.iter().filter(|d| !d.unlinking).count(), 1);

    let id = s.phone.add("while_away.jpg", 23, 5_000);
    until("A got the item", 20, || s.pcs[0].local(id) == Some(LocalFileState::Present)).await;
    assert_eq!(s.row(0).state, SyncState::Synced);

    s.together(1).await;
    s.together(2).await;
    until("both PCs were told", 40, || owed_notices(&s.phone) == 0).await;
    for i in [1, 2] {
        until("the PC unlinked itself", 10, || s.pcs[i].core.paired_device().unwrap().is_none()).await;
        assert!(unpaired_by(&s.pcs[i], s.phone.id).await, "{}", s.pcs[i].name);
    }
    assert_eq!(s.phone.core.status().devices.len(), 1);
    s.settle().await;
    s.shutdown().await;
}

// B logs out while connected: the phone drops B only, A keeps syncing.
#[tokio::test(flavor = "multi_thread")]
async fn star_b_logs_out_connected() {
    let s = StarRig::new(2).await;
    s.pcs[1].core.unpair().await.unwrap();
    until("the phone dropped B", 15, || s.phone.core.status().devices.len() == 1).await;
    assert!(unpaired_by(&s.phone, s.pcs[1].id).await, "the phone was told by B");
    assert_eq!(s.phone.core.status().devices[0].name, "PC-1");
    assert_eq!(owed_notices(&s.phone), 0, "nothing is owed to a PC that left on its own");
    let id = s.phone.add("after_logout.jpg", 24, 5_000);
    until("A got the item", 20, || s.pcs[0].local(id) == Some(LocalFileState::Present)).await;
    s.settle().await;
    s.shutdown().await;
}

// B logs out while the phone cannot hear it: the phone still lists B until it next dials it, is
// refused as not linked, and drops B.
#[tokio::test(flavor = "multi_thread")]
async fn star_b_logs_out_while_away() {
    let s = StarRig::new(2).await;
    s.apart(1).await;
    s.pcs[1].core.unpair().await.unwrap();
    assert_eq!(s.phone.core.status().devices.len(), 2, "the phone cannot know yet");
    s.together(1).await;
    until("the phone dropped B", 30, || s.phone.core.status().devices.len() == 1).await;
    assert!(unpaired_by(&s.phone, s.pcs[1].id).await);
    assert_eq!(s.phone.core.status().devices[0].name, "PC-1");
    s.settle().await;
    s.shutdown().await;
}

// B links to another phone: the old phone drops B only; A and the old phone are untouched, and B syncs with the new one.
#[tokio::test(flavor = "multi_thread")]
async fn star_b_relinks_to_other_phone() {
    let s = StarRig::new(2).await;
    let phone2 = Dev::new(Role::Phone, "Second phone", &TestClock::new(T0));
    link(&s.pcs[1], &phone2, true).await;
    until("the old phone dropped B", 30, || s.phone.core.status().devices.len() == 1).await;
    assert!(unpaired_by(&s.phone, s.pcs[1].id).await);
    assert_eq!(s.phone.core.status().devices[0].name, "PC-1");
    phone2.wait_state(SyncState::Synced).await;
    s.pcs[1].wait_state(SyncState::Synced).await;
    assert_eq!(s.row(0).state, SyncState::Synced, "A is untouched");

    let id = s.phone.add("old_phone_item.jpg", 25, 5_000);
    until("A got the item", 20, || s.pcs[0].local(id) == Some(LocalFileState::Present)).await;
    assert!(s.pcs[1].local(id).is_none(), "B no longer follows the old phone");
    phone2.stop().await;
    s.shutdown().await;
}

// One PC refusing the version does not hide the others: the aggregate names it, the other row
// stays Synced and keeps syncing.
#[tokio::test(flavor = "multi_thread")]
async fn star_refused_pc_does_not_mask_others() {
    let s = StarRig::new(2).await;
    s.pcs[0].core.debug_set_proto(1);
    s.phone.core.debug_kill_connection_of(s.pcs[0].id);
    until("the phone shows A refused", 20, || s.phone.core.status().state == SyncState::Refused).await;
    let st = s.phone.core.status();
    assert_eq!(st.message.as_deref(), Some("Update Tugboat on PC-1"));
    assert_eq!(s.row(0).state, SyncState::Refused);
    assert_eq!(s.row(0).message.as_deref(), Some("Update Tugboat on PC-1"));
    until("B stays Synced", 10, || s.row_state(1) == Some(SyncState::Synced)).await;

    let id = s.phone.add("b_only.jpg", 26, 5_000);
    until("B got the item", 20, || s.pcs[1].local(id) == Some(LocalFileState::Present)).await;
    assert!(s.pcs[0].local(id).is_none(), "A refused the session");
    s.shutdown().await;
}

// A PC that is away is normal: the phone as a whole is Synced while another PC is, and the away PC shows Offline.
#[tokio::test(flavor = "multi_thread")]
async fn star_one_pc_away_aggregate_synced() {
    let s = StarRig::new(2).await;
    s.apart(1).await;
    assert_eq!(s.row(1).state, SyncState::Offline);
    assert_eq!(s.row(0).state, SyncState::Synced);
    assert_eq!(s.phone.core.status().state, SyncState::Synced);
    let id = s.phone.add("one_away.jpg", 27, 5_000);
    until("A got the item", 20, || s.pcs[0].local(id) == Some(LocalFileState::Present)).await;
    s.settle().await;
    assert_eq!(s.phone.core.status().state, SyncState::Synced);
    assert_eq!(s.phone.core.status().last_synced_at, s.row(0).last_synced_at.max(s.row(1).last_synced_at));
    s.shutdown().await;
}

// A file only B holds, with B away and the phone not yet holding it: A is still Synced (it waits
// for the phone, not for B), the rows count what each device can serve, and it all arrives once B returns.
#[tokio::test(flavor = "multi_thread")]
async fn star_offline_pc_does_not_hold_syncing() {
    let clock = TestClock::new(T0);
    let (phone, free, _fail) = Dev::phone_with_space("Test phone", &clock, u64::MAX);
    let s = StarRig::with_phone(phone, 2).await;
    s.phone.core.debug_set_guard_recheck_ms(100);
    free.store(0, Ordering::SeqCst);

    let id = s.pcs[1].add("only_b.jpg", 28, 40_000);
    until("the phone listed the item, not downloaded", 20, || s.phone.local(id) == Some(LocalFileState::Pending)).await;
    until("A has the record", 20, || s.pcs[0].has_item(id)).await;
    s.apart(1).await;
    until("A's row Synced", 20, || s.row_state(0) == Some(SyncState::Synced)).await;
    assert_eq!(s.row(0).downloading, 0, "A cannot serve the file");
    assert_eq!(s.row(1).downloading, 1, "B can");
    assert_eq!(s.phone.core.status().state, SyncState::Synced);
    assert_eq!(s.phone.core.status().downloading, 1);

    free.store(u64::MAX, Ordering::SeqCst);
    s.together(1).await;
    until("the file on all three", 30, || present_on(&[&s.pcs[0], &s.pcs[1], &s.phone], id)).await;
    s.settle().await;
    s.shutdown().await;
}

// The storage warning belongs to the phone: one PC's session ending does not clear it while another PC's downloads are held back.
#[tokio::test(flavor = "multi_thread")]
async fn star_storage_low_survives_one_session_end() {
    let clock = TestClock::new(T0);
    let (phone, free, _fail) = Dev::phone_with_space("Test phone", &clock, u64::MAX);
    let s = StarRig::with_phone(phone, 2).await;
    s.phone.core.debug_set_guard_recheck_ms(100);
    free.store(0, Ordering::SeqCst);

    let id = s.pcs[0].add("held_back.jpg", 29, 40_000);
    until("the warning is up", 20, || s.phone.core.status().storage_low).await;
    s.apart(1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(s.phone.core.status().storage_low, "B's session ended; A's downloads are still held back");

    free.store(u64::MAX, Ordering::SeqCst);
    until("the file arrived and the warning cleared", 30, || s.phone.local(id) == Some(LocalFileState::Present) && !s.phone.core.status().storage_low).await;
    s.shutdown().await;
}

// A PC without the holder test (an older build) asks the phone for a file the phone does not hold
// yet, hears "not found", marks it Lost, and gets it on its next connection.
#[tokio::test(flavor = "multi_thread")]
async fn star_legacy_pc_without_holder_test() {
    let clock = TestClock::new(T0);
    let (phone, free, _fail) = Dev::phone_with_space("Test phone", &clock, u64::MAX);
    let s = StarRig::with_phone(phone, 2).await;
    s.phone.core.debug_set_guard_recheck_ms(100);
    s.pcs[1].core.debug_disable_holder_test();
    free.store(0, Ordering::SeqCst);

    let id = s.pcs[0].add("early.jpg", 30, 40_000);
    until("B marked it Lost", 20, || s.pcs[1].local(id) == Some(LocalFileState::Lost)).await;
    assert!(s.pcs[1].error_kinds().iter().any(|k| k == "file_gone"), "{:?}", s.pcs[1].errors());

    free.store(u64::MAX, Ordering::SeqCst);
    until("the phone has the file", 30, || s.phone.local(id) == Some(LocalFileState::Present)).await;
    s.pcs[1].core.debug_kill_connection();
    until("B got it on its next connection", 30, || s.pcs[1].local(id) == Some(LocalFileState::Present)).await;
    s.settle().await;
    assert_eq!(s.pcs[1].file_bytes(id), s.pcs[0].file_bytes(id));
    s.shutdown().await;
}

// Measurement, not an assertion on time: what `status()` costs with 20 000 items and five linked
// devices, none of which holds any file (the worst case for the waiting count). The counts are checked.
#[tokio::test(flavor = "multi_thread")]
async fn star_status_cost() {
    const ITEMS: u32 = 20_000;
    const PENDING: u32 = 200;
    let phone = Dev::new(Role::Phone, "Cost phone", &TestClock::new(T0));
    let ids: Vec<DeviceId> = (1..=5u8).map(|n| DeviceId([n; 32])).collect();
    let self_id = phone.id;
    phone.core.debug_conn(|c| {
        for (i, id) in ids.iter().enumerate() {
            let row = DeviceRow {
                id: *id,
                name: format!("PC-{}", i + 1),
                is_self: false,
                role: Role::Pc,
                paired_at: None,
                unpaired_at: None,
                last_seen_at: None,
                last_synced_at: None,
                addresses: Vec::new(),
            };
            store::set_paired(c, &row, T0).unwrap();
        }
        c.execute_batch("BEGIN").unwrap();
        for n in 0..ITEMS + PENDING {
            let v = tugboat_core::Version { lamport: 1, device: self_id };
            let pending = n >= ITEMS;
            let content_dev = if pending { ids[n as usize % 5] } else { self_id };
            let hash = tugboat_core::BlobHash(*blake3::hash(&n.to_le_bytes()).as_bytes());
            let rec = tugboat_core::ItemRecord {
                id: ItemId(uuid::Uuid::from_u128(0x1000 + n as u128)),
                captured_at: T0,
                origin: self_id,
                geo: None,
                name: tugboat_core::Versioned { v, val: format!("cost{n}") },
                content: tugboat_core::Versioned {
                    v: tugboat_core::Version { lamport: 1, device: content_dev },
                    val: tugboat_core::Content { hash, size: 100, kind: tugboat_core::MediaKind::Image, ext: "jpg".into(), thumb: None },
                },
                location: tugboat_core::Versioned { v, val: tugboat_core::Location { folder: tugboat_core::INBOX_ID, entered_at: T0 } },
                pinned: tugboat_core::Versioned { v, val: false },
                deleted: tugboat_core::Versioned { v, val: None },
            };
            store::insert_item(c, &rec, if pending { LocalFileState::Pending } else { LocalFileState::Present }, T0).unwrap();
            if !pending {
                // Device k confirmed the items whose number divides by k + 2.
                for (k, id) in ids.iter().enumerate() {
                    if n % (k as u32 + 2) == 0 {
                        store::insert_receipt(c, &tugboat_core::ReceiptRecord { hash, device: *id, received_at: T0 }, T0).unwrap();
                    }
                }
            }
        }
        c.execute_batch("COMMIT").unwrap();
    });

    let t = Instant::now();
    let st = phone.core.status();
    let first = t.elapsed();
    let t = Instant::now();
    for _ in 0..5 {
        phone.core.status();
    }
    let mean = t.elapsed() / 5;
    println!("star_status_cost: status() with {ITEMS} items and 5 devices: first call {first:?}, mean of the next 5 {mean:?}");

    let missing = |k: u32| (0..ITEMS).filter(|n| n % (k + 2) != 0).count() as u32;
    assert_eq!(st.waiting, (0..ITEMS).filter(|n| (0..5).any(|k| n % (k + 2) != 0)).count() as u32, "items missing on at least one device");
    assert_eq!(st.downloading, PENDING);
    assert_eq!(st.devices.len(), 5);
    for (k, d) in st.devices.iter().enumerate() {
        assert_eq!(d.waiting, missing(k as u32), "{}: waiting", d.name);
        assert_eq!(d.downloading, PENDING / 5, "{}: downloading", d.name);
    }
}
