//! Host-side proof of the exported API: a PhoneCore (the object Kotlin holds) pairs with an
//! in-process PC-role core over 127.0.0.1, sends a captured file with a thumbnail, gets a file
//! pushed from the PC, and returns typed errors. Binds 127.0.0.1 only (0.0.0.0 pops a Windows
//! Firewall dialog). PhoneCore calls block on their own runtime, so they run on plain threads,
//! never inside a tokio context.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use tugboat_core::{
    events::{Event, PairingEvent, SyncState as CoreSyncState},
    facade::{device_id_from_secret, NetworkConfig},
    ops::{AddKind, NewLocalFile},
    AppStorage, Core, CoreConfig, Role, SystemClock, KEEP_ID,
};
use tugboat_ffi::*;

fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(secs), "timed out after {secs}s waiting for: {what}");
        thread::sleep(Duration::from_millis(25));
    }
}

fn jpeg_bytes(seed: u8, len: usize) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00];
    out.extend((0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)));
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

struct Events(Mutex<Vec<FfiEvent>>);
impl EventListener for Events {
    fn on_event(&self, e: FfiEvent) {
        self.0.lock().unwrap().push(e);
    }
}

fn phone_config(dir: &Path) -> PhoneConfig {
    PhoneConfig {
        data_dir: dir.join("phone").to_string_lossy().into_owned(),
        device_name: "Test phone".into(),
        app_version: "ffi-test".into(),
        secret_key: generate_secret_key().unwrap(),
        bind_addr: Some("127.0.0.1:0".into()),
    }
}

struct Pc {
    rt: tokio::runtime::Runtime,
    core: Core,
    events: Arc<Mutex<Vec<Event>>>,
}

fn open_pc(dir: &Path) -> Pc {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).unwrap();
    let cfg = CoreConfig {
        data_dir: dir.join("pc").join("data"),
        role: Role::Pc,
        self_id: device_id_from_secret(&secret),
        device_name: "TEST-PC".into(),
        app_version: "ffi-test".into(),
    };
    let storage = AppStorage::new(dir.join("pc").join("storage")).unwrap();
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
    let port = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    rt.block_on(core.start_network(NetworkConfig { bind: format!("127.0.0.1:{port}").parse().unwrap(), mdns: false, secret_key: secret })).unwrap();
    Pc { rt, core, events }
}

impl Pc {
    fn code(&self) -> Option<String> {
        self.events.lock().unwrap().iter().find_map(|e| match e {
            Event::Pairing(PairingEvent::Code { code, .. }) => Some(code.clone()),
            _ => None,
        })
    }
}

fn phone_code(ev: &Events) -> Option<String> {
    ev.0.lock().unwrap().iter().find_map(|e| match e {
        FfiEvent::PairingCode { code, .. } => Some(code.clone()),
        _ => None,
    })
}

fn all_items(phone: &PhoneCore) -> Vec<ItemInfo> {
    phone.items(None, false, None, 500).unwrap().rows
}

