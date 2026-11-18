//! Adversarial tests for 0.2.0 on the PC: each one states a rule and tries to break it with two
//! real cores on loopback (127.0.0.1). A failing test means the rule does not hold.

mod common;

use std::{path::PathBuf, time::Duration};

use common::*;
use tugboat_core::{
    ops::{AddKind, NewLocalFile},
    CoreError, ItemId, ItemState, LocalFileState, Retention, Settings, TestClock,
};

fn add_bytes(dev: &Dev, name: &str, bytes: &[u8]) -> ItemId {
    let dir = dev.dir.path().join("adv020-src");
    std::fs::create_dir_all(&dir).unwrap();
    let src: PathBuf = dir.join(format!("{}-{}", bytes.len(), blake3::hash(name.as_bytes()).to_hex()));
    std::fs::write(&src, bytes).unwrap();
    dev.core
        .add_local_file(NewLocalFile { src, name: name.to_string(), folder: None, captured_at: dev.clock_now(), geo: None, thumb: None, how: AddKind::Added, ntfs: None })
        .unwrap_or_else(|e| panic!("add {name}: {e}"))
}

fn present(d: &Dev, id: ItemId) -> bool {
    d.local(id) == Some(LocalFileState::Present)
}

/// Known bug "QR older than 2 minutes says 'That is not a Tugboat code'". The desktop app opens a
/// NEW pairing window when the old one runs out, so a phone holding the old
/// QR reaches an OPEN pairing endpoint with a stale secret. The payload's own expiry has passed by
/// the phone's clock, so the core must say "expired", not "wrong pairing code".
#[tokio::test(flavor = "multi_thread")]
async fn adv020_a_stale_qr_after_the_pc_reopened_pairing_says_expired() {
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;
    let old = r.pc.core.begin_pairing(false).unwrap();
    r.pc.clock.advance(3 * 60_000);
    r.phone.clock.advance(3 * 60_000);
    r.pc.core.tick().unwrap();
    // The PC screen shows a fresh QR (new window, new secret).
    let _fresh = r.pc.core.begin_pairing(false).unwrap();
    match r.phone.core.pair_with(&old).await {
        Err(CoreError::Pairing { code, msg }) => {
            assert_eq!(code, "expired", "a stale QR must say expired (msg was {msg:?})");
        }
        other => panic!("expected a pairing refusal, got {:?}", other.map(|d| d.name)),
    }
    r.shutdown().await;
}

/// Storage guard edge: a record whose declared size is over this device's max_blob_bytes must be
/// refused ONCE ("TOO LARGE"), as fetch_item promises. When free space is also short, the guard in
/// `schedule` sees it first and holds it back for ever: the phone shows PHONE STORAGE LOW for a
/// file that will never be taken, and the TOO LARGE refusal never happens.
#[tokio::test(flavor = "multi_thread")]
async fn adv020_an_oversize_record_is_refused_not_held_by_the_storage_guard() {
    let pc = tugboat_core_dev_pc();
    let (phone, _free, _fail) = Dev::phone_with_space("Test phone", &TestClock::new(T0), 3_000);
    phone.core.save_settings(&Settings { storage_guard_bytes: 1_000, max_blob_bytes: 2_000, ..Settings::default() }).unwrap();
    phone.core.debug_set_guard_recheck_ms(100);
    pair(&pc, &phone).await;
    let rig = Rig { pc, phone };
    rig.settle().await;
    let big = add_bytes(&rig.pc, "big.bin", &[7u8; 5_000]);
    until("the phone knows the record", 20, || rig.phone.has_item(big)).await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let st = rig.phone.core.status();
    let local = rig.phone.local(big);
    rig.shutdown().await;
    assert_eq!(local, Some(LocalFileState::Refused), "declared over max_blob_bytes: refused once (storage_low={})", st.storage_low);
    assert!(!st.storage_low, "no PHONE STORAGE LOW banner for a file that is refused anyway");
}

fn tugboat_core_dev_pc() -> Dev {
    Dev::new(tugboat_core::Role::Pc, "TEST-PC", &TestClock::new(T0))
}

/// On-time expiry, then Restore: the phone's photo expires while the PC is away (the PC never
/// got it). Restoring it on the phone must make it live again on both devices and the late PC must
/// then download it (no data lost by the dropped hold when the user acts in time).
#[tokio::test(flavor = "multi_thread")]
async fn adv020_restored_after_on_time_expiry_reaches_the_late_pc() {
    let r = Rig::new().await;
    r.apart().await;
    let id = r.phone.add("late.jpg", 31, 4_000);
    r.phone.clock.advance(25 * HOUR);
    r.pc.clock.advance(25 * HOUR);
    r.phone.core.tick().unwrap();
    let v = r.phone.view(id).unwrap();
    assert!(matches!(v.state, ItemState::Expired { .. }), "expired on time while the PC is away: {:?}", v.state);
    assert!(v.never_reached_peer, "the never-reached note is set");
    r.phone.core.restore_item(id).unwrap();
    assert!(matches!(r.phone.view(id).unwrap().state, ItemState::Live { .. }), "restore makes it live");
    r.together().await;
    until("the late PC downloads the restored item", 25, || present(&r.pc, id)).await;
    assert!(matches!(r.pc.view(id).unwrap().state, ItemState::Live { .. }));
    assert_eq!(r.pc.file_bytes(id), r.phone.file_bytes(id));
    r.shutdown().await;
}

/// Linked expiry: a move on the PC into a 7-day folder resets folder_entered_at; the phone derives
/// the same state from the synced fields, so neither device expires it at the Inbox's 24 h.
#[tokio::test(flavor = "multi_thread")]
async fn adv020_a_move_on_one_device_extends_expiry_on_both() {
    let r = Rig::new().await;
    let id = r.phone.add("moved.jpg", 32, 4_000);
    until("PC holds it", 20, || present(&r.pc, id)).await;
    r.phone.clock.advance(20 * HOUR);
    r.pc.clock.advance(20 * HOUR);
    let week = r.pc.core.create_folder("Week", Retention::Secs { secs: 7 * 24 * 3600 }).unwrap();
    r.pc.core.move_item(id, week).unwrap();
    until("the move reaches the phone", 20, || r.phone.view(id).is_some_and(|v| v.folder == week)).await;
    r.phone.clock.advance(10 * HOUR);
    r.pc.clock.advance(10 * HOUR);
    r.phone.core.tick().unwrap();
    r.pc.core.tick().unwrap();
    for d in [&r.pc, &r.phone] {
        let v = d.view(id).unwrap();
        assert!(matches!(v.state, ItemState::Live { .. }), "{}: still live 30 h after capture: {:?}", d.name, v.state);
        assert_eq!(v.local, LocalFileState::Present, "{}: file still held", d.name);
    }
    r.shutdown().await;
}

/// Any file type, zero bytes, phone -> PC (the any_file tests cover an empty file only PC -> phone).
#[tokio::test(flavor = "multi_thread")]
async fn adv020_a_zero_byte_file_goes_phone_to_pc() {
    let r = Rig::new().await;
    let id = add_bytes(&r.phone, "empty.log", b"");
    until("PC holds the empty file", 20, || present(&r.pc, id)).await;
    assert!(r.pc.file_bytes(id).is_empty());
    r.shutdown().await;
}
