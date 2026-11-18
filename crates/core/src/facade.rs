//! The Core facade: the one API both apps call (desktop via Tauri commands, phone via the
//! UniFFI crate in M3). Owner: sync builder. Thin: validates input, calls ops/bin/store,
//! emits events, pokes the sync session. No rule lives here.
//!
//! Threading: `Core` is cheap to clone (Arc). The SQLite connection sits behind a Mutex;
//! every method holds it only for its own call and never across an await. (ops::add_local_file
//! copies the file while it holds the lock: a very large video stalls sync for the length of
//! the copy; see the sync builder's open issues.)
//! Errors are returned AND (for background work: sweep, sync, fetch) sent as Event::Error.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use rusqlite::Connection;
use tokio::sync::{broadcast, Notify};

use crate::{
    activity, bin,
    clock::Clock,
    db::Db,
    events::{Event, SyncState, SyncStatus},
    expiry, folders,
    model::*,
    names,
    ops::{self, Ctx, NewLocalFile},
    placement::Placement,
    places::{self, Fix, Route},
    store,
    sync::{net, pair, wire},
    CoreError, Result,
};

#[derive(Debug, Clone)]
pub struct CoreConfig {
    /// %LOCALAPPDATA%\Local Tugboat (PC) or filesDir (phone): db, thumbs/, logs.
    pub data_dir: PathBuf,
    pub role: Role,
    /// This device's public key (the app loads the secret from Credential Manager /
    /// Keystore and passes the derived public key here; the secret goes to `start_network`).
    pub self_id: DeviceId,
    /// "NIGEL-PC" / phone model. Shown to the other device.
    pub device_name: String,
    pub app_version: String,
}

/// Network settings for `start_network`.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// PC: 0.0.0.0:<udp_port> (or 127.0.0.1 when env TUGBOAT_BIND=127.0.0.1, read by the
    /// desktop app); phone: 0.0.0.0:0. Tests: 127.0.0.1.
    pub bind: SocketAddr,
    pub mdns: bool,
    pub secret_key: [u8; 32],
}

/// The public key (device id) of an iroh secret key, so apps need no iroh dependency.
pub fn device_id_from_secret(secret: &[u8; 32]) -> DeviceId {
    DeviceId(*tugboat_net::iroh::SecretKey::from(*secret).public().as_bytes())
}

/// Mutable sync status behind `Core::status`.
#[derive(Debug, Default)]
pub(crate) struct StatusInner {
    pub state: Option<SyncState>,
    pub message: Option<String>,
    pub last_synced_at: Option<i64>,
    pub last_emitted: Option<SyncStatus>,
}

pub(crate) struct Inner {
    pub cfg: CoreConfig,
    pub db: Mutex<Db>,
    pub clock: Arc<dyn Clock>,
    pub placement: Arc<dyn Placement>,
    pub events: broadcast::Sender<Event>,
    /// A local change happened: the live session sends Notify.
    pub local_changed: Notify,
    /// Something may need downloading (restore, sweep, new record).
    pub kick: Notify,
    /// The connect loop should look again (pairing done, resumed, started).
    pub wake: Notify,
    pub status: Mutex<StatusInner>,
    pub paused: Mutex<Option<String>>,
    pub net: Mutex<Option<net::NetState>>,
    pub active: Mutex<Option<net::Active>>,
    pub pairing: Mutex<Option<pair::PairState>>,
    pub proto: AtomicU32,
    /// Pairing protocol number the phone speaks (tests play an older phone).
    pub pair_proto: AtomicU32,
    pub retry_base_ms: AtomicU64,
    /// Test hook: extra delay before the PC saves a confirmed pairing (0 in the apps).
    pub pair_save_delay_ms: AtomicU64,
    pub generation: AtomicU64,
    /// Bytes of downloads that continued from an existing part file (tests assert on it).
    pub resumed_bytes: AtomicU64,
    /// Phone: a task is delivering an unpair notice to a replaced PC while the connect loop
    /// serves the new one.
    pub notice_running: std::sync::atomic::AtomicBool,
    /// Phone: the storage guard is holding at least one download back (SyncStatus.storage_low).
    pub storage_low: std::sync::atomic::AtomicBool,
    /// How long a blocked download queue waits before it looks at the free space again (ms).
    pub guard_recheck_ms: AtomicU64,
    /// Hash (hex) of every blob or thumbnail request this device answered or refused (tests
    /// assert "zero", or that one hash was never asked for).
    pub blob_requests: Mutex<Vec<String>>,
    /// Content hashes with a download task alive (any session). Two tasks must never write the
    /// same part file, and a task outlives the session that started it.
    pub fetching: Mutex<std::collections::HashSet<BlobHash>>,
}

#[derive(Clone)]
pub struct Core {
    pub(crate) inner: Arc<Inner>,
}

