//! Adversarial checks of the exported phone API. Binds 127.0.0.1 only.
//! A failing test here means the rule in its doc comment is broken.

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

/// A phone already paired to PC A scans PC B's code. The phone links up to five PCs, so it adds
/// B beside A: A is not told anything and stays linked (the old behaviour replaced A and had to
/// tell it). Neither PC may be left believing a pairing the phone does not keep.
#[test]
fn second_pc_is_linked_beside_the_first() {
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

    pair(&phone, &b).expect("a second PC links beside the first");
    assert!(b.core.paired_device().unwrap().is_some(), "B lists the phone");
    // Nothing tells A it was dropped: give a wrong notice time to arrive.
    thread::sleep(Duration::from_secs(2));
    assert!(a.core.paired_device().unwrap().is_some(), "PC-A is still linked to the phone");
    assert!(!phone.status().devices.iter().any(|d| d.unlinking), "no unlink notice is owed to anybody");
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

/// Same as above, but PC A is switched off when the phone links PC B (the usual case: the user is
/// at a different desk). A is still linked: back on the same address it keeps its pairing, and
/// nothing is owed to it.
#[test]
fn second_pc_while_first_is_offline_leaves_the_first_linked() {
    let dir = tempfile::tempdir().unwrap();
    let phone = open_phone(dir.path());
    let a = open_pc(dir.path(), "PC-A");
    let b = open_pc(dir.path(), "PC-B");

    pair(&phone, &a).expect("first pairing");
    assert!(until(20, || phone.status().state == SyncState::Synced), "phone synced with A");

    a.rt.block_on(a.core.stop_network());
    pair(&phone, &b).expect("link B while A is off");
    a.restart_network();

    thread::sleep(Duration::from_secs(3));
    assert!(a.core.paired_device().unwrap().is_some(), "PC-A came back and is still linked to the phone");
    assert!(!phone.status().devices.iter().any(|d| d.unlinking), "no unlink notice is owed to PC-A");
}

/// A switched-off PC must not hold up the one just linked: the phone has to reach Synced with
/// PC B while A is still off.
#[test]
fn an_offline_pc_does_not_block_syncing_with_the_new_pc() {
    let dir = tempfile::tempdir().unwrap();
    let phone = open_phone(dir.path());
    let a = open_pc(dir.path(), "PC-A");
    let b = open_pc(dir.path(), "PC-B");

    pair(&phone, &a).expect("first pairing");
    assert!(until(20, || phone.status().state == SyncState::Synced), "phone synced with A");
    a.rt.block_on(a.core.stop_network());
    pair(&phone, &b).expect("link B while A is off");
    assert!(until(30, || b.core.paired_device().unwrap().is_some()), "B lists the phone");
    let b_id = hex::encode(device_id_from_secret(&b.secret).0);
    let b_synced = || phone.status().devices.iter().any(|d| d.id == b_id && d.state == SyncState::Synced);
    assert!(until(30, b_synced), "phone synced with B while A is still off");
}
