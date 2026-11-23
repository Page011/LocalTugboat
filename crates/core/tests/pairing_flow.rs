//! Pairing, unpairing, refusals and the fake-phone tool, over loopback. Scenarios S11, S12,
//! S14, S15, S21.

mod common;

use std::{
    future::Future,
    pin::Pin,
    process::{Command, Stdio},
    sync::Arc,
    task::{Context, Poll, Waker},
    time::Duration,
};

use common::*;
use tugboat_core::{
    events::{Event, PairingEvent, SyncState},
    ActivityAction, CoreError, LocalFileState, Role, TestClock,
};
use tugboat_net::{
    node::{AccessGate, Node, NodeConfig, Role as NetRole},
    pairing::{pc_read_hello, pc_send_decision, PairDecision, PairingWindow},
    NetError, ALPN_SYNC,
};

fn pairing_code_of(e: &CoreError) -> String {
    match e {
        CoreError::Pairing { code, .. } => code.clone(),
        other => panic!("expected a pairing error, got {other:?}"),
    }
}

// S11: another protocol number is refused on both devices with the same one-line text, shown
// as a state (never a silent no-sync), logged in the activity, and no record is exchanged.
#[tokio::test(flavor = "multi_thread")]
async fn s11_version_mismatch_is_refused_visibly_on_both_devices() {
    let r = Rig::unconnected();
    r.phone.core.debug_set_proto(tugboat_core::sync::wire::PROTO + 1);
    let from_phone_before_pairing = r.phone.add("secret.jpg", 1, 1_000);
    pair(&r.pc, &r.phone).await;
    r.phone.wait_state(SyncState::Refused).await;
    r.pc.wait_state(SyncState::Refused).await;
    // One PC linked: both devices say "the PC".
    assert_eq!(r.pc.core.status().message.as_deref(), Some("Update Tugboat on the PC"), "{}", r.pc.name);
    assert_eq!(r.phone.core.status().message.as_deref(), Some("Update Tugboat on the PC"), "{}", r.phone.name);
    for d in [&r.pc, &r.phone] {
        assert!(d.has_activity(ActivityAction::SyncError), "{}: {:?}", d.name, d.activity_text());
        assert!(d.errors().iter().any(|(k, m)| k == "version_mismatch" && m.contains("Update Tugboat")), "{}: {:?}", d.name, d.errors());
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!r.pc.has_item(from_phone_before_pairing), "no record crossed the refused connection");
    assert_eq!(r.pc.digest().1, 3, "the PC holds only its default folders");
    r.shutdown().await;
}

// A phone on protocol 2 (0.3.0 or older, before Delete forever) is refused on both devices, names
// itself as the one to update, and exchanges no record with the new PC.
#[tokio::test(flavor = "multi_thread")]
async fn a_protocol_2_phone_is_refused_on_both_devices() {
    let r = Rig::unconnected();
    r.phone.core.debug_set_proto(2);
    let before_pairing = r.phone.add("old_phone.jpg", 1, 1_000);
    pair(&r.pc, &r.phone).await;
    r.phone.wait_state(SyncState::Refused).await;
    r.pc.wait_state(SyncState::Refused).await;
    for d in [&r.pc, &r.phone] {
        assert_eq!(d.core.status().message.as_deref(), Some("Update Tugboat on the phone"), "{}", d.name);
        assert!(d.has_activity(ActivityAction::SyncError), "{}: {:?}", d.name, d.activity_text());
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!r.pc.has_item(before_pairing), "no record crossed the refused connection");
    r.shutdown().await;
}

