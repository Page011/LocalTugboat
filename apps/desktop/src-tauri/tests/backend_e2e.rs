//! The desktop backend against a real phone-role Core over 127.0.0.1: pairing through the QR
//! payload, photos both ways through the real library folder (Explorer drops, moves, a missing
//! library), tray states and UI events. Everything binds loopback only (a 0.0.0.0 bind pops a
//! Windows Firewall dialog) and uses a throwaway profile dir and an in-memory key store: the
//! real library and the real Credential Manager key are never touched.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::Value;
use tugboat_core::{
    events::{Event, PairingEvent, SyncState},
    facade::{device_id_from_secret, NetworkConfig},
    ops::{AddKind, NewLocalFile},
    AppStorage, Core, CoreConfig, ItemId, ItemQuery, Role, Settings, SystemClock,
};
use tugboat_desktop_lib::{
    backend::{ev, Backend, Emitter, StartOptions},
    keys::{KeyError, KeyStore},
    library::LibraryOptions,
    paths::Paths,
    tray::TrayState,
    updates::{take_show_after_update, Updates, FOUND_KEY, LAST_CHECK_KEY, SETTING_KEY, SHOW_AFTER_KEY},
};

struct MemKeys(Mutex<Option<[u8; 32]>>);

impl KeyStore for MemKeys {
    fn load(&self) -> Result<Option<[u8; 32]>, KeyError> {
        Ok(*self.0.lock().unwrap())
    }
    fn save(&self, key: &[u8; 32]) -> Result<(), KeyError> {
        *self.0.lock().unwrap() = Some(*key);
        Ok(())
    }
}

#[derive(Default)]
struct Collect {
    events: Mutex<Vec<(String, Value)>>,
    tray: Mutex<Vec<TrayState>>,
}

