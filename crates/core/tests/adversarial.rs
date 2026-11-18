//! Adversarial tests: each one names a scenario that could break a rule of the Bin, expiry or
//! sync behaviour; the comment above it states the rule. Network tests bind 127.0.0.1 only
//! (common harness).

mod common;

use common::*;
use tugboat_core::{
    ops::{AddKind, NewLocalFile},
    store, DeviceId, DeviceRow, ItemState, LocalFileState, ReceiptRecord, Retention, Role, TestClock,
};

/// A device that "is or was paired" with a phone that has confirmed `hash`: the expiry clock
/// then runs for items with that hash No network needed.
fn confirm_by_paired_phone(d: &Dev, hash: tugboat_core::BlobHash) {
    let phone = DeviceId([0xBB; 32]);
    d.core.debug_conn(|c| {
        store::set_paired(
            c,
            &DeviceRow {
                id: phone,
                name: "phone".into(),
                is_self: false,
                role: Role::Phone,
                paired_at: None,
                unpaired_at: None,
                last_seen_at: None,
                addresses: Vec::new(),
            },
            T0,
        )
        .expect("paired row");
        store::insert_receipt(c, &ReceiptRecord { hash, device: phone, received_at: T0 }, T0).expect("receipt");
    });
}

// Rule: "Expired and deleted items sit in the Bin for 7 days (setting), then
// that device deletes its copy of the file." and "A kept (pinned) item never expires."
// Removing Keep from an item kept for 10 days must put it in the Bin for the full Bin days.
// Today: binned_at = max(due, receipt seen_at), both 10 days in the past, so the very first
// sweep after the un-Keep PURGES the file at once: it never sits in the Bin at all.
#[tokio::test(flavor = "multi_thread")]
async fn unkeep_of_an_old_item_purges_at_once_instead_of_binning() {
    let clock = TestClock::new(T0);
    let d = Dev::new(Role::Pc, "pc", &clock);
    let id = d.add("kept.jpg", 1, 3_000);
    confirm_by_paired_phone(&d, d.record(id).unwrap().content.val.hash);
    d.core.set_pinned(id, true).unwrap();

    clock.advance(10 * DAY);
    d.core.tick().unwrap();
    assert_eq!(d.view(id).unwrap().state, ItemState::Kept);
    assert_eq!(d.local(id), Some(LocalFileState::Present));

    // The user takes Keep off (Inbox, 24 h). The item may expire now, but into the Bin.
    d.core.set_pinned(id, false).unwrap();
    d.core.tick().unwrap();
    assert_eq!(
        d.local(id),
        Some(LocalFileState::Binned),
        "un-Keep must move the file to the Bin for {} days, not delete it on the spot",
        7
    );
    assert!(d.bin().iter().any(|v| v.id == id), "the item is listed in the Bin so it can be restored");
}

// Rule: "Changing a folder's retention applies to every item in it. If that
// would expire items at once, the app asks first: 'This moves 132 items to the Bin.'" plus the
// 7-day Bin rule above. A Keep folder with 30-day-old items changed to 24 h: the preview says
// the items move to the Bin, but today the facade's sweep purges them immediately (no Bin time).
#[tokio::test(flavor = "multi_thread")]
async fn shortening_folder_retention_purges_old_items_without_bin_time() {
    let clock = TestClock::new(T0);
    let d = Dev::new(Role::Pc, "pc", &clock);
    let work = d.core.create_folder("Work", Retention::Keep).unwrap();
    let src = d.jpeg("site.jpg", 2, 4_000);
    let id = d
        .core
        .add_local_file(NewLocalFile {
            src,
            name: "site.jpg".into(),
            folder: Some(work),
            captured_at: T0,
            geo: None,
            thumb: None,
            how: AddKind::Added,
            ntfs: None,
        })
        .unwrap();
    confirm_by_paired_phone(&d, d.record(id).unwrap().content.val.hash);

    clock.advance(30 * DAY);
    d.core.tick().unwrap();
    assert_eq!(d.view(id).unwrap().state, ItemState::FolderKeep);

    let day = Retention::Secs { secs: 24 * 3600 };
    assert_eq!(d.core.retention_preview(work, day).unwrap(), 1, "the confirmation says 1 item moves to the Bin");
    d.core.set_retention(work, day).unwrap();
    assert_eq!(d.local(id), Some(LocalFileState::Binned), "the item went to the Bin as the confirmation promised, not straight to purge");
    assert!(d.bin().iter().any(|v| v.id == id), "restorable from the Bin");
}

// Rule: "If only the other device still has the file,
// it is fetched from there." A momentary "not found" from the PC (file renamed in Explorer
// before the watcher updated file_ref, library drive briefly offline) marks the phone's item
// Lost FOREVER: no retry on later connects, the phone never sends a receipt, and the PC shows
// "Waiting for phone" for ever while the phone says "File no longer on either device".
#[tokio::test(flavor = "multi_thread")]
async fn transient_not_found_makes_an_item_permanently_lost() {
    let r = Rig::new().await;
    r.apart().await;
    let id = r.pc.add("drop.jpg", 3, 6_000);
    let held = std::path::PathBuf::from(r.pc.view(id).unwrap().path.expect("PC holds the file"));
    let hidden = held.with_extension("away");
    std::fs::rename(&held, &hidden).unwrap();

    r.together().await;
    until("the phone gave up on the file", 20, || r.phone.local(id) == Some(LocalFileState::Lost)).await;

    // The file is back on the PC (watcher caught up / drive reconnected).
    std::fs::rename(&hidden, &held).unwrap();
    r.apart().await;
    r.together().await;
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_secs(20) && r.phone.local(id) != Some(LocalFileState::Present) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        r.phone.local(id),
        Some(LocalFileState::Present),
        "the PC holds the file again, so the phone must fetch it (state now {:?}, PC view {:?})",
        r.phone.local(id),
        r.pc.view(id).map(|v| v.state)
    );
    r.shutdown().await;
}