#[test]
fn pair_send_receive_push_and_typed_errors() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Events(Mutex::new(Vec::new())));
    let phone = PhoneCore::open(phone_config(dir.path()), events.clone()).unwrap();
    let pc = open_pc(dir.path());

    // Typed errors before any network: junk text and a public address never leave the phone.
    assert!(matches!(phone.pair_with("hello".into()), Err(SnapError::PairBadPayload(_))));
    assert!(matches!(inspect_pairing_payload("hello".into()), Err(SnapError::PairBadPayload(_))));
    assert!(matches!(phone.unpair(), Err(SnapError::NotPaired(_))), "nothing paired yet");
    assert_eq!(phone.status().state, SyncState::Unpaired);
    assert!(phone.paired_pc().unwrap().is_none());
    // No PC, nothing to expire: the three default folders exist.
    let names: Vec<String> = phone.folders().unwrap().into_iter().map(|f| f.name).collect();
    assert_eq!(names, ["Inbox", "Keep", "Unsorted"]);

    // ---- pairing from a scanned payload, code shown on both ----
    phone.start_network(false).unwrap();
    assert!(matches!(phone.start_network(false), Err(SnapError::Invalid(_))), "second start is refused, not doubled");
    let payload = pc.core.begin_pairing(false).unwrap();
    assert_eq!(inspect_pairing_payload(payload.clone()).unwrap().pc_name, "TEST-PC");
    let phone2 = phone.clone();
    let pairing = thread::spawn(move || phone2.pair_with(payload));
    until("both screens show a code", 10, || pc.code().is_some() && phone_code(&events).is_some());
    assert_eq!(pc.code(), phone_code(&events), "same 6-digit code on both screens");
    pc.rt.block_on(pc.core.confirm_pairing(true)).unwrap();
    let paired = pairing.join().unwrap().expect("pairing completes");
    assert_eq!(paired.name, "TEST-PC");
    assert_eq!(phone.paired_pc().unwrap().unwrap().id, paired.id);
    until("PairingCompleted event", 5, || events.0.lock().unwrap().iter().any(|e| matches!(e, FfiEvent::PairingCompleted { peer_name } if peer_name == "TEST-PC")));
    until("both synced", 20, || phone.status().state == SyncState::Synced && pc.core.status().state == CoreSyncState::Synced);
    assert_eq!(phone.status().peer_name.as_deref(), Some("TEST-PC"));

    // ---- phone captures a file with a thumbnail; it reaches the PC; the phone sees the receipt ----
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let photo = jpeg_bytes(1, 300_000);
    let before = now_ms();
    let id = phone
        .add_file(NewFile {
            src_path: write(&src, "cam1.jpg", &photo),
            name: "IMG_20261117_140233.jpg".into(),
            folder_id: None,
            captured_at_ms: 1_790_000_000_000,
            geo: Some(GeoFix { lat: -30.5, lon: 151.6, accuracy_m: 8.0 }),
            thumb_path: Some(write(&src, "cam1.thumb.jpg", &jpeg_bytes(2, 3_000))),
            imported: false,
        })
        .unwrap();
    let mine = phone.item(id.clone()).unwrap().expect("item exists at once");
    assert_eq!(mine.name, "IMG_20261117_140233.jpg");
    assert_eq!(mine.kind, MediaKind::Image);
    assert_eq!(mine.local, LocalFileState::Present);
    assert_eq!(mine.folder_id, phone.folders().unwrap()[0].id, "Inbox by default");
    let thumb = mine.thumb_path.clone().expect("thumbnail held");
    assert!(Path::new(&thumb).exists());
    let item_uuid = tugboat_core::ItemId(uuid::Uuid::parse_str(&id).unwrap());
    until("the photo reaches the PC", 20, || {
        pc.core.item(item_uuid).unwrap().is_some_and(|v| v.local == tugboat_core::LocalFileState::Present)
    });
    let on_pc = pc.core.item(item_uuid).unwrap().unwrap();
    assert_eq!(std::fs::read(on_pc.path.as_ref().unwrap()).unwrap(), photo, "bytes identical on the PC");
    until("the thumbnail reaches the PC", 10, || pc.core.item(item_uuid).unwrap().is_some_and(|v| v.thumb_path.is_some()));
    // The PC's receipt turns "Waiting for PC" into a countdown: Inbox is 24 hours from the capture here.
    until("the item shows a countdown", 20, || matches!(phone.item(id.clone()).unwrap().unwrap().state, ItemStateInfo::Live { .. }));
    let after = now_ms();
    let ItemStateInfo::Live { expires_at_ms } = phone.item(id.clone()).unwrap().unwrap().state else { unreachable!() };
    assert!((before + DAY_MS..=after + DAY_MS).contains(&expires_at_ms), "expires 24 h after it entered Inbox: {expires_at_ms}");

    // ---- the PC pushes a file; it appears on the phone, identical, with an event ----
    let pushed = jpeg_bytes(3, 150_000);
    let pushed_src = dir.path().join("pc-src");
    std::fs::create_dir_all(&pushed_src).unwrap();
    let pushed_id = pc
        .core
        .add_local_file(NewLocalFile {
            src: write(&pushed_src, "from-pc.jpg", &pushed).into(),
            name: "from-pc.jpg".into(),
            folder: None,
            captured_at: now_ms(),
            geo: None,
            thumb: None,
            how: AddKind::Added,
            ntfs: None,
        })
        .unwrap();
    let pushed_s = pushed_id.0.to_string();
    until("the pushed file arrives on the phone", 20, || phone.item(pushed_s.clone()).unwrap().is_some_and(|v| v.local == LocalFileState::Present));
    let got = phone.item(pushed_s.clone()).unwrap().unwrap();
    assert_eq!(std::fs::read(got.path.as_ref().unwrap()).unwrap(), pushed, "bytes identical on the phone");
    assert_eq!(got.name, "from-pc.jpg");
    assert!(all_items(&phone).iter().any(|i| i.id == pushed_s), "listed in the gallery");
    until("ItemsChanged names the pushed item", 5, || {
        events.0.lock().unwrap().iter().any(|e| matches!(e, FfiEvent::ItemsChanged { ids } if ids.contains(&pushed_s)))
    });
    assert!(events.0.lock().unwrap().iter().any(|e| matches!(e, FfiEvent::Sync { .. })), "sync status events reach the listener");

    // ---- typed errors: a missing file and a non-media file; neither creates an item ----
    let count = all_items(&phone).len();
    let bad = |src_path: String, name: &str| NewFile {
        src_path,
        name: name.into(),
        folder_id: None,
        captured_at_ms: now_ms(),
        geo: None,
        thumb_path: None,
        imported: true,
    };
    let missing = dir.path().join("src").join("nope.jpg").to_string_lossy().into_owned();
    match phone.add_file(bad(missing, "nope.jpg")) {
        Err(SnapError::Io(m)) => assert!(m.contains("nope.jpg"), "the message names the file: {m}"),
        other => panic!("a missing file must be Io, got {other:?}"),
    }
    let exe = write(&src, "evil.jpg", b"MZ\x90\x00 this is a program, not a photo");
    assert!(matches!(phone.add_file(bad(exe, "evil.jpg")), Err(SnapError::NotMedia(_))), "an exe renamed .jpg is refused by its bytes");
    let gone_folder = uuid::Uuid::from_u128(0xdead).to_string();
    let to_nowhere = NewFile { folder_id: Some(gone_folder), ..bad(write(&src, "ok.jpg", &jpeg_bytes(4, 1000)), "ok.jpg") };
    assert!(matches!(phone.add_file(to_nowhere), Err(SnapError::Invalid(_))), "an unknown folder is refused");
    assert!(matches!(phone.add_file(NewFile { folder_id: Some("zzz".into()), ..bad(String::new(), "x.jpg") }), Err(SnapError::InvalidInput(_))));
    assert_eq!(all_items(&phone).len(), count, "failed adds left no item behind");

    // ---- keep, move, delete (Bin), restore, activity ----
    phone.set_pinned(id.clone(), true).unwrap();
    assert_eq!(phone.item(id.clone()).unwrap().unwrap().state, ItemStateInfo::Kept);
    let keep = phone.folders().unwrap().into_iter().find(|f| f.kind == FolderKind::Keep).unwrap();
    assert_eq!(keep.id, KEEP_ID.0.to_string());
    assert_eq!(keep.retention_secs, None);
    phone.move_item(id.clone(), keep.id.clone()).unwrap();
    assert_eq!(phone.item(id.clone()).unwrap().unwrap().folder_id, keep.id);
    assert_eq!(phone.items(Some(keep.id.clone()), false, None, 50).unwrap().rows.len(), 1);
    phone.delete_item(id.clone()).unwrap();
    assert!(!all_items(&phone).iter().any(|i| i.id == id), "deleted items leave the gallery");
    let bin = phone.items(None, true, None, 50).unwrap().rows;
    assert_eq!(bin.len(), 1);
    assert!(matches!(bin[0].state, ItemStateInfo::Deleted { .. }) && bin[0].local == LocalFileState::Binned && bin[0].path.is_some());
    phone.restore_item(id.clone()).unwrap();
    assert!(all_items(&phone).iter().any(|i| i.id == id), "restored");
    assert!(phone.items(None, true, None, 50).unwrap().rows.is_empty(), "Bin is empty again");
    let acts: Vec<ActivityAction> = phone.activity(None, 100).unwrap().into_iter().map(|a| a.action).collect();
    for want in [ActivityAction::Captured, ActivityAction::Moved, ActivityAction::Deleted, ActivityAction::Restored, ActivityAction::Received, ActivityAction::Paired] {
        assert!(acts.contains(&want), "activity has {want:?}: {acts:?}");
    }
    let page1 = phone.activity(None, 2).unwrap();
    let page2 = phone.activity(Some(page1.last().unwrap().id), 2).unwrap();
    assert!(page1.iter().all(|a| page2.iter().all(|b| b.id < a.id)), "activity pages do not overlap");

    // ---- settings ----
    let mut s = phone.settings().unwrap();
    assert!(s.stay_connected && s.bin_days == 7);
    s.bin_days = 3;
    s.stay_connected = false;
    phone.save_settings(s.clone()).unwrap();
    assert_eq!(phone.settings().unwrap(), s);
    s.bin_days = 0;
    assert!(matches!(phone.save_settings(s), Err(SnapError::Invalid(_))), "nonsense is refused, the old value stays");
    assert_eq!(phone.settings().unwrap().bin_days, 3);

    // ---- unpair: the connected PC is told ----
    phone.unpair().unwrap();
    assert_eq!(phone.status().state, SyncState::Unpaired);
    assert!(phone.paired_pc().unwrap().is_none());
    until("the PC is unpaired too", 15, || pc.core.paired_device().unwrap().is_none());
    assert!(matches!(phone.unpair(), Err(SnapError::NotPaired(_))));
    assert!(all_items(&phone).iter().any(|i| i.id == id), "files stay after unpair");

    phone.stop_network();
    pc.rt.block_on(pc.core.stop_network());
}