/// Lock helper: a poisoned lock means another thread panicked mid-write; the data in these
/// structs is plain state, so continue with it instead of cascading the panic.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Core {
    /// Open the db (db::Db::open: backup + migrate, refuses newer), ensure self row and
    /// default folders, run one bin::sweep. Returns the core and an event receiver (more
    /// via `subscribe`).
    pub fn open(cfg: CoreConfig, clock: Arc<dyn Clock>, placement: Arc<dyn Placement>) -> Result<(Core, broadcast::Receiver<Event>)> {
        let db = Db::open(&cfg.data_dir)?;
        store::ensure_self(&db.conn, cfg.self_id, &cfg.device_name, cfg.role)?;
        let thumbs = cfg.data_dir.join("thumbs");
        std::fs::create_dir_all(&thumbs).map_err(|e| CoreError::io(&thumbs, e))?;
        let last_synced_at = store::get_value(&db.conn, "last_synced_at")?.and_then(|v| v.parse::<i64>().ok());
        let (tx, rx) = broadcast::channel(1024);
        let core = Core {
            inner: Arc::new(Inner {
                cfg,
                db: Mutex::new(db),
                clock,
                placement,
                events: tx,
                local_changed: Notify::new(),
                kick: Notify::new(),
                wake: Notify::new(),
                status: Mutex::new(StatusInner { last_synced_at, ..Default::default() }),
                paused: Mutex::new(None),
                net: Mutex::new(None),
                active: Mutex::new(None),
                pairing: Mutex::new(None),
                proto: AtomicU32::new(wire::PROTO),
                pair_proto: AtomicU32::new(tugboat_net::pairing::PAIR_PROTO),
                retry_base_ms: AtomicU64::new(2000),
                pair_save_delay_ms: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                resumed_bytes: AtomicU64::new(0),
                notice_running: std::sync::atomic::AtomicBool::new(false),
                storage_low: std::sync::atomic::AtomicBool::new(false),
                guard_recheck_ms: AtomicU64::new(30_000),
                blob_requests: Mutex::new(Vec::new()),
                fetching: Mutex::new(std::collections::HashSet::new()),
            }),
        };
        core.sweep_and_emit()?;
        Ok((core, rx))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    pub fn config(&self) -> &CoreConfig {
        &self.inner.cfg
    }

    // ----- internals shared with sync/* -----

    pub(crate) fn now(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    pub(crate) fn emit(&self, e: Event) {
        // No receiver means nobody is listening, which is not an error.
        let _ = self.inner.events.send(e);
    }

    pub(crate) fn emit_error(&self, kind: &str, message: &str) {
        tracing::warn!(kind, "{message}");
        self.emit(Event::Error { kind: kind.to_string(), message: message.to_string() });
    }

    pub(crate) fn thumbs_dir(&self) -> PathBuf {
        self.inner.cfg.data_dir.join("thumbs")
    }

    pub(crate) fn thumb_path(&self, hash: BlobHash) -> PathBuf {
        self.thumbs_dir().join(format!("{}.jpg", hex::encode(hash.0)))
    }

    /// Run `f` with the connection and an op context (now + placement).
    pub(crate) fn with_ctx<T>(&self, f: impl FnOnce(&mut Connection, &Ctx<'_>) -> Result<T>) -> Result<T> {
        let ctx = Ctx { self_id: self.inner.cfg.self_id, now: self.now(), placement: &*self.inner.placement };
        let mut db = lock(&self.inner.db);
        f(&mut db.conn, &ctx)
    }

    /// Test hook: run the live path guard's rule for `addr` on the active connection, as the
    /// guard does when a path opens. A real non-local path cannot be produced on loopback.
    /// Returns true when the connection was closed.
    #[doc(hidden)]
    pub fn debug_enforce_path(&self, addr: &str) -> bool {
        let conn = lock(&self.inner.active).as_ref().map(|a| a.conn.clone());
        match (conn, addr.parse()) {
            (Some(conn), Ok(ip)) => conn.enforce_path(&tugboat_net::iroh::TransportAddr::Ip(ip)),
            _ => false,
        }
    }

    /// Test hook: direct database access (digest checks, backups, dropping a log row).
    #[doc(hidden)]
    pub fn debug_conn<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> T {
        let mut db = lock(&self.inner.db);
        f(&mut db.conn)
    }

    /// Test hook: speak another protocol number (version refusal test).
    #[doc(hidden)]
    pub fn debug_set_proto(&self, proto: u32) {
        self.inner.proto.store(proto, Ordering::SeqCst);
    }

    /// Test hook: speak another PAIRING protocol number when this device is the phone.
    #[doc(hidden)]
    pub fn debug_set_pair_proto(&self, proto: u32) {
        self.inner.pair_proto.store(proto, Ordering::SeqCst);
    }

    /// Test hook: blob retry back-off base (default 2000 ms).
    #[doc(hidden)]
    pub fn debug_set_retry_base_ms(&self, ms: u64) {
        self.inner.retry_base_ms.store(ms.max(1), Ordering::SeqCst);
    }

    /// Test and QA hook: write an item RECORD straight into the database (and the change log, so
    /// it syncs) without any file behind it, with the name, extension, kind, thumbnail and local
    /// state given: the way a hostile or newer peer's record would arrive. `fake_phone
    /// send-refused` uses it to put a refused name on the PC's Not synced list.
    #[doc(hidden)]
    pub fn debug_add_record(&self, name: &str, ext: &str, kind: MediaKind, thumb: Option<Thumb>, local: LocalFileState) -> Result<ItemId> {
        let id = self.with_ctx(|c, ctx| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let v = store::next_version(&tx, ctx.self_id)?;
            let id = ItemId::new(ctx.now);
            let rec = ItemRecord {
                id,
                captured_at: ctx.now,
                origin: ctx.self_id,
                geo: None,
                name: Versioned { v, val: name.to_string() },
                content: Versioned { v, val: Content { hash: BlobHash(*blake3::hash(name.as_bytes()).as_bytes()), size: name.len() as u64, kind, ext: ext.to_string(), thumb } },
                location: Versioned { v, val: Location { folder: INBOX_ID, entered_at: ctx.now } },
                pinned: Versioned { v, val: false },
                deleted: Versioned { v, val: None },
            };
            store::insert_item(&tx, &rec, local, ctx.now)?;
            store::log_change(&tx, &EntityKey::Item { id }, group::ALL, v.lamport)?;
            tx.commit()?;
            Ok(id)
        })?;
        self.wrote(&[id], false, false);
        Ok(id)
    }

    /// Test hook: how long a download queue blocked by the storage guard waits before it checks
    /// the free space again (default 30 000 ms).
    #[doc(hidden)]
    pub fn debug_set_guard_recheck_ms(&self, ms: u64) {
        self.inner.guard_recheck_ms.store(ms.max(1), Ordering::SeqCst);
    }

    /// Test hook: how many blob or thumbnail requests the peer has made of this device (a refused
    /// record must cost zero).
    #[doc(hidden)]
    pub fn debug_blob_requests(&self) -> usize {
        lock(&self.inner.blob_requests).len()
    }

    /// Test hook: has the peer asked this device for the blob or thumbnail with this hash?
    #[doc(hidden)]
    pub fn debug_blob_requested(&self, hash: BlobHash) -> bool {
        lock(&self.inner.blob_requests).contains(&hex::encode(hash.0))
    }

    /// Test hook: make the PC slow to save a confirmed pairing (a busy disk or CPU).
    #[doc(hidden)]
    pub fn debug_set_pairing_save_delay_ms(&self, ms: u64) {
        self.inner.pair_save_delay_ms.store(ms, Ordering::SeqCst);
    }

    /// Test hook: total bytes that downloads resumed from an existing part file.
    #[doc(hidden)]
    pub fn debug_resumed_bytes(&self) -> u64 {
        self.inner.resumed_bytes.load(Ordering::SeqCst)
    }

    /// Test hook: drop the live connection without telling the peer why (network loss).
    #[doc(hidden)]
    pub fn debug_kill_connection(&self) {
        if let Some(a) = lock(&self.inner.active).as_ref() {
            a.conn.close(tugboat_net::close::NORMAL, "connection dropped (test)");
        }
    }

    // ----- status -----

    /// Current sync status, computed from the db and the connection state.
    pub fn status(&self) -> SyncStatus {
        let now = self.now();
        let (peer, waiting, downloading, owes_unpair) = {
            let db = lock(&self.inner.db);
            let peer = store::paired_peer(&db.conn).ok().flatten();
            let owes_unpair = store::get_pending_unpair(&db.conn).ok().flatten().is_some();
            let waiting = waiting_count(&db.conn, now).unwrap_or(0);
            let downloading = store::items_in_state(&db.conn, &[LocalFileState::Pending], 100_000).map(|v| v.len() as u32).unwrap_or(0);
            (peer, waiting, downloading, owes_unpair)
        };
        let st = lock(&self.inner.status);
        let paused = lock(&self.inner.paused).clone();
        let (state, message) = if peer.is_none() {
            // An unpair the other device has not heard of is shown, never silent.
            (SyncState::Unpaired, owes_unpair.then(|| "PC NOT TOLD YET - will tell it when it is reachable".to_string()))
        } else if let Some(reason) = paused {
            (SyncState::Paused, Some(reason))
        } else {
            (st.state.unwrap_or(SyncState::Offline), st.message.clone())
        };
        let storage_low = self.inner.storage_low.load(Ordering::SeqCst);
        SyncStatus { state, peer_name: peer.map(|p| p.name), last_synced_at: st.last_synced_at, waiting, downloading, storage_low, message }
    }

    /// Set the connection-derived state, then emit Event::Sync when the status changed.
    pub(crate) fn set_state(&self, state: SyncState, message: Option<String>) {
        {
            let mut st = lock(&self.inner.status);
            st.state = Some(state);
            st.message = message;
        }
        self.emit_status();
    }

    pub(crate) fn mark_synced(&self, now: i64) {
        lock(&self.inner.status).last_synced_at = Some(now);
        let r = self.with_ctx(|c, _| store::set_value(c, "last_synced_at", &now.to_string()));
        if let Err(e) = r {
            self.emit_error(e.kind(), &format!("could not save the last synced time: {e}"));
        }
    }

    pub(crate) fn emit_status(&self) {
        let s = self.status();
        let mut st = lock(&self.inner.status);
        if st.last_emitted.as_ref() != Some(&s) {
            st.last_emitted = Some(s.clone());
            drop(st);
            self.emit(Event::Sync(s));
        }
    }

    // ----- queries -----

    pub fn folders(&self) -> Result<Vec<FolderView>> {
        let now = self.now();
        let db = lock(&self.inner.db);
        folders::folder_views(&db.conn, now, self.inner.cfg.role == Role::Pc)
    }

    pub fn items(&self, q: &ItemQuery) -> Result<Page<ItemView>> {
        let now = self.now();
        let limit = if q.limit == 0 { 50 } else { q.limit.clamp(1, 500) } as usize;
        let db = lock(&self.inner.db);
        let c = &db.conn;
        let fmap: HashMap<FolderId, FolderRecord> = store::all_folders(c)?.into_iter().map(|f| (f.id, f)).collect();
        let shown_folder = |rec: &ItemRecord| match fmap.get(&rec.location.val.folder) {
            Some(f) if f.deleted.val.is_none() => f.id,
            _ => UNSORTED_ID,
        };
        let wanted = |rec: &ItemRecord| q.folder.is_none_or(|f| shown_folder(rec) == f);
        let mut rows: Vec<ItemView> = Vec::new();
        if q.bin {
            let mut after = q.after;
            'bin: loop {
                let page = bin::bin_page(c, after, 500, now)?;
                let got = page.len();
                for (rec, state, lfs) in page {
                    after = Some(rec.id);
                    if wanted(&rec) {
                        rows.push(self.item_view(c, &fmap, &rec, lfs, state));
                        if rows.len() == limit {
                            break 'bin;
                        }
                    }
                }
                if got < 500 {
                    break;
                }
            }
        } else {
            let mut after = q.after;
            // The SQL folder filter is on the raw folder id; Unsorted also shows items of deleted
            // or unknown folders, so it is filtered here instead.
            let sql_folder = q.folder.filter(|f| *f != UNSORTED_ID);
            'scan: loop {
                let page = store::items_page(c, sql_folder, after, 500)?;
                let got = page.len();
                for (rec, lfs) in page {
                    after = Some(rec.id);
                    // The PC lists a file it refused under Not synced, not as a tile; the phone keeps its tile.
                    if !wanted(&rec) || (lfs == LocalFileState::Refused && self.inner.cfg.role == Role::Pc) {
                        continue;
                    }
                    let state = bin::derive_state_in(c, &fmap, &rec, now)?;
                    if expiry::in_bin(state) {
                        continue;
                    }
                    rows.push(self.item_view(c, &fmap, &rec, lfs, state));
                    if rows.len() == limit {
                        break 'scan;
                    }
                }
                if got < 500 {
                    break;
                }
            }
        }
        let next = if rows.len() == limit { rows.last().map(|r| r.id.0.to_string()) } else { None };
        Ok(Page { rows, next })
    }

    pub fn item(&self, id: ItemId) -> Result<Option<ItemView>> {
        let now = self.now();
        let db = lock(&self.inner.db);
        let c = &db.conn;
        let Some((rec, lfs)) = store::get_item(c, id)? else { return Ok(None) };
        let fmap: HashMap<FolderId, FolderRecord> = store::all_folders(c)?.into_iter().map(|f| (f.id, f)).collect();
        let state = bin::derive_state_in(c, &fmap, &rec, now)?;
        Ok(Some(self.item_view(c, &fmap, &rec, lfs, state)))
    }

    fn item_view(&self, c: &Connection, fmap: &HashMap<FolderId, FolderRecord>, rec: &ItemRecord, lfs: LocalFileState, state: ItemState) -> ItemView {
        let folder = match fmap.get(&rec.location.val.folder) {
            Some(f) if f.deleted.val.is_none() => f.id,
            _ => UNSORTED_ID,
        };
        let path = match lfs {
            LocalFileState::Present | LocalFileState::Binned => store::get_file_ref(c, rec.id).ok().flatten().map(|r| r.path),
            _ => None,
        };
        // A File never shows a thumbnail, whatever its record carries.
        let thumb_path = rec
            .content
            .val
            .thumb
            .as_ref()
            .filter(|_| rec.content.val.kind != MediaKind::File)
            .map(|t| self.thumb_path(t.hash))
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned());
        // Expired before the paired device ever confirmed the file: the Bin says so (silent-failure rule).
        let never_reached_peer = matches!(state, ItemState::Expired { .. }) && bin::missing_at_peer(c, rec.content.val.hash).ok().flatten().is_some();
        ItemView {
            id: rec.id,
            name: crate::names::display_name(&rec.name.val, &rec.content.val.ext),
            folder,
            kind: rec.content.val.kind,
            size: rec.content.val.size,
            ext: rec.content.val.ext.clone(),
            captured_at: rec.captured_at,
            origin: rec.origin,
            pinned: rec.pinned.val,
            state,
            local: lfs,
            path,
            thumb_path,
            never_reached_peer,
        }
    }

    /// Items this device will not hold because the file is refused (an always-refused name from
    /// the other device, a wrong type, too large): local state Refused, not in the Bin, newest
    /// first. The PC lists them in its Not synced list, the phone as REFUSED tiles.
    pub fn refused_items(&self, limit: u32) -> Result<Vec<ItemView>> {
        let now = self.now();
        let db = lock(&self.inner.db);
        let c = &db.conn;
        let fmap: HashMap<FolderId, FolderRecord> = store::all_folders(c)?.into_iter().map(|f| (f.id, f)).collect();
        let mut rows: Vec<ItemView> = Vec::new();
        let mut recs: Vec<(ItemRecord, LocalFileState)> = Vec::new();
        for id in store::item_ids_where(c, "local_file_state = 'refused'")? {
            if let Some(x) = store::get_item(c, id)? {
                recs.push(x);
            }
        }
        recs.sort_by(|a, b| b.0.captured_at.cmp(&a.0.captured_at).then_with(|| b.0.id.cmp(&a.0.id)));
        for (rec, lfs) in recs {
            let state = bin::derive_state_in(c, &fmap, &rec, now)?;
            if expiry::in_bin(state) {
                continue;
            }
            rows.push(self.item_view(c, &fmap, &rec, lfs, state));
            if rows.len() >= limit.clamp(1, 500) as usize {
                break;
            }
        }
        Ok(rows)
    }

    /// Activity, newest first. `before` = last id of the previous page.
    pub fn activity(&self, before: Option<i64>, limit: u32) -> Result<Vec<ActivityEntry>> {
        let db = lock(&self.inner.db);
        activity::name_items(&db.conn, activity::list(&db.conn, before, limit)?)
    }

    pub fn settings(&self) -> Result<Settings> {
        let db = lock(&self.inner.db);
        store::load_settings(&db.conn)
    }

    pub fn paired_device(&self) -> Result<Option<DeviceRow>> {
        let db = lock(&self.inner.db);
        store::paired_peer(&db.conn)
    }

    /// M4 confirmation count ("This moves 132 items to the Bin").
    pub fn retention_preview(&self, folder: FolderId, new: Retention) -> Result<u32> {
        let now = self.now();
        let db = lock(&self.inner.db);
        folders::retention_preview(&db.conn, folder, new, now)
    }

    /// Phone capture routing (places::route over live places). M4 UI; tested in M1.
    pub fn route_capture(&self, fix: Option<Fix>) -> Result<Route> {
        let db = lock(&self.inner.db);
        let c = &db.conn;
        let all = store::all_places(c)?;
        let live_folder = |f: FolderId| store::get_folder(c, f).ok().flatten().is_some_and(|r| r.deleted.val.is_none());
        Ok(places::route(&all, fix, &live_folder))
    }

    // ----- user actions (ops::*, then sweep if needed, events, Notify) -----

    /// After a successful local write: tell the live session and the UI.
    fn wrote(&self, items: &[ItemId], folders: bool, places: bool) {
        if !items.is_empty() {
            self.emit(Event::ItemsChanged { ids: items.to_vec() });
        }
        if folders {
            self.emit(Event::FoldersChanged);
        }
        if places {
            self.emit(Event::PlacesChanged);
        }
        self.emit(Event::ActivityAdded);
        self.inner.local_changed.notify_one();
        self.emit_status();
    }

    pub fn add_local_file(&self, mut f: NewLocalFile) -> Result<ItemId> {
        if let Some(p) = lock(&self.inner.paused).clone() {
            return Err(CoreError::Paused(p));
        }
        // The thumbnail is hashed and copied here; ops::add_local_file takes the finished Thumb.
        // Only photos and videos get a thumbnail: a thumbnail offered for any other file is dropped.
        if names::media_family_of_name(&f.name).is_none() {
            f.thumb = None;
        }
        // Thumbnails are content-addressed and shared between items, so a failed add leaves its
        // thumbnail in place: another import may already point at the same file (no thumbnail GC
        // exists, the cost is one small file).
        let thumb = match f.thumb.take() {
            None => None,
            Some(src) => {
                let (hash, size) = ops::hash_file(&src)?;
                let dest = self.thumb_path(hash);
                if !dest.exists() {
                    // Copy to a name unique to this call, then rename: never a half-written
                    // thumbnail, and two imports of the same bytes never share a temp file.
                    let tmp = self.thumbs_dir().join(format!("{}.{}.tmp", hex::encode(hash.0), uuid::Uuid::now_v7().simple()));
                    if let Err(e) = std::fs::copy(&src, &tmp) {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(CoreError::io(&src, e));
                    }
                    if let Err(e) = std::fs::rename(&tmp, &dest) {
                        let _ = std::fs::remove_file(&tmp);
                        // A concurrent import published the same thumbnail first: that is success.
                        if !dest.exists() {
                            return Err(CoreError::io(&dest, e));
                        }
                    }
                }
                Some(Thumb { hash, size })
            }
        };
        let id = self.with_ctx(|c, ctx| ops::add_local_file(c, ctx, &f, thumb.clone()))?;
        self.wrote(&[id], false, false);
        Ok(id)
    }

    pub fn set_pinned(&self, id: ItemId, pinned: bool) -> Result<()> {
        self.with_ctx(|c, ctx| ops::set_pinned(c, ctx, id, pinned))?;
        self.wrote(&[id], false, false);
        Ok(())
    }

    pub fn move_item(&self, id: ItemId, folder: FolderId) -> Result<()> {
        self.with_ctx(|c, ctx| ops::move_item(c, ctx, id, folder))?;
        self.wrote(&[id], false, false);
        Ok(())
    }

    pub fn rename_item(&self, id: ItemId, name: &str) -> Result<()> {
        self.with_ctx(|c, ctx| ops::rename_item(c, ctx, id, name))?;
        self.wrote(&[id], false, false);
        Ok(())
    }

    pub fn delete_item(&self, id: ItemId) -> Result<()> {
        let r = self.with_ctx(|c, ctx| ops::delete_item(c, ctx, id));
        // A placement failure after the record change stands: tell the UI and the peer anyway.
        if matches!(r, Ok(()) | Err(CoreError::Io { .. })) {
            self.wrote(&[id], false, false);
        }
        r
    }

    pub fn restore_item(&self, id: ItemId) -> Result<()> {
        let r = self.with_ctx(|c, ctx| ops::restore_item(c, ctx, id));
        if matches!(r, Ok(()) | Err(CoreError::Io { .. })) {
            self.wrote(&[id], false, false);
            // A purged file comes back as Pending: fetch it from the other device.
            self.inner.kick.notify_one();
        }
        r
    }

    pub fn create_folder(&self, name: &str, retention: Retention) -> Result<FolderId> {
        let id = self.with_ctx(|c, ctx| ops::create_folder(c, ctx, name, retention))?;
        self.wrote(&[], true, false);
        Ok(id)
    }

    pub fn rename_folder(&self, id: FolderId, name: &str) -> Result<()> {
        self.with_ctx(|c, ctx| ops::rename_folder(c, ctx, id, name))?;
        self.wrote(&[], true, false);
        Ok(())
    }

    pub fn set_retention(&self, id: FolderId, retention: Retention) -> Result<()> {
        self.with_ctx(|c, ctx| ops::set_retention(c, ctx, id, retention))?;
        // The change is committed: tell the UI and the peer before the sweep can fail.
        self.wrote(&[], true, false);
        // A shorter retention can expire items at once: let the sweep move their files.
        self.sweep_and_emit()?;
        Ok(())
    }

    pub fn delete_folder(&self, id: FolderId) -> Result<()> {
        let r = self.with_ctx(|c, ctx| ops::delete_folder(c, ctx, id));
        if matches!(r, Ok(()) | Err(CoreError::Io { .. })) {
            self.wrote(&[], true, false);
            self.emit(Event::ItemsChanged { ids: Vec::new() });
        }
        r
    }

    pub fn put_place(&self, id: Option<PlaceId>, name: &str, area: Area, target: FolderId) -> Result<PlaceId> {
        let pid = self.with_ctx(|c, ctx| ops::put_place(c, ctx, id, name, area, target))?;
        self.wrote(&[], false, true);
        Ok(pid)
    }

    pub fn delete_place(&self, id: PlaceId) -> Result<()> {
        self.with_ctx(|c, ctx| ops::delete_place(c, ctx, id))?;
        self.wrote(&[], false, true);
        Ok(())
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        {
            let db = lock(&self.inner.db);
            store::save_settings(&db.conn, s)?;
        }
        self.emit(Event::SettingsChanged);
        Ok(())
    }

    // ----- PC library glue (M2; desktop watcher calls these) -----

    /// Watcher: an existing item's file content changed (ops::content_changed).
    pub fn local_content_changed(&self, id: ItemId, path: PathBuf) -> Result<()> {
        self.with_ctx(|c, ctx| ops::content_changed(c, ctx, id, &path))?;
        self.wrote(&[id], false, false);
        Ok(())
    }

    /// Watcher: a held file moved/renamed on disk (matched by NTFS id or size+hash).
    /// Updates file_ref; a folder change = ops::move_item, a name change = ops::rename_item.
    pub fn local_file_moved(&self, id: ItemId, new_path: PathBuf, folder: FolderId, name: &str) -> Result<()> {
        self.with_ctx(|c, ctx| {
            let (rec, _) = store::get_item(c, id)?.ok_or(CoreError::NotFound { what: "item", id: id.0.to_string() })?;
            // The file is already at its new place: point the record at it first, so the
            // placement's relocate during move/rename finds it there (C6).
            let old = store::get_file_ref(c, id)?;
            store::put_file_ref(
                c,
                &store::FileRef {
                    item: id,
                    path: new_path.to_string_lossy().into_owned(),
                    volume_serial: old.as_ref().and_then(|o| o.volume_serial),
                    file_id: old.as_ref().and_then(|o| o.file_id),
                    mtime_ms: old.as_ref().and_then(|o| o.mtime_ms),
                },
            )?;
            if rec.location.val.folder != folder {
                ops::move_item(c, ctx, id, folder)?;
            }
            if rec.name.val != name {
                ops::rename_item(c, ctx, id, name)?;
            }
            Ok(())
        })?;
        self.wrote(&[id], false, false);
        Ok(())
    }

    /// Rescan/watcher: these held files are gone from the library (not matched to a move).
    /// The desktop applies the bulk-missing pause rule BEFORE calling this.
    pub fn local_files_missing(&self, ids: &[ItemId]) -> Result<()> {
        // Each id commits on its own: on a failure the ones already done still get their event
        // and wake the sync session, then the error is returned.
        let mut done: Vec<ItemId> = Vec::new();
        for id in ids {
            // The local-state write autocommits outside the delete's transaction: once it ran, the
            // item changed even if a later step fails.
            let mut state_written = false;
            let r = self.with_ctx(|c, ctx| {
                // The file is already gone: record that first, so the delete does not try to
                // move a file that is not there. The item is Binned with no file ref (not
                // Purged: Purged items are tombstones the Bin screen never lists), so it shows
                // in the Bin, and a restore fetches it back from the other device (bin.rs).
                if let Some((_, lfs)) = store::get_item(c, *id)? {
                    if matches!(lfs, LocalFileState::Present | LocalFileState::Binned) {
                        store::set_local_state(c, *id, LocalFileState::Binned, ctx.now)?;
                        state_written = true;
                        store::delete_file_ref(c, *id)?;
                    }
                }
                ops::delete_item(c, ctx, *id)
            });
            // Same rule as delete_item: a placement (Io) failure after the record change stands.
            if matches!(r, Ok(()) | Err(CoreError::Io { .. })) || state_written {
                done.push(*id);
            }
            match r {
                Ok(()) => {}
                Err(e) => {
                    if !done.is_empty() {
                        self.wrote(&done, false, false);
                    }
                    return Err(e);
                }
            }
        }
        self.wrote(ids, false, false);
        Ok(())
    }

    /// Look up an item by its file's NTFS identity / path (move matching).
    pub fn item_by_file_id(&self, volume_serial: u64, file_id: [u8; 16]) -> Result<Option<ItemId>> {
        let db = lock(&self.inner.db);
        store::item_by_file_id(&db.conn, volume_serial, file_id)
    }

    pub fn item_by_path(&self, path: &str) -> Result<Option<ItemId>> {
        let db = lock(&self.inner.db);
        store::item_by_path(&db.conn, path)
    }

    /// Phone: record in the activity history that the app was updated and carried its data over from
    /// the old file names (the Kotlin side did the renames before the core opened). Text is cut at 300 chars.
    pub fn log_upgrade(&self, detail: &str) -> Result<()> {
        self.with_ctx(|c, ctx| activity::log(c, ctx.now, None, None, ActivityAction::Upgraded, detail).map(|_| ()))
    }

    /// Pause/resume sync (library missing, bulk missing). Paused = no session work, no sweep
    /// moves on the PC side, state Paused with `reason` as the message.
    pub fn set_paused(&self, reason: Option<String>) {
        let was = std::mem::replace(&mut *lock(&self.inner.paused), reason.clone());
        let logged = self.with_ctx(|c, ctx| match (&was, &reason) {
            (None, Some(r)) => activity::log(c, ctx.now, None, None, ActivityAction::Paused, r).map(|_| ()),
            (Some(_), None) => activity::log(c, ctx.now, None, None, ActivityAction::Resumed, "").map(|_| ()),
            _ => Ok(()),
        });
        if let Err(e) = logged {
            self.emit_error(e.kind(), &format!("could not log the pause: {e}"));
        }
        if reason.is_some() {
            // Drop the live connection: a paused library must not exchange changes.
            if let Some(a) = lock(&self.inner.active).as_ref() {
                a.conn.close(tugboat_net::close::NORMAL, "sync paused");
            }
        } else {
            self.inner.wake.notify_one();
            self.inner.kick.notify_one();
        }
        self.emit(Event::ActivityAdded);
        self.emit_status();
    }

    /// Live folders as (id, kind, name), without item counts: the cheap list the desktop rescan
    /// needs on every pass (`folders()` also derives the state of every item to count them).
    pub fn live_folders(&self) -> Result<Vec<(FolderId, FolderKind, String)>> {
        let db = lock(&self.inner.db);
        Ok(store::all_folders(&db.conn)?
            .into_iter()
            .filter(|f| f.deleted.val.is_none())
            .map(|f| (f.id, f.kind, f.name.val))
            .collect())
    }

    /// Every file this device holds in its library with its NTFS identity (desktop rescan input).
    pub fn held_files(&self) -> Result<Vec<store::HeldFile>> {
        let db = lock(&self.inner.db);
        store::held_files(&db.conn)
    }

    /// Desktop: remember the modified time (unix ms) of the file an item holds, as the watcher
    /// saw it at the item's current path. Errors when the item holds no file.
    pub fn set_file_mtime(&self, id: ItemId, mtime_ms: i64) -> Result<()> {
        let db = lock(&self.inner.db);
        store::set_file_mtime(&db.conn, id, mtime_ms)
    }

    /// Desktop: remember the NTFS identity of an item's file (set after a scan recognised it).
    pub fn set_file_identity(&self, id: ItemId, volume_serial: u64, file_id: [u8; 16]) -> Result<()> {
        let db = lock(&self.inner.db);
        store::set_file_identity(&db.conn, id, volume_serial, file_id)
    }

    /// Free-form local values (library_path, library_id).
    pub fn get_value(&self, key: &str) -> Result<Option<String>> {
        let db = lock(&self.inner.db);
        store::get_value(&db.conn, key)
    }

    pub fn set_value(&self, key: &str, value: &str) -> Result<()> {
        let db = lock(&self.inner.db);
        store::set_value(&db.conn, key, value)
    }

    // ----- maintenance -----

    /// bin::sweep now (skipped while paused); events for what moved; items that became Pending
    /// are queued for download. Returns the report so callers can read the next due time.
    pub(crate) fn sweep_and_emit(&self) -> Result<bin::SweepReport> {
        if lock(&self.inner.paused).is_some() {
            return Ok(bin::SweepReport::default());
        }
        let now = self.now();
        let rep = {
            let mut db = lock(&self.inner.db);
            bin::sweep(&mut db.conn, &*self.inner.placement, self.inner.cfg.self_id, now)?
        };
        let mut ids: Vec<ItemId> = Vec::new();
        for list in [&rep.to_bin, &rep.returned, &rep.purged, &rep.want] {
            ids.extend(list.iter().copied());
        }
        ids.sort();
        ids.dedup();
        if !ids.is_empty() {
            self.emit(Event::ItemsChanged { ids });
            self.emit(Event::ActivityAdded);
            self.emit_status();
        }
        for (id, msg) in &rep.errors {
            self.emit_error("placement", &format!("item {}: {msg}", id.0));
        }
        if !rep.want.is_empty() {
            self.inner.kick.notify_one();
        }
        Ok(rep)
    }

    /// Sweep only the items a sync page touched (a full sweep per page would cost O(library)
    /// per received receipt on a big first sync). A large batch falls back to the full sweep.
    pub(crate) fn sweep_changed(&self, ids: &[ItemId]) -> Result<()> {
        if lock(&self.inner.paused).is_some() {
            return Ok(());
        }
        if ids.len() > 200 {
            return self.sweep_and_emit().map(|_| ());
        }
        let now = self.now();
        let rep = {
            let mut db = lock(&self.inner.db);
            bin::sweep_items(&mut db.conn, &*self.inner.placement, self.inner.cfg.self_id, ids, now)?
        };
        for (id, msg) in &rep.errors {
            self.emit_error("placement", &format!("item {}: {msg}", id.0));
        }
        Ok(())
    }

    /// bin::sweep now; returns the next due time. The app calls it on a timer
    /// (at next_due, at most 60 s apart). Also expires an open pairing window.
    pub fn tick(&self) -> Result<Option<i64>> {
        pair::tick(self);
        let rep = self.sweep_and_emit()?;
        Ok(rep.next_due)
    }

    // ----- network and pairing -----

    /// Start the endpoint and the connection loop: PC accepts; phone dials the paired PC
    /// (cached addresses first, then mDNS), reconnecting with backoff 1 s .. 30 s.
    pub async fn start_network(&self, cfg: NetworkConfig) -> Result<()> {
        net::start(self, cfg).await
    }

    pub async fn stop_network(&self) {
        net::stop(self).await
    }

    /// PC: open a pairing window; emits Pairing(WindowOpen{payload}). `replace_existing`
    /// must be true when a phone is already paired (else AlreadyPaired); the old phone is
    /// unpaired only when the new pairing completes.
    pub fn begin_pairing(&self, replace_existing: bool) -> Result<String> {
        pair::begin(self, replace_existing)
    }

    /// PC: user clicked [Pair] / [Cancel] on the code dialog.
    pub async fn confirm_pairing(&self, accept: bool) -> Result<()> {
        pair::confirm(self, accept).await
    }

    /// PC: is a pairing window open now? Lets a UI that missed events (a lagged stream) recompute
    /// what it tracks from the events.
    pub fn pairing_open(&self) -> bool {
        lock(&self.inner.pairing).is_some()
    }

    /// PC: close the pairing window (the user closed the QR dialog).
    pub fn cancel_pairing(&self) {
        pair::cancel(self)
    }

    /// Phone: pair from a scanned QR payload. Emits Pairing(Code), then Completed/Cancelled.
    pub async fn pair_with(&self, payload: &str) -> Result<DeviceRow> {
        pair::pair_with(self, payload, std::future::pending::<()>()).await
    }

    /// `pair_with` that the user can abandon: `cancel` resolving ends it with a `cancelled`
    /// pairing error (and the Cancelled event) as long as the PC's decision has not been read.
    /// After that the pairing completes whatever `cancel` does, so both devices agree.
    pub async fn pair_with_cancel(&self, payload: &str, cancel: impl std::future::Future<Output = ()> + Unpin) -> Result<DeviceRow> {
        pair::pair_with(self, payload, cancel).await
    }

    /// Unpair the peer: sends Unpair if connected, then ops::unpair. Files stay.
    pub async fn unpair(&self) -> Result<()> {
        net::unpair(self).await
    }

    /// Phone: the Wi-Fi changed or the app came to the front; skip the reconnect back-off
    /// (up to 30 s) and dial the PC now. Harmless on a connected phone and on the PC.
    pub fn reconnect_now(&self) {
        self.inner.wake.notify_one();
    }
}

