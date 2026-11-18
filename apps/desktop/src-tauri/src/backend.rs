//! The desktop backend: Core + the PC library + keys + network + firewall, with no Tauri
//! types in it, so the whole thing runs (and is tested) without a window. `commands.rs` is the
//! thin Tauri layer over these methods; `lib.rs` wires the window, tray and plugins.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tugboat_core::{
    events::{Event, PairingEvent, SyncStatus},
    facade::{device_id_from_secret, NetworkConfig},
    names, ActivityEntry, Clock, Core, CoreConfig, DeviceId, FolderId, FolderKind, FolderView, ItemId, ItemQuery, ItemView, Page, Role, Settings, SystemClock,
    INBOX_ID,
};
use tokio::{sync::broadcast, task::JoinHandle};
use ts_rs::TS;

use crate::{
    drop::{self, DropLine, DropProgress, DropReport, DropState, DropTarget, NativeDrop},
    error::{CmdError, CmdResult},
    firewall::{self, FirewallInfo},
    keys::{self, KeyStore},
    library::{
        config::LibraryConfig,
        placement::{is_inside, PcLibrary},
        Library, LibraryOptions, LibraryStatus, Worker,
    },
    paths::{self, Paths},
    qr,
    tray::{self, TrayState},
};

/// Event names the webview listens for (apps/desktop/src/lib/api.ts mirrors these).
pub mod ev {
    /// `tugboat_core::Event`.
    pub const CORE: &str = "core-event";
    /// `LibraryStatus`.
    pub const LIBRARY: &str = "library-status";
    /// `FirewallInfo`.
    pub const FIREWALL: &str = "firewall-status";
    /// The event stream lagged: re-read everything on screen.
    pub const RESYNC: &str = "resync";
    /// `TrayState`.
    pub const TRAY: &str = "tray-state";
    /// `DropState`: files dragged over the window (the overlay names the target).
    pub const DROP_STATE: &str = "drop-state";
    /// `DropProgress`: how far the file being copied is.
    pub const DROP_PROGRESS: &str = "drop-progress";
    /// `DropReport`: what the drop did.
    pub const DROP_RESULT: &str = "drop-result";
}

