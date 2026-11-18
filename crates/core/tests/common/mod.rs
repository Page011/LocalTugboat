//! Shared harness for the two-core integration tests. Every network test binds 127.0.0.1 only
//! (binding 0.0.0.0 pops a Windows Firewall dialog) and uses an injected `TestClock` per device,
//! so expiry and Bin days are driven without sleeping. Only the network timeouts and the
//! reconnect back-off run on real time.
#![allow(dead_code)]

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tugboat_core::{
    events::{Event, PairingEvent, SyncState},
    facade::{device_id_from_secret, NetworkConfig},
    ops::{AddKind, NewLocalFile},
    sync::digest,
    store, AppStorage, Core, CoreConfig, DeviceId, ItemId, ItemQuery, ItemRecord, ItemView, LocalFileState, Role, TestClock,
};

/// 2026-09-21: an arbitrary fixed start (same as the logic tests).
pub const T0: i64 = 1_790_000_000_000;
pub const HOUR: i64 = 3_600_000;
pub const DAY: i64 = 24 * HOUR;

/// Poll `f` every 20 ms until it is true; panic with `what` after `secs`.
pub async fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    loop {
        if f() {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(secs), "timed out after {secs}s waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").expect("a free UDP port");
    s.local_addr().expect("local addr").port()
}

/// One device: a Core, its injected clock, its temp dir and an event log.
pub struct Dev {
    pub core: Core,
    pub clock: TestClock,
    pub dir: tempfile::TempDir,
    pub secret: [u8; 32],
    pub id: DeviceId,
    pub role: Role,
    pub name: String,
    pub port: u16,
    pub events: Arc<Mutex<Vec<Event>>>,
}

pub fn random_secret() -> [u8; 32] {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).expect("random key");
    k
}

impl Dev {
    pub fn new(role: Role, name: &str, clock: &TestClock) -> Dev {
        Dev::with_secret(role, name, clock, random_secret())
    }

    pub fn with_secret(role: Role, name: &str, clock: &TestClock, secret: [u8; 32]) -> Dev {
        let dir = tempfile::tempdir().expect("temp dir");
        Dev::open_in(dir, role, name, clock, secret)
    }

