//! Local user/system actions as database writes. Owner: logic builder.
//!
//! Every op: one IMMEDIATE transaction; each written field group gets
//! `store::next_version(self)` and one change_log row (`store::log_change`); one activity
//! row; file moves go through the Placement AFTER the transaction commits, and a placement
//! failure is returned (the record change stands; the next `bin::sweep` / relocate retry
//! fixes the file). Ops never emit events; the facade does.
//!
//! Rules per op (PLAN Folders and expiry):
//! O1 add_local_file: sniff the head (names::check_media) -> NotMedia refuses the file;
//!    hash with streamed BLAKE3 (never whole file in memory); create an ItemRecord with all
//!    groups at ONE new version; location = (folder or Inbox, entered_at = now);
//!    placement.adopt; file_ref; own receipt (hash, self, now) logged as a change;
//!    activity Captured or Added. Same hash already held by a live item -> still a new item
//!    (two shares of one photo are two items), blob reused on disk where the placement can.
//!    The file is adopted BEFORE the record is written, and removed again if the write fails:
//!    never a record without a file, never an orphan file.
//! O2 set_pinned(true/false): pinned group. Activity Kept / Unkept. Not allowed on an item in
//!    the Bin (restore first) -> Invalid.
//! O3 move_item: placement group = (folder, entered_at = now) (PLAN: "Moving an item to
//!    another folder resets folder_entered_at to now"). Target folder must exist and not be
//!    deleted -> else Invalid. Same folder -> no-op. Activity Moved "from -> to". An item in
//!    the Bin must be restored first -> Invalid.
//! O4 rename_item: name group; name cleaned with names::clean_stem rules for the stem
//!    (display name must be storable on the PC); the stored name is "<stem>.<ext>";
//!    Activity Renamed.
//! O5 delete_item: deleted = Some(now); if pinned, also pinned = false in the same op
//!    (a user delete is the later intent). Activity Deleted. An item already in the Bin is a no-op.
//! O6 restore_item (PLAN: "Restoring from the Bin puts the item back with a fresh full
//!    retention"): Deleted -> deleted = None AND placement entered_at = now (same folder;
//!    unknown/deleted folder -> Unsorted). Expired -> placement entered_at = now.
//!    Local Purged -> Pending ("If only the other device still has the file, it is fetched
//!    from there"). Not in the Bin -> Invalid. Activity Restored.
//! O7 content_changed (PC edit): re-hash; same hash -> no-op; else content group with the
//!    new hash/size (ext unchanged, thumb None until a new one is supplied); own receipt for
//!    the new hash. Activity Edited. Expiry now waits for the peer's receipt of the new hash.
//! O8 Folder ops: create (clean_folder_name, retention > 0 or Keep), rename, set_retention
//!    (folders::retention_preview is the caller's confirmation step), delete (folders F5).
//!    Default folders: delete -> Invalid; the Keep folder's retention cannot be changed.
//! O9 Place ops: add/update/delete with validation lat -90..90, lon -180..180, radius > 0,
//!    target folder live.
//! O10 pairing_completed(peer): store::set_paired; if another peer was paired, it is
//!    unpaired first (one phone per PC); delete any stale cursor for the new key (new peer =
//!    full snapshot). Activity Paired. Phone side (pairing_completed_phone): the replaced PC
//!    is also recorded as a pending unpair notice, delivered when it is reachable.
//! O11 unpair: set_unpaired, delete_cursor, clear_log. Synced files stay. Activity Unpaired.
//! O12 blob_committed (sync fetch flow, after placement.commit): local = Present, file_ref,
//!    own receipt (logged change), activity Received. blob_refused: local = Refused, activity
//!    Refused. blob_lost (peer said not found): local = Lost (soft: asked again on every connect), activity Lost.

use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::{activity, bin, expiry, folders, model::*, names, placement::Placement, store, CoreError, Result};

/// Who/when/where for an op.
pub struct Ctx<'a> {
    pub self_id: DeviceId,
    pub now: i64,
    pub placement: &'a dyn Placement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddKind {
    /// Phone camera.
    Captured,
    /// PC library drop or phone share import.
    Added,
}

#[derive(Debug, Clone)]
pub struct NewLocalFile {
    pub src: PathBuf,
    /// Display name, e.g. "IMG_20261117_140233.jpg" or the dropped file's name.
    pub name: String,
    /// None = Inbox.
    pub folder: Option<FolderId>,
    pub captured_at: i64,
    pub geo: Option<Geo>,
    /// Thumbnail file made by the creating device (PLAN: thumbnails are made by the device
    /// that created the item; JPEG, longest side 512 px). The facade hashes it, copies it to
    /// <data_dir>/thumbs/<hash hex>.jpg and passes the resulting `Thumb` to `add_local_file`.
    pub thumb: Option<PathBuf>,
    pub how: AddKind,
    /// PC: NTFS identity (volume serial, FILE_ID_128) for move matching.
    pub ntfs: Option<(u64, [u8; 16])>,
}

// ---------- helpers ----------

fn begin(c: &mut Connection) -> Result<Transaction<'_>> {
    Ok(c.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

fn load_item(c: &Connection, id: ItemId) -> Result<(ItemRecord, LocalFileState)> {
    store::get_item(c, id)?.ok_or(CoreError::NotFound { what: "item", id: id.0.to_string() })
}

fn live_folder(c: &Connection, id: FolderId) -> Result<FolderRecord> {
    match store::get_folder(c, id)? {
        Some(f) if f.deleted.val.is_none() => Ok(f),
        _ => Err(CoreError::Invalid("the folder does not exist or was deleted".into())),
    }
}

fn folder_name(c: &Connection, id: FolderId) -> String {
    folders::effective_folder(c, id).map(|f| f.name.val).unwrap_or_default()
}

/// Name-rule failures that mean "not a photo or video" refuse the file; the rest are name errors.
fn media_err(e: names::NameError) -> CoreError {
    match e {
        names::NameError::ExtensionNotAllowed(_) | names::NameError::NotMedia | names::NameError::MagicMismatch { .. } => {
            CoreError::NotMedia(e.to_string())
        }
        other => CoreError::Name(other),
    }
}

/// First bytes of a file (up to SNIFF_LEN).
fn read_head(path: &Path) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut buf = vec![0u8; names::SNIFF_LEN];
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    buf.truncate(n);
    Ok(buf)
}

/// Point `file_ref` at a new path, keeping the NTFS identity (a rename on one volume keeps it).
fn update_path(c: &Connection, item: ItemId, path: &Path) -> Result<()> {
    let old = store::get_file_ref(c, item)?;
    store::put_file_ref(
        c,
        &store::FileRef {
            item,
            path: path.to_string_lossy().into_owned(),
            volume_serial: old.as_ref().and_then(|o| o.volume_serial),
            file_id: old.as_ref().and_then(|o| o.file_id),
        },
    )
}

fn log_item(c: &Connection, id: ItemId, g: &str, v: Version) -> Result<()> {
    store::log_change(c, &EntityKey::Item { id }, g, v.lamport)?;
    Ok(())
}

/// Insert our receipt for `hash` and log it as a change when it is new.
fn own_receipt(c: &Connection, ctx: &Ctx<'_>, hash: BlobHash, lamport: u64) -> Result<()> {
    let r = ReceiptRecord { hash, device: ctx.self_id, received_at: ctx.now };
    if store::insert_receipt(c, &r, ctx.now)? {
        store::log_change(c, &EntityKey::Receipt { hash, device: ctx.self_id }, group::ALL, lamport)?;
    }
    Ok(())
}

/// Streamed BLAKE3 of a file and its size (64 KiB reads; never whole file in memory).
pub fn hash_file(path: &Path) -> Result<(BlobHash, u64)> {
    let mut f = std::fs::File::open(path).map_err(|e| CoreError::io(path, e))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                h.update(&buf[..n]);
                size += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    Ok((BlobHash(*h.finalize().as_bytes()), size))
}

// ---------- O1 ----------