/// Where backend events go: the Tauri window, or a list in a test.
pub trait Emitter: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: serde_json::Value);
    /// The tray icon and tooltip should show this.
    fn tray(&self, state: TrayState, tooltip: &str);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AppInfo {
    pub version: String,
    pub device_name: String,
    /// This PC's own device id: the activity list uses it to tell "This PC" from a former phone.
    pub device_id: DeviceId,
    /// The UDP address the endpoint is bound to.
    pub bind: String,
    /// Set when the network could not start (port in use, no interface): sync is down.
    pub network_error: Option<String>,
    pub data_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairedDevice {
    pub id: DeviceId,
    pub name: String,
    #[ts(type = "number | null")]
    pub paired_at: Option<i64>,
    #[ts(type = "number | null")]
    pub last_seen_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingStart {
    /// The QR as SVG text, black on white.
    pub svg: String,
    /// The same payload as text (for the fake-phone tool and "copy pairing text").
    pub payload: String,
    /// PC clock, unix ms: when the QR stops working.
    #[ts(type = "number")]
    pub expires_at: i64,
}

pub struct StartOptions {
    pub paths: Paths,
    /// Value of TUGBOAT_BIND, if set.
    pub bind_env: Option<String>,
    pub app_version: String,
    pub device_name: String,
    pub library: LibraryOptions,
    pub mdns: bool,
    /// Tests skip the COM firewall query.
    pub check_firewall: bool,
}

struct Shared {
    emitter: Arc<dyn Emitter>,
    /// The library thread, told to scan when Core's records change (see `Worker::poke`).
    worker: Arc<Mutex<Option<Worker>>>,
    tray: Mutex<Option<TrayState>>,
    pairing_open: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn emit<T: Serialize>(&self, name: &str, payload: &T) {
        match serde_json::to_value(payload) {
            Ok(v) => self.emitter.emit(name, v),
            Err(e) => tracing::error!("event {name} could not be serialised: {e}"),
        }
    }

    fn set_tray(&self, s: &SyncStatus) {
        let state = tray::tray_state(s);
        // The tooltip carries the peer and counts, so it is refreshed on every status.
        self.emitter.tray(state, &tray::tooltip(state, s));
        let changed = lock(&self.tray).replace(state) != Some(state);
        if changed {
            self.emit(ev::TRAY, &state);
        }
    }

    fn on_core_event(&self, e: &Event) {
        match e {
            Event::Sync(s) => self.set_tray(s),
            Event::ItemsChanged { .. } | Event::FoldersChanged => {
                if let Some(w) = lock(&self.worker).as_ref() {
                    w.poke();
                }
            }
            Event::Pairing(PairingEvent::WindowOpen { .. }) => self.pairing_open.store(true, Ordering::SeqCst),
            Event::Pairing(PairingEvent::Completed { .. } | PairingEvent::Cancelled { .. }) => self.pairing_open.store(false, Ordering::SeqCst),
            _ => {}
        }
        self.emit(ev::CORE, e);
    }
}

pub struct Backend {
    pub core: Core,
    pub library: Library,
    pub place: Arc<PcLibrary>,
    pub paths: Paths,
    shared: Arc<Shared>,
    worker: Arc<Mutex<Option<Worker>>>,
    firewall: Arc<Mutex<FirewallInfo>>,
    secret: [u8; 32],
    bind_env: Option<String>,
    mdns: bool,
    app_version: String,
    bind: Mutex<SocketAddr>,
    network_error: Mutex<Option<String>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The folder the gallery is showing (None = Inbox), set by the UI. Only a folder id: never a path.
    drop_view: Mutex<Option<FolderId>>,
    /// The target named by the overlay at the moment files entered the window.
    drop_locked: Mutex<Option<DropTarget>>,
    /// One drop copies at a time; a second waits its turn.
    drop_run: Mutex<()>,
}

impl Backend {
    /// Start everything. Must run inside a tokio runtime (Core's network tasks spawn on it).
    /// Errors here are fatal for the app (database newer than the app, key store failing,
    /// unreadable library config); a network that cannot start is NOT fatal: it is reported
    /// through `AppInfo.network_error` and an error event.
    pub async fn start(opts: StartOptions, keystore: &dyn KeyStore, emitter: Arc<dyn Emitter>) -> Result<Arc<Backend>, String> {
        let paths = opts.paths.clone();
        std::fs::create_dir_all(&paths.data_dir).map_err(|e| format!("CANNOT CREATE {} - {e}", paths.data_dir.display()))?;
        let secret = keys::load_or_create(keystore).map_err(|e| e.to_string())?;
        let cfg = LibraryConfig::load(&paths.data_dir).map_err(|e| e.to_string())?;
        let place = Arc::new(PcLibrary::new(cfg.path.clone(), paths.bin_dir.clone(), cfg.dirs.clone()));

        let core_cfg = CoreConfig {
            data_dir: paths.data_dir.clone(),
            role: Role::Pc,
            self_id: device_id_from_secret(&secret),
            device_name: opts.device_name.clone(),
            app_version: opts.app_version.clone(),
        };
        let (core, rx) = Core::open(core_cfg, Arc::new(SystemClock), place.clone()).map_err(|e| format!("{e}"))?;

        let worker: Arc<Mutex<Option<Worker>>> = Arc::new(Mutex::new(None));
        let shared = Arc::new(Shared { emitter: emitter.clone(), worker: worker.clone(), tray: Mutex::new(None), pairing_open: AtomicBool::new(false) });
        let notify_shared = shared.clone();
        let library = Library::open(
            core.clone(),
            place.clone(),
            paths.data_dir.clone(),
            cfg,
            &paths.default_library,
            opts.library.clone(),
            Box::new(move |s: &LibraryStatus| notify_shared.emit(ev::LIBRARY, s)),
        )
        .map_err(|e| format!("CANNOT PREPARE THE LIBRARY FOLDER - {e}"))?;

        let backend = Arc::new(Backend {
            core: core.clone(),
            library: library.clone(),
            place,
            paths,
            shared: shared.clone(),
            worker,
            firewall: Arc::new(Mutex::new(FirewallInfo::from_status(firewall::FirewallStatus::Ok))),
            secret,
            bind_env: opts.bind_env,
            mdns: opts.mdns,
            app_version: opts.app_version,
            bind: Mutex::new(SocketAddr::from(([0, 0, 0, 0], 0))),
            network_error: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            drop_view: Mutex::new(None),
            drop_locked: Mutex::new(None),
            drop_run: Mutex::new(()),
        });

        // A quit during a drop leaves its temp copy in `.tugboat`: remove it.
        if let Some(root) = library.root() {
            let n = drop::clean_stale(&root);
            if n > 0 {
                tracing::info!("removed {n} unfinished drop copies");
            }
        }
        shared.set_tray(&core.status());
        *lock(&backend.worker) = Some(library.start());
        backend.restart_network().await;

        let mut tasks = lock(&backend.tasks);
        tasks.push(tokio::spawn(pump(rx, shared.clone(), core.clone())));
        tasks.push(tokio::spawn(ticker(core.clone(), shared.clone())));
        if opts.check_firewall {
            tasks.push(tokio::spawn(firewall_loop(backend.firewall.clone(), shared.clone(), core.clone())));
        }
        drop(tasks);
        Ok(backend)
    }

    /// Stop the tasks, the library worker and the network. Tests use it; the app just exits.
    pub async fn shutdown(&self) {
        for t in lock(&self.tasks).drain(..) {
            t.abort();
        }
        lock(&self.worker).take();
        self.core.stop_network().await;
    }

    /// (Re)start the network from the saved port and the bind override. A failure is stored
    /// and emitted, never dropped.
    pub async fn restart_network(&self) {
        self.core.stop_network().await;
        let result = async {
            let port = self.core.settings().map_err(|e| e.to_string())?.udp_port;
            let bind = paths::parse_bind(self.bind_env.as_deref(), port)?;
            *lock(&self.bind) = bind;
            self.core.start_network(NetworkConfig { bind, mdns: self.mdns, secret_key: self.secret }).await.map_err(|e| e.to_string())
        }
        .await;
        match result {
            Ok(()) => *lock(&self.network_error) = None,
            Err(e) => {
                let msg = format!("NETWORK NOT STARTED - {e}");
                tracing::error!("{msg}");
                *lock(&self.network_error) = Some(msg.clone());
                self.shared.emit(ev::CORE, &Event::Error { kind: "network".into(), message: msg });
            }
        }
    }

    // ---------- app ----------

    /// True for a QA run (TUGBOAT_PROFILE set): it must not register itself with Windows.
    pub fn paths_profile_active(&self) -> bool {
        self.paths.key_user != "pc-secret-key"
    }

    pub fn app_info(&self) -> AppInfo {
        AppInfo {
            version: self.app_version.clone(),
            device_name: self.core.config().device_name.clone(),
            device_id: self.core.config().self_id,
            bind: lock(&self.bind).to_string(),
            network_error: lock(&self.network_error).clone(),
            data_dir: self.paths.data_dir.display().to_string(),
        }
    }

    pub fn status(&self) -> SyncStatus {
        self.core.status()
    }

    // ---------- gallery, bin, activity ----------

    pub fn folders(&self) -> CmdResult<Vec<FolderView>> {
        Ok(self.core.folders()?)
    }

    pub fn items(&self, q: ItemQuery) -> CmdResult<Page<ItemView>> {
        Ok(self.core.items(&q)?)
    }

    pub fn set_pinned(&self, id: ItemId, pinned: bool) -> CmdResult<()> {
        Ok(self.core.set_pinned(id, pinned)?)
    }

    pub fn move_item(&self, id: ItemId, folder: FolderId) -> CmdResult<()> {
        Ok(self.core.move_item(id, folder)?)
    }

    pub fn rename_item(&self, id: ItemId, name: &str) -> CmdResult<()> {
        Ok(self.core.rename_item(id, name)?)
    }

    pub fn delete_item(&self, id: ItemId) -> CmdResult<()> {
        Ok(self.core.delete_item(id)?)
    }

    pub fn restore_item(&self, id: ItemId) -> CmdResult<()> {
        Ok(self.core.restore_item(id)?)
    }

    pub fn activity(&self, before: Option<i64>, limit: u32) -> CmdResult<Vec<ActivityEntry>> {
        Ok(self.core.activity(before, limit.clamp(1, 500))?)
    }

    /// The file of a held item, for "open" and "show in folder". Only a file that Core holds,
    /// inside the library or the Bin, whose name is not an always-refused one, is ever handed to the
    /// shell: the id comes from the webview, the path never does. Windows still applies its own
    /// "downloaded from another computer" check to a received program (the shell is asked, never
    /// told to skip the zone check).
    pub fn item_path(&self, id: ItemId) -> CmdResult<PathBuf> {
        let view = self.core.item(id)?.ok_or_else(|| CmdError::new("not_found", "ITEM NOT FOUND"))?;
        let path = PathBuf::from(view.path.ok_or_else(|| CmdError::new("not_held", "FILE IS NOT ON THIS PC"))?);
        let in_library = self.library.root().is_some_and(|r| is_inside(&r, &path));
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !(in_library || is_inside(&self.paths.bin_dir, &path)) || names::is_refused_name(&file_name) {
            return Err(CmdError::new("forbidden", "THAT FILE CANNOT BE OPENED"));
        }
        if !path.is_file() {
            return Err(CmdError::new("not_held", "FILE IS NOT ON THIS PC"));
        }
        Ok(path)
    }

    /// JPEG bytes of an item's thumbnail (served to the webview by the `snapthumb` scheme).
    pub fn thumbnail(&self, id: &str) -> Option<Vec<u8>> {
        let id = ItemId(uuid::Uuid::parse_str(id).ok()?);
        let view = self.core.item(id).ok().flatten()?;
        let path = PathBuf::from(view.thumb_path?);
        // Thumbnails live in one folder; anything else is not served.
        is_inside(&self.paths.data_dir.join("thumbs"), &path).then(|| std::fs::read(path).ok()).flatten()
    }

    // ---------- settings ----------

    pub fn settings(&self) -> CmdResult<Settings> {
        Ok(self.core.settings()?)
    }

    pub async fn save_settings(&self, s: Settings) -> CmdResult<()> {
        let old = self.core.settings()?;
        self.core.save_settings(&s)?;
        if old.udp_port != s.udp_port {
            self.restart_network().await;
        }
        Ok(())
    }

    // ---------- pairing ----------

    pub fn paired_device(&self) -> CmdResult<Option<PairedDevice>> {
        Ok(self.core.paired_device()?.map(|d| PairedDevice { id: d.id, name: d.name, paired_at: d.paired_at, last_seen_at: d.last_seen_at }))
    }

    /// Open the pairing window and draw its QR. An already paired phone is replaced only when
    /// the new pairing completes.
    pub fn pairing_begin(&self) -> CmdResult<PairingStart> {
        let replace = self.core.paired_device()?.is_some();
        let payload = self.core.begin_pairing(replace)?;
        let svg = qr::svg(&payload).map_err(|m| CmdError::new("qr", m))?;
        let expires_at = qr::expiry_of(&payload).ok_or_else(|| CmdError::new("qr", "PAIRING DATA HAS NO EXPIRY"))?;
        Ok(PairingStart { svg, payload, expires_at })
    }

    pub async fn pairing_confirm(&self, accept: bool) -> CmdResult<()> {
        Ok(self.core.confirm_pairing(accept).await?)
    }

    pub fn pairing_cancel(&self) {
        self.core.cancel_pairing();
    }

    pub async fn unpair(&self) -> CmdResult<()> {
        Ok(self.core.unpair().await?)
    }

    // ---------- library ----------

    pub fn library_status(&self) -> LibraryStatus {
        self.library.status()
    }

    fn restart_worker(&self) {
        let mut w = lock(&self.worker);
        w.take(); // drop joins the old thread
        *w = Some(self.library.start());
    }

    pub fn library_choose(&self, path: &str, accept_cloud: bool) -> CmdResult<()> {
        self.library.choose(Path::new(path), accept_cloud)?;
        self.restart_worker();
        Ok(())
    }

    pub fn library_locate(&self, path: &str) -> CmdResult<()> {
        self.library.locate(Path::new(path))?;
        self.restart_worker();
        Ok(())
    }

    pub fn library_resolve_missing(&self, bin: bool) -> CmdResult<()> {
        self.library.resolve_missing(bin)?;
        Ok(())
    }

    /// A full scan on the library thread (the Rescan button).
    pub fn library_rescan(&self) {
        if let Some(w) = lock(&self.worker).as_ref() {
            w.request_scan(true);
        }
    }

    // ---------- drag and drop ----------

    /// The gallery tells which folder it shows (None = All = Inbox). Called on every change, and with
    /// None when the gallery is not on screen, so a drop on another screen says "Copy to Inbox".
    pub fn drop_target_set(&self, folder: Option<FolderId>) {
        *lock(&self.drop_view) = folder;
    }

    /// The folder a drop would land in right now: the viewed one when it is still a live folder,
    /// else Inbox.
    pub fn drop_target(&self) -> CmdResult<DropTarget> {
        let live = self.core.live_folders()?;
        let want = *lock(&self.drop_view);
        let pick = want.and_then(|w| live.iter().find(|(id, _, _)| *id == w)).or_else(|| live.iter().find(|(id, kind, _)| *id == INBOX_ID || *kind == FolderKind::Inbox));
        let (id, kind, name) = pick.cloned().ok_or_else(|| CmdError::new("not_found", "INBOX NOT FOUND"))?;
        Ok(DropTarget { id, kind, name })
    }

    /// Files entered the window: fix the target now (the overlay names it) and tell the overlay.
    pub fn drop_enter(&self, paths: &[PathBuf]) {
        let (files, folders) = drop::count_kinds(paths);
        match self.drop_target() {
            Ok(t) => {
                let state = DropState::Enter { files, folders, target_name: t.name.clone() };
                *lock(&self.drop_locked) = Some(t);
                self.shared.emit(ev::DROP_STATE, &state);
            }
            Err(e) => self.shared.emit(ev::CORE, &Event::Error { kind: "drop".into(), message: e.message }),
        }
    }

    pub fn drop_leave(&self) {
        *lock(&self.drop_locked) = None;
        self.shared.emit(ev::DROP_STATE, &DropState::Leave);
    }

    /// Files were dropped: copy them (blocking: call off the UI thread). Always ends with one `drop-result`.
    pub fn drop_files(&self, files: NativeDrop) -> DropReport {
        let _turn = lock(&self.drop_run);
        let locked = lock(&self.drop_locked).take();
        self.shared.emit(ev::DROP_STATE, &DropState::Leave);
        let report = match locked.map(Ok).unwrap_or_else(|| self.drop_target()) {
            Ok(target) => {
                let shared = self.shared.clone();
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop::copy_dropped(&self.library, files, &target, &mut |p: DropProgress| shared.emit(ev::DROP_PROGRESS, &p))
                }));
                run.unwrap_or_else(|_| DropReport {
                    folder_name: target.name.clone(),
                    failed: vec![DropLine { name: String::new(), reason: "COPY FAILED - internal error, see the log".into() }],
                    ..Default::default()
                })
            }
            Err(e) => DropReport { failed: vec![DropLine { name: String::new(), reason: format!("COPY FAILED - {}", e.message) }], ..Default::default() },
        };
        self.shared.emit(ev::DROP_RESULT, &report);
        report
    }

    // ---------- firewall ----------

    pub fn firewall(&self) -> FirewallInfo {
        lock(&self.firewall).clone()
    }

    /// Ask the firewall now (blocking: call off the UI thread) and publish a change.
    pub fn firewall_recheck(&self) -> FirewallInfo {
        let info = firewall::check(self.core.settings().ok().map(|s| s.udp_port));
        set_firewall(&self.firewall, &self.shared, info.clone());
        info
    }

    /// The [Fix] button: elevated `netsh` adds the allow rule; the next check shows the result.
    pub fn firewall_fix(&self) -> CmdResult<()> {
        firewall::fix().map_err(|m| CmdError::new("firewall", m))
    }
}