impl Emitter for Collect {
    fn emit(&self, event: &str, payload: Value) {
        self.events.lock().unwrap().push((event.to_string(), payload));
    }
    fn tray(&self, state: TrayState, _tooltip: &str) {
        self.tray.lock().unwrap().push(state);
    }
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out after {secs}s waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn jpeg(seed: u8, len: usize) -> Vec<u8> {
    let mut v = vec![seed; len];
    v[..4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
    v
}

struct Pc {
    be: Arc<Backend>,
    out: Arc<Collect>,
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn start_pc() -> Pc {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("pc");
    let paths = Paths { data_dir: dir.clone(), bin_dir: dir.join("Bin"), default_library: dir.join("library"), key_user: "test".into() };
    let out = Arc::new(Collect::default());
    let opts = StartOptions {
        paths,
        bind_env: Some("127.0.0.1".into()),
        app_version: "e2e".into(),
        device_name: "E2E-PC".into(),
        library: LibraryOptions { stable_for: Duration::from_millis(200), debounce: Duration::from_millis(100), max_debounce: Duration::from_secs(1), rescan_every: Duration::from_secs(600) },
        mdns: false,
        check_firewall: false,
    };
    let be = Backend::start(opts, &MemKeys(Mutex::new(None)), out.clone()).await.expect("backend starts");
    // A free port for this run (the default 47300 may be taken by a real app or another test).
    let s = Settings { udp_port: free_udp_port(), ..be.settings().unwrap() };
    be.save_settings(s).await.unwrap();
    assert_eq!(be.app_info().network_error, None, "{:?}", be.app_info());
    let root = be.library.root().expect("the default library was created");
    Pc { be, out, root, _tmp: tmp }
}

fn phone(tmp: &std::path::Path) -> (Core, [u8; 32]) {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).unwrap();
    let cfg = CoreConfig { data_dir: tmp.join("phone-data"), role: Role::Phone, self_id: device_id_from_secret(&secret), device_name: "E2E-PHONE".into(), app_version: "e2e".into() };
    let (core, _rx) = Core::open(cfg, Arc::new(SystemClock), Arc::new(AppStorage::new(tmp.join("phone-storage")).unwrap())).unwrap();
    (core, secret)
}

fn code_events(out: &Collect) -> Vec<String> {
    out.events
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == ev::CORE)
        .filter_map(|(_, v)| serde_json::from_value::<Event>(v.clone()).ok())
        .filter_map(|e| match e {
            Event::Pairing(PairingEvent::Code { code, .. }) => Some(code),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_and_photos_both_ways_through_the_real_library_folder() {
    let pc = start_pc().await;
    let be = &pc.be;
    assert!(be.app_info().bind.starts_with("127.0.0.1:"), "TUGBOAT_BIND is honoured: {}", be.app_info().bind);

    // ----- pair: QR payload -> code on both screens -> [Pair] -----
    let tmp = tempfile::tempdir().unwrap();
    let (ph, secret) = phone(tmp.path());
    ph.start_network(NetworkConfig { bind: "127.0.0.1:0".parse().unwrap(), mdns: false, secret_key: secret }).await.unwrap();
    let start = be.pairing_begin().unwrap();
    assert!(start.svg.contains("<svg") && start.payload.starts_with("localtugboat://pair?") && start.payload.contains("127.0.0.1"));
    assert!(start.expires_at > 1_700_000_000_000, "an expiry in unix ms");
    let phone_core = ph.clone();
    let payload = start.payload.clone();
    let pairing = tokio::spawn(async move { phone_core.pair_with(&payload).await });
    until("the PC shows a code", 10, || !code_events(&pc.out).is_empty()).await;
    be.pairing_confirm(true).await.unwrap();
    pairing.await.unwrap().expect("phone paired");
    assert_eq!(be.paired_device().unwrap().unwrap().name, "E2E-PHONE");
    until("synced", 20, || be.status().state == SyncState::Synced && ph.status().state == SyncState::Synced).await;

    // The UI was told: library status, tray states ended on Synced, sync events carried as JSON.
    assert!(pc.out.events.lock().unwrap().iter().any(|(n, _)| n == ev::LIBRARY), "library-status was emitted");
    until("the tray shows synced", 10, || pc.out.tray.lock().unwrap().last() == Some(&TrayState::Synced)).await;
    assert!(pc.out.events.lock().unwrap().iter().any(|(n, v)| n == ev::CORE && v["t"] == "sync"));

    // ----- phone -> PC: three camera shots appear as real files in the library root -----
    let mut shots: Vec<(ItemId, Vec<u8>)> = Vec::new();
    for i in 0..3u8 {
        let src = tmp.path().join(format!("cam{i}.jpg"));
        let bytes = jpeg(10 + i, 4000 + i as usize);
        std::fs::write(&src, &bytes).unwrap();
        let id = ph
            .add_local_file(NewLocalFile { src, name: format!("IMG_{i}.jpg"), folder: None, captured_at: 1_790_000_000_000, geo: None, thumb: None, how: AddKind::Captured, ntfs: None })
            .unwrap();
        shots.push((id, bytes));
    }
    until("3 files in the library folder", 20, || (0..3).all(|i| pc.root.join(format!("IMG_{i}.jpg")).exists())).await;
    for (i, (_, bytes)) in shots.iter().enumerate() {
        assert_eq!(&std::fs::read(pc.root.join(format!("IMG_{i}.jpg"))).unwrap(), bytes, "bytes identical");
    }
    until("PC lists 3 items", 10, || be.items(ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.len() == 3).await;
    // The PC's rescan must not mistake what sync just wrote for a user drop: still exactly 3 items.
    be.library_rescan();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(be.items(ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.len(), 3, "no duplicates");

    // ----- PC -> phone: a file dropped into the library folder -----
    let dropped = jpeg(77, 3000);
    std::fs::write(pc.root.join("from_explorer.jpg"), &dropped).unwrap();
    until("the phone received the drop", 25, || ph.items(&ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.iter().any(|r| r.name == "from_explorer.jpg")).await;
    let phone_item = ph.items(&ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.into_iter().find(|r| r.name == "from_explorer.jpg").unwrap();
    until("the phone holds the bytes", 20, || ph.item(phone_item.id).unwrap().is_some_and(|v| v.path.is_some())).await;
    assert_eq!(std::fs::read(ph.item(phone_item.id).unwrap().unwrap().path.unwrap()).unwrap(), dropped);

    // ----- Explorer move into a new subfolder: the folder and the move reach the phone -----
    std::fs::create_dir(pc.root.join("Work")).unwrap();
    std::fs::rename(pc.root.join("from_explorer.jpg"), pc.root.join("Work").join("from_explorer.jpg")).unwrap();
    until("the phone sees folder Work", 25, || ph.folders().unwrap().iter().any(|f| f.name == "Work")).await;
    let work = ph.folders().unwrap().into_iter().find(|f| f.name == "Work").unwrap();
    until("the phone item is in Work", 25, || ph.item(phone_item.id).unwrap().is_some_and(|v| v.folder == work.id)).await;
    assert_eq!(be.items(ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.len(), 4, "a move, not a copy");

    // ----- phone moves a photo to Work: the file moves into the PC's Work directory -----
    ph.move_item(shots[0].0, work.id).unwrap();
    until("IMG_0.jpg moved into Work on the PC", 25, || pc.root.join("Work").join("IMG_0.jpg").exists() && !pc.root.join("IMG_0.jpg").exists()).await;
    be.library_rescan();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(be.items(ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.len(), 4);
    assert!(pc.root.join("Work").join("IMG_0.jpg").exists(), "the scan did not undo the placement's move");

    // ----- the library folder disappears: sync pauses, nothing is deleted, it resumes -----
    let gone = pc.root.with_file_name("library-away");
    std::fs::rename(&pc.root, &gone).unwrap();
    be.library_rescan();
    until("sync paused", 15, || be.status().state == SyncState::Paused).await;
    assert!(be.status().message.unwrap().contains("LIBRARY FOLDER NOT FOUND"));
    until("the tray shows offline", 10, || pc.out.tray.lock().unwrap().last() == Some(&TrayState::Offline)).await;
    std::fs::rename(&gone, &pc.root).unwrap();
    be.library_rescan();
    until("sync resumed and synced", 30, || be.status().state == SyncState::Synced).await;
    assert_eq!(be.items(ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.len(), 4, "nothing was lost");
    assert_eq!(ph.items(&ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.len(), 4);

    // ----- unpair from the PC: the phone follows, the files stay -----
    be.unpair().await.unwrap();
    assert!(be.paired_device().unwrap().is_none());
    until("the phone unpaired itself", 15, || ph.paired_device().unwrap().is_none()).await;
    assert!(pc.root.join("IMG_1.jpg").exists(), "synced files stay");

    ph.stop_network().await;
    be.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_phone_using_the_qr_cancels_pairing_and_the_ui_gets_the_message() {
    let pc = start_pc().await;
    let be = &pc.be;
    let tmp = tempfile::tempdir().unwrap();
    let (a, sa) = phone(&tmp.path().join("a"));
    let (b, sb) = phone(&tmp.path().join("b"));
    for (c, s) in [(&a, sa), (&b, sb)] {
        c.start_network(NetworkConfig { bind: "127.0.0.1:0".parse().unwrap(), mdns: false, secret_key: s }).await.unwrap();
    }
    let payload = be.pairing_begin().unwrap().payload;
    let (ca, p1) = (a.clone(), payload.clone());
    let first = tokio::spawn(async move { ca.pair_with(&p1).await });
    until("a code is pending", 10, || !code_events(&pc.out).is_empty()).await;
    let second = b.pair_with(&payload).await;
    assert!(second.is_err(), "the second phone is refused");
    assert!(first.await.unwrap().is_err(), "the first phone is cancelled too");
    until("the PC shows the warning", 10, || {
        pc.out.events.lock().unwrap().iter().any(|(n, v)| n == ev::CORE && v["t"] == "pairing" && v["kind"] == "cancelled" && v["message"].as_str().is_some_and(|m| m.contains("PAIRING BLOCKED")))
    })
    .await;
    assert!(be.paired_device().unwrap().is_none(), "nobody was paired");
    a.stop_network().await;
    b.stop_network().await;
    be.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_start_on_the_same_profile_keeps_the_identity_and_the_library() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("pc");
    let keys = MemKeys(Mutex::new(None));
    let mk = || StartOptions {
        paths: Paths { data_dir: dir.clone(), bin_dir: dir.join("Bin"), default_library: dir.join("library"), key_user: "test".into() },
        bind_env: Some("127.0.0.1".into()),
        app_version: "e2e".into(),
        device_name: "E2E-PC".into(),
        library: LibraryOptions::default(),
        mdns: false,
        check_firewall: false,
    };
    let first = Backend::start(mk(), &keys, Arc::new(Collect::default())).await.unwrap();
    let (id1, root1) = (first.core.config().self_id, first.library.root().unwrap());
    std::fs::write(root1.join("keep.jpg"), jpeg(5, 500)).unwrap();
    until("imported", 15, || first.items(ItemQuery { limit: 10, ..Default::default() }).unwrap().rows.len() == 1).await;
    first.shutdown().await;
    drop(first);
    let second = Backend::start(mk(), &keys, Arc::new(Collect::default())).await.unwrap();
    assert_eq!(second.core.config().self_id, id1, "same key store = same identity");
    assert_eq!(second.library.root().unwrap(), root1, "the library path was remembered");
    assert_eq!(second.items(ItemQuery { limit: 10, ..Default::default() }).unwrap().rows.len(), 1, "no re-import, no duplicate");
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn any_file_travels_both_ways_the_pc_marks_what_it_receives_and_a_dropped_file_syncs() {
    use tugboat_desktop_lib::{drop::NativeDrop, winfs};

    let pc = start_pc().await;
    let be = &pc.be;
    let tmp = tempfile::tempdir().unwrap();
    let (ph, secret) = phone(tmp.path());
    ph.start_network(NetworkConfig { bind: "127.0.0.1:0".parse().unwrap(), mdns: false, secret_key: secret }).await.unwrap();
    let payload = be.pairing_begin().unwrap().payload;
    let phone_core = ph.clone();
    let pairing = tokio::spawn(async move { phone_core.pair_with(&payload).await });
    until("the PC shows a code", 10, || !code_events(&pc.out).is_empty()).await;
    be.pairing_confirm(true).await.unwrap();
    pairing.await.unwrap().expect("phone paired");
    until("synced", 20, || be.status().state == SyncState::Synced && ph.status().state == SyncState::Synced).await;

    // ----- phone -> PC: a PDF and a file with no extension -----
    let pdf = b"%PDF-1.7 report body".to_vec();
    let make = b"all:\n\techo hi\n".to_vec();
    for (name, bytes) in [("report.pdf", &pdf), ("Makefile", &make)] {
        let src = tmp.path().join(format!("src-{name}"));
        std::fs::write(&src, bytes).unwrap();
        ph.add_local_file(NewLocalFile { src, name: name.into(), folder: None, captured_at: 1_790_000_000_000, geo: None, thumb: None, how: AddKind::Added, ntfs: None }).unwrap();
    }
    until("both files in the library folder", 25, || pc.root.join("report.pdf").exists() && pc.root.join("Makefile").exists()).await;
    assert_eq!(std::fs::read(pc.root.join("report.pdf")).unwrap(), pdf);
    assert_eq!(std::fs::read(pc.root.join("Makefile")).unwrap(), make);
    for n in ["report.pdf", "Makefile"] {
        assert_eq!(winfs::read_zone_mark(&pc.root.join(n)).as_deref(), Some(winfs::ZONE_TEXT), "{n} was marked as downloaded when it arrived");
    }
    let rows = be.items(ItemQuery { limit: 100, ..Default::default() }).unwrap().rows;
    let r = rows.iter().find(|i| i.name == "report.pdf").unwrap();
    assert_eq!((r.kind, r.ext.as_str()), (tugboat_core::MediaKind::File, "pdf"));
    assert_eq!(rows.iter().find(|i| i.name == "Makefile").unwrap().ext, "");
    // Open / Show in Explorer work for any held file inside the library, whatever its type.
    for (name, item) in [("report.pdf", r), ("Makefile", rows.iter().find(|i| i.name == "Makefile").unwrap())] {
        let p = be.item_path(item.id).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(tugboat_desktop_lib::library::placement::same_path(&p, &pc.root.join(name)), "{name}: {}", p.display());
    }

    // ----- PC -> phone: a Word file and a file with no extension put in the folder by the user -----
    let docx = b"PK docx bytes".to_vec();
    std::fs::write(pc.root.join("contract.docx"), &docx).unwrap();
    std::fs::write(pc.root.join("README"), b"read me").unwrap();
    let phone_rows = || ph.items(&ItemQuery { limit: 100, ..Default::default() }).unwrap().rows;
    until("the phone has both", 30, || ["contract.docx", "README"].iter().all(|n| phone_rows().iter().any(|r| r.name == *n))).await;
    let docx_item = phone_rows().into_iter().find(|r| r.name == "contract.docx").unwrap();
    assert_eq!(docx_item.kind, tugboat_core::MediaKind::File);
    until("the phone holds the docx bytes", 20, || ph.item(docx_item.id).unwrap().is_some_and(|v| v.path.is_some())).await;
    assert_eq!(std::fs::read(ph.item(docx_item.id).unwrap().unwrap().path.unwrap()).unwrap(), docx);
    assert!(winfs::read_zone_mark(&pc.root.join("contract.docx")).is_none(), "the user's own file gets no mark");

    // ----- a shortcut in the folder never leaves the PC and is listed -----
    std::fs::write(pc.root.join("evil.lnk"), b"x").unwrap();
    be.library_rescan();
    until("the shortcut is listed as refused", 15, || be.library_status().not_synced.iter().any(|n| n.name == "evil.lnk" && n.reason.starts_with("REFUSED"))).await;

    // ----- a file dropped on the window: copied in, synced to the phone, source untouched -----
    let usb = tmp.path().join("usb");
    std::fs::create_dir_all(&usb).unwrap();
    let dropped = usb.join("dropped.pdf");
    std::fs::write(&dropped, b"%PDF-1.4 dropped").unwrap();
    let target = be.drop_end(); // what the window's Drop event does first
    let rep = be.drop_files(NativeDrop::from_native(std::slice::from_ref(&dropped)), target);
    assert_eq!(rep.copied, ["dropped.pdf"], "{rep:?}");
    assert!(dropped.exists(), "the source is never touched");
    assert!(pc.root.join("dropped.pdf").is_file());
    until("the phone has the dropped pdf", 30, || phone_rows().iter().any(|r| r.name == "dropped.pdf")).await;
    assert!(!phone_rows().iter().any(|r| r.name == "evil.lnk"), "a refused shortcut never reached the phone");
    {
        let evs = pc.out.events.lock().unwrap();
        assert!(evs.iter().any(|(n, v)| n == ev::DROP_RESULT && v["copied"][0] == "dropped.pdf"), "the UI got the result");
        assert!(evs.iter().any(|(n, v)| n == ev::DROP_STATE && v["t"] == "leave"));
    }

    ph.stop_network().await;
    be.shutdown().await;
}

/// A drop that waits behind another keeps the folder it was dropped on, whatever is dragged over the window
/// afterwards, and the window's event thread never waits for the folder lookup (it runs on a worker thread).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drop_keeps_the_folder_it_was_dropped_on_and_a_finished_drag_is_never_announced_late() {
    use tugboat_core::Retention;
    use tugboat_desktop_lib::drop::NativeDrop;

    let pc = start_pc().await;
    let be = &pc.be;
    let (y, z) = (be.core.create_folder("Y", Retention::Keep).unwrap(), be.core.create_folder("Z", Retention::Keep).unwrap());
    let usb = pc._tmp.path().join("usb");
    std::fs::create_dir_all(&usb).unwrap();
    let (a, b) = (usb.join("a.txt"), usb.join("b.txt"));
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&b, b"b").unwrap();
    let enters = |name: &str| pc.out.events.lock().unwrap().iter().filter(|(n, v)| n == ev::DROP_STATE && v["t"] == "enter" && v["target_name"] == name).count();

    // B is dropped while the overlay says "Y".
    be.drop_target_set(Some(y));
    be.drop_enter(vec![b.clone()]);
    until("the overlay names Y", 10, || enters("Y") == 1).await;
    let for_b = be.drop_end();
    assert_eq!(for_b.as_ref().map(|t| t.name.as_str()), Some("Y"));
    // Later drags change what is on screen and what the slot holds.
    be.drop_target_set(Some(z));
    be.drop_enter(vec![a.clone()]);
    until("the overlay names Z", 10, || enters("Z") == 1).await;
    be.drop_leave();
    // B's turn comes only now: it still goes to Y.
    let rep = be.drop_files(NativeDrop::from_native(std::slice::from_ref(&b)), for_b);
    assert_eq!((rep.folder_name.as_str(), rep.copied.as_slice()), ("Y", ["b.txt".to_string()].as_slice()), "{rep:?}");
    assert!(pc.root.join("Y").join("b.txt").is_file() && !pc.root.join("Z").join("b.txt").exists());

    // A drag that ends at once is never announced after its Leave, whenever the lookup finishes.
    for _ in 0..20 {
        be.drop_enter(vec![a.clone()]);
        be.drop_leave();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let last = pc.out.events.lock().unwrap().iter().filter(|(n, _)| n == ev::DROP_STATE).map(|(_, v)| v["t"].clone()).next_back();
    assert_eq!(last, Some(Value::from("leave")), "the overlay is not left showing");
    be.shutdown().await;
}

/// Link a phone-role core to the PC and wait until both are synced.
async fn link_phone(pc: &Pc, tmp: &std::path::Path) -> Core {
    link_phone_keeping_key(pc, tmp).await.0
}

/// `link_phone`, also giving the phone's key so a test can stop its network and start it again.
async fn link_phone_keeping_key(pc: &Pc, tmp: &std::path::Path) -> (Core, [u8; 32]) {
    let (ph, secret) = phone(tmp);
    ph.start_network(NetworkConfig { bind: "127.0.0.1:0".parse().unwrap(), mdns: false, secret_key: secret }).await.unwrap();
    let start = pc.be.pairing_begin().unwrap();
    let (core, payload) = (ph.clone(), start.payload.clone());
    let pairing = tokio::spawn(async move { core.pair_with(&payload).await });
    until("the PC shows a code", 10, || !code_events(&pc.out).is_empty()).await;
    pc.be.pairing_confirm(true).await.unwrap();
    pairing.await.unwrap().expect("phone paired");
    until("synced", 20, || pc.be.status().state == SyncState::Synced && ph.status().state == SyncState::Synced).await;
    (ph, secret)
}

fn files_under(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map(|rd| rd.filter_map(|e| e.ok()).filter(|e| e.path().is_file()).count()).unwrap_or(0)
}

// Delete forever, both directions, through the real library folder: the phone deletes one item forever
// and the PC's copy leaves its Bin; the PC empties its Bin and the phone's copies go. Both legs here
// start from a copy already in the PC's Bin; the next test covers a file still in the library folder
// when the purge arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_forever_removes_files_from_the_library_and_the_bin_in_both_directions() {
    let pc = start_pc().await;
    let be = &pc.be;
    let tmp = tempfile::tempdir().unwrap();
    let ph = link_phone(&pc, tmp.path()).await;
    let bin_dir = pc._tmp.path().join("pc").join("Bin");
    let all = || ItemQuery { limit: 100, ..Default::default() };
    let mut ids = Vec::new();
    for i in 0..3u8 {
        let src = tmp.path().join(format!("cam{i}.jpg"));
        std::fs::write(&src, jpeg(30 + i, 4000)).unwrap();
        ids.push(ph.add_local_file(NewLocalFile { src, name: format!("DEL_{i}.jpg"), folder: None, captured_at: 1_790_000_000_000, geo: None, thumb: None, how: AddKind::Captured, ntfs: None }).unwrap());
    }
    until("three files in the library", 25, || (0..3).all(|i| pc.root.join(format!("DEL_{i}.jpg")).exists())).await;

    // The phone deletes item 0 and then forever: the PC's Bin copy goes, nothing stays in the library.
    ph.delete_item(ids[0]).unwrap();
    until("the PC moved it to its Bin", 25, || !pc.root.join("DEL_0.jpg").exists() && files_under(&bin_dir) == 1).await;
    ph.purge_item(ids[0]).unwrap();
    until("the PC removed its Bin copy", 25, || files_under(&bin_dir) == 0).await;
    until("the PC lists two items and an empty Bin", 10, || {
        be.items(all()).unwrap().rows.len() == 2 && be.items(ItemQuery { bin: true, ..all() }).unwrap().rows.is_empty()
    })
    .await;
    be.library_rescan();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!pc.root.join("DEL_0.jpg").exists(), "the rescan did not bring it back");
    assert_eq!(be.items(all()).unwrap().rows.len(), 2);
    assert_eq!(be.bin_count().unwrap().count, 0);

    // The PC deletes the other two and empties its Bin: the count it shows is the count it deletes.
    for i in 1..3 {
        let id = be.items(all()).unwrap().rows.into_iter().find(|r| r.name == format!("DEL_{i}.jpg")).unwrap().id;
        be.delete_item(id).unwrap();
    }
    until("both in the PC's Bin", 15, || be.bin_count().unwrap().count == 2 && files_under(&bin_dir) == 2).await;
    let asked = be.bin_count().unwrap();
    assert_eq!(be.empty_bin(asked.count, &asked.token).unwrap(), 2);
    until("the PC's Bin folder is empty", 15, || files_under(&bin_dir) == 0).await;
    until("the phone dropped both items", 25, || {
        ids.iter().all(|id| ph.item(*id).unwrap().is_none()) && ph.items(&ItemQuery { bin: true, limit: 100, ..Default::default() }).unwrap().rows.is_empty()
    })
    .await;
    be.library_rescan();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(be.items(all()).unwrap().rows.is_empty() && files_under(&pc.root) == 0, "nothing left in the library");
    assert!(be.status().message.is_none(), "{:?}", be.status());

    ph.stop_network().await;
    be.shutdown().await;
}

// The PC is away while the phone deletes an item and then deletes it forever, so the PC gets both in one
// record while the file is still in the library folder: one that arrived by sync, and one the user dropped
// into the folder a moment before. The files go at once, the user's other file in the same folder stays,
// and neither the watcher nor a rescan reports anything or imports a file back as a new item.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_forever_of_a_file_still_in_the_library_leaves_nothing_to_import() {
    let pc = start_pc().await;
    let be = &pc.be;
    let tmp = tempfile::tempdir().unwrap();
    let (ph, secret) = link_phone_keeping_key(&pc, tmp.path()).await;
    let bin_dir = pc._tmp.path().join("pc").join("Bin");
    let all = || ItemQuery { limit: 100, ..Default::default() };

    // One file from the phone, one dropped by the user a moment ago, and one the user keeps.
    let src = tmp.path().join("cam.jpg");
    std::fs::write(&src, jpeg(41, 4000)).unwrap();
    let from_phone = ph.add_local_file(NewLocalFile { src, name: "AWAY_A.jpg".into(), folder: None, captured_at: 1_790_000_000_000, geo: None, thumb: None, how: AddKind::Captured, ntfs: None }).unwrap();
    until("the phone's file is in the library", 25, || pc.root.join("AWAY_A.jpg").exists()).await;
    std::fs::write(pc.root.join("mine.jpg"), jpeg(43, 3000)).unwrap();
    std::fs::write(pc.root.join("dropped_b.jpg"), jpeg(42, 3000)).unwrap();
    let phone_rows = || ph.items(&ItemQuery { limit: 100, ..Default::default() }).unwrap().rows;
    until("the phone has the dropped file and the user's file", 25, || phone_rows().iter().any(|r| r.name == "dropped_b.jpg") && phone_rows().iter().any(|r| r.name == "mine.jpg")).await;
    let dropped = phone_rows().into_iter().find(|r| r.name == "dropped_b.jpg").unwrap().id;
    until("the PC lists all three", 10, || be.items(all()).unwrap().rows.len() == 3).await;

    // The PC is away: the phone deletes both forever.
    ph.stop_network().await;
    for id in [from_phone, dropped] {
        ph.delete_item(id).unwrap();
        ph.purge_item(id).unwrap();
    }
    ph.start_network(NetworkConfig { bind: "127.0.0.1:0".parse().unwrap(), mdns: false, secret_key: secret }).await.unwrap();
    until("both files left the library", 40, || !pc.root.join("AWAY_A.jpg").exists() && !pc.root.join("dropped_b.jpg").exists()).await;
    until("the PC lists only the kept file", 15, || be.items(all()).unwrap().rows.len() == 1).await;
    assert_eq!(files_under(&bin_dir), 0, "nothing was moved to the Bin either");

    be.library_rescan();
    tokio::time::sleep(Duration::from_millis(900)).await;
    let rows = be.items(all()).unwrap().rows;
    assert!(rows.len() == 1 && rows[0].name == "mine.jpg", "the rescan imported nothing: {:?}", rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>());
    assert!(pc.root.join("mine.jpg").exists(), "the user's other file is untouched");
    assert!(!pc.root.join("AWAY_A.jpg").exists() && !pc.root.join("dropped_b.jpg").exists());
    assert_eq!(be.bin_count().unwrap().count, 0);
    // The activity says "deleted forever" for each and nothing about a lost, missing or user-deleted file.
    let acts = be.activity(None, 200).unwrap();
    for id in [from_phone, dropped] {
        let mine: Vec<_> = acts.iter().filter(|a| a.item == Some(id)).map(|a| a.action).collect();
        assert!(mine.contains(&tugboat_core::ActivityAction::DeletedForever), "{mine:?}");
        assert!(
            mine.iter().all(|a| !matches!(a, tugboat_core::ActivityAction::Deleted | tugboat_core::ActivityAction::Lost | tugboat_core::ActivityAction::Refused | tugboat_core::ActivityAction::Paused)),
            "{mine:?}"
        );
    }
    assert!(be.status().message.is_none(), "{:?}", be.status());

    ph.stop_network().await;
    be.shutdown().await;
}

// The update install stops the library thread and the network first and starts them again when the installer does
// not run (the user said no to the Windows prompt). The call comes from a blocking thread, as in the app.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_sync_for_an_update_install_is_undone_when_the_installer_does_not_start() {
    let pc = start_pc().await;
    let be = pc.be.clone();
    let tmp = tempfile::tempdir().unwrap();
    let ph = link_phone(&pc, tmp.path()).await;

    // resume without a quiesce does nothing.
    let bind = be.app_info().bind;
    be.resume().await;
    assert_eq!(be.app_info().bind, bind);
    assert_eq!(be.status().state, SyncState::Synced);

    let rt = tokio::runtime::Handle::current();
    let (b, r) = (be.clone(), rt.clone());
    tokio::task::spawn_blocking(move || {
        r.block_on(b.quiesce());
        r.block_on(b.quiesce()); // twice is the same as once
    })
    .await
    .unwrap();
    until("the phone sees the PC go", 20, || ph.status().state != SyncState::Synced).await;

    let (b, r) = (be.clone(), rt);
    tokio::task::spawn_blocking(move || r.block_on(b.resume())).await.unwrap();
    assert_eq!(be.app_info().network_error, None);
    until("synced again", 40, || be.status().state == SyncState::Synced && ph.status().state == SyncState::Synced).await;

    // The library thread is back too: a file dropped into the folder reaches the phone.
    std::fs::write(pc.root.join("after_quiesce.jpg"), jpeg(5, 2500)).unwrap();
    until("the phone received the file", 30, || ph.items(&ItemQuery { limit: 100, ..Default::default() }).unwrap().rows.iter().any(|r| r.name == "after_quiesce.jpg")).await;

    ph.stop_network().await;
    be.shutdown().await;
}

// The "Check for updates" switch and the last check time live in Core's local settings (this PC only): they survive
// a restart, the switch is on until it is turned off, and a stored value that is not "false" keeps it on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_update_switch_and_the_last_check_time_are_kept_on_this_pc() {
    let pc = start_pc().await;
    let be = pc.be.clone();

    let up = Updates::new(be.clone(), "0.3.0");
    assert!(up.enabled(), "on by default");
    assert_eq!(up.last_check(), None);

    up.set_enabled(false).unwrap();
    assert_eq!(be.core.get_value(SETTING_KEY).unwrap().as_deref(), Some("false"));
    assert!(!Updates::new(be.clone(), "0.3.0").enabled(), "still off after a restart");
    // The Core Settings the phone also knows are untouched by it.
    assert_eq!(be.settings().unwrap(), Settings { udp_port: be.settings().unwrap().udp_port, ..Settings::default() });

    up.set_enabled(true).unwrap();
    assert!(Updates::new(be.clone(), "0.3.0").enabled());

    be.core.set_value(SETTING_KEY, "garbage").unwrap();
    assert!(Updates::new(be.clone(), "0.3.0").enabled(), "only an explicit false turns it off");

    be.core.set_value(LAST_CHECK_KEY, "1800000000").unwrap();
    assert_eq!(Updates::new(be.clone(), "0.3.0").last_check(), Some(1_800_000_000), "the saved time is read back after a restart");
    be.core.set_value(LAST_CHECK_KEY, "not a number").unwrap();
    assert_eq!(Updates::new(be.clone(), "0.3.0").last_check(), None, "an unreadable time means never checked");

    be.shutdown().await;
}

// A version found and downloaded before a restart is checked again soon after start, but only while it is still newer
// than the running version; the mark that makes the window show after an install is taken exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_found_update_survives_a_restart_and_the_window_mark_is_taken_once() {
    let pc = start_pc().await;
    let be = pc.be.clone();

    assert!(!Updates::new(be.clone(), "0.3.0").found_pending(), "nothing found yet");
    be.core.set_value(FOUND_KEY, "0.4.0").unwrap();
    assert!(Updates::new(be.clone(), "0.3.0").found_pending(), "newer than the running version: fetch it again");
    assert!(Updates::new(be.clone(), "0.3.9").found_pending());
    // Installed by hand since (same or newer): forgotten, and the stored value is cleared.
    assert!(!Updates::new(be.clone(), "0.4.0").found_pending());
    assert_eq!(be.core.get_value(FOUND_KEY).unwrap().as_deref().map(str::trim), Some(""));
    be.core.set_value(FOUND_KEY, "junk").unwrap();
    assert!(!Updates::new(be.clone(), "0.3.0").found_pending(), "an unreadable version is never fetched");

    assert!(!take_show_after_update(&be), "no mark: a start with Windows stays in the tray");
    be.core.set_value(SHOW_AFTER_KEY, "1").unwrap();
    assert!(take_show_after_update(&be), "the start after an install shows the window");
    assert!(!take_show_after_update(&be), "only that one start");

    be.shutdown().await;
}
