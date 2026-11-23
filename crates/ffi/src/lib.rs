//! tugboat-ffi: the phone's view of tugboat-core, exported to Kotlin with UniFFI.
//!
//! Role: PHONE only. `PhoneCore` owns a tokio runtime, the core (SQLite + app-private file
//! storage + sync), a ticker (Bin sweep / pairing expiry) and an event pump. Kotlin never
//! sees iroh, SQL or paths it must build itself.
//!
//! Threading rules for the Kotlin side (also in apps/android/FFI.md):
//! - Every method is a plain blocking call. Call them from Dispatchers.IO / a worker thread,
//!   never from the main thread and never from inside `EventListener.on_event`.
//! - Only `start_network`, `stop_network`, `pair_with` and `unlink` wait on the network;
//!   the rest only touch SQLite and are fast, except `add_file`, which copies and hashes the
//!   file (tens of ms for a photo, seconds for a long video).
//! - `EventListener.on_event` runs on a Rust runtime thread: hand the event to your own
//!   scope and return at once.
//! - Ids are strings (UUID for items and folders, 64 hex chars for devices); times are unix ms.
//!
//! No rule lives here: every method validates its arguments, calls the core facade and
//! converts the result.

mod error;
mod logging;

use std::{
    net::SocketAddr,
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use tugboat_core::{
    facade::{device_id_from_secret, NetworkConfig},
    ops::{AddKind, NewLocalFile},
    places::{Fix, Route},
    ActivityEntry, AppStorage, Clock, Core, CoreConfig, DeviceId, DeviceRow, Event, FolderId, FolderView, Geo, ItemId, ItemQuery, ItemState,
    ItemView, Placement, Retention, Role, SystemClock,
};
use tokio::sync::{broadcast::error::RecvError, Notify};

pub use error::SnapError;
// Core types that are exported unchanged. Their definitions are restated for UniFFI; the
// compiler stops the build if core adds a variant or a field and this list is not updated.
pub use tugboat_core::events::{DeviceStatus, SyncState, SyncStatus};
pub use tugboat_core::{ActivityAction, FolderKind, LocalFileState, MediaKind, Settings};

uniffi::setup_scaffolding!();

#[uniffi::remote(Enum)]
pub enum FolderKind {
    Inbox,
    Keep,
    Unsorted,
    User,
}

#[uniffi::remote(Enum)]
pub enum MediaKind {
    Image,
    Video,
    /// Any other file (0.2.0): shown as a file tile, opened by the phone's default app.
    File,
}

#[uniffi::remote(Enum)]
pub enum LocalFileState {
    Pending,
    Present,
    Binned,
    Purged,
    Lost,
    Refused,
}

#[uniffi::remote(Enum)]
pub enum SyncState {
    Unpaired,
    Offline,
    Connecting,
    Syncing,
    Synced,
    Paused,
    Refused,
}

#[uniffi::remote(Enum)]
pub enum ActivityAction {
    Captured,
    Added,
    Received,
    Moved,
    Renamed,
    Kept,
    Unkept,
    Edited,
    Deleted,
    Binned,
    Expired,
    Restored,
    Returned,
    Purged,
    Refused,
    Conflict,
    Lost,
    FolderCreated,
    FolderRenamed,
    FolderDeleted,
    RetentionChanged,
    Paired,
    Unpaired,
    Paused,
    Resumed,
    SyncError,
    Upgraded,
}

#[uniffi::remote(Record)]
pub struct DeviceStatus {
    pub id: String,
    pub name: String,
    pub state: SyncState,
    pub message: Option<String>,
    pub paired_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    pub last_synced_at: Option<i64>,
    pub waiting: u32,
    pub downloading: u32,
    pub unlinking: bool,
}

#[uniffi::remote(Record)]
pub struct SyncStatus {
    pub state: SyncState,
    pub peer_name: Option<String>,
    pub last_synced_at: Option<i64>,
    pub waiting: u32,
    pub downloading: u32,
    pub storage_low: bool,
    pub message: Option<String>,
    pub devices: Vec<DeviceStatus>,
}

#[uniffi::remote(Record)]
pub struct Settings {
    pub bin_days: u32,
    pub storage_guard_bytes: u64,
    pub missing_pause_files: u32,
    pub missing_pause_pct: u32,
    pub udp_port: u16,
    pub max_blob_bytes: u64,
    pub stay_connected: bool,
}

// ---------- records Kotlin gets ----------

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PhoneConfig {
    /// filesDir. The database, thumbnails, logs and held files all live under it.
    pub data_dir: String,
    /// Shown on the PC ("Pixel 9", the phone model).
    pub device_name: String,
    pub app_version: String,
    /// The 32-byte iroh secret key, unwrapped from the Android Keystore by the app for this
    /// call only. Make one with `generate_secret_key()`. Core never writes it to disk.
    pub secret_key: Vec<u8>,
    /// Tests only: "127.0.0.1:0". None = "0.0.0.0:0" (all interfaces, random UDP port).
    pub bind_addr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FolderInfo {
    pub id: String,
    pub kind: FolderKind,
    pub name: String,
    /// None = Keep (never expires). Otherwise the retention in seconds.
    pub retention_secs: Option<u64>,
    /// Items not in the Bin.
    pub count: u32,
}

/// Derived expiry state of an item (never stored, worked out from synced fields).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Enum)]
pub enum ItemStateInfo {
    /// Deleted by the user; in the Bin since `binned_at_ms`.
    Deleted { binned_at_ms: i64 },
    /// Expired; in the Bin since `binned_at_ms`.
    Expired { binned_at_ms: i64 },
    /// Pinned: "Kept".
    Kept,
    /// In a folder that never expires: "Keep".
    FolderKeep,
    /// "Expires in 5h": `expires_at_ms` minus now.
    Live { expires_at_ms: i64 },
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ItemInfo {
    pub id: String,
    pub name: String,
    pub folder_id: String,
    pub kind: MediaKind,
    pub size: u64,
    pub ext: String,
    pub captured_at_ms: i64,
    pub pinned: bool,
    pub state: ItemStateInfo,
    /// What this phone holds. Pending = waiting to download from the PC.
    pub local: LocalFileState,
    /// Absolute path of the held file (also for items in the Bin), when held.
    pub path: Option<String>,
    /// Absolute path of the JPEG thumbnail, when held (never for a File).
    pub thumb_path: Option<String>,
    /// In the Bin because it EXPIRED and the PC never confirmed holding it: shown as "NOT ON PC".
    pub never_reached_peer: bool,
    /// Names of the linked PCs that do not hold this item's file yet.
    pub missing_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ItemPage {
    pub rows: Vec<ItemInfo>,
    /// Pass back as `after` for the next page; None = last page.
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ActivityInfo {
    /// Pass the last id of a page as `before` for the next page.
    pub id: i64,
    pub at_ms: i64,
    pub item_id: Option<String>,
    pub action: ActivityAction,
    /// Short human text, e.g. "Inbox".
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PairedPc {
    /// 64 hex chars.
    pub id: String,
    pub name: String,
    pub paired_at_ms: Option<i64>,
    pub last_seen_at_ms: Option<i64>,
    /// Last-known "ip:port" addresses, dialed first.
    pub addresses: Vec<String>,
}

/// What a scanned or deep-linked pairing text says, for the "Pair with this PC?" step.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PairingPreview {
    pub pc_name: String,
    pub addresses: Vec<String>,
    /// By the PC's clock. Only the PC decides expiry; this is for display.
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct GeoFix {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: f64,
}

/// A location fix for place matching. Core uses it only if `age_ms` < 2 min and
/// `accuracy_m` <= 100 m.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct LocationFix {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: f64,
    pub age_ms: i64,
}

/// Where the next shot goes by place rules.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum CaptureRoute {
    /// Inside a place: use `folder_id`.
    Place { place_id: String, folder_id: String },
    /// Places exist but there is no usable fix: the "No location" chip, folder Unsorted.
    Unsorted,
    /// No place applies: use the folder on the viewfinder chip (Inbox by default).
    Default,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct NewFile {
    /// A file the app owns (camera output in cacheDir, or the copy made by the share target).
    /// Core COPIES it into app storage; the caller deletes the source afterwards.
    pub src_path: String,
    /// Display name, e.g. "IMG_20261117_140233.jpg" or "report.pdf". ANY file type is accepted.
    /// A photo/video extension (jpg, mp4 ...) must match the file's magic bytes, else
    /// `SnapError.NotMedia`; every other extension, or none, is a plain file. A name that is
    /// always refused (.lnk .url .scf .library-ms .searchConnector-ms, desktop.ini, autorun.inf)
    /// fails with `SnapError.Refused`, and so does a file over `Settings.max_blob_bytes`.
    /// Core never changes the extension: a photo with no extension in its name stays a plain
    /// file, so the app gives photos and videos their sniffed extension (as it already does).
    pub name: String,
    /// None = Inbox.
    pub folder_id: Option<String>,
    pub captured_at_ms: i64,
    pub geo: Option<GeoFix>,
    /// JPEG thumbnail, longest side 512 px, made by the app. Copied by core.
    pub thumb_path: Option<String>,
    /// false = camera shot, true = imported ("Share > Tugboat"). Only changes the activity text.
    pub imported: bool,
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum FfiEvent {
    /// The sync status changed.
    Sync { status: SyncStatus },
    /// These items changed (empty list = unknown set, re-read what you show).
    ItemsChanged { ids: Vec<String> },
    FoldersChanged,
    PlacesChanged,
    ActivityAdded,
    SettingsChanged,
    /// Pairing: the code to show large ("482 913"). The PC owner must see the same.
    PairingCode { code: String, peer_name: String },
    PairingCompleted { device_id: String, peer_name: String },
    /// `code` is the stable PairError tag ("expired", "secret_reused", ...), `message` one line.
    PairingCancelled { code: String, message: String },
    /// A PC was unlinked here, or logged out on the PC (`by_peer`).
    Unpaired { device_id: String, by_peer: bool },
    /// Something failed in the background. `kind` is stable snake_case; `message` is one line.
    Error { kind: String, message: String },
    /// The listener fell behind and events were dropped: re-query everything on screen.
    Resync,
}

#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: FfiEvent);
}

// ---------- conversions ----------

fn parse_uuid(what: &str, s: &str) -> Result<uuid::Uuid, SnapError> {
    uuid::Uuid::parse_str(s.trim()).map_err(|e| SnapError::InvalidInput(format!("{what} is not a valid id ({e}): {s:?}")))
}

fn item_id(s: &str) -> Result<ItemId, SnapError> {
    parse_uuid("item id", s).map(ItemId)
}

fn folder_id(s: &str) -> Result<FolderId, SnapError> {
    parse_uuid("folder id", s).map(FolderId)
}

/// A device id as `DeviceStatus.id` shows it: 64 hex chars.
fn device_id_of(s: &str) -> Result<DeviceId, SnapError> {
    let bad = |why: String| SnapError::InvalidInput(format!("device id is not 64 hex characters ({why}): {s:?}"));
    let raw = hex::decode(s.trim()).map_err(|e| bad(e.to_string()))?;
    let key: [u8; 32] = raw.try_into().map_err(|r: Vec<u8>| bad(format!("{} bytes", r.len())))?;
    Ok(DeviceId(key))
}

fn folder_info(v: FolderView) -> FolderInfo {
    FolderInfo {
        id: v.id.0.to_string(),
        kind: v.kind,
        name: v.name,
        retention_secs: match v.retention {
            Retention::Keep => None,
            Retention::Secs { secs } => Some(secs),
        },
        count: v.count,
    }
}

fn state_info(s: ItemState) -> ItemStateInfo {
    match s {
        ItemState::Deleted { binned_at } => ItemStateInfo::Deleted { binned_at_ms: binned_at },
        ItemState::Expired { binned_at } => ItemStateInfo::Expired { binned_at_ms: binned_at },
        ItemState::Kept => ItemStateInfo::Kept,
        ItemState::FolderKeep => ItemStateInfo::FolderKeep,
        ItemState::Live { expires_at } => ItemStateInfo::Live { expires_at_ms: expires_at },
    }
}

fn item_info(v: ItemView) -> ItemInfo {
    ItemInfo {
        id: v.id.0.to_string(),
        name: v.name,
        folder_id: v.folder.0.to_string(),
        kind: v.kind,
        size: v.size,
        ext: v.ext,
        captured_at_ms: v.captured_at,
        pinned: v.pinned,
        state: state_info(v.state),
        local: v.local,
        path: v.path,
        thumb_path: v.thumb_path,
        never_reached_peer: v.never_reached_peer,
        missing_on: v.missing_on,
    }
}

fn activity_info(a: ActivityEntry) -> ActivityInfo {
    ActivityInfo { id: a.id, at_ms: a.at, item_id: a.item.map(|i| i.0.to_string()), action: a.action, detail: a.detail }
}

fn paired_pc(r: DeviceRow) -> PairedPc {
    PairedPc { id: r.id.to_string(), name: r.name, paired_at_ms: r.paired_at, last_seen_at_ms: r.last_seen_at, addresses: r.addresses }
}

/// Phone-side view of a core event. The PC-only "window open" event has no phone meaning.
fn ffi_event(e: Event) -> Option<FfiEvent> {
    use tugboat_core::events::PairingEvent as P;
    Some(match e {
        Event::Sync(status) => FfiEvent::Sync { status },
        Event::ItemsChanged { ids } => FfiEvent::ItemsChanged { ids: ids.into_iter().map(|i| i.0.to_string()).collect() },
        Event::FoldersChanged => FfiEvent::FoldersChanged,
        Event::PlacesChanged => FfiEvent::PlacesChanged,
        Event::ActivityAdded => FfiEvent::ActivityAdded,
        Event::SettingsChanged => FfiEvent::SettingsChanged,
        Event::Pairing(P::Code { code, peer_name }) => FfiEvent::PairingCode { code, peer_name },
        Event::Pairing(P::Completed { peer, peer_name }) => FfiEvent::PairingCompleted { device_id: peer.to_string(), peer_name },
        Event::Pairing(P::Cancelled { code, message }) => FfiEvent::PairingCancelled { code, message },
        Event::Pairing(P::Unpaired { peer, by_peer }) => FfiEvent::Unpaired { device_id: peer.to_string(), by_peer },
        Event::Pairing(P::WindowOpen { .. }) => return None,
        Event::Error { kind, message } => FfiEvent::Error { kind, message },
    })
}

/// A throwing listener (a Kotlin exception inside on_event) must not kill the pump.
fn deliver(listener: &Arc<dyn EventListener>, event: FfiEvent) {
    if catch_unwind(AssertUnwindSafe(|| listener.on_event(event))).is_err() {
        tracing::error!("the event listener threw; the event was dropped");
    }
}

// ---------- top-level functions ----------

/// A new random 32-byte secret key for `PhoneConfig.secret_key`. The app wraps it with an
/// Android Keystore AES-GCM key and stores only the wrapped bytes.
#[uniffi::export]
pub fn generate_secret_key() -> Result<Vec<u8>, SnapError> {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).map_err(|e| SnapError::Io(format!("NO RANDOM NUMBERS - {e}")))?;
    Ok(k.to_vec())
}

/// The extension core would give a file called `name`: lowercase, 1..=16 of a-z 0-9 - _, or ""
/// (no extension). Same rule as the PC; use it instead of re-implementing it in Kotlin.
#[uniffi::export]
pub fn names_ext_of(name: String) -> String {
    tugboat_core::names::ext_of(&name)
}

/// True for the names that never sync, in either direction: .lnk .url .scf .library-ms
/// .searchConnector-ms, desktop.ini, autorun.inf (case-insensitive, trailing dots and spaces and
/// NTFS stream suffixes ignored). Core refuses them too (`SnapError.Refused`); the app uses this to
/// say so before copying a shared file.
#[uniffi::export]
pub fn names_is_refused(name: String) -> bool {
    tugboat_core::names::is_refused_name(&name)
}

/// The one-line text for a refused name: "REFUSED - shortcut and folder-setting files never sync".
#[uniffi::export]
pub fn names_refused_text() -> String {
    tugboat_core::names::REFUSED_TEXT.to_string()
}

/// Check a scanned QR text or deep link before pairing: is it a Tugboat pairing code,
/// and for which PC. Fails with `PairBadPayload` for anything else (including a code that
/// names a non-local address). Does not touch the network.
#[uniffi::export]
pub fn inspect_pairing_payload(payload: String) -> Result<PairingPreview, SnapError> {
    use tugboat_net::pairing::PairPayload;
    match PairPayload::parse(&payload) {
        Ok(p) => Ok(PairingPreview { pc_name: p.pc_name, addresses: p.addrs.iter().map(|a| a.to_string()).collect(), expires_at_ms: p.expires_at_ms }),
        Err(e) => Err(SnapError::PairBadPayload(tugboat_core::sync::pair::pair_text(&e))),
    }
}

// ---------- the object ----------

#[derive(uniffi::Object)]
pub struct PhoneCore {
    // Option only so Drop can hand the runtime to shutdown_background.
    rt: Option<tokio::runtime::Runtime>,
    core: Core,
    storage: Arc<AppStorage>,
    secret: [u8; 32],
    bind: SocketAddr,
    pair_cancel: Arc<Notify>,
    log_path: PathBuf,
    self_id: DeviceId,
}

impl Drop for PhoneCore {
    fn drop(&mut self) {
        // Safe from any thread, including a runtime thread (a plain drop would panic there).
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

impl PhoneCore {
    fn rt(&self) -> &tokio::runtime::Runtime {
        // Only Drop takes it.
        self.rt.as_ref().expect("runtime is alive until drop")
    }
}

#[uniffi::export]
impl PhoneCore {
    /// Open the phone core in `config.data_dir`: logging, database (backup + migrate; a newer
    /// database is refused with `NewerDatabase`), default folders, one Bin sweep. Does not touch
    /// the network: call `start_network` next.
    #[uniffi::constructor]
    pub fn open(config: PhoneConfig, listener: Arc<dyn EventListener>) -> Result<Arc<PhoneCore>, SnapError> {
        let secret: [u8; 32] = config
            .secret_key
            .as_slice()
            .try_into()
            .map_err(|_| SnapError::InvalidInput(format!("the secret key must be 32 bytes, got {}", config.secret_key.len())))?;
        let bind: SocketAddr = match config.bind_addr.as_deref() {
            None => SocketAddr::from(([0, 0, 0, 0], 0)),
            Some(s) => s.parse().map_err(|e| SnapError::InvalidInput(format!("bad bind address {s:?}: {e}")))?,
        };
        let data_dir = PathBuf::from(&config.data_dir);
        std::fs::create_dir_all(&data_dir).map_err(|e| SnapError::Io(format!("cannot create {}: {e} [{:?}]", data_dir.display(), e.kind())))?;

        let report_to = listener.clone();
        let log_path = match logging::init(
            &data_dir,
            std::sync::Arc::new(move |text| deliver(&report_to, FfiEvent::Error { kind: "log".into(), message: text })),
        ) {
            Ok(p) => p,
            Err((p, e)) => {
                // Not fatal: the phone works without a log file, but the user hears about it.
                deliver(&listener, FfiEvent::Error { kind: "log".into(), message: format!("LOG FILE NOT WRITABLE - {}: {e}", p.display()) });
                p
            }
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tugboat-rt")
            .enable_all()
            .build()
            .map_err(|e| SnapError::Io(format!("cannot start the background runtime: {e}")))?;
        let self_id = device_id_from_secret(&secret);
        let app_version = config.app_version.clone();
        let storage = Arc::new(AppStorage::new(data_dir.join("storage")).map_err(SnapError::from)?);
        let cfg = CoreConfig { data_dir, role: Role::Phone, self_id, device_name: config.device_name, app_version: config.app_version };
        let (core, mut rx) = {
            let _guard = rt.enter();
            let placement: Arc<dyn Placement> = storage.clone();
            Core::open(cfg, Arc::new(SystemClock), placement).map_err(SnapError::from)?
        };

        let pump_listener = listener.clone();
        rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) => {
                        if let Some(ev) = ffi_event(e) {
                            deliver(&pump_listener, ev);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("event listener fell behind, {n} events dropped");
                        deliver(&pump_listener, FfiEvent::Resync);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });

        // Bin sweep and pairing-window expiry: at the core's next due time, at most 60 s apart.
        let tick_core = core.clone();
        let tick_listener = listener.clone();
        rt.spawn(async move {
            loop {
                let wait_ms = match tick_core.tick() {
                    Ok(Some(next)) => (next - SystemClock.now_ms()).clamp(1_000, 60_000),
                    Ok(None) => 60_000,
                    Err(e) => {
                        tracing::error!("tick failed: {e}");
                        deliver(&tick_listener, FfiEvent::Error { kind: e.kind().into(), message: e.to_string() });
                        60_000
                    }
                };
                tokio::time::sleep(Duration::from_millis(wait_ms as u64)).await;
            }
        });

        tracing::info!(app = %app_version, id = %self_id, "phone core open");
        Ok(Arc::new(PhoneCore { rt: Some(rt), core, storage, secret, bind, pair_cancel: Arc::new(Notify::new()), log_path, self_id }))
    }

    /// This phone's public key, 64 hex chars (its device id on the PC).
    pub fn device_id(&self) -> String {
        self.self_id.to_string()
    }

    /// Absolute path of the current log file. `<path>.1` and `<path>.2` are the two older
    /// rotated files when they exist. Share all that exist (oldest first) for "Export log".
    pub fn log_path(&self) -> String {
        self.log_path.to_string_lossy().into_owned()
    }

    /// Free bytes on the volume holding the phone copies (for the storage guard banner).
    pub fn free_space_bytes(&self) -> Result<u64, SnapError> {
        self.storage.available_space().map_err(SnapError::from)
    }

    // ----- network -----

    /// Start the endpoint and the connect loop. `mdns` = look for the PC by name on the local
    /// network when its cached addresses fail. Fails with `Invalid` when already started.
    /// Blocks until the endpoint is bound.
    pub fn start_network(&self, mdns: bool) -> Result<(), SnapError> {
        let cfg = NetworkConfig { bind: self.bind, mdns, secret_key: self.secret };
        self.rt().block_on(self.core.start_network(cfg)).map_err(SnapError::from)
    }

    pub fn stop_network(&self) {
        self.rt().block_on(self.core.stop_network());
    }

    /// The Wi-Fi changed, or the app came to the front: dial the PC now instead of waiting out
    /// the reconnect back-off (up to 30 s).
    pub fn reconnect_now(&self) {
        self.core.reconnect_now();
    }

    pub fn status(&self) -> SyncStatus {
        self.core.status()
    }

    // ----- pairing -----

    /// Pair with the PC named by a scanned QR text (or a deep link). Blocks until the PC owner
    /// clicks Pair (up to about 2 minutes). Meanwhile the listener gets `PairingCode`: show it
    /// large. Errors are typed: `PairBadPayload`, `PairExpired`, `PairSecretReused`,
    /// `PairCancelled`, `PairConfirmTimeout`, `NetTimeout` (wrong network or PC asleep),
    /// `NetUnreachable`, `NetNotLocal`, `PairVersionMismatch`. On success the phone connects
    /// to the PC by itself.
    pub fn pair_with(&self, payload: String) -> Result<PairedPc, SnapError> {
        let core = self.core.clone();
        let cancel = self.pair_cancel.clone();
        let result = self.rt().block_on(async move {
            let mut cancelled = std::pin::pin!(cancel.notified());
            // Register before the exchange starts, so a cancel made at any moment is seen.
            cancelled.as_mut().enable();
            // Core ends the exchange on a cancel only while that is still safe: once the PC's
            // decision is read the pairing is stored whatever the user taps. It emits the
            // Cancelled event itself.
            core.pair_with_cancel(&payload, &mut cancelled).await
        });
        result.map(paired_pc).map_err(Into::into)
    }

    /// Abandon a `pair_with` that is waiting. It returns `PairCancelled`. No effect otherwise.
    pub fn cancel_pairing(&self) {
        self.pair_cancel.notify_waiters();
    }

    /// Unlink one PC by its id (`DeviceStatus.id`). Files stay on both devices. A PC that cannot
    /// be told now is told when it is reachable (its `devices` row shows `unlinking` meanwhile). A bad id is `InvalidInput`, an unknown one `NotPaired`.
    pub fn unlink(&self, device_id: String) -> Result<(), SnapError> {
        let id = device_id_of(&device_id)?;
        self.rt().block_on(self.core.unpair_device(id)).map_err(SnapError::from)
    }

    // ----- folders, items -----

    pub fn folders(&self) -> Result<Vec<FolderInfo>, SnapError> {
        Ok(self.core.folders().map_err(SnapError::from)?.into_iter().map(folder_info).collect())
    }

    /// Items sorted newest first. `folder_id` None = all folders. `bin` true = only items in the
    /// Bin, false = only items not in the Bin. `limit` 0 = 50, at most 500.
    pub fn items(&self, folder_id_filter: Option<String>, bin: bool, after: Option<String>, limit: u32) -> Result<ItemPage, SnapError> {
        let q = ItemQuery {
            folder: folder_id_filter.as_deref().map(folder_id).transpose()?,
            bin,
            after: after.as_deref().map(item_id).transpose()?,
            limit,
        };
        let page = self.core.items(&q).map_err(SnapError::from)?;
        Ok(ItemPage { rows: page.rows.into_iter().map(item_info).collect(), next: page.next })
    }

    pub fn item(&self, id: String) -> Result<Option<ItemInfo>, SnapError> {
        Ok(self.core.item(item_id(&id)?).map_err(SnapError::from)?.map(item_info))
    }

    /// Take a captured or imported file into Tugboat. Returns the new item id. The record
    /// is written and the sync is triggered before this returns; the file reaches the PC in the
    /// background. Typed errors: `NotMedia` (a photo/video extension whose bytes are not one), `Refused`
    /// (an always-refused name, or over the size limit), `Io` (source missing or unreadable, disk
    /// full), `Invalid` (target folder deleted), `Paused`.
    pub fn add_file(&self, file: NewFile) -> Result<String, SnapError> {
        let f = NewLocalFile {
            src: PathBuf::from(&file.src_path),
            name: file.name,
            folder: file.folder_id.as_deref().map(folder_id).transpose()?,
            captured_at: file.captured_at_ms,
            geo: file.geo.map(|g| Geo { lat: g.lat, lon: g.lon, accuracy_m: g.accuracy_m }),
            thumb: file.thumb_path.map(PathBuf::from),
            how: if file.imported { AddKind::Added } else { AddKind::Captured },
            ntfs: None,
        };
        Ok(self.core.add_local_file(f).map_err(SnapError::from)?.0.to_string())
    }

    /// Keep (true) or un-keep (false). A kept item never expires.
    pub fn set_pinned(&self, id: String, pinned: bool) -> Result<(), SnapError> {
        self.core.set_pinned(item_id(&id)?, pinned).map_err(SnapError::from)
    }

    /// Move to another folder. The retention clock restarts there.
    pub fn move_item(&self, id: String, folder_id_target: String) -> Result<(), SnapError> {
        self.core.move_item(item_id(&id)?, folder_id(&folder_id_target)?).map_err(SnapError::from)
    }

    /// Delete = to the Bin (restorable for `Settings.bin_days`).
    pub fn delete_item(&self, id: String) -> Result<(), SnapError> {
        self.core.delete_item(item_id(&id)?).map_err(SnapError::from)
    }

    /// Restore from the Bin with a fresh full retention. A file already purged here is fetched
    /// from the PC (`local` = Pending until it arrives).
    pub fn restore_item(&self, id: String) -> Result<(), SnapError> {
        self.core.restore_item(item_id(&id)?).map_err(SnapError::from)
    }

    /// Camera routing: which folder does the next shot go to. Pass the newest location fix or
    /// None. See `CaptureRoute`.
    pub fn route_capture(&self, fix: Option<LocationFix>) -> Result<CaptureRoute, SnapError> {
        let fix = fix.map(|f| Fix { lat: f.lat, lon: f.lon, accuracy_m: f.accuracy_m, age_ms: f.age_ms });
        Ok(match self.core.route_capture(fix).map_err(SnapError::from)? {
            Route::Place { place, folder } => CaptureRoute::Place { place_id: place.0.to_string(), folder_id: folder.0.to_string() },
            Route::Unsorted => CaptureRoute::Unsorted,
            Route::Default => CaptureRoute::Default,
        })
    }

    // ----- activity, settings -----

    /// Activity, newest first. `before` = the last `id` of the previous page.
    pub fn activity(&self, before: Option<i64>, limit: u32) -> Result<Vec<ActivityInfo>, SnapError> {
        Ok(self.core.activity(before, limit).map_err(SnapError::from)?.into_iter().map(activity_info).collect())
    }

    /// Add "Upgraded" to the activity history (the app carried its data over from the old file names).
    pub fn log_upgrade(&self, detail: String) -> Result<(), SnapError> {
        self.core.log_upgrade(&detail).map_err(SnapError::from)
    }

    pub fn settings(&self) -> Result<Settings, SnapError> {
        self.core.settings().map_err(SnapError::from)
    }

    /// Save settings. Core validates the values and refuses nonsense with `Invalid`.
    pub fn save_settings(&self, settings: Settings) -> Result<(), SnapError> {
        self.core.save_settings(&settings).map_err(SnapError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Sink(Mutex<Vec<FfiEvent>>);
    impl EventListener for Sink {
        fn on_event(&self, e: FfiEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    fn open_phone(dir: &std::path::Path) -> (Arc<PhoneCore>, Arc<Sink>) {
        let sink = Arc::new(Sink(Mutex::new(Vec::new())));
        let phone = PhoneCore::open(
            PhoneConfig {
                data_dir: dir.to_string_lossy().into_owned(),
                device_name: "Test phone".into(),
                app_version: "test".into(),
                secret_key: generate_secret_key().unwrap(),
                bind_addr: Some("127.0.0.1:0".into()),
            },
            sink.clone(),
        )
        .unwrap();
        (phone, sink)
    }

    /// Camera rules reach Kotlin as the right chip state: no places -> Default; places + stale
    /// or missing fix -> Unsorted; inside the smallest matching place -> that place's folder.
    #[test]
    fn route_capture_maps_core_routes() {
        let dir = tempfile::tempdir().unwrap();
        let (phone, _sink) = open_phone(dir.path());
        let fix = |lat: f64, age_ms: i64| Some(LocationFix { lat, lon: 151.0, accuracy_m: 10.0, age_ms });
        assert_eq!(phone.route_capture(None).unwrap(), CaptureRoute::Default, "no places yet");
        assert_eq!(phone.route_capture(fix(-33.0, 1000)).unwrap(), CaptureRoute::Default);

        let work = phone.core.create_folder("Work", Retention::Keep).unwrap();
        let big = phone.core.put_place(None, "Site", tugboat_core::Area { lat: -33.0, lon: 151.0, radius_m: 500.0 }, work).unwrap();
        let small = phone.core.put_place(None, "Office", tugboat_core::Area { lat: -33.0, lon: 151.0, radius_m: 50.0 }, tugboat_core::INBOX_ID).unwrap();
        assert_ne!(big, small);

        assert_eq!(phone.route_capture(None).unwrap(), CaptureRoute::Unsorted, "places exist, no fix");
        assert_eq!(phone.route_capture(fix(-33.0, 121_000)).unwrap(), CaptureRoute::Unsorted, "a 2 minute old fix is not used");
        assert_eq!(
            phone.route_capture(Some(LocationFix { lat: -33.0, lon: 151.0, accuracy_m: 101.0, age_ms: 0 })).unwrap(),
            CaptureRoute::Unsorted,
            "accuracy over 100 m is not used"
        );
        match phone.route_capture(fix(-33.0, 1000)).unwrap() {
            CaptureRoute::Place { place_id, folder_id } => {
                assert_eq!(place_id, small.0.to_string(), "the smallest radius wins");
                assert_eq!(folder_id, tugboat_core::INBOX_ID.0.to_string());
            }
            other => panic!("expected the Office place, got {other:?}"),
        }
        // About 3 km north of the centre: inside no place, so the chip folder applies.
        assert_eq!(phone.route_capture(fix(-32.97, 1000)).unwrap(), CaptureRoute::Default);
    }

    #[test]
    fn bad_arguments_are_typed_input_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (phone, _sink) = open_phone(dir.path());
        assert!(matches!(phone.item("not-a-uuid".into()), Err(SnapError::InvalidInput(_))));
        assert!(matches!(phone.items(Some("x".into()), false, None, 10), Err(SnapError::InvalidInput(_))));
        assert!(matches!(phone.items(None, false, Some("x".into()), 10), Err(SnapError::InvalidInput(_))));
        assert!(matches!(phone.set_pinned("".into(), true), Err(SnapError::InvalidInput(_))));
        assert!(matches!(phone.move_item(uuid::Uuid::nil().to_string(), "x".into()), Err(SnapError::InvalidInput(_))));
        // Well-formed id that does not exist: NotFound, not a crash.
        assert!(matches!(phone.delete_item(uuid::Uuid::nil().to_string()), Err(SnapError::NotFound(_))));
        let cfg = |key: Vec<u8>, bind: Option<&str>| PhoneConfig {
            data_dir: dir.path().join("other").to_string_lossy().into_owned(),
            device_name: "x".into(),
            app_version: "x".into(),
            secret_key: key,
            bind_addr: bind.map(String::from),
        };
        let sink: Arc<dyn EventListener> = Arc::new(Sink(Mutex::new(Vec::new())));
        assert!(matches!(PhoneCore::open(cfg(vec![1; 31], None), sink.clone()), Err(SnapError::InvalidInput(_))), "short key");
        assert!(matches!(PhoneCore::open(cfg(vec![1; 32], Some("nonsense")), sink), Err(SnapError::InvalidInput(_))), "bad bind");
    }

    #[test]
    fn status_maps_devices() {
        let dir = tempfile::tempdir().unwrap();
        let (phone, _sink) = open_phone(dir.path());
        assert!(phone.status().devices.is_empty(), "nothing linked yet");
        let id = DeviceId([0xab; 32]);
        phone.core.debug_conn(|c| {
            let row = DeviceRow {
                id,
                name: "WORK-PC".into(),
                is_self: false,
                role: Role::Pc,
                paired_at: None,
                unpaired_at: None,
                last_seen_at: None,
                last_synced_at: None,
                addresses: Vec::new(),
            };
            tugboat_core::store::set_paired(c, &row, 1_000).unwrap();
        });
        let st = phone.status();
        assert_eq!(st.devices.len(), 1);
        let d = &st.devices[0];
        assert_eq!(d.id, "ab".repeat(32), "64 hex chars");
        assert_eq!(d.name, "WORK-PC");
        assert_eq!(d.state, SyncState::Offline, "linked, no connection yet");
        assert_eq!((d.waiting, d.downloading, d.unlinking), (0, 0, false));
        assert_eq!(st.peer_name.as_deref(), Some("WORK-PC"));
        match ffi_event(Event::Sync(st.clone())) {
            Some(FfiEvent::Sync { status }) => assert_eq!(status, st),
            other => panic!("expected a Sync event, got {other:?}"),
        }
    }

    #[test]
    fn unpaired_event_carries_id_and_by_peer() {
        use tugboat_core::events::PairingEvent as P;
        let id = DeviceId([7; 32]);
        match ffi_event(Event::Pairing(P::Unpaired { peer: id, by_peer: true })) {
            Some(FfiEvent::Unpaired { device_id, by_peer }) => assert_eq!((device_id, by_peer), (id.to_string(), true)),
            other => panic!("got {other:?}"),
        }
        match ffi_event(Event::Pairing(P::Completed { peer: id, peer_name: "PC".into() })) {
            Some(FfiEvent::PairingCompleted { device_id, peer_name }) => assert_eq!((device_id, peer_name), (id.to_string(), "PC".into())),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unlink_rejects_bad_hex_and_unknown_ids() {
        let dir = tempfile::tempdir().unwrap();
        let (phone, _sink) = open_phone(dir.path());
        for bad in ["", "not hex", "abcd", &"zz".repeat(32), &"ab".repeat(33)] {
            assert!(matches!(phone.unlink(bad.to_string()), Err(SnapError::InvalidInput(_))), "{bad:?}");
        }
        // A well-formed id that is not linked.
        assert!(matches!(phone.unlink("ab".repeat(32)), Err(SnapError::NotPaired(_))));
    }

    #[test]
    fn inspect_pairing_payload_accepts_only_tugboat_codes() {
        for bad in ["", "hello", "https://example.com/?k=1", "localtugboat://pair?v=1"] {
            assert!(matches!(inspect_pairing_payload(bad.into()), Err(SnapError::PairBadPayload(_))), "{bad:?}");
        }
        // A well-formed code naming a public address is refused: the phone never dials it.
        let public = format!("localtugboat://pair?v=1&k={}&a=8.8.8.8:47300&s={}&e=1&n=PC", "ab".repeat(32), "cd".repeat(16));
        assert!(matches!(inspect_pairing_payload(public), Err(SnapError::PairBadPayload(_))));
        let ok = format!("localtugboat://pair?v=1&k={}&a=192.168.1.5:47300,10.0.0.2:47300&s={}&e=1790000000000&n=OFFICE%2DPC", "ab".repeat(32), "cd".repeat(16));
        let p = inspect_pairing_payload(ok).unwrap();
        assert_eq!(p.pc_name, "OFFICE-PC");
        assert_eq!(p.addresses, vec!["192.168.1.5:47300", "10.0.0.2:47300"]);
        assert_eq!(p.expires_at_ms, 1_790_000_000_000);
    }

    #[test]
    fn open_writes_a_log_file_and_reports_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let (phone, _sink) = open_phone(dir.path());
        let p = PathBuf::from(phone.log_path());
        assert!(p.starts_with(dir.path().join("logs")) && p.exists());
        assert!(phone.free_space_bytes().unwrap() > 0);
        assert_eq!(phone.device_id().len(), 64);
        assert_eq!(phone.settings().unwrap(), Settings::default());
    }
}