// S12a: both screens show the same code; the PC completes only on its click.
#[tokio::test(flavor = "multi_thread")]
async fn s12a_codes_match_and_pairing_completes_on_the_click() {
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;
    let payload = r.pc.core.begin_pairing(false).unwrap();
    assert!(payload.starts_with("localtugboat://pair?"));
    let phone = r.phone.core.clone();
    let task = tokio::spawn(async move { phone.pair_with(&payload).await });
    until("codes", 10, || r.pc.code().is_some() && r.phone.code().is_some()).await;
    let code = r.pc.code().unwrap();
    assert_eq!(code.len(), 7, "shown as 482 913: {code}");
    assert_eq!(code, r.phone.code().unwrap());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(r.pc.core.paired_device().unwrap().is_none(), "nothing is paired before the click");
    assert!(!task.is_finished());
    r.pc.core.confirm_pairing(true).await.unwrap();
    let row = task.await.unwrap().unwrap();
    assert_eq!(row.id, r.pc.id);
    assert_eq!(r.pc.core.paired_device().unwrap().unwrap().id, r.phone.id);
    assert_eq!(r.phone.core.paired_device().unwrap().unwrap().name, "TEST-PC");
    r.settle().await;
    r.shutdown().await;
}

// S12b: the PC user clicks Cancel: the phone is told, nothing is stored on either side.
#[tokio::test(flavor = "multi_thread")]
async fn s12b_cancel_on_the_pc_stores_nothing() {
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;
    let payload = r.pc.core.begin_pairing(false).unwrap();
    let phone = r.phone.core.clone();
    let task = tokio::spawn(async move { phone.pair_with(&payload).await });
    until("codes", 10, || r.pc.code().is_some() && r.phone.code().is_some()).await;
    r.pc.core.confirm_pairing(false).await.unwrap();
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(pairing_code_of(&err), "cancelled");
    until("phone event", 5, || r.phone.cancelled().is_some()).await;
    assert_eq!(r.phone.cancelled().unwrap(), ("cancelled".into(), "Pairing cancelled".into()));
    until("PC event", 5, || r.pc.cancelled().is_some()).await;
    assert_eq!(r.pc.cancelled().unwrap().0, "cancelled");
    for d in [&r.pc, &r.phone] {
        assert!(d.core.paired_device().unwrap().is_none(), "{}: nothing stored", d.name);
        assert!(!d.completed());
    }
    let r2 = r.pc.core.confirm_pairing(true).await.unwrap_err();
    assert_eq!(pairing_code_of(&r2), "not_open", "a cancelled window cannot be completed");
    r.shutdown().await;
}

// S12c: a second phone uses the secret while a code is pending: both phones and the PC see the
// cancellation, the PC says who is to blame in one line, and nobody is paired.
#[tokio::test(flavor = "multi_thread")]
async fn s12c_second_use_of_the_secret_cancels_for_both_phones() {
    let pc = Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0));
    let a = Dev::new(Role::Phone, "phone A", &TestClock::new(T0));
    let b = Dev::new(Role::Phone, "phone B", &TestClock::new(T0));
    pc.start().await;
    a.start().await;
    b.start().await;
    let payload = pc.core.begin_pairing(false).unwrap();
    let (ac, p1) = (a.core.clone(), payload.clone());
    let ta = tokio::spawn(async move { ac.pair_with(&p1).await });
    until("phone A's code on the PC", 10, || pc.code().is_some()).await;
    let bc = b.core.clone();
    let tb = tokio::spawn(async move { bc.pair_with(&payload).await });

    let (ra, rb) = (ta.await.unwrap().unwrap_err(), tb.await.unwrap().unwrap_err());
    assert_eq!(pairing_code_of(&ra), "secret_reused");
    assert_eq!(pairing_code_of(&rb), "secret_reused");
    until("the PC saw the cancellation", 10, || pc.cancelled().is_some()).await;
    assert_eq!(pc.cancelled().unwrap(), ("secret_reused".into(), "PAIRING BLOCKED - someone else used this code".into()));
    for d in [&a, &b] {
        until("the phone saw the cancellation", 10, || d.cancelled().is_some()).await;
        assert_eq!(d.cancelled().unwrap().0, "secret_reused", "{}", d.name);
        assert!(d.core.paired_device().unwrap().is_none());
    }
    assert!(pc.core.paired_device().unwrap().is_none());
    assert!(pc.core.confirm_pairing(true).await.is_err(), "no click can complete it now");
    a.stop().await;
    b.stop().await;
    pc.stop().await;
}