fn set_firewall(slot: &Mutex<FirewallInfo>, shared: &Shared, info: FirewallInfo) {
    let changed = {
        let mut cur = lock(slot);
        let changed = *cur != info;
        *cur = info.clone();
        changed
    };
    if changed {
        shared.emit(ev::FIREWALL, &info);
    }
}

/// Core events to the UI (and the tray). A lagged receiver tells the UI to re-read.
async fn pump(mut rx: broadcast::Receiver<Event>, shared: Arc<Shared>, core: Core) {
    loop {
        match rx.recv().await {
            Ok(e) => shared.on_core_event(&e),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("UI event stream lagged by {n}; asking the UI to re-read");
                shared.set_tray(&core.status());
                // The lost events may have held the window's open or close: ask Core.
                shared.pairing_open.store(core.pairing_open(), Ordering::SeqCst);
                shared.emitter.emit(ev::RESYNC, serde_json::Value::Null);
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Core's timers: the Bin sweep at the next due time (at most a minute apart) and, while the
/// pairing window is open, once a second so an expired window closes on time.
async fn ticker(core: Core, shared: Arc<Shared>) {
    loop {
        let c = core.clone();
        let next = match tokio::task::spawn_blocking(move || c.tick()).await {
            Ok(Ok(next)) => next,
            Ok(Err(e)) => {
                shared.emit(ev::CORE, &Event::Error { kind: e.kind().into(), message: format!("BIN SWEEP FAILED - {e}") });
                None
            }
            Err(_) => return, // the runtime is shutting down
        };
        let wait_ms: i64 = if shared.pairing_open.load(Ordering::SeqCst) {
            1000
        } else {
            next.map_or(60_000, |t| (t - SystemClock.now_ms()).clamp(1000, 60_000))
        };
        tokio::time::sleep(Duration::from_millis(wait_ms as u64)).await;
    }
}

/// The firewall is checked at start and every 5 minutes (a policy refresh can change it).
async fn firewall_loop(slot: Arc<Mutex<FirewallInfo>>, shared: Arc<Shared>, core: Core) {
    loop {
        // Read the port each round: the user can change it in Settings.
        let port = core.settings().ok().map(|s| s.udp_port);
        match tokio::task::spawn_blocking(move || firewall::check(port)).await {
            Ok(info) => set_firewall(&slot, &shared, info),
            Err(_) => return,
        }
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}