/// Items held here (not in the Bin) that the paired device has not confirmed holding: no receipt
/// from the CURRENTLY paired device for the item's current hash ("PC not found, 3 waiting").
/// 0 while unpaired. The SQL narrows to the rare set without a receipt; `derive_state` still
/// decides who is in the Bin, so the expiry rules stay in one place. Expiry itself never waits
/// for these items (0.2.0).
fn waiting_count(c: &Connection, now: i64) -> Result<u32> {
    let Some(peer) = store::paired_peer(c)? else { return Ok(0) };
    let fmap: HashMap<FolderId, FolderRecord> = store::all_folders(c)?.into_iter().map(|f| (f.id, f)).collect();
    let candidates = store::item_ids_where(
        c,
        &format!(
            "local_file_state = 'present' AND deleted_at IS NULL AND NOT EXISTS (SELECT 1 FROM receipt r WHERE r.blob_hash = item.blob_hash AND r.device_id = x'{}')",
            hex::encode(peer.id.0)
        ),
    )?;
    let mut n = 0;
    for id in candidates {
        let Some((rec, _)) = store::get_item(c, id)? else { continue };
        if !expiry::in_bin(bin::derive_state_in(c, &fmap, &rec, now)?) {
            n += 1;
        }
    }
    Ok(n)
}