// S12d: an expired window refuses; a wrong secret is refused and the window stays open for the
// right one; an already paired PC needs replace_existing. The phone does not replace a PC: a second
// PC links beside the first.
#[tokio::test(flavor = "multi_thread")]
async fn s12d_expiry_wrong_secret_and_replace_rules() {
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;

    // Wrong secret: refused, window still open.
    let payload = r.pc.core.begin_pairing(false).unwrap();
    let (before, rest) = payload.split_once("&s=").unwrap();
    let wrong = format!("{before}&s={}{}", "0".repeat(32), &rest[32..]);
    let err = r.phone.core.pair_with(&wrong).await.unwrap_err();
    assert_eq!(pairing_code_of(&err), "bad_secret");
    assert!(r.pc.cancelled().is_none(), "the window is still open");

    // Expiry: the PC clock passes the 2 minutes.
    r.pc.clock.advance(121_000);
    r.pc.core.tick().unwrap();
    until("the PC saw the expiry", 10, || r.pc.cancelled().is_some()).await;
    assert_eq!(r.pc.cancelled().unwrap().0, "expired");
    assert_eq!(r.pc.cancelled().unwrap().1, "PAIRING CODE EXPIRED - show a new QR");
    let err = r.phone.core.pair_with(&payload).await.unwrap_err();
    assert_eq!(pairing_code_of(&err), "not_open", "the PC no longer offers the pairing protocol");

    // A fresh window works; after it, a second one needs replace_existing.
    let payload = r.pc.core.begin_pairing(false).unwrap();
    let phone = r.phone.core.clone();
    let task = tokio::spawn(async move { phone.pair_with(&payload).await });
    until("the PC shows the code", 10, || r.pc.code().is_some()).await;
    r.pc.core.confirm_pairing(true).await.unwrap();
    task.await.unwrap().unwrap();
    assert!(matches!(r.pc.core.begin_pairing(false), Err(CoreError::AlreadyPaired)));
    assert!(r.pc.core.begin_pairing(true).is_ok(), "replace_existing opens a window");
    // A garbage QR is a pairing error, not a crash.
    let err = r.phone.core.pair_with("hello").await.unwrap_err();
    assert_eq!(pairing_code_of(&err), "bad_payload");

    // The phone links a second PC; the first stays linked and is told nothing.
    let pc2 = Dev::new(Role::Pc, "SECOND-PC", &TestClock::new(T0));
    pair(&pc2, &r.phone).await;
    assert_eq!(r.phone.core.status().devices.len(), 2, "both PCs are linked");
    assert!(r.pc.core.paired_device().unwrap().is_some(), "the first PC is still linked");
    assert!(!r.pc.has_event(|e| matches!(e, Event::Pairing(PairingEvent::Unpaired { .. }))), "and was not told it was replaced");
    pc2.stop().await;
    r.shutdown().await;
}