    /// Open (or reopen) a device in `dir`.
    pub fn open_in(dir: tempfile::TempDir, role: Role, name: &str, clock: &TestClock, secret: [u8; 32]) -> Dev {
        let id = device_id_from_secret(&secret);
        let cfg = CoreConfig {
            data_dir: dir.path().join("data"),
            role,
            self_id: id,
            device_name: name.to_string(),
            app_version: "test-1".into(),
        };
        let storage = AppStorage::new(dir.path().join("storage")).expect("storage");
        let (core, mut rx) = Core::open(cfg, Arc::new(clock.clone()), Arc::new(storage)).expect("open core");
        let events = Arc::new(Mutex::new(Vec::new()));
        let log = events.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) => log.lock().unwrap().push(e),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        });
        Dev { core, clock: clock.clone(), dir, secret, id, role, name: name.to_string(), port: if role == Role::Pc { free_udp_port() } else { 0 }, events }
    }

    pub fn bind(&self) -> SocketAddr {
        format!("127.0.0.1:{}", self.port).parse().unwrap()
    }

    pub async fn start(&self) {
        self.core.start_network(NetworkConfig { bind: self.bind(), mdns: false, secret_key: self.secret }).await.expect("start network");
    }

    pub async fn stop(&self) {
        self.core.stop_network().await;
    }

    // ----- events -----

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    pub fn has_event(&self, f: impl Fn(&Event) -> bool) -> bool {
        self.events.lock().unwrap().iter().any(f)
    }

    pub fn count_events(&self, f: impl Fn(&Event) -> bool) -> usize {
        self.events.lock().unwrap().iter().filter(|e| f(e)).count()
    }

    /// (kind, message) of every Event::Error so far.
    pub fn errors(&self) -> Vec<(String, String)> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Error { kind, message } => Some((kind, message)),
                _ => None,
            })
            .collect()
    }

    pub fn error_kinds(&self) -> Vec<String> {
        self.errors().into_iter().map(|e| e.0).collect()
    }

    /// The last pairing code event, as shown ("482 913").
    pub fn code(&self) -> Option<String> {
        self.events().into_iter().rev().find_map(|e| match e {
            Event::Pairing(PairingEvent::Code { code, .. }) => Some(code),
            _ => None,
        })
    }

    pub fn cancelled(&self) -> Option<(String, String)> {
        self.events().into_iter().rev().find_map(|e| match e {
            Event::Pairing(PairingEvent::Cancelled { code, message }) => Some((code, message)),
            _ => None,
        })
    }

    pub fn completed(&self) -> bool {
        self.has_event(|e| matches!(e, Event::Pairing(PairingEvent::Completed { .. })))
    }

    // ----- state -----

    pub fn state(&self) -> SyncState {
        self.core.status().state
    }

    pub async fn wait_state(&self, want: SyncState) {
        let what = format!("{} to reach {want:?} (now {:?})", self.name, self.state());
        until(&what, 20, || self.state() == want).await;
    }

    pub fn view(&self, id: ItemId) -> Option<ItemView> {
        self.core.item(id).expect("item query")
    }

    pub fn record(&self, id: ItemId) -> Option<ItemRecord> {
        self.core.debug_conn(|c| store::get_item(c, id).expect("get item").map(|(r, _)| r))
    }

    pub fn local(&self, id: ItemId) -> Option<LocalFileState> {
        self.core.debug_conn(|c| store::get_item(c, id).expect("get item").map(|(_, l)| l))
    }

    pub fn has_item(&self, id: ItemId) -> bool {
        self.record(id).is_some()
    }

    pub fn items(&self) -> Vec<ItemView> {
        self.core.items(&ItemQuery { folder: None, bin: false, after: None, limit: 500 }).expect("items").rows
    }

    pub fn bin(&self) -> Vec<ItemView> {
        self.core.items(&ItemQuery { folder: None, bin: true, after: None, limit: 500 }).expect("bin").rows
    }

    pub fn digest(&self) -> (String, u64) {
        self.core.debug_conn(|c| digest::compute(c).expect("digest"))
    }

    pub fn max_lamport(&self) -> u64 {
        self.core.debug_conn(|c| store::max_lamport(c).expect("lamport"))
    }

    pub fn has_receipt(&self, hash: tugboat_core::BlobHash, from: DeviceId) -> bool {
        self.core.debug_conn(|c| store::get_receipt(c, hash, from).expect("receipt").is_some())
    }

    pub fn activity_text(&self) -> Vec<String> {
        self.core.activity(None, 500).expect("activity").into_iter().map(|a| format!("{:?}: {}", a.action, a.detail)).collect()
    }

    pub fn has_activity(&self, action: tugboat_core::ActivityAction) -> bool {
        self.core.activity(None, 500).expect("activity").iter().any(|a| a.action == action)
    }

    // ----- files -----

    /// A JPEG-looking file (real JPEG magic and end marker, unique body) under <dir>/src.
    pub fn jpeg(&self, name: &str, seed: u64, len: usize) -> PathBuf {
        let dir = self.dir.path().join("src");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, jpeg_bytes(seed, len)).unwrap();
        p
    }

    pub fn add(&self, name: &str, seed: u64, len: usize) -> ItemId {
        let src = self.jpeg(name, seed, len);
        self.core
            .add_local_file(NewLocalFile {
                src,
                name: name.to_string(),
                folder: None,
                captured_at: self.clock_now(),
                geo: None,
                thumb: None,
                how: AddKind::Captured,
                ntfs: None,
            })
            .expect("add_local_file")
    }

    pub fn clock_now(&self) -> i64 {
        use tugboat_core::Clock;
        self.clock.now_ms()
    }

    pub fn file_bytes(&self, id: ItemId) -> Vec<u8> {
        let path = self.view(id).and_then(|v| v.path).unwrap_or_else(|| panic!("{} holds no file for the item", self.name));
        std::fs::read(path).expect("read held file")
    }

    pub fn parts_dir(&self) -> PathBuf {
        self.dir.path().join("storage").join("parts")
    }

    pub fn items_dir(&self) -> PathBuf {
        self.dir.path().join("storage").join("items")
    }
}