/// A well-formed code whose address nobody answers on: a typed network error within the dial
/// bound, the listener hears why, and nothing is stored.
#[test]
fn a_dead_address_is_a_network_error_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Events(Mutex::new(Vec::new())));
    let phone = PhoneCore::open(phone_config(dir.path()), events.clone()).unwrap();
    phone.start_network(false).unwrap();
    let dead = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    let payload = ghost_payload(port);
    let t0 = Instant::now();
    let r = phone.pair_with(payload);
    assert!(matches!(r, Err(SnapError::NetTimeout(_)) | Err(SnapError::NetUnreachable(_)) | Err(SnapError::Net(_))), "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(20));
    until("the screen hears why", 5, || events.0.lock().unwrap().iter().any(|e| matches!(e, FfiEvent::PairingCancelled { .. })));
    assert_eq!(phone.status().state, SyncState::Unpaired, "a failed pairing stores nothing");
    phone.stop_network();
}

/// A silent address: pair_with waits; cancel_pairing releases it at once.
#[test]
fn cancel_pairing_ends_a_waiting_pair_with() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Events(Mutex::new(Vec::new())));
    let phone = PhoneCore::open(phone_config(dir.path()), events.clone()).unwrap();
    phone.start_network(false).unwrap();
    let dead = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let payload = ghost_payload(dead.local_addr().unwrap().port());
    let p2 = phone.clone();
    let waiting = thread::spawn(move || p2.pair_with(payload));
    thread::sleep(Duration::from_millis(500));
    let t0 = Instant::now();
    phone.cancel_pairing();
    let r = waiting.join().unwrap();
    assert!(matches!(r, Err(SnapError::PairCancelled(_))), "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(3), "cancel is immediate");
    until("the cancel reaches the listener", 5, || events.0.lock().unwrap().iter().any(|e| matches!(e, FfiEvent::PairingCancelled { code, .. } if code == "cancelled")));
    // A cancel with nobody waiting is a no-op (it must not poison the next pairing).
    phone.cancel_pairing();
    phone.stop_network();
}

fn ghost_payload(port: u16) -> String {
    // A real public key (an invalid one would fail before the network is tried).
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).unwrap();
    let key = device_id_from_secret(&secret);
    format!("localtugboat://pair?v=1&k={key}&a=127.0.0.1:{port}&s={}&e={}&n=Ghost", "cd".repeat(16), now_ms() + 120_000)
}

const DAY_MS: i64 = 86_400_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}