// The PC's confirm waits up to 15 s for the phone to take the decision. A window opened during that
// wait is a new one: confirm must not close it or shut the pairing port behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_window_opened_during_confirm_is_left_alone() {
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;
    let payload = r.pc.core.begin_pairing(false).unwrap();
    let phone = r.phone.core.clone();
    let task = tokio::spawn(async move { phone.pair_with(&payload).await });
    until("codes", 10, || r.pc.code().is_some() && r.phone.code().is_some()).await;

    // Run confirm up to its delivery wait, then open a new window while it is parked there.
    let pc = r.pc.core.clone();
    let mut confirm = Box::pin(async move { pc.confirm_pairing(true).await });
    let first = Pin::new(&mut confirm).poll(&mut Context::from_waker(Waker::noop()));
    // Always true, however fast the loopback round trip is: confirm takes the spent window and
    // closes the port before its first await. (The delivery wait itself waits on a transport ACK
    // that a test cannot hold, so on a very slow runner the first poll may be Ready.)
    assert!(!r.pc.core.pairing_open(), "confirm took the spent window before its delivery wait");
    let new_payload = r.pc.core.begin_pairing(true).unwrap();
    match first {
        Poll::Ready(res) => res.unwrap(),
        Poll::Pending => confirm.await.unwrap(),
    }
    task.await.unwrap().unwrap();

    assert!(r.pc.core.pairing_open(), "the new window is still open");
    // And its QR still works: a second phone gets as far as the code.
    let other = Dev::new(Role::Phone, "other phone", &TestClock::new(T0));
    other.start().await;
    let oc = other.core.clone();
    let codes = r.pc.count_events(|e| matches!(e, Event::Pairing(PairingEvent::Code { .. })));
    let t2 = tokio::spawn(async move { oc.pair_with(&new_payload).await });
    until("the second phone's code reaches the PC", 10, || r.pc.count_events(|e| matches!(e, Event::Pairing(PairingEvent::Code { .. }))) > codes).await;
    r.pc.core.confirm_pairing(false).await.unwrap();
    let _ = t2.await;
    other.stop().await;
    r.shutdown().await;
}

// The user taps Cancel on the phone after the PC's decision has arrived: the pairing is already
// stored on the PC, so it must be stored on the phone too. (The cancel used to drop the whole
// exchange while the phone still waited for the PC to close, leaving the PC paired and the phone not.)
#[tokio::test(flavor = "multi_thread")]
async fn cancel_after_the_decision_arrived_still_completes_the_pairing() {
    let phone = Dev::new(Role::Phone, "Test phone", &TestClock::new(T0));
    phone.start().await;
    let node = Node::start(NodeConfig { bind: "127.0.0.1:0".parse().unwrap(), role: NetRole::Pc, mdns: false, secret_key: random_secret() }, Arc::new(AllowAll)).await.unwrap();
    node.set_pairing_open(true);
    let mut window = PairingWindow::open(node.id(), T0).unwrap();
    let payload = window.payload(node.qr_addrs(), "Fake PC").encode();

    // A PC that accepts at once and then keeps the connection open: the phone's last step waits for the close.
    // Readiness is signalled, never timed: `decided` fires once the whole decision is acknowledged
    // by the phone's transport (so it is buffered there and beats the cancel); the connection
    // stays open until `release`, which the test sends only after the phone is done, so no close
    // can race the phone's read. (The phone leaves by itself 2 s after the PC's stream ends.)
    let (decided_tx, decided_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let fake = node.clone();
    let pc_task = tokio::spawn(async move {
        let acc = fake.accept().await.unwrap().unwrap();
        let (mut tx, hello) = pc_read_hello(&acc.peer, Duration::from_secs(10)).await.unwrap();
        window.on_hello(acc.peer.peer(), &hello, T0 + 1).unwrap();
        window.confirm(T0 + 2).unwrap();
        pc_send_decision(&mut tx, &PairDecision::Accepted { pc_name: "Fake PC".into() }).await.unwrap();
        let _ = decided_tx.send(());
        let _ = release_rx.await;
        drop(acc);
    });

    let cancel = Arc::new(tokio::sync::Notify::new());
    let (core, c2) = (phone.core.clone(), cancel.clone());
    let task = tokio::spawn(async move {
        let mut cancelled = std::pin::pin!(c2.notified());
        cancelled.as_mut().enable();
        core.pair_with_cancel(&payload, &mut cancelled).await
    });
    until("the phone shows the code", 10, || phone.code().is_some()).await;
    tokio::time::timeout(Duration::from_secs(30), decided_rx).await.expect("the decision was delivered in time").unwrap(); // the decision is in; the phone waits for the close
    cancel.notify_waiters();
    let row = task.await.unwrap().expect("the cancel came too late to undo the pairing");
    let _ = release_tx.send(());
    assert_eq!(row.id, tugboat_core::DeviceId(node.id()));
    assert!(phone.core.paired_device().unwrap().is_some(), "the phone stored the pairing");
    pc_task.abort();
    phone.stop().await;
    node.shutdown().await;
}

// And before the decision a cancel still ends the exchange at once.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_before_the_decision_ends_the_pairing_and_stores_nothing() {
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;
    let payload = r.pc.core.begin_pairing(false).unwrap();
    let cancel = Arc::new(tokio::sync::Notify::new());
    let (core, c2) = (r.phone.core.clone(), cancel.clone());
    let task = tokio::spawn(async move {
        let mut cancelled = std::pin::pin!(c2.notified());
        cancelled.as_mut().enable();
        core.pair_with_cancel(&payload, &mut cancelled).await
    });
    until("codes", 10, || r.pc.code().is_some() && r.phone.code().is_some()).await;
    cancel.notify_waiters();
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(pairing_code_of(&err), "cancelled");
    until("the phone saw the cancellation", 10, || r.phone.cancelled().is_some()).await;
    assert_eq!(r.phone.cancelled().unwrap().0, "cancelled");
    assert!(r.phone.core.paired_device().unwrap().is_none());
    r.shutdown().await;
}

