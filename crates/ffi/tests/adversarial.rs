//! Adversarial checks of the exported phone API (verifier stage). Binds 127.0.0.1 only.
//! A failing test here is a finding, not a broken test: see the doc comment on each.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use tugboat_core::{
    events::{Event, PairingEvent},
    facade::{device_id_from_secret, NetworkConfig},
    AppStorage, Core, CoreConfig, Role, SystemClock,
};
use tugboat_ffi::*;

fn until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while !f() {
        if t0.elapsed() > Duration::from_secs(secs) {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
    true
}

struct Events(Mutex<Vec<FfiEvent>>);
impl EventListener for Events {
    fn on_event(&self, e: FfiEvent) {
        self.0.lock().unwrap().push(e);
    }
}

struct Pc {
    rt: tokio::runtime::Runtime,
    core: Core,
    events: Arc<Mutex<Vec<Event>>>,
    secret: [u8; 32],
    port: u16,
}

fn open_pc(dir: &Path, name: &str) -> Pc {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).unwrap();
    let cfg = CoreConfig {
        data_dir: dir.join(name).join("data"),
        role: Role::Pc,
        self_id: device_id_from_secret(&secret),
        device_name: name.into(),
        app_version: "adv".into(),
    };
    let storage = AppStorage::new(dir.join(name).join("storage")).unwrap();
    let (core, mut rx) = {
        let _g = rt.enter();
        Core::open(cfg, Arc::new(SystemClock), Arc::new(storage)).unwrap()
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let log = events.clone();
    rt.spawn(async move {
        while let Ok(e) = rx.recv().await {
            log.lock().unwrap().push(e);
        }
    });
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    rt.block_on(core.start_network(NetworkConfig { bind: format!("127.0.0.1:{port}").parse().unwrap(), mdns: false, secret_key: secret }))
        .unwrap();
    Pc { rt, core, events, secret, port }
}

impl Pc {
    fn restart_network(&self) {
        let cfg = NetworkConfig { bind: format!("127.0.0.1:{}", self.port).parse().unwrap(), mdns: false, secret_key: self.secret };
        self.rt.block_on(self.core.start_network(cfg)).unwrap();
    }
}

fn pc_has_code(pc: &Pc) -> bool {
    pc.events.lock().unwrap().iter().any(|e| matches!(e, Event::Pairing(PairingEvent::Code { .. })))
}

/// Pair `phone` with `pc` (PC owner clicks Pair once the code shows).
fn pair(phone: &Arc<PhoneCore>, pc: &Pc) -> Result<PairedPc, SnapError> {
    let payload = pc.core.begin_pairing(false).unwrap();
    let p2 = phone.clone();
    let t = thread::spawn(move || p2.pair_with(payload));
    assert!(until(10, || pc_has_code(pc)), "PC shows a code");
    pc.rt.block_on(pc.core.confirm_pairing(true)).unwrap();
    t.join().unwrap()
}

/// A phone already paired to PC A scans PC B's code (the Android deep-link guard
/// "Already paired. Unpair in Settings first" is skipped on a cold start because the
/// paired state is not loaded yet). Today the phone silently re-pairs: PC A is never told
/// and keeps a pairing with a phone that will never dial it again. Expected: either the
/// phone refuses with AlreadyPaired, or PC A is told (as `unpair()` does).
#[test]
fn repair_while_paired_tells_old_pc_or_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Events(Mutex::new(Vec::new())));
    let phone = PhoneCore::open(
        PhoneConfig {
            data_dir: dir.path().join("phone").to_string_lossy().into_owned(),
            device_name: "Adv phone".into(),
            app_version: "adv".into(),
            secret_key: generate_secret_key().unwrap(),
            bind_addr: Some("127.0.0.1:0".into()),
        },
        events,
    )
    .unwrap();
    phone.start_network(false).unwrap();
    let a = open_pc(dir.path(), "PC-A");
    let b = open_pc(dir.path(), "PC-B");

    pair(&phone, &a).expect("first pairing");
    assert!(until(20, || phone.status().state == SyncState::Synced), "phone synced with A");
    assert!(a.core.paired_device().unwrap().is_some());

    match pair(&phone, &b) {
        Err(SnapError::AlreadyPaired(_)) => {} // refused: fine
        Err(other) => panic!("unexpected error {other:?}"),
        Ok(_) => {
            // Re-paired: the old PC must hear about it, like an explicit unpair.
            assert!(
                until(20, || a.core.paired_device().unwrap().is_none()),
                "phone switched to PC-B but PC-A still believes it is paired (never told)"
            );
        }
    }
}

fn open_phone(dir: &Path) -> Arc<PhoneCore> {
    let phone = PhoneCore::open(
        PhoneConfig {
            data_dir: dir.join("phone").to_string_lossy().into_owned(),
            device_name: "Adv phone".into(),
            app_version: "adv".into(),
            secret_key: generate_secret_key().unwrap(),
            bind_addr: Some("127.0.0.1:0".into()),
        },
        Arc::new(Events(Mutex::new(Vec::new()))),
    )
    .unwrap();
    phone.start_network(false).unwrap();
    phone
}

/// Same as above, but PC A is switched off when the phone re-pairs with PC B (the usual case:
/// the user is at a different desk). An explicit `unpair()` keeps a pending notice and tells
/// the PC once it is reachable; a re-pair does not (ops::pairing_completed calls unpair_in_tx
/// with no pending notice). PC A, back on the same address, never learns it lost the phone.
#[test]
fn repair_while_old_pc_offline_still_tells_old_pc() {
    let dir = tempfile::tempdir().unwrap();
    let phone = open_phone(dir.path());
    let a = open_pc(dir.path(), "PC-A");
    let b = open_pc(dir.path(), "PC-B");

    pair(&phone, &a).expect("first pairing");
    assert!(until(20, || phone.status().state == SyncState::Synced), "phone synced with A");

    a.rt.block_on(a.core.stop_network());
    pair(&phone, &b).expect("re-pair with B while A is off");
    a.restart_network();

    assert!(
        until(30, || a.core.paired_device().unwrap().is_none()),
        "PC-A came back but still believes the phone is paired: the re-pair left no unpair notice for it"
    );
}

/// The notice for a switched-off old PC must not hold up the new one: the phone has to reach
/// Synced with PC B while A is still off.
#[test]
fn old_pc_notice_does_not_block_syncing_with_the_new_pc() {
    let dir = tempfile::tempdir().unwrap();
    let phone = open_phone(dir.path());
    let a = open_pc(dir.path(), "PC-A");
    let b = open_pc(dir.path(), "PC-B");

    pair(&phone, &a).expect("first pairing");
    assert!(until(20, || phone.status().state == SyncState::Synced), "phone synced with A");
    a.rt.block_on(a.core.stop_network());
    pair(&phone, &b).expect("re-pair with B while A is off");
    assert!(until(30, || b.core.paired_device().unwrap().is_some()), "B lists the phone");
    assert!(until(30, || phone.status().state == SyncState::Synced), "phone synced with B while A is still off");
}
