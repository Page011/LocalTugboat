//! Pairing, unpairing, refusals and the fake-phone tool, over loopback. Design ids S11, S12,
//! S14, S15, S21.

mod common;

use std::{
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use common::*;
use tugboat_core::{
    events::{Event, PairingEvent, SyncState},
    ActivityAction, CoreError, LocalFileState, Role, TestClock,
};
use tugboat_net::{
    node::{AccessGate, Node, NodeConfig, Role as NetRole},
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
    r.phone.core.debug_set_proto(3);
    let from_phone_before_pairing = r.phone.add("secret.jpg", 1, 1_000);
    pair(&r.pc, &r.phone).await;
    r.phone.wait_state(SyncState::Refused).await;
    r.pc.wait_state(SyncState::Refused).await;
    for d in [&r.pc, &r.phone] {
        assert_eq!(d.core.status().message.as_deref(), Some("Update Tugboat on the PC"), "{}", d.name);
        assert!(d.has_activity(ActivityAction::SyncError), "{}: {:?}", d.name, d.activity_text());
        assert!(d.errors().iter().any(|(k, m)| k == "version_mismatch" && m.contains("Update Tugboat")), "{}: {:?}", d.name, d.errors());
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!r.pc.has_item(from_phone_before_pairing), "no record crossed the refused connection");
    assert_eq!(r.pc.digest().1, 3, "the PC holds only its default folders");
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
// right one; an already paired PC needs replace_existing.
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
    for d in [&r.pc, &r.phone] {
        assert!(d.has_event(|e| matches!(e, Event::Pairing(PairingEvent::Unpaired { .. }))), "{}", d.name);
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
    r.shutdown().await;
}

struct AllowAll;
impl AccessGate for AllowAll {
    fn is_paired(&self, _: &[u8; 32]) -> bool {
        true
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

fn fake_phone_exe() -> std::path::PathBuf {
    let me = std::env::current_exe().unwrap();
    let debug = me.parent().and_then(|p| p.parent()).expect("target/debug").to_path_buf();
    let exe = debug.join("examples").join(format!("fake_phone{}", std::env::consts::EXE_SUFFIX));
    if !exe.exists() {
        // `cargo test --test x` does not build examples: build this one.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let ok = Command::new(cargo).args(["build", "-p", "tugboat-core", "--example", "fake_phone"]).status().unwrap().success();
        assert!(ok && exe.exists(), "could not build the fake_phone example at {}", exe.display());
    }
    exe
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