// S14a: unpair while connected: the other device unpairs too and the connection closes; files stay.
#[tokio::test(flavor = "multi_thread")]
async fn s14a_unpair_while_connected() {
    let r = Rig::new().await;
    let id = r.phone.add("stays.jpg", 7, 4_000);
    until("item on the PC", 15, || r.pc.local(id) == Some(LocalFileState::Present)).await;
    r.settle().await;

    r.pc.core.unpair().await.unwrap();
    until("the phone unpaired itself", 10, || r.phone.core.paired_device().unwrap().is_none()).await;
    assert!(r.pc.core.paired_device().unwrap().is_none());
    // The PC did it; the phone was told by it.
    until("the Unpaired events", 10, || {
        r.pc.has_event(|e| matches!(e, Event::Pairing(PairingEvent::Unpaired { by_peer: false, .. })))
            && r.phone.has_event(|e| matches!(e, Event::Pairing(PairingEvent::Unpaired { by_peer: true, .. })))
    })
    .await;
    for d in [&r.pc, &r.phone] {
        assert_eq!(d.state(), SyncState::Unpaired, "{}", d.name);
        assert!(d.has_activity(ActivityAction::Unpaired));
        assert_eq!(d.core.debug_conn(|c| c.query_row("SELECT count(*) FROM change_log", [], |r| r.get::<_, i64>(0)).unwrap()), 0, "{}: log cleared", d.name);
    }
    assert_eq!(r.pc.local(id), Some(LocalFileState::Present), "synced files stay");
    assert_eq!(r.phone.local(id), Some(LocalFileState::Present));

    // Pairing again: the first sync is a full snapshot both ways.
    let more = r.pc.add("after.jpg", 8, 3_000);
    r.phone.stop().await;
    r.pc.stop().await;
    let r2 = Rig { pc: r.pc, phone: r.phone };
    pair(&r2.pc, &r2.phone).await;
    until("snapshot after re-pairing", 20, || r2.phone.local(more) == Some(LocalFileState::Present)).await;
    r2.settle().await;
    r2.shutdown().await;
}

// S14b: unpaired while apart: the next connect is closed as NOT PAIRED and the phone unpairs itself.
#[tokio::test(flavor = "multi_thread")]
async fn s14b_unpair_while_apart_is_learned_on_the_next_connect() {
    let r = Rig::new().await;
    r.apart().await;
    r.pc.core.unpair().await.unwrap();
    assert!(r.phone.core.paired_device().unwrap().is_some(), "the phone cannot know yet");
    r.together().await;
    until("the phone learned it was unpaired", 15, || r.phone.core.paired_device().unwrap().is_none()).await;
    assert_eq!(r.phone.state(), SyncState::Unpaired);
    assert!(r.phone.has_activity(ActivityAction::Unpaired));
    until("the Unpaired event", 10, || r.phone.has_event(|e| matches!(e, Event::Pairing(PairingEvent::Unpaired { by_peer: true, .. })))).await;
    assert!(r.phone.core.status().devices.is_empty());
    r.shutdown().await;
}