/// O1. `thumb` = the facade's copy of `f.thumb` (see NewLocalFile::thumb).
pub fn add_local_file(c: &mut Connection, ctx: &Ctx<'_>, f: &NewLocalFile, thumb: Option<Thumb>) -> Result<ItemId> {
    let head = read_head(&f.src)?;
    // Extension: from the name when it has one; a name without one is typed by its bytes.
    let name_ext = f.name.rsplit_once('.').map(|(_, e)| e.to_string()).filter(|e| !e.is_empty());
    let ext_in = match name_ext {
        Some(e) => e,
        None => names::sniff(&head).map(|fam| fam.default_ext().to_string()).ok_or_else(|| CoreError::NotMedia(names::NameError::NotMedia.to_string()))?,
    };
    let (ext, family) = names::check_media(&ext_in, &head).map_err(media_err)?;
    let (hash, size) = hash_file(&f.src)?;

    let folder = live_folder(c, f.folder.unwrap_or(INBOX_ID))?;
    let id = ItemId::new(ctx.now);
    let stem: String = names::strip_ext(&f.name, &ext).chars().filter(|ch| !ch.is_control()).take(250).collect();
    let stem = if stem.trim().is_empty() { names::fallback_stem(f.captured_at) } else { stem };
    let display = format!("{stem}.{ext}");

    // Place the file first: no record without a file.
    let mut target = placement_target(id, hash, &display, &ext, f.captured_at, &folder);
    let path = ctx.placement.adopt(&f.src, &target)?;
    target.current = Some(path.clone());

    let written = (|| -> Result<()> {
        let tx = begin(c)?;
        let v = store::next_version(&tx, ctx.self_id)?;
        let rec = ItemRecord {
            id,
            captured_at: f.captured_at,
            origin: ctx.self_id,
            geo: f.geo,
            name: Versioned { v, val: display.clone() },
            content: Versioned { v, val: Content { hash, size, kind: family.kind(), ext: ext.clone(), thumb: thumb.clone() } },
            location: Versioned { v, val: Location { folder: folder.id, entered_at: ctx.now } },
            pinned: Versioned { v, val: false },
            deleted: Versioned { v, val: None },
        };
        store::insert_item(&tx, &rec, LocalFileState::Present, ctx.now)?;
        store::put_file_ref(
            &tx,
            &store::FileRef { item: id, path: path.to_string_lossy().into_owned(), volume_serial: f.ntfs.map(|n| n.0), file_id: f.ntfs.map(|n| n.1) },
        )?;
        log_item(&tx, id, group::ALL, v)?;
        own_receipt(&tx, ctx, hash, v.lamport)?;
        let action = match f.how {
            AddKind::Captured => ActivityAction::Captured,
            AddKind::Added => ActivityAction::Added,
        };
        activity::log(&tx, ctx.now, Some(ctx.self_id), Some(id), action, &folder.name.val)?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(e) = written {
        // Never leave an orphan file when the record could not be written. The PC library's
        // adopt returns the user's own file in place: purge is a no-op for files it did not move.
        if ctx.placement.purge(&target).is_err() {
            tracing::warn!("could not remove the file of a failed add: {}", path.display());
        }
        return Err(e);
    }
    Ok(id)
}

fn placement_target(id: ItemId, hash: BlobHash, name: &str, ext: &str, captured_at: i64, folder: &FolderRecord) -> crate::placement::FileTarget {
    crate::placement::FileTarget {
        item: id,
        hash,
        name: name.to_string(),
        ext: ext.to_string(),
        captured_at,
        folder: folder.id,
        folder_kind: folder.kind,
        folder_name: folder.name.val.clone(),
        current: None,
    }
}

// ---------- O2-O7 ----------

/// O2.
pub fn set_pinned(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, pinned: bool) -> Result<()> {
    let tx = begin(c)?;
    let (rec, _) = load_item(&tx, id)?;
    if expiry::in_bin(bin::derive_state(&tx, &rec, ctx.now)?) {
        return Err(CoreError::Invalid("the item is in the Bin; restore it first".into()));
    }
    if rec.pinned.val == pinned {
        return Ok(());
    }
    let v = store::next_version(&tx, ctx.self_id)?;
    let mut new = rec.clone();
    new.pinned = Versioned { v, val: pinned };
    store::update_item(&tx, &new)?;
    log_item(&tx, id, group::PINNED, v)?;
    let action = if pinned { ActivityAction::Kept } else { ActivityAction::Unkept };
    activity::log(&tx, ctx.now, Some(ctx.self_id), Some(id), action, &folder_name(&tx, rec.location.val.folder))?;
    tx.commit()?;
    Ok(())
}

/// Hand a changed item to the placement and record the new path (after the record is committed).
fn relocate_if_held(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId) -> Result<()> {
    let (rec, lfs) = load_item(c, id)?;
    if lfs != LocalFileState::Present {
        return Ok(());
    }
    let target = bin::file_target(c, &rec)?;
    let path = ctx.placement.relocate(&target)?;
    update_path(c, id, &path)
}

/// O3.
pub fn move_item(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, folder: FolderId) -> Result<()> {
    {
        let tx = begin(c)?;
        let (rec, _) = load_item(&tx, id)?;
        if expiry::in_bin(bin::derive_state(&tx, &rec, ctx.now)?) {
            return Err(CoreError::Invalid("the item is in the Bin; restore it first".into()));
        }
        let to = live_folder(&tx, folder)?;
        if rec.location.val.folder == folder {
            return Ok(());
        }
        let v = store::next_version(&tx, ctx.self_id)?;
        let mut new = rec.clone();
        new.location = Versioned { v, val: Location { folder, entered_at: ctx.now } };
        store::update_item(&tx, &new)?;
        log_item(&tx, id, group::PLACEMENT, v)?;
        let detail = format!("{} -> {}", folder_name(&tx, rec.location.val.folder), to.name.val);
        activity::log(&tx, ctx.now, Some(ctx.self_id), Some(id), ActivityAction::Moved, &detail)?;
        tx.commit()?;
    }
    relocate_if_held(c, ctx, id)
}

/// O4.
pub fn rename_item(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, name: &str) -> Result<()> {
    {
        let tx = begin(c)?;
        let (rec, _) = load_item(&tx, id)?;
        let ext = rec.content.val.ext.clone();
        let stem = names::clean_stem(names::strip_ext(name, &ext))?;
        let display = format!("{stem}.{ext}");
        if display == rec.name.val {
            return Ok(());
        }
        let v = store::next_version(&tx, ctx.self_id)?;
        let mut new = rec.clone();
        new.name = Versioned { v, val: display.clone() };
        store::update_item(&tx, &new)?;
        log_item(&tx, id, group::NAME, v)?;
        activity::log(&tx, ctx.now, Some(ctx.self_id), Some(id), ActivityAction::Renamed, &format!("{} -> {}", rec.name.val, display))?;
        tx.commit()?;
    }
    relocate_if_held(c, ctx, id)
}

/// Write pinned = false (when pinned) and deleted = Some(now) for one item, inside a transaction.
fn delete_in_tx(tx: &Connection, ctx: &Ctx<'_>, rec: &ItemRecord) -> Result<()> {
    let mut new = rec.clone();
    if rec.pinned.val {
        // A user delete is the later intent: clear the pin first so the two groups never conflict.
        let vp = store::next_version(tx, ctx.self_id)?;
        new.pinned = Versioned { v: vp, val: false };
        log_item(tx, rec.id, group::PINNED, vp)?;
    }
    let vd = store::next_version(tx, ctx.self_id)?;
    new.deleted = Versioned { v: vd, val: Some(ctx.now) };
    store::update_item(tx, &new)?;
    log_item(tx, rec.id, group::DELETED, vd)?;
    activity::log(tx, ctx.now, Some(ctx.self_id), Some(rec.id), ActivityAction::Deleted, &folder_name(tx, rec.location.val.folder))?;
    Ok(())
}

/// O5.
pub fn delete_item(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId) -> Result<()> {
    let lfs = {
        let tx = begin(c)?;
        let (rec, lfs) = load_item(&tx, id)?;
        if expiry::in_bin(bin::derive_state(&tx, &rec, ctx.now)?) {
            return Ok(());
        }
        delete_in_tx(&tx, ctx, &rec)?;
        tx.commit()?;
        lfs
    };
    if lfs == LocalFileState::Present {
        bin::sweep_item(c, ctx.placement, ctx.self_id, id, ctx.now)?;
    }
    Ok(())
}

/// O6.
pub fn restore_item(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId) -> Result<()> {
    let lfs = {
        let tx = begin(c)?;
        let (rec, lfs) = load_item(&tx, id)?;
        let state = bin::derive_state(&tx, &rec, ctx.now)?;
        if !expiry::in_bin(state) {
            return Err(CoreError::Invalid("the item is not in the Bin".into()));
        }
        let mut new = rec.clone();
        if expiry::effectively_deleted(&rec) {
            let vd = store::next_version(&tx, ctx.self_id)?;
            new.deleted = Versioned { v: vd, val: None };
            log_item(&tx, id, group::DELETED, vd)?;
        }
        // Fresh full retention, in the folder it was in (Unsorted when that folder is gone).
        let folder = folders::effective_folder(&tx, rec.location.val.folder)?;
        let vp = store::next_version(&tx, ctx.self_id)?;
        new.location = Versioned { v: vp, val: Location { folder: folder.id, entered_at: ctx.now } };
        log_item(&tx, id, group::PLACEMENT, vp)?;
        store::update_item(&tx, &new)?;
        if matches!(lfs, LocalFileState::Purged | LocalFileState::Lost) {
            store::set_local_state(&tx, id, LocalFileState::Pending, ctx.now)?;
        }
        activity::log(&tx, ctx.now, Some(ctx.self_id), Some(id), ActivityAction::Restored, &folder.name.val)?;
        tx.commit()?;
        lfs
    };
    if lfs == LocalFileState::Binned {
        bin::sweep_item(c, ctx.placement, ctx.self_id, id, ctx.now)?;
    }
    Ok(())
}

/// O7.
pub fn content_changed(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, path: &Path) -> Result<()> {
    let (hash, size) = hash_file(path)?;
    let tx = begin(c)?;
    let (rec, _) = load_item(&tx, id)?;
    if rec.content.val.hash == hash {
        return Ok(());
    }
    let head = read_head(path)?;
    let (_, family) = names::check_media(&rec.content.val.ext, &head).map_err(media_err)?;
    let v = store::next_version(&tx, ctx.self_id)?;
    let mut new = rec.clone();
    new.content = Versioned { v, val: Content { hash, size, kind: family.kind(), ext: rec.content.val.ext.clone(), thumb: None } };
    store::update_item(&tx, &new)?;
    log_item(&tx, id, group::CONTENT, v)?;
    own_receipt(&tx, ctx, hash, v.lamport)?;
    if store::get_file_ref(&tx, id)?.is_some_and(|r| Path::new(&r.path) != path) {
        update_path(&tx, id, path)?;
    }
    activity::log(&tx, ctx.now, Some(ctx.self_id), Some(id), ActivityAction::Edited, &folder_name(&tx, rec.location.val.folder))?;
    tx.commit()?;
    Ok(())
}

// ---------- O8 folders ----------

fn check_retention(r: Retention) -> Result<()> {
    match r {
        Retention::Secs { secs: 0 } => Err(CoreError::Invalid("retention must be longer than 0 seconds".into())),
        Retention::Secs { secs } if secs > i64::MAX as u64 / 1000 => Err(CoreError::Invalid("retention is too long".into())),
        _ => Ok(()),
    }
}

/// O8.
pub fn create_folder(c: &mut Connection, ctx: &Ctx<'_>, name: &str, retention: Retention) -> Result<FolderId> {
    let clean = names::clean_folder_name(name)?;
    check_retention(retention)?;
    let tx = begin(c)?;
    let v = store::next_version(&tx, ctx.self_id)?;
    let id = FolderId::new(ctx.now);
    let rec = FolderRecord {
        id,
        kind: FolderKind::User,
        name: Versioned { v, val: clean.clone() },
        retention: Versioned { v, val: retention },
        deleted: Versioned { v, val: None },
    };
    store::put_folder(&tx, &rec)?;
    store::log_change(&tx, &EntityKey::Folder { id }, group::ALL, v.lamport)?;
    activity::log(&tx, ctx.now, Some(ctx.self_id), None, ActivityAction::FolderCreated, &clean)?;
    tx.commit()?;
    Ok(id)
}

pub fn rename_folder(c: &mut Connection, ctx: &Ctx<'_>, id: FolderId, name: &str) -> Result<()> {
    let clean = names::clean_folder_name(name)?;
    let tx = begin(c)?;
    let rec = live_folder(&tx, id)?;
    if rec.name.val == clean {
        return Ok(());
    }
    let v = store::next_version(&tx, ctx.self_id)?;
    let mut new = rec.clone();
    new.name = Versioned { v, val: clean.clone() };
    store::put_folder(&tx, &new)?;
    store::log_change(&tx, &EntityKey::Folder { id }, group::NAME, v.lamport)?;
    activity::log(&tx, ctx.now, Some(ctx.self_id), None, ActivityAction::FolderRenamed, &format!("{} -> {}", rec.name.val, clean))?;
    tx.commit()?;
    Ok(())
}

pub fn set_retention(c: &mut Connection, ctx: &Ctx<'_>, id: FolderId, retention: Retention) -> Result<()> {
    check_retention(retention)?;
    let tx = begin(c)?;
    let rec = live_folder(&tx, id)?;
    if rec.kind == FolderKind::Keep {
        return Err(CoreError::Invalid("the Keep folder keeps everything; its retention cannot change".into()));
    }
    if rec.retention.val == retention {
        return Ok(());
    }
    let v = store::next_version(&tx, ctx.self_id)?;
    let mut new = rec.clone();
    new.retention = Versioned { v, val: retention };
    store::put_folder(&tx, &new)?;
    store::log_change(&tx, &EntityKey::Folder { id }, group::RETENTION, v.lamport)?;
    let show = |r: Retention| match r {
        Retention::Keep => "Keep".to_string(),
        Retention::Secs { secs } => format!("{secs} s"),
    };
    activity::log(&tx, ctx.now, Some(ctx.self_id), None, ActivityAction::RetentionChanged, &format!("{}: {} -> {}", rec.name.val, show(rec.retention.val), show(retention)))?;
    tx.commit()?;
    // Items re-derive their state from the new retention; the files follow at the next sweep.
    Ok(())
}

pub fn delete_folder(c: &mut Connection, ctx: &Ctx<'_>, id: FolderId) -> Result<()> {
    let held: Vec<ItemId> = {
        let tx = begin(c)?;
        let Some(rec) = store::get_folder(&tx, id)? else {
            return Err(CoreError::NotFound { what: "folder", id: id.0.to_string() });
        };
        if rec.kind != FolderKind::User {
            return Err(CoreError::Invalid("the default folders cannot be deleted".into()));
        }
        if rec.deleted.val.is_some() {
            return Ok(());
        }
        let v = store::next_version(&tx, ctx.self_id)?;
        let mut new = rec.clone();
        new.deleted = Versioned { v, val: Some(ctx.now) };
        store::put_folder(&tx, &new)?;
        store::log_change(&tx, &EntityKey::Folder { id }, group::DELETED, v.lamport)?;
        // F5: every item that is not deleted goes to the Bin. Expired items are included: with
        // the folder gone their derived state would turn into Keep and they would leave the Bin.
        let mut held = Vec::new();
        for item_id in store::item_ids_in_folder(&tx, id)? {
            let (item, lfs) = load_item(&tx, item_id)?;
            if expiry::effectively_deleted(&item) {
                continue;
            }
            delete_in_tx(&tx, ctx, &item)?;
            if lfs == LocalFileState::Present {
                held.push(item_id);
            }
        }
        activity::log(&tx, ctx.now, Some(ctx.self_id), None, ActivityAction::FolderDeleted, &rec.name.val)?;
        tx.commit()?;
        held
    };
    // Files follow after the commit; the first failure is returned, the rest are still tried.
    let mut first: Option<CoreError> = None;
    for item_id in held {
        if let Err(e) = bin::sweep_item(c, ctx.placement, ctx.self_id, item_id, ctx.now) {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

// ---------- O9 places ----------

fn check_area(a: &Area) -> Result<()> {
    let ok = a.lat.is_finite() && a.lon.is_finite() && a.radius_m.is_finite() && (-90.0..=90.0).contains(&a.lat) && (-180.0..=180.0).contains(&a.lon) && a.radius_m > 0.0;
    if ok {
        Ok(())
    } else {
        Err(CoreError::Invalid("place needs latitude -90..90, longitude -180..180 and a radius above 0".into()))
    }
}

fn clean_place_name(name: &str) -> Result<String> {
    let n: String = name.chars().filter(|c| !c.is_control()).collect();
    let n = n.trim().to_string();
    if n.is_empty() || n.chars().count() > 100 {
        return Err(CoreError::Invalid("place name must be 1 to 100 characters".into()));
    }
    Ok(n)
}

/// O9.
pub fn put_place(c: &mut Connection, ctx: &Ctx<'_>, id: Option<PlaceId>, name: &str, area: Area, target: FolderId) -> Result<PlaceId> {
    let name = clean_place_name(name)?;
    check_area(&area)?;
    let tx = begin(c)?;
    live_folder(&tx, target)?;
    match id {
        None => {
            let v = store::next_version(&tx, ctx.self_id)?;
            let pid = PlaceId::new(ctx.now);
            let rec = PlaceRecord {
                id: pid,
                name: Versioned { v, val: name.clone() },
                area: Versioned { v, val: area },
                target: Versioned { v, val: target },
                deleted: Versioned { v, val: None },
            };
            store::put_place(&tx, &rec)?;
            store::log_change(&tx, &EntityKey::Place { id: pid }, group::ALL, v.lamport)?;
            activity::log(&tx, ctx.now, Some(ctx.self_id), None, ActivityAction::FolderCreated, &format!("place {name}"))?;
            tx.commit()?;
            Ok(pid)
        }
        Some(pid) => {
            let rec = store::get_place(&tx, pid)?.ok_or(CoreError::NotFound { what: "place", id: pid.0.to_string() })?;
            if rec.deleted.val.is_some() {
                return Err(CoreError::Invalid("the place was deleted".into()));
            }
            let mut new = rec.clone();
            let key = EntityKey::Place { id: pid };
            if rec.name.val != name {
                let v = store::next_version(&tx, ctx.self_id)?;
                new.name = Versioned { v, val: name };
                store::log_change(&tx, &key, group::NAME, v.lamport)?;
            }
            if rec.area.val != area {
                let v = store::next_version(&tx, ctx.self_id)?;
                new.area = Versioned { v, val: area };
                store::log_change(&tx, &key, group::AREA, v.lamport)?;
            }
            if rec.target.val != target {
                let v = store::next_version(&tx, ctx.self_id)?;
                new.target = Versioned { v, val: target };
                store::log_change(&tx, &key, group::TARGET, v.lamport)?;
            }
            if new != rec {
                store::put_place(&tx, &new)?;
                tx.commit()?;
            }
            Ok(pid)
        }
    }
}

pub fn delete_place(c: &mut Connection, ctx: &Ctx<'_>, id: PlaceId) -> Result<()> {
    let tx = begin(c)?;
    let rec = store::get_place(&tx, id)?.ok_or(CoreError::NotFound { what: "place", id: id.0.to_string() })?;
    if rec.deleted.val.is_some() {
        return Ok(());
    }
    let v = store::next_version(&tx, ctx.self_id)?;
    let mut new = rec.clone();
    new.deleted = Versioned { v, val: Some(ctx.now) };
    store::put_place(&tx, &new)?;
    store::log_change(&tx, &EntityKey::Place { id }, group::DELETED, v.lamport)?;
    tx.commit()?;
    Ok(())
}

// ---------- O10 / O11 pairing ----------

/// Unpair steps shared by O10 and O11, inside a transaction.
fn unpair_in_tx(tx: &Connection, ctx: &Ctx<'_>, peer: DeviceId) -> Result<()> {
    store::set_unpaired(tx, peer, ctx.now)?;
    store::delete_cursor(tx, peer)?;
    store::clear_log(tx)?;
    activity::log(tx, ctx.now, Some(peer), None, ActivityAction::Unpaired, "")?;
    Ok(())
}

/// O10 (PC side). The replaced phone learns it was dropped when it next dials (NOT_PAIRED).
pub fn pairing_completed(c: &mut Connection, ctx: &Ctx<'_>, peer: &DeviceRow) -> Result<()> {
    pairing_completed_in(c, ctx, peer, false)
}

/// O10 (phone side). The phone never gets dialled, so when it replaces a paired PC it keeps an
/// unpair notice for the old PC (same record as an explicit unpair made while apart) and
/// delivers it when that PC is reachable. Only one notice is kept: a notice still owed to a
/// third PC is replaced.
pub fn pairing_completed_phone(c: &mut Connection, ctx: &Ctx<'_>, peer: &DeviceRow) -> Result<()> {
    pairing_completed_in(c, ctx, peer, true)
}

fn pairing_completed_in(c: &mut Connection, ctx: &Ctx<'_>, peer: &DeviceRow, owe_notice: bool) -> Result<()> {
    let tx = begin(c)?;
    let replaced = store::paired_peer(&tx)?.filter(|old| old.id != peer.id);
    if let Some(old) = &replaced {
        unpair_in_tx(&tx, ctx, old.id)?;
    }
    store::set_paired(&tx, peer, ctx.now)?;
    // Paired again with a device we still owed an unpair notice: the notice is void.
    if store::get_pending_unpair(&tx)?.is_some_and(|p| p.peer_id().is_ok_and(|id| id == peer.id)) {
        store::clear_pending_unpair(&tx)?;
    }
    if let (true, Some(old)) = (owe_notice, &replaced) {
        store::set_pending_unpair(&tx, &store::PendingUnpair { peer: hex::encode(old.id.0), addresses: old.addresses.clone() })?;
    }
    // A new (or returning) peer starts with a full snapshot.
    store::delete_cursor(&tx, peer.id)?;
    activity::log(&tx, ctx.now, Some(peer.id), None, ActivityAction::Paired, &peer.name)?;
    tx.commit()?;
    Ok(())
}

/// O11.
pub fn unpair(c: &mut Connection, ctx: &Ctx<'_>, peer: DeviceId) -> Result<()> {
    let tx = begin(c)?;
    unpair_in_tx(&tx, ctx, peer)?;
    tx.commit()?;
    Ok(())
}

// ---------- O12 fetch results ----------

/// O12.
pub fn blob_committed(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, hash: BlobHash, path: &Path, from: DeviceId) -> Result<()> {
    let tx = begin(c)?;
    let (rec, _) = load_item(&tx, id)?;
    if rec.content.val.hash != hash {
        // The content changed again while this download ran: the file is stale.
        return Err(CoreError::Invalid("the downloaded file no longer matches the item".into()));
    }
    store::set_local_state(&tx, id, LocalFileState::Present, ctx.now)?;
    update_path(&tx, id, path)?;
    own_receipt(&tx, ctx, hash, store::max_lamport(&tx)?)?;
    activity::log(&tx, ctx.now, Some(from), Some(id), ActivityAction::Received, &folder_name(&tx, rec.location.val.folder))?;
    tx.commit()?;
    Ok(())
}

/// The peer refused the file, or it failed the type check. Only a Pending (or Lost) item whose
/// content is still `hash` is marked: a download that ran while the content changed again, or
/// while the item was binned or committed by another path, says nothing about the item now.
/// Returns whether the item was marked.
pub fn blob_refused(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, hash: BlobHash, reason: &str) -> Result<bool> {
    let tx = begin(c)?;
    let (rec, lfs) = load_item(&tx, id)?;
    if rec.content.val.hash != hash || !matches!(lfs, LocalFileState::Pending | LocalFileState::Lost) {
        return Ok(false);
    }
    store::set_local_state(&tx, id, LocalFileState::Refused, ctx.now)?;
    store::delete_file_ref(&tx, id)?;
    activity::log(&tx, ctx.now, None, Some(id), ActivityAction::Refused, reason)?;
    tx.commit()?;
    Ok(true)
}

/// The peer said "not found" for `hash`. Lost is a soft state: the sync loop asks again on every
/// connect (and every few minutes), because "not found" is often momentary (file renamed in
/// Explorer before the watcher caught up, library drive briefly away). Same guards as
/// `blob_refused`. Returns true only when the item newly became Lost, so a repeat answer does
/// not log or report again.
pub fn blob_lost(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, hash: BlobHash) -> Result<bool> {
    let tx = begin(c)?;
    let (rec, lfs) = load_item(&tx, id)?;
    if rec.content.val.hash != hash || lfs != LocalFileState::Pending {
        return Ok(false);
    }
    store::set_local_state(&tx, id, LocalFileState::Lost, ctx.now)?;
    activity::log(&tx, ctx.now, None, Some(id), ActivityAction::Lost, "file not on the other device")?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{DAY_MS, HOUR_MS};
    use crate::db::Db;
    use crate::store::test_support::*;
    use uuid::Uuid;

    /// change_log rows as (entity, group) in seq order.
    fn log_rows(c: &Connection) -> Vec<(String, String)> {
        let mut st = c.prepare("SELECT entity, field_group FROM change_log ORDER BY seq").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
    }

    fn groups(c: &Connection, entity: &str) -> Vec<String> {
        log_rows(c).into_iter().filter(|(e, _)| e == entity).map(|(_, g)| g).collect()
    }

    fn row(e: &str, g: &str) -> (String, String) {
        (e.to_string(), g.to_string())
    }

    // L19: add_local_file happy path.
    #[test]
    fn add_local_file_records_everything() {
        let mut r = Rig::new();
        let src = r.write_jpeg("hello");
        let expected = blake3::hash(&std::fs::read(&src).unwrap());
        let now = r.now();
        let id = r.add("hello");
        let rec = r.rec(id);
        assert_eq!(rec.content.val.hash.0, *expected.as_bytes(), "hash = BLAKE3 of the bytes");
        assert_eq!(rec.content.val.size, 4 + 5);
        assert_eq!(rec.location.val, Location { folder: INBOX_ID, entered_at: now });
        assert_eq!(rec.name.val, "hello.jpg");
        assert_eq!(rec.origin, p());
        assert_eq!(rec.content.val.kind, MediaKind::Image);
        assert!(!rec.pinned.val && rec.deleted.val.is_none());
        let vs = [rec.name.v, rec.content.v, rec.location.v, rec.pinned.v, rec.deleted.v];
        assert!(vs.iter().all(|v| *v == vs[0] && v.device == p()), "all groups at ONE version");
        assert_eq!(r.lfs(id), LocalFileState::Present);
        assert!(r.path(id).unwrap().exists());
        assert!(src.exists(), "source left for the caller");
        // Own receipt exists, is logged, and does not qualify as a PEER receipt.
        assert!(store::get_receipt(&r.db.conn, rec.content.val.hash, p()).unwrap().is_some());
        assert_eq!(store::peer_receipt_seen_at(&r.db.conn, rec.content.val.hash).unwrap(), None);
        assert_eq!(log_rows(&r.db.conn), vec![row("item", "all"), row("receipt", "all")]);
        assert_eq!(r.actions(), vec![ActivityAction::Captured]);
        // Two shares of the same bytes: two items, one receipt row.
        let id2 = r.add("hello");
        assert_ne!(id, id2);
        assert_eq!(groups(&r.db.conn, "receipt").len(), 1, "no second receipt for the same hash");
    }

    // L19: exe renamed .jpg -> NotMedia, nothing stored anywhere.
    #[test]
    fn add_refuses_non_media_and_leaves_no_trace() {
        let mut r = Rig::new();
        let src = r.dir.path().join("evil.jpg");
        std::fs::write(&src, b"MZ\x90\0\x03\0\0\0\x04\0\0\0\xff\xff\0\0").unwrap();
        let now = r.now();
        let mk = |name: &str, src: &Path| NewLocalFile { src: src.to_path_buf(), name: name.into(), folder: None, captured_at: now, geo: None, thumb: None, how: AddKind::Added, ntfs: None };
        for name in ["evil.jpg", "evil.exe", "evil"] {
            let e = r.run(|c, x| add_local_file(c, x, &mk(name, &src), None)).unwrap_err();
            assert!(matches!(e, CoreError::NotMedia(_)), "{name}: {e}");
        }
        assert_eq!(store::count_items(&r.db.conn).unwrap(), 0);
        assert_eq!(store::head_seq(&r.db.conn).unwrap(), 0);
        assert!(std::fs::read_dir(r.pl.root.join("items")).unwrap().next().is_none());
        // Missing source: an Io error, not a panic.
        let gone = r.dir.path().join("gone.jpg");
        assert!(matches!(r.run(|c, x| add_local_file(c, x, &mk("gone.jpg", &gone), None)), Err(CoreError::Io { .. })));
        // A name without an extension is typed by its bytes.
        let ok = r.write_jpeg("noext");
        let id = r.run(|c, x| add_local_file(c, x, &mk("noext", &ok), None)).unwrap();
        assert_eq!(r.rec(id).name.val, "noext.jpg");
        // An empty / hostile display name still stores; the record never carries control chars.
        let id = r.run(|c, x| add_local_file(c, x, &mk("a\u{202E}b\0c.JPG", &ok), None)).unwrap();
        assert!(!r.rec(id).name.val.contains('\0'));
        assert!(r.rec(id).name.val.ends_with(".jpg"));
    }

    #[test]
    fn add_into_a_missing_or_deleted_folder_is_invalid_and_leaves_no_file() {
        let mut r = Rig::new();
        let src = r.write_jpeg("x");
        let now = r.now();
        let mk = |folder: FolderId| NewLocalFile { src: src.clone(), name: "x.jpg".into(), folder: Some(folder), captured_at: now, geo: None, thumb: None, how: AddKind::Added, ntfs: None };
        assert!(matches!(r.run(|c, x| add_local_file(c, x, &mk(FolderId(Uuid::from_u128(0x99))), None)), Err(CoreError::Invalid(_))));
        let fid = r.run(|c, x| create_folder(c, x, "Work", Retention::Keep)).unwrap();
        r.run(|c, x| delete_folder(c, x, fid)).unwrap();
        assert!(matches!(r.run(|c, x| add_local_file(c, x, &mk(fid), None)), Err(CoreError::Invalid(_))));
        assert!(std::fs::read_dir(r.pl.root.join("items")).unwrap().next().is_none());
    }

    // A failed record write removes the file it just adopted (no orphan).
    #[test]
    fn failed_record_write_removes_the_adopted_file() {
        let mut r = Rig::new();
        let src = r.write_jpeg("one");
        let now = r.now();
        let mk = || NewLocalFile { src: src.clone(), name: "one.jpg".into(), folder: None, captured_at: now, geo: None, thumb: None, how: AddKind::Added, ntfs: None };
        r.run(|c, x| add_local_file(c, x, &mk(), None)).unwrap();
        // Make the receipt insert fail, the last write of the transaction.
        r.db.conn.execute_batch("CREATE TRIGGER boom BEFORE INSERT ON activity BEGIN SELECT RAISE(ABORT, 'boom'); END;").unwrap();
        let e = r.run(|c, x| add_local_file(c, x, &mk(), None)).unwrap_err();
        assert!(matches!(e, CoreError::Db(_)), "{e}");
        assert_eq!(store::count_items(&r.db.conn).unwrap(), 1, "record rolled back");
        assert_eq!(store::head_seq(&r.db.conn).unwrap(), 2, "no change-log rows from the failed add");
        let files = std::fs::read_dir(r.pl.root.join("items")).unwrap().count();
        assert_eq!(files, 1, "the second file was removed again");
    }

    // L28: one change_log row per written group.
    #[test]
    fn each_op_logs_one_row_per_written_group() {
        let mut r = Rig::new();
        let id = r.add("a");
        let fid = r.run(|c, x| create_folder(c, x, "Work", Retention::Keep)).unwrap();
        let base = log_rows(&r.db.conn).len();
        r.run(|c, x| set_pinned(c, x, id, true)).unwrap();
        r.run(|c, x| rename_item(c, x, id, "b")).unwrap();
        r.run(|c, x| move_item(c, x, id, fid)).unwrap();
        let rows = log_rows(&r.db.conn);
        assert_eq!(&rows[base..], &[row("item", "pinned"), row("item", "name"), row("item", "placement")]);
        // Delete of a pinned item: pinned + deleted. Restore of a deleted item: deleted + placement.
        r.run(|c, x| delete_item(c, x, id)).unwrap();
        r.run(|c, x| restore_item(c, x, id)).unwrap();
        let rows = log_rows(&r.db.conn);
        assert_eq!(&rows[base + 3..], &[row("item", "pinned"), row("item", "deleted"), row("item", "deleted"), row("item", "placement")]);
        // Versions strictly increase across the ops (lamport = max + 1).
        let lamports: Vec<i64> = {
            let mut st = r.db.conn.prepare("SELECT lamport FROM change_log ORDER BY seq").unwrap();
            st.query_map([], |x| x.get(0)).unwrap().map(|x| x.unwrap()).collect()
        };
        assert!(lamports.windows(2).all(|w| w[0] <= w[1]), "{lamports:?}");
        assert_eq!(*lamports.last().unwrap() as u64, store::max_lamport(&r.db.conn).unwrap());
    }

    // L17: move resets entered_at; deleted/unknown target -> Invalid; same folder no-op.
    #[test]
    fn move_item_rules() {
        let mut r = Rig::new();
        let id = r.add("a");
        let fid = r.run(|c, x| create_folder(c, x, "Work", Retention::Secs { secs: 3600 })).unwrap();
        r.clock.advance(5 * HOUR_MS);
        let before_log = log_rows(&r.db.conn).len();
        let ver_before = r.rec(id).location.v;
        // Same folder: no-op, nothing written.
        r.run(|c, x| move_item(c, x, id, INBOX_ID)).unwrap();
        assert_eq!(log_rows(&r.db.conn).len(), before_log);
        assert_eq!(r.rec(id).location.v, ver_before);
        // Move: entered_at resets to now, folder changes, file stays Present.
        r.run(|c, x| move_item(c, x, id, fid)).unwrap();
        let rec = r.rec(id);
        assert_eq!(rec.location.val, Location { folder: fid, entered_at: r.now() });
        assert!(rec.location.v > ver_before);
        assert_eq!(r.lfs(id), LocalFileState::Present);
        let moved = activity::list(&r.db.conn, None, 1).unwrap().remove(0);
        assert_eq!(moved.action, ActivityAction::Moved);
        assert_eq!(moved.detail, "Inbox -> Work");
        // Unknown and deleted targets.
        assert!(matches!(r.run(|c, x| move_item(c, x, id, FolderId(Uuid::from_u128(0x55)))), Err(CoreError::Invalid(_))));
        let gone = r.run(|c, x| create_folder(c, x, "Gone", Retention::Keep)).unwrap();
        r.run(|c, x| delete_folder(c, x, gone)).unwrap();
        assert!(matches!(r.run(|c, x| move_item(c, x, id, gone)), Err(CoreError::Invalid(_))));
        assert_eq!(r.rec(id).location.val.folder, fid, "failed moves change nothing");
        // Unknown item.
        assert!(matches!(r.run(|c, x| move_item(c, x, ItemId(Uuid::from_u128(1)), fid)), Err(CoreError::NotFound { .. })));
        // An item in the Bin must be restored first.
        r.run(|c, x| delete_item(c, x, id)).unwrap();
        assert!(matches!(r.run(|c, x| move_item(c, x, id, INBOX_ID)), Err(CoreError::Invalid(_))));
        assert!(matches!(r.run(|c, x| set_pinned(c, x, id, true)), Err(CoreError::Invalid(_))));
    }

    // L18: delete of a pinned item writes pinned = false + deleted in one op.
    #[test]
    fn delete_pinned_item_clears_the_pin_in_the_same_op() {
        let mut r = Rig::new();
        let id = r.add("a");
        r.run(|c, x| set_pinned(c, x, id, true)).unwrap();
        r.clock.advance(HOUR_MS);
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| delete_item(c, x, id)).unwrap();
        let rec = r.rec(id);
        assert!(!rec.pinned.val);
        assert_eq!(rec.deleted.val, Some(r.now()));
        assert!(rec.deleted.v > rec.pinned.v, "delete is the later intent");
        assert_eq!(&log_rows(&r.db.conn)[n..], &[row("item", "pinned"), row("item", "deleted")]);
        assert_eq!(crate::merge::delete_vs_keep(&rec), None);
        // File moved to the Bin by the op itself.
        assert_eq!(r.lfs(id), LocalFileState::Binned);
        // Deleting again is a no-op.
        r.run(|c, x| delete_item(c, x, id)).unwrap();
        assert_eq!(log_rows(&r.db.conn).len(), n + 2);
        // Unpinned item: only the deleted group is written.
        let id2 = r.add("b");
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| delete_item(c, x, id2)).unwrap();
        assert_eq!(&log_rows(&r.db.conn)[n..], &[row("item", "deleted")]);
    }

    #[test]
    fn pin_unpin_and_rename() {
        let mut r = Rig::new();
        let id = r.add("a");
        r.run(|c, x| set_pinned(c, x, id, true)).unwrap();
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| set_pinned(c, x, id, true)).unwrap();
        assert_eq!(log_rows(&r.db.conn).len(), n, "pinning a pinned item writes nothing");
        r.run(|c, x| set_pinned(c, x, id, false)).unwrap();
        assert!(r.actions().starts_with(&[ActivityAction::Unkept, ActivityAction::Kept]));
        // Rename keeps the extension and rejects unsafe names.
        r.run(|c, x| rename_item(c, x, id, "Site photo")).unwrap();
        assert_eq!(r.rec(id).name.val, "Site photo.jpg");
        r.run(|c, x| rename_item(c, x, id, "Other.JPG")).unwrap();
        assert_eq!(r.rec(id).name.val, "Other.jpg");
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| rename_item(c, x, id, "Other.jpg")).unwrap();
        assert_eq!(log_rows(&r.db.conn).len(), n, "same name: no-op");
        for bad in ["a:b", "a/b", "CON", "..", "", "a|b"] {
            assert!(matches!(r.run(|c, x| rename_item(c, x, id, bad)), Err(CoreError::Name(_))), "{bad:?}");
        }
        assert_eq!(r.rec(id).name.val, "Other.jpg");
    }

    // L20: a PC edit gets a new hash and waits for a NEW receipt even though the old hash had one.
    #[test]
    fn content_change_waits_for_a_new_receipt() {
        let mut r = Rig::new();
        let id = r.add("a");
        r.confirm(id);
        let old_hash = r.rec(id).content.val.hash;
        let state = |r: &Rig, now| bin::derive_state(&r.db.conn, &r.rec(id), now).unwrap();
        assert!(matches!(state(&r, r.now()), ItemState::Live { .. }));
        // Edit the held file in place.
        let path = r.path(id).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b" edited");
        std::fs::write(&path, &bytes).unwrap();
        r.clock.advance(HOUR_MS);
        let before = log_rows(&r.db.conn).len();
        r.run(|c, x| content_changed(c, x, id, &path)).unwrap();
        let rec = r.rec(id);
        assert_ne!(rec.content.val.hash, old_hash);
        assert_eq!(rec.content.val.hash.0, *blake3::hash(&bytes).as_bytes());
        assert_eq!(rec.content.val.size, bytes.len() as u64);
        assert_eq!(rec.content.val.thumb, None);
        assert_eq!(&log_rows(&r.db.conn)[before..], &[row("item", "content"), row("receipt", "all")]);
        assert_eq!(*r.actions().first().unwrap(), ActivityAction::Edited);
        // Long past the retention: still Waiting, because the peer has not confirmed the new bytes.
        r.clock.advance(10 * DAY_MS);
        assert_eq!(state(&r, r.now()), ItemState::Waiting);
        assert!(r.sweep().to_bin.is_empty());
        // The peer confirms the new hash: now it expires.
        store::insert_receipt(&r.db.conn, &ReceiptRecord { hash: rec.content.val.hash, device: f(), received_at: 1 }, r.now()).unwrap();
        assert!(matches!(state(&r, r.now()), ItemState::Expired { .. }));
        // Same bytes again: no-op.
        let rec_before = r.rec(id);
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| content_changed(c, x, id, &path)).unwrap();
        assert_eq!(r.rec(id), rec_before);
        assert_eq!(log_rows(&r.db.conn).len(), n);
    }

    #[test]
    fn content_change_to_non_media_is_refused() {
        let mut r = Rig::new();
        let id = r.add("a");
        let path = r.path(id).unwrap();
        std::fs::write(&path, b"MZ\x90\0\x03\0\0\0\x04\0\0\0\xff\xff\0\0").unwrap();
        let before = r.rec(id);
        let e = r.run(|c, x| content_changed(c, x, id, &path)).unwrap_err();
        assert!(matches!(e, CoreError::NotMedia(_)), "{e}");
        assert_eq!(r.rec(id), before);
    }

    // L12: receipt qualification through the whole stack; never paired -> Waiting after 1 year.
    #[test]
    fn nothing_expires_without_a_qualifying_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let pl = crate::placement::AppStorage::new(dir.path().join("s")).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        store::ensure_self(&db.conn, p(), "PC", Role::Pc).unwrap();
        let mut t = T0;
        let src = dir.path().join("a.jpg");
        std::fs::write(&src, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]).unwrap();
        let ctx = Ctx { self_id: p(), now: t, placement: &pl };
        let id = add_local_file(&mut db.conn, &ctx, &NewLocalFile { src, name: "a.jpg".into(), folder: None, captured_at: t, geo: None, thumb: None, how: AddKind::Added, ntfs: None }, None).unwrap();
        let hash = store::get_item(&db.conn, id).unwrap().unwrap().0.content.val.hash;
        // Zero devices ever paired: one year later still Waiting, nothing binned.
        t += 365 * DAY_MS;
        let rep = bin::sweep(&mut db.conn, &pl, p(), t).unwrap();
        assert!(rep.to_bin.is_empty());
        let state = |db: &Db, t| bin::derive_state(&db.conn, &store::get_item(&db.conn, id).unwrap().unwrap().0, t).unwrap();
        assert_eq!(state(&db, t), ItemState::Waiting);
        // A receipt from a device we never paired with does not count.
        store::insert_receipt(&db.conn, &ReceiptRecord { hash, device: f(), received_at: 1 }, t).unwrap();
        assert_eq!(state(&db, t), ItemState::Waiting);
        // Pair it: the stored receipt now qualifies (it is the same device).
        let peer = DeviceRow { id: f(), name: "Phone".into(), is_self: false, role: Role::Phone, paired_at: None, unpaired_at: None, last_seen_at: None, addresses: vec![] };
        pairing_completed(&mut db.conn, &Ctx { now: t, ..ctx }, &peer).unwrap();
        assert!(matches!(state(&db, t), ItemState::Expired { .. }));
        // Unpaired but was paired: still qualifies.
        unpair(&mut db.conn, &Ctx { now: t, ..ctx }, f()).unwrap();
        assert!(matches!(state(&db, t), ItemState::Expired { .. }));
        // Re-paired with a NEW key (reinstalled phone): the old key's receipt still counts for the old file...
        let new_phone = DeviceRow { id: dev(0xCC), ..peer.clone() };
        pairing_completed(&mut db.conn, &Ctx { now: t, ..ctx }, &new_phone).unwrap();
        assert!(matches!(state(&db, t), ItemState::Expired { .. }), "PLAN: receipt from a device this one is or was paired with");
        // ...but a file the old phone never confirmed waits for the NEW key.
        let src2 = dir.path().join("b.jpg");
        std::fs::write(&src2, [0xFF, 0xD8, 0xFF, 0xE0, 9, 9]).unwrap();
        let id2 = add_local_file(&mut db.conn, &Ctx { now: t, ..ctx }, &NewLocalFile { src: src2, name: "b.jpg".into(), folder: None, captured_at: t, geo: None, thumb: None, how: AddKind::Added, ntfs: None }, None).unwrap();
        let rec2 = store::get_item(&db.conn, id2).unwrap().unwrap().0;
        let st2 = |db: &Db, t| bin::derive_state(&db.conn, &rec2, t).unwrap();
        assert_eq!(st2(&db, t + 400 * DAY_MS), ItemState::Waiting);
        store::insert_receipt(&db.conn, &ReceiptRecord { hash: rec2.content.val.hash, device: dev(0xCC), received_at: 1 }, t).unwrap();
        assert!(matches!(st2(&db, t + 400 * DAY_MS), ItemState::Expired { .. }));
    }

    // L21: deleting a folder bins its items (files follow), refuses default folders.
    #[test]
    fn folder_ops_and_deletion_bins_items() {
        let mut r = Rig::new();
        let fid = r.run(|c, x| create_folder(c, x, "  Site 4. ", Retention::Secs { secs: 3600 })).unwrap();
        assert_eq!(store::get_folder(&r.db.conn, fid).unwrap().unwrap().name.val, "Site 4");
        assert_eq!(groups(&r.db.conn, "folder"), vec!["all"]);
        // Names and retention are validated.
        for bad in ["a:b", ".tugboat", "", "CON", "x/y"] {
            assert!(matches!(r.run(|c, x| create_folder(c, x, bad, Retention::Keep)), Err(CoreError::Name(_))), "{bad:?}");
        }
        assert!(matches!(r.run(|c, x| create_folder(c, x, "Zero", Retention::Secs { secs: 0 })), Err(CoreError::Invalid(_))));
        assert!(matches!(r.run(|c, x| set_retention(c, x, fid, Retention::Secs { secs: 0 })), Err(CoreError::Invalid(_))));
        r.run(|c, x| rename_folder(c, x, fid, "Site 5")).unwrap();
        assert!(matches!(r.run(|c, x| rename_folder(c, x, fid, "a:b")), Err(CoreError::Name(_))));
        // Keep folder: retention cannot change; defaults cannot be deleted.
        assert!(matches!(r.run(|c, x| set_retention(c, x, KEEP_ID, Retention::Secs { secs: 5 })), Err(CoreError::Invalid(_))));
        for d in [INBOX_ID, KEEP_ID, UNSORTED_ID] {
            assert!(matches!(r.run(|c, x| delete_folder(c, x, d)), Err(CoreError::Invalid(_))));
        }
        assert!(matches!(r.run(|c, x| delete_folder(c, x, FolderId(Uuid::from_u128(0x321)))), Err(CoreError::NotFound { .. })));
        // Items: one live, one pinned, one already in the Bin, one expired (peer confirmed, 2 h old).
        let a = r.add("a");
        let b = r.add("b");
        let d = r.add("d");
        let e = r.add("e");
        for id in [a, b, d, e] {
            r.run(|c, x| move_item(c, x, id, fid)).unwrap();
        }
        r.run(|c, x| set_pinned(c, x, b, true)).unwrap();
        r.run(|c, x| delete_item(c, x, d)).unwrap();
        r.confirm(e);
        r.clock.advance(2 * HOUR_MS);
        r.sweep();
        assert_eq!(r.lfs(e), LocalFileState::Binned, "expired by the folder's 1 h retention");
        let d_deleted_at = r.rec(d).deleted.val;
        r.run(|c, x| delete_folder(c, x, fid)).unwrap();
        for id in [a, b, e] {
            assert!(r.rec(id).deleted.val.is_some(), "deleted with the folder");
            assert_eq!(r.lfs(id), LocalFileState::Binned, "files follow into the Bin");
            assert!(r.path(id).unwrap().starts_with(r.pl.root.join("bin")));
        }
        assert!(!r.rec(b).pinned.val);
        assert_eq!(r.rec(d).deleted.val, d_deleted_at, "already-deleted item untouched");
        assert!(store::get_folder(&r.db.conn, fid).unwrap().unwrap().deleted.val.is_some());
        // The expired item did not pop out of the Bin when its folder vanished.
        let st = bin::derive_state(&r.db.conn, &r.rec(e), r.now()).unwrap();
        assert!(expiry::in_bin(st));
        // Deleting again is a no-op.
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| delete_folder(c, x, fid)).unwrap();
        assert_eq!(log_rows(&r.db.conn).len(), n);
        assert!(matches!(r.run(|c, x| rename_folder(c, x, fid, "x")), Err(CoreError::Invalid(_))));
    }

    #[test]
    fn retention_change_logs_one_row_and_is_noop_when_equal() {
        let mut r = Rig::new();
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| set_retention(c, x, INBOX_ID, Retention::Secs { secs: 86_400 })).unwrap();
        assert_eq!(log_rows(&r.db.conn).len(), n, "equal retention: nothing");
        r.run(|c, x| set_retention(c, x, INBOX_ID, Retention::Keep)).unwrap();
        assert_eq!(&log_rows(&r.db.conn)[n..], &[row("folder", "retention")]);
        assert_eq!(store::get_folder(&r.db.conn, INBOX_ID).unwrap().unwrap().retention.val, Retention::Keep);
        assert_eq!(*r.actions().first().unwrap(), ActivityAction::RetentionChanged);
    }

    #[test]
    fn place_ops_validate_and_log() {
        let mut r = Rig::new();
        let work = r.run(|c, x| create_folder(c, x, "Work", Retention::Keep)).unwrap();
        let area = Area { lat: -30.5, lon: 151.6, radius_m: 150.0 };
        let n = log_rows(&r.db.conn).len();
        let pid = r.run(|c, x| put_place(c, x, None, " Site ", area, work)).unwrap();
        assert_eq!(&log_rows(&r.db.conn)[n..], &[row("place", "all")]);
        let got = store::get_place(&r.db.conn, pid).unwrap().unwrap();
        assert_eq!((got.name.val.as_str(), got.area.val, got.target.val), ("Site", area, work));
        // Validation.
        for bad in [Area { lat: 91.0, ..area }, Area { lat: -91.0, ..area }, Area { lon: 181.0, ..area }, Area { radius_m: 0.0, ..area }, Area { radius_m: -5.0, ..area }, Area { lat: f64::NAN, ..area }, Area { radius_m: f64::INFINITY, ..area }] {
            assert!(matches!(r.run(|c, x| put_place(c, x, None, "x", bad, work)), Err(CoreError::Invalid(_))), "{bad:?}");
        }
        assert!(matches!(r.run(|c, x| put_place(c, x, None, "", area, work)), Err(CoreError::Invalid(_))));
        assert!(matches!(r.run(|c, x| put_place(c, x, None, "x", area, FolderId(Uuid::from_u128(0x33)))), Err(CoreError::Invalid(_))), "unknown target folder");
        // Update touches only the groups that changed.
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| put_place(c, x, Some(pid), "Site", Area { radius_m: 300.0, ..area }, work)).unwrap();
        assert_eq!(&log_rows(&r.db.conn)[n..], &[row("place", "area")]);
        r.run(|c, x| put_place(c, x, Some(pid), "Site", Area { radius_m: 300.0, ..area }, work)).unwrap();
        assert_eq!(log_rows(&r.db.conn).len(), n + 1, "identical update is a no-op");
        r.run(|c, x| put_place(c, x, Some(pid), "Site B", Area { radius_m: 300.0, ..area }, KEEP_ID)).unwrap();
        assert_eq!(&log_rows(&r.db.conn)[n + 1..], &[row("place", "name"), row("place", "target")]);
        // Delete.
        r.run(|c, x| delete_place(c, x, pid)).unwrap();
        assert!(store::get_place(&r.db.conn, pid).unwrap().unwrap().deleted.val.is_some());
        r.run(|c, x| delete_place(c, x, pid)).unwrap();
        assert!(matches!(r.run(|c, x| put_place(c, x, Some(pid), "x", area, work)), Err(CoreError::Invalid(_))));
        assert!(matches!(r.run(|c, x| delete_place(c, x, PlaceId(Uuid::from_u128(1)))), Err(CoreError::NotFound { .. })));
    }

    // O10/O11: one phone per PC, new peer = snapshot, unpair clears log and cursor, files stay.
    #[test]
    fn pairing_and_unpairing() {
        let mut r = Rig::new(); // f() is already paired
        let id = r.add("a");
        store::set_pulled(&r.db.conn, f(), 5).unwrap();
        assert!(store::head_seq(&r.db.conn).unwrap() > 0);
        let new_phone = DeviceRow { id: dev(0xCC), name: "New phone".into(), is_self: false, role: Role::Phone, paired_at: None, unpaired_at: None, last_seen_at: None, addresses: vec!["10.0.0.5:5000".into()] };
        r.run(|c, x| pairing_completed(c, x, &new_phone)).unwrap();
        assert!(!store::is_paired(&r.db.conn, f()).unwrap(), "previous phone unpaired first");
        assert!(store::is_paired(&r.db.conn, dev(0xCC)).unwrap());
        assert_eq!(store::get_cursor(&r.db.conn, f()).unwrap(), None);
        assert_eq!(store::get_cursor(&r.db.conn, dev(0xCC)).unwrap(), None, "new peer starts with a full snapshot");
        assert!(store::changes_since(&r.db.conn, 0, 100).unwrap().0.is_empty(), "log cleared");
        let acts = r.actions();
        assert!(acts.contains(&ActivityAction::Paired) && acts.contains(&ActivityAction::Unpaired));
        assert_eq!(r.lfs(id), LocalFileState::Present, "files stay");
        // A stale cursor for the new key is dropped on pairing.
        store::set_pulled(&r.db.conn, dev(0xCC), 9).unwrap();
        r.run(|c, x| pairing_completed(c, x, &new_phone)).unwrap();
        assert_eq!(store::get_cursor(&r.db.conn, dev(0xCC)).unwrap(), None);
        // Explicit unpair.
        r.run(|c, x| unpair(c, x, dev(0xCC))).unwrap();
        assert!(store::paired_peer(&r.db.conn).unwrap().is_none());
        assert!(store::ever_paired(&r.db.conn).unwrap());
        assert!(matches!(r.run(|c, x| unpair(c, x, dev(0xDD))), Err(CoreError::NotFound { .. })));
        assert_eq!(r.lfs(id), LocalFileState::Present);
    }

    // O10 phone side: replacing a paired PC leaves a notice for the old one; the PC side does not.
    #[test]
    fn phone_repair_keeps_an_unpair_notice_for_the_old_pc() {
        let mut r = Rig::new(); // f() is already paired
        store::set_last_seen(&r.db.conn, f(), 1, &["10.0.0.9:4000".to_string()]).unwrap();
        let other = DeviceRow { id: dev(0xCC), name: "Other PC".into(), is_self: false, role: Role::Pc, paired_at: None, unpaired_at: None, last_seen_at: None, addresses: vec![] };
        // PC side: no notice.
        let mut pc = Rig::new();
        pc.run(|c, x| pairing_completed(c, x, &other)).unwrap();
        assert!(store::get_pending_unpair(&pc.db.conn).unwrap().is_none());
        // Phone side: notice names the old PC with its last known address.
        r.run(|c, x| pairing_completed_phone(c, x, &other)).unwrap();
        let n = store::get_pending_unpair(&r.db.conn).unwrap().expect("notice owed to the old PC");
        assert_eq!(n.peer_id().unwrap(), f());
        assert_eq!(n.addresses, vec!["10.0.0.9:4000".to_string()]);
        // Pairing again with the same PC (no replacement) leaves the notice alone.
        r.run(|c, x| pairing_completed_phone(c, x, &other)).unwrap();
        assert_eq!(store::get_pending_unpair(&r.db.conn).unwrap().unwrap().peer_id().unwrap(), f());
        // Going back to the old PC voids its notice and owes one to the other PC.
        let back = DeviceRow { id: f(), ..other.clone() };
        r.run(|c, x| pairing_completed_phone(c, x, &back)).unwrap();
        let n = store::get_pending_unpair(&r.db.conn).unwrap().expect("now owed to the other PC");
        assert_eq!(n.peer_id().unwrap(), dev(0xCC));
    }

    // O12.
    #[test]
    fn blob_results() {
        let mut r = Rig::new();
        let it = item(5, f(), v(3, f()));
        store::insert_item(&r.db.conn, &it, LocalFileState::Pending, T0).unwrap();
        let part = r.pl.part_path(&it.content.val.hash).unwrap();
        std::fs::write(&part, [0xFF, 0xD8, 0xFF, 0xE0]).unwrap();
        let target = bin::file_target(&r.db.conn, &it).unwrap();
        let path = r.pl.commit(&part, &target).unwrap();
        // A stale download (hash no longer matches the record) is refused.
        let e = r.run(|c, x| blob_committed(c, x, it.id, hash(77), &path, f())).unwrap_err();
        assert!(matches!(e, CoreError::Invalid(_)));
        assert_eq!(r.lfs(it.id), LocalFileState::Pending);
        r.run(|c, x| blob_committed(c, x, it.id, it.content.val.hash, &path, f())).unwrap();
        assert_eq!(r.lfs(it.id), LocalFileState::Present);
        assert_eq!(r.path(it.id), Some(path.clone()));
        assert!(store::get_receipt(&r.db.conn, it.content.val.hash, p()).unwrap().is_some(), "own receipt after a verified file");
        assert_eq!(groups(&r.db.conn, "receipt"), vec!["all"], "and it is logged so the peer pulls it");
        assert_eq!(*r.actions().first().unwrap(), ActivityAction::Received);
        // Refused and lost.
        let it2 = item(6, f(), v(3, f()));
        store::insert_item(&r.db.conn, &it2, LocalFileState::Pending, T0).unwrap();
        // A stale answer (content changed since) marks nothing.
        assert!(!r.run(|c, x| blob_refused(c, x, it2.id, hash(77), "type check failed")).unwrap());
        assert!(!r.run(|c, x| blob_lost(c, x, it2.id, hash(77))).unwrap());
        assert_eq!(r.lfs(it2.id), LocalFileState::Pending);
        assert!(r.run(|c, x| blob_refused(c, x, it2.id, it2.content.val.hash, "type check failed")).unwrap());
        assert_eq!(r.lfs(it2.id), LocalFileState::Refused);
        assert!(store::get_receipt(&r.db.conn, it2.content.val.hash, p()).unwrap().is_none(), "no receipt for a refused file");
        // Lost: only from Pending, reported once.
        let it3 = item(9, f(), v(3, f()));
        store::insert_item(&r.db.conn, &it3, LocalFileState::Pending, T0).unwrap();
        assert!(r.run(|c, x| blob_lost(c, x, it3.id, it3.content.val.hash)).unwrap());
        assert_eq!(r.lfs(it3.id), LocalFileState::Lost);
        assert!(!r.run(|c, x| blob_lost(c, x, it3.id, it3.content.val.hash)).unwrap(), "a repeat answer is not logged again");
        assert_eq!(r.actions().iter().filter(|a| **a == ActivityAction::Lost).count(), 1);
        assert!(r.actions().contains(&ActivityAction::Refused));
        assert!(matches!(r.run(|c, x| blob_lost(c, x, ItemId(Uuid::from_u128(1)), hash(1))), Err(CoreError::NotFound { .. })));
    }

    #[test]
    fn hash_file_streams_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.bin");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let (h, size) = hash_file(&p).unwrap();
        assert_eq!(h.0, *blake3::hash(&data).as_bytes());
        assert_eq!(size, 300_000);
        assert!(matches!(hash_file(&dir.path().join("nope")), Err(CoreError::Io { .. })));
    }
}
