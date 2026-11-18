//! Tests for the verifier's second-round findings: violations and refused dials are surfaced,
//! an unpair made while apart reaches the other device, a missing file is retried quietly.
//! Network tests bind 127.0.0.1 only (common harness).

mod common;

use common::*;
use tugboat_core::{events::SyncState, ActivityAction, LocalFileState};

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
    r.pc.core.debug_conn(|c| c.execute_batch("ALTER TABLE device_hidden RENAME TO device").unwrap());
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