struct AllowAll;
impl AccessGate for AllowAll {
    fn is_paired(&self, _: &[u8; 32]) -> Result<bool, String> {
        Ok(true)
    }
}

// S15: a key that was never paired reaches the sync protocol and is closed as NOT PAIRED, and the
// PC's data is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn s15_unpaired_third_key_gets_nothing() {
    let r = Rig::new().await;
    let id = r.pc.add("private.jpg", 9, 4_000);
    r.settle().await;
    let before = r.pc.digest();

    let stranger = Node::start(NodeConfig { bind: "127.0.0.1:0".parse().unwrap(), role: NetRole::Phone, mdns: false, secret_key: random_secret() }, Arc::new(AllowAll))
        .await
        .unwrap();
    let conn = stranger.dial(r.pc.id.0, &[r.pc.bind()], ALPN_SYNC, Duration::from_secs(5)).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await.expect("the PC closed the connection");
    assert!(matches!(closed, NetError::ClosedByPeer { code: 1, .. }), "closed as NOT_PAIRED: {closed:?}");
    assert_eq!(r.pc.digest(), before, "no record was exchanged or changed");
    assert!(r.pc.core.debug_conn(|c| tugboat_core::store::get_device(c, tugboat_core::DeviceId(stranger.id())).unwrap().is_none()), "the stranger left no trace");
    assert_eq!(r.pc.state(), SyncState::Synced, "the real phone is unaffected");
    assert!(r.pc.has_item(id));
    stranger.shutdown().await;
    r.shutdown().await;
}

// ---------- S21: the fake_phone example against an in-process PC ----------

/// The cargo flags that build into the profile directory this test binary lives in
/// (`target/<profile>/deps/<test>`): `debug` is the dev profile, `release` is `--release`, any
/// other name is a custom profile.
fn profile_args(profile_dir: &str) -> Vec<String> {
    match profile_dir {
        "debug" => vec![],
        "release" => vec!["--release".into()],
        other => vec!["--profile".into(), other.into()],
    }
}

fn fake_phone_exe() -> std::path::PathBuf {
    let me = std::env::current_exe().unwrap();
    let profile_dir = me.parent().and_then(|p| p.parent()).expect("target/<profile>").to_path_buf();
    let exe = profile_dir.join("examples").join(format!("fake_phone{}", std::env::consts::EXE_SUFFIX));
    if !exe.exists() {
        // `cargo test --test x` does not build examples: build this one, in the same profile
        // (a `--release` test run looks in target/release, not target/debug).
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let profile = profile_dir.file_name().and_then(|n| n.to_str()).expect("profile directory name").to_string();
        let ok = Command::new(cargo)
            .args(["build", "-p", "tugboat-core", "--example", "fake_phone"])
            .args(profile_args(&profile))
            .status()
            .unwrap()
            .success();
        assert!(ok && exe.exists(), "could not build the fake_phone example at {}", exe.display());
    }
    exe
}

#[test]
fn the_example_is_built_in_the_profile_the_test_runs_in() {
    assert!(profile_args("debug").is_empty());
    assert_eq!(profile_args("release"), vec!["--release"]);
    assert_eq!(profile_args("dist"), vec!["--profile", "dist"]);
}