/// Deterministic JPEG-looking bytes: magic, seeded body, end marker.
pub fn jpeg_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00];
    let mut body = vec![0u8; len];
    blake3::Hasher::new().update(&seed.to_le_bytes()).finalize_xof().fill(&mut body);
    out.extend_from_slice(&body);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// Pair a PC and a phone through the real pairing flow. Both end up paired; the phone's connect
/// loop is running and dials the PC at once. Returns when both report Completed.
pub async fn pair(pc: &Dev, phone: &Dev) {
    // Count events first: a re-pairing must wait for NEW code/completed events, not old ones.
    let (codes_pc, codes_phone) = (pc.count_events(is_code), phone.count_events(is_code));
    let (done_pc, done_phone) = (pc.count_events(is_completed), phone.count_events(is_completed));
    pc.start().await;
    phone.start().await;
    let payload = pc.core.begin_pairing(false).expect("begin pairing");
    let phone_core = phone.core.clone();
    let task = tokio::spawn(async move { phone_core.pair_with(&payload).await });
    until("the PC to show a code", 10, || pc.count_events(is_code) > codes_pc).await;
    until("the phone to show a code", 10, || phone.count_events(is_code) > codes_phone).await;
    assert_eq!(pc.code(), phone.code(), "both screens show the same code");
    pc.core.confirm_pairing(true).await.expect("confirm pairing");
    task.await.expect("phone task").expect("phone paired");
    until("both Completed events", 10, || pc.count_events(is_completed) > done_pc && phone.count_events(is_completed) > done_phone).await;
}

fn is_code(e: &Event) -> bool {
    matches!(e, Event::Pairing(PairingEvent::Code { .. }))
}

fn is_completed(e: &Event) -> bool {
    matches!(e, Event::Pairing(PairingEvent::Completed { .. }))
}

/// A connected, paired and synced pair of devices with separate clocks.
pub struct Rig {
    pub pc: Dev,
    pub phone: Dev,
}

impl Rig {
    pub async fn new() -> Rig {
        let r = Rig::unconnected();
        pair(&r.pc, &r.phone).await;
        r.settle().await;
        r
    }

    /// Devices exist but are not started or paired.
    pub fn unconnected() -> Rig {
        Rig { pc: Dev::new(Role::Pc, "TEST-PC", &TestClock::new(T0)), phone: Dev::new(Role::Phone, "Test phone", &TestClock::new(T0)) }
    }

    /// Both Synced and holding identical records.
    pub async fn settle(&self) {
        self.pc.wait_state(SyncState::Synced).await;
        self.phone.wait_state(SyncState::Synced).await;
        until("equal digests", 10, || self.pc.digest() == self.phone.digest()).await;
        // Equal once is not settled: a receipt still in flight changes both digests a moment
        // later (s15 compared a digest taken in that gap, about 1 run in 6). Require the same
        // digest on both sides after a short quiet time.
        loop {
            let seen = self.pc.digest();
            tokio::time::sleep(Duration::from_millis(300)).await;
            if self.pc.digest() == seen && self.phone.digest() == seen {
                return;
            }
            until("equal digests again", 10, || self.pc.digest() == self.phone.digest()).await;
        }
    }

    /// Take the phone off the network, and wait until the PC has noticed.
    pub async fn apart(&self) {
        self.phone.stop().await;
        self.pc.wait_state(SyncState::Offline).await;
    }

    /// Bring the phone back; it dials the PC by itself.
    pub async fn together(&self) {
        self.phone.start().await;
    }

    pub async fn shutdown(&self) {
        self.phone.stop().await;
        self.pc.stop().await;
    }
}