async fn run_fake(args: Vec<String>) -> (bool, String) {
    let exe = fake_phone_exe();
    let child = Command::new(exe).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("start fake_phone");
    let out = tokio::time::timeout(Duration::from_secs(90), tokio::task::spawn_blocking(move || child.wait_with_output()))
        .await
        .expect("fake_phone finished in time")
        .unwrap()
        .unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

// S21: `fake_phone pair`, `send 3` and `list` against a PC core: the user trial tool works.
#[tokio::test(flavor = "multi_thread")]
async fn s21_fake_phone_pairs_sends_and_lists() {
    let pc = Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0));
    pc.start().await;
    let payload = pc.core.begin_pairing(false).unwrap();
    let data = tempfile::tempdir().unwrap();
    let data_arg = data.path().display().to_string();

    let exe = fake_phone_exe();
    let child = Command::new(exe)
        .args(["pair", &payload, "--data", &data_arg])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start fake_phone");
    until("the PC shows the code", 30, || pc.code().is_some()).await;
    pc.core.confirm_pairing(true).await.unwrap();
    let out = tokio::task::spawn_blocking(move || child.wait_with_output()).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}");
    assert!(text.contains(&format!("CODE {}", pc.code().unwrap())), "the tool shows the PC's code: {text}");
    assert!(text.contains("PAIRED with TEST-PC"), "{text}");

    let (ok, text) = run_fake(vec!["send".into(), "3".into(), "--data".into(), data_arg.clone()]).await;
    assert!(ok, "{text}");
    assert!(text.contains("SENT 3 items"), "{text}");
    until("3 items Present on the PC", 20, || pc.items().iter().filter(|v| v.local == LocalFileState::Present).count() == 3).await;

    // Any file type, and a refused-name record, for QA of the PC's galleries and Not synced list.
    let docx = data.path().join("Plan.docx");
    std::fs::write(&docx, b"PK\x03\x04 pretend docx").unwrap();
    let (ok, text) = run_fake(vec!["send-file".into(), docx.display().to_string(), "--data".into(), data_arg.clone()]).await;
    assert!(ok, "{text}");
    assert!(text.contains("SENT 1 file"), "{text}");
    until("the docx is a File item on the PC", 20, || pc.items().iter().any(|v| v.name == "Plan.docx" && v.kind == tugboat_core::MediaKind::File && v.local == LocalFileState::Present)).await;
    let (ok, text) = run_fake(vec!["send-refused".into(), "evil.lnk".into(), "--data".into(), data_arg.clone()]).await;
    assert!(ok, "{text}");
    until("the refused record is listed on the PC", 20, || pc.core.refused_items(50).unwrap().iter().any(|v| v.name == "evil.lnk")).await;

    let (ok, text) = run_fake(vec!["list".into(), "--data".into(), data_arg.clone()]).await;
    assert!(ok, "{text}");
    assert!(text.contains("5 items") || text.contains("4 items"), "3 photos + the docx (+ the refused record on the phone): {text}");
    // --proto 1 plays a 0.1.x phone: the tool reports the refusal text.
    let (ok, text) = run_fake(vec!["list".into(), "--proto".into(), "1".into(), "--data".into(), data_arg]).await;
    assert!(!ok && text.contains("Update Tugboat on the phone"), "{text}");
    pc.stop().await;
}

// A PC that is slow to save the pairing (2 s, a busy disk or CPU): the phone must not make its
// first sync dial before the save, or the PC refuses it as "not paired" and the phone unpairs
// itself a moment after pairing (seen on a loaded CI runner).
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_pairing_save_on_the_pc_still_ends_paired_on_both() {
    let r = Rig::unconnected();
    r.pc.start().await;
    r.phone.start().await;
    r.pc.core.debug_set_pairing_save_delay_ms(2000);
    let payload = r.pc.core.begin_pairing(false).unwrap();
    let phone = r.phone.core.clone();
    let task = tokio::spawn(async move { phone.pair_with(&payload).await });
    until("codes", 10, || r.pc.code().is_some() && r.phone.code().is_some()).await;
    r.pc.core.confirm_pairing(true).await.unwrap();
    task.await.unwrap().unwrap();
    r.settle().await;
    assert!(r.phone.core.paired_device().unwrap().is_some(), "the phone is still paired");
    assert!(r.pc.core.paired_device().unwrap().is_some(), "the PC is still paired");
    r.shutdown().await;
}
