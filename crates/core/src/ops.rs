//! Local user/system actions as database writes.
//!
//! Every op: one IMMEDIATE transaction; each written field group gets
//! `store::next_version(self)` and one change_log row (`store::log_change`); one activity
//! row; file moves go through the Placement AFTER the transaction commits, and a placement
//! failure is returned (the record change stands; the next `bin::sweep` / relocate retry
//! fixes the file). Ops never emit events; the facade does.
//!
//! Rules per op:
//! O1 add_local_file: an always-refused name -> Refused before anything is read; the head is
//!    classified (names::classify): a photo/video extension must match its bytes (else NotMedia,
//!    a renamed program never poses as a photo), any other safe extension or none is a plain File
//!    (no thumbnail, nothing promoted); a file over Settings.max_blob_bytes -> Refused("TOO LARGE ...");
//!    hash with streamed BLAKE3 (never whole file in memory); create an ItemRecord with all
//!    groups at ONE new version; location = (folder or Inbox, entered_at = now);
//!    placement.adopt; file_ref; own receipt (hash, self, now) logged as a change;
//!    activity Captured or Added. Same hash already held by a live item -> still a new item
//!    (two shares of one photo are two items), blob reused on disk where the placement can.
//!    The file is adopted BEFORE the record is written, and removed again if the write fails:
//!    never a record without a file, never an orphan file.
//! O2 set_pinned(true/false): pinned group. Activity Kept / Unkept. Not allowed on an item in
//!    the Bin (restore first) -> Invalid.
//! O3 move_item: placement group = (folder, entered_at = now) (moving an item to
//!    another folder resets folder_entered_at to now). Target folder must exist and not be
//!    deleted -> else Invalid. Same folder -> no-op. Activity Moved "from -> to". An item in
//!    the Bin must be restored first -> Invalid.
//! O4 rename_item: name group; name cleaned with names::clean_stem rules for the stem
//!    (display name must be storable on the PC); the stored name is "<stem>.<ext>" ("<stem>"
//!    when the item has no extension); the extension is the record's own and never changes; a
//!    result that is an always-refused name (stem "desktop" + ext "ini") -> Refused; Activity Renamed.
//! O5 delete_item: deleted = Some(now); if pinned, also pinned = false in the same op
//!    (a user delete is the later intent). Activity Deleted. An item already in the Bin is a no-op.
//! O6 restore_item (restoring from the Bin puts the item back with a fresh full
//!    retention): Deleted -> deleted = None AND placement entered_at = now (same folder;
//!    unknown/deleted folder -> Unsorted). Expired -> placement entered_at = now.
//!    Local Purged -> Pending ("If only the other device still has the file, it is fetched
//!    from there"). Not in the Bin -> Invalid. Activity Restored.
//! O6b purge_items (Delete forever, Empty Bin): `purged` = Some(now) with ONE new version for the
//!    whole call, one change_log row each, thumbnail queued for deletion, name, saved location and
//!    thumbnail cleared from the record (`scrub_purged`), Activity DeletedForever.
//!    Every item must be in the Bin (restore and edit race: Invalid, nothing written); one already
//!    purged is skipped. The file goes through the Bin sweep after the commit. Other devices delete
//!    their copy when they merge it (merge M10); nothing waits for them. Every other op on a purged
//!    item answers NotFound: it is gone.
//! O7 content_changed (PC edit): re-hash; same hash -> no-op; else content group with the
//!    new hash/size (kind and ext unchanged, thumb None until a new one is supplied); own receipt for
//!    the new hash. Activity Edited. A photo/video whose new bytes no longer match -> NotMedia.
//!    Expiry does not look at receipts (0.2.0): the edit keeps the item's due time.
//! O8 Folder ops: create (clean_folder_name, retention > 0 or Keep), rename, set_retention
//!    (folders::retention_preview is the caller's confirmation step), delete (folders F5).
//!    Default folders: delete -> Invalid; the Keep folder's retention cannot be changed.
//! O9 Place ops: add/update/delete with validation lat -90..90, lon -180..180, radius > 0,
//!    target folder live.
//! O10 pairing_completed(peer): store::set_paired; if another peer was paired, it is
//!    unpaired first (one phone per PC); delete any stale cursor for the new key (new peer =
//!    full snapshot). Activity Paired. Phone side (pairing_completed_phone): never replaces; it
//!    links up to MAX_LINKED PCs, counted inside the transaction (the same PC linking again is
//!    not counted). Over the limit the PC, which already saved the link, is stored as unpaired
//!    and owed an unpair notice, and the call fails with TooManyDevices.
//! O11 unpair: set_unpaired, delete_cursor, trim the change log to what the remaining peers have
//!    not acked yet. Synced files stay. Activity Unpaired.
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
    /// Thumbnail file made by the creating device (thumbnails are made by the device
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
    match store::get_item(c, id)? {
        // A purged item is a tombstone: every op treats it as gone.
        Some((rec, lfs)) if rec.purged.is_none() => Ok((rec, lfs)),
        _ => Err(CoreError::NotFound { what: "item", id: id.0.to_string() }),
    }
}

/// The item, or None when it was deleted forever meanwhile (a download result says nothing then).
fn load_unless_purged(c: &Connection, id: ItemId) -> Result<Option<(ItemRecord, LocalFileState)>> {
    match store::get_item(c, id)? {
        Some((rec, _)) if rec.purged.is_some() => Ok(None),
        Some(x) => Ok(Some(x)),
        None => Err(CoreError::NotFound { what: "item", id: id.0.to_string() }),
    }
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

/// Name-rule failures: an always-refused name is `Refused` (final, visible, never retried); a
/// photo/video extension whose content is not a photo or video is `NotMedia` (a renamed
/// program); the rest are name errors.
fn media_err(e: names::NameError) -> CoreError {
    match e {
        names::NameError::Refused(_) => CoreError::Refused(names::REFUSED_TEXT.to_string()),
        names::NameError::NotMedia | names::NameError::MagicMismatch { .. } | names::NameError::KindMismatch(_) => {
            CoreError::NotMedia(e.to_string())
        }
        other => CoreError::Name(other),
    }
}

/// "TOO LARGE - over 16 GB": the one-line refusal for a file above `Settings.max_blob_bytes`.
pub fn too_large_text(max_blob_bytes: u64) -> String {
    let gb = max_blob_bytes / (1024 * 1024 * 1024);
    if gb >= 1 {
        format!("TOO LARGE - over {gb} GB")
    } else {
        format!("TOO LARGE - over {} MB", max_blob_bytes / (1024 * 1024))
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

/// Point `file_ref` at a new path, keeping the NTFS identity (a rename on one volume keeps it)
/// and the recorded modified time (a move keeps the file's bytes and mtime).
fn update_path(c: &Connection, item: ItemId, path: &Path) -> Result<()> {
    let old = store::get_file_ref(c, item)?;
    store::put_file_ref(
        c,
        &store::FileRef {
            item,
            path: path.to_string_lossy().into_owned(),
            volume_serial: old.as_ref().and_then(|o| o.volume_serial),
            file_id: old.as_ref().and_then(|o| o.file_id),
            mtime_ms: old.as_ref().and_then(|o| o.mtime_ms),
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

/// O1. `thumb` = the facade's copy of `f.thumb` (see NewLocalFile::thumb); dropped for a File.
pub fn add_local_file(c: &mut Connection, ctx: &Ctx<'_>, f: &NewLocalFile, thumb: Option<Thumb>) -> Result<ItemId> {
    // Always-refused names are turned away before anything is read.
    if names::is_refused_name(&f.name) {
        return Err(CoreError::Refused(names::REFUSED_TEXT.to_string()));
    }
    let head = read_head(&f.src)?;
    // Extension: the name's own (no promotion: "Makefile" with JPEG bytes stays a plain file).
    let name = names::prep(&f.name);
    let (kind, ext) = names::classify(&names::ext_of(&name), &head).map_err(media_err)?;
    // A file over the size limit is refused here, once and visibly: the other device would refuse
    // it and ask again for ever.
    let max = store::load_settings(c)?.max_blob_bytes;
    let len = std::fs::metadata(&f.src).map_err(|e| CoreError::io(&f.src, e))?.len();
    if len > max {
        return Err(CoreError::Refused(too_large_text(max)));
    }
    let (hash, size) = hash_file(&f.src)?;
    let thumb = if kind == MediaKind::File { None } else { thumb };

    let folder = live_folder(c, f.folder.unwrap_or(INBOX_ID))?;
    let id = ItemId::new(ctx.now);
    let stem: String = names::strip_ext(&name, &ext).chars().take(250).collect();
    let stem = if stem.trim().is_empty() { names::fallback_stem(f.captured_at) } else { stem };
    let display = names::join_ext(&stem, &ext);
    // The record's name is what the other device builds its file name from.
    if names::is_refused_name(&display) {
        return Err(CoreError::Refused(names::REFUSED_TEXT.to_string()));
    }

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
            content: Versioned { v, val: Content { hash, size, kind, ext: ext.clone(), thumb: thumb.clone() } },
            location: Versioned { v, val: Location { folder: folder.id, entered_at: ctx.now } },
            pinned: Versioned { v, val: false },
            deleted: Versioned { v, val: None },
            purged: None,
        };
        store::insert_item(&tx, &rec, LocalFileState::Present, ctx.now)?;
        store::put_file_ref(
            &tx,
            &store::FileRef { item: id, path: path.to_string_lossy().into_owned(), volume_serial: f.ntfs.map(|n| n.0), file_id: f.ntfs.map(|n| n.1), mtime_ms: None },
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
        // Never leave an orphan file when the record could not be written. A file adopted in place
        // (the PC library returns the very path it was given) is the user's own and stays: only a copy
        // the placement made is removed.
        if path != f.src && ctx.placement.purge(&target).is_err() {
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
            // Nothing to record, but an earlier try may have committed and failed to relocate:
            // a retry with the same arguments must still bring the file to its place.
            drop(tx);
            return relocate_if_held(c, ctx, id);
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

/// The display name `name` would give an item of type `ext`, or why it is refused.
fn renamed_display(name: &str, ext: &str) -> Result<String> {
    let stem = names::clean_stem(names::strip_ext(name, ext))?;
    let display = names::join_ext(&stem, ext);
    // A name that would be one of the always-refused ones (stem "desktop" + ext "ini") never starts.
    if names::is_refused_name(&display) {
        return Err(CoreError::Refused(names::REFUSED_TEXT.to_string()));
    }
    // A plain file without an extension cannot take a photo/video name: it would be placed
    // under it and open as a photo.
    if !names::plain_name_ok(&display, ext) {
        return Err(media_err(names::NameError::NotMedia));
    }
    Ok(display)
}

/// Whether `rename_item` would accept `name`, without changing anything.
pub fn check_rename(c: &Connection, id: ItemId, name: &str) -> Result<()> {
    let (rec, _) = load_item(c, id)?;
    renamed_display(name, &rec.content.val.ext).map(|_| ())
}

/// O4.
pub fn rename_item(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, name: &str) -> Result<()> {
    {
        let tx = begin(c)?;
        let (rec, _) = load_item(&tx, id)?;
        let display = renamed_display(name, &rec.content.val.ext)?;
        if display == rec.name.val {
            // Same as move_item: a retry after a failed relocate must still relocate.
            drop(tx);
            return relocate_if_held(c, ctx, id);
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

/// O6b. Returns how many items were purged now (one already purged is not counted).
pub fn purge_items(c: &mut Connection, ctx: &Ctx<'_>, ids: &[ItemId]) -> Result<u32> {
    let done: Vec<ItemId> = {
        let tx = begin(c)?;
        let v = store::next_version(&tx, ctx.self_id)?;
        let mut done = Vec::new();
        for id in ids {
            let Some((rec, _)) = store::get_item(&tx, *id)? else {
                return Err(CoreError::NotFound { what: "item", id: id.0.to_string() });
            };
            if rec.purged.is_some() {
                continue;
            }
            if !expiry::in_bin(bin::derive_state(&tx, &rec, ctx.now)?) {
                return Err(CoreError::Invalid("the item is not in the Bin".into()));
            }
            let mut new = rec.clone();
            new.purged = Some(Versioned { v, val: ctx.now });
            new.scrub_purged();
            store::update_item(&tx, &new)?;
            log_item(&tx, *id, group::PURGED, v)?;
            if let Some(t) = &rec.content.val.thumb {
                store::trash_thumb(&tx, t.hash)?;
            }
            activity::log(&tx, ctx.now, Some(ctx.self_id), Some(*id), ActivityAction::DeletedForever, &folder_name(&tx, rec.location.val.folder))?;
            done.push(*id);
        }
        tx.commit()?;
        done
    };
    // Files follow after the commit; the first failure is returned, the rest are still tried.
    let mut first: Option<CoreError> = None;
    for id in &done {
        if let Err(e) = bin::sweep_item(c, ctx.placement, ctx.self_id, *id, ctx.now) {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(done.len() as u32), Err)
}

/// O7.
pub fn content_changed(c: &mut Connection, ctx: &Ctx<'_>, id: ItemId, path: &Path) -> Result<()> {
    // A file over the cap is refused from its size alone: hashing it would hold the database for
    // the whole read, on every pass while it keeps growing. A same-size touch still hashes, so an
    // unchanged file is not reported as too large.
    {
        let (rec, _) = load_item(c, id)?;
        let max = store::load_settings(c)?.max_blob_bytes;
        let on_disk = std::fs::metadata(path).map_err(|e| CoreError::io(path, e))?.len();
        if on_disk > max && on_disk != rec.content.val.size {
            return Err(CoreError::Refused(too_large_text(max)));
        }
    }
    let (hash, size) = hash_file(path)?;
    let tx = begin(c)?;
    let (rec, _) = load_item(&tx, id)?;
    if rec.content.val.hash == hash {
        return Ok(());
    }
    // The kind never changes with an edit: a File stays a File; a photo or video whose new bytes
    // no longer match its type is refused as NotMedia (a program saved over a photo).
    let kind = rec.content.val.kind;
    if kind != MediaKind::File {
        let head = read_head(path)?;
        names::verify_received(kind, &rec.content.val.ext, &head).map_err(media_err)?;
    }
    let max = store::load_settings(&tx)?.max_blob_bytes;
    if size > max {
        return Err(CoreError::Refused(too_large_text(max)));
    }
    let v = store::next_version(&tx, ctx.self_id)?;
    let mut new = rec.clone();
    new.content = Versioned { v, val: Content { hash, size, kind, ext: rec.content.val.ext.clone(), thumb: None } };
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

/// Unpair steps shared by O10 and O11, inside a transaction. The log is trimmed, not emptied:
/// the other linked PCs may still need rows this peer has already had.
fn unpair_in_tx(tx: &Connection, ctx: &Ctx<'_>, peer: DeviceId, by_peer: bool) -> Result<()> {
    store::set_unpaired(tx, peer, ctx.now)?;
    store::delete_cursor(tx, peer)?;
    store::trim_log_for_peers(tx, ctx.now)?;
    activity::log(tx, ctx.now, Some(peer), None, ActivityAction::Unpaired, if by_peer { "by the other device" } else { "" })?;
    Ok(())
}

/// O10 (PC side). The replaced phone learns it was dropped when it next dials (NOT_PAIRED).
pub fn pairing_completed(c: &mut Connection, ctx: &Ctx<'_>, peer: &DeviceRow) -> Result<()> {
    pairing_completed_in(c, ctx, peer, false)
}

/// O10 (phone side). Links one more PC beside the others; fails with `TooManyDevices` when
/// `MAX_LINKED` other PCs are linked already.
pub fn pairing_completed_phone(c: &mut Connection, ctx: &Ctx<'_>, peer: &DeviceRow) -> Result<()> {
    pairing_completed_in(c, ctx, peer, true)
}

fn pairing_completed_in(c: &mut Connection, ctx: &Ctx<'_>, peer: &DeviceRow, phone: bool) -> Result<()> {
    let tx = begin(c)?;
    if phone {
        if store::count_paired_except(&tx, peer.id)? >= MAX_LINKED {
            // The PC saved the link before this device was asked: keep it as removed and tell it.
            store::set_paired(&tx, peer, ctx.now)?;
            store::set_unpaired(&tx, peer.id, ctx.now)?;
            store::add_pending_unpair(&tx, &store::PendingUnpair { peer: hex::encode(peer.id.0), addresses: peer.addresses.clone() })?;
            tx.commit()?;
            return Err(CoreError::TooManyDevices);
        }
    } else if let Some(old) = store::paired_peer(&tx)?.filter(|old| old.id != peer.id) {
        // A PC has one phone: linking another logs out of the old one.
        unpair_in_tx(&tx, ctx, old.id, false)?;
    }
    store::set_paired(&tx, peer, ctx.now)?;
    // Paired again with a device we still owed an unpair notice: the notice is void.
    store::remove_pending_unpair(&tx, peer.id)?;
    // A new (or returning) peer starts with a full snapshot.
    store::delete_cursor(&tx, peer.id)?;
    activity::log(&tx, ctx.now, Some(peer.id), None, ActivityAction::Paired, &peer.name)?;
    tx.commit()?;
    Ok(())
}

/// O11. `by_peer`: the other device did it (it logged out, or no longer knows this one).
pub fn unpair(c: &mut Connection, ctx: &Ctx<'_>, peer: DeviceId, by_peer: bool) -> Result<()> {
    let tx = begin(c)?;
    unpair_in_tx(&tx, ctx, peer, by_peer)?;
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
    // A freshly downloaded file has no recorded modified time yet (the PC watcher stamps it).
    if let Some(mut fr) = store::get_file_ref(&tx, id)? {
        fr.mtime_ms = None;
        store::put_file_ref(&tx, &fr)?;
    }
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
    let Some((rec, lfs)) = load_unless_purged(&tx, id)? else { return Ok(false) };
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
    let Some((rec, lfs)) = load_unless_purged(&tx, id)? else { return Ok(false) };
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

    // Add_local_file happy path.
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
        assert!(!store::peer_has_receipt(&r.db.conn, rec.content.val.hash, f()).unwrap(), "no receipt from the peer");
        assert_eq!(log_rows(&r.db.conn), vec![row("item", "all"), row("receipt", "all")]);
        assert_eq!(r.actions(), vec![ActivityAction::Captured]);
        // Two shares of the same bytes: two items, one receipt row.
        let id2 = r.add("hello");
        assert_ne!(id, id2);
        assert_eq!(groups(&r.db.conn, "receipt").len(), 1, "no second receipt for the same hash");
    }

    // An exe renamed .jpg -> NotMedia, nothing stored anywhere. An exe under its own name is
    // simply a file now (any file type syncs) and never gets a photo kind.
    #[test]
    fn add_refuses_a_program_posing_as_a_photo_and_leaves_no_trace() {
        let mut r = Rig::new();
        let src = r.dir.path().join("evil.jpg");
        std::fs::write(&src, b"MZ\x90\0\x03\0\0\0\x04\0\0\0\xff\xff\0\0").unwrap();
        let now = r.now();
        let mk = |name: &str, src: &Path| NewLocalFile { src: src.to_path_buf(), name: name.into(), folder: None, captured_at: now, geo: None, thumb: None, how: AddKind::Added, ntfs: None };
        for name in ["evil.jpg", "EVIL.JPEG", "evil.png", "evil.mp4", "evil.heic"] {
            let e = r.run(|c, x| add_local_file(c, x, &mk(name, &src), None)).unwrap_err();
            assert!(matches!(e, CoreError::NotMedia(_)), "{name}: {e}");
        }
        assert_eq!(store::count_items(&r.db.conn).unwrap(), 0);
        assert_eq!(store::head_seq(&r.db.conn).unwrap(), 0);
        assert!(std::fs::read_dir(r.pl.root.join("items")).unwrap().next().is_none());
        // Missing source: an Io error, not a panic.
        let gone = r.dir.path().join("gone.jpg");
        assert!(matches!(r.run(|c, x| add_local_file(c, x, &mk("gone.jpg", &gone), None)), Err(CoreError::Io { .. })));
        // The same bytes under honest names are plain files.
        let id = r.run(|c, x| add_local_file(c, x, &mk("evil.exe", &src), None)).unwrap();
        assert_eq!((r.rec(id).content.val.kind, r.rec(id).content.val.ext.clone()), (MediaKind::File, "exe".to_string()));
        let id = r.run(|c, x| add_local_file(c, x, &mk("evil", &src), None)).unwrap();
        assert_eq!((r.rec(id).content.val.kind, r.rec(id).content.val.ext.clone()), (MediaKind::File, String::new()));
        // A photo with no extension in its name is NOT promoted any more: it stays a plain file.
        let ok = r.write_jpeg("noext");
        let id = r.run(|c, x| add_local_file(c, x, &mk("noext", &ok), None)).unwrap();
        assert_eq!(r.rec(id).name.val, "noext");
        assert_eq!(r.rec(id).content.val.kind, MediaKind::File);
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

    // A file adopted in place is the user's own: when the record cannot be written it stays, even for a
    // placement whose purge removes whatever file it is given.
    #[test]
    fn failed_record_write_keeps_a_file_adopted_in_place() {
        use crate::placement::{FileTarget, Placement};
        use std::path::{Path, PathBuf};
        struct InPlace(crate::placement::AppStorage);
        impl Placement for InPlace {
            fn part_path(&self, hash: &BlobHash) -> Result<PathBuf> {
                self.0.part_path(hash)
            }
            fn commit(&self, part: &Path, t: &FileTarget) -> Result<PathBuf> {
                self.0.commit(part, t)
            }
            fn adopt(&self, src: &Path, _t: &FileTarget) -> Result<PathBuf> {
                Ok(src.to_path_buf())
            }
            fn relocate(&self, t: &FileTarget) -> Result<PathBuf> {
                self.0.relocate(t)
            }
            fn to_bin(&self, t: &FileTarget) -> Result<PathBuf> {
                self.0.to_bin(t)
            }
            fn from_bin(&self, t: &FileTarget) -> Result<PathBuf> {
                self.0.from_bin(t)
            }
            fn purge(&self, t: &FileTarget) -> Result<()> {
                match &t.current {
                    Some(p) => std::fs::remove_file(p).map_err(|e| CoreError::io(p, e)),
                    None => Ok(()),
                }
            }
            fn available_space(&self) -> Result<u64> {
                self.0.available_space()
            }
        }
        let mut r = Rig::new();
        let own = r.write_jpeg("mine");
        let now = r.now();
        let pl = InPlace(crate::placement::AppStorage::new(r.dir.path().join("other")).unwrap());
        let ctx = Ctx { self_id: p(), now, placement: &pl };
        r.db.conn.execute_batch("CREATE TRIGGER boom BEFORE INSERT ON activity BEGIN SELECT RAISE(ABORT, 'boom'); END;").unwrap();
        let f = NewLocalFile { src: own.clone(), name: "mine.jpg".into(), folder: None, captured_at: now, geo: None, thumb: None, how: AddKind::Added, ntfs: None };
        assert!(add_local_file(&mut r.db.conn, &ctx, &f, None).is_err());
        assert!(own.exists(), "the user's own file was not deleted");
        assert_eq!(store::count_items(&r.db.conn).unwrap(), 0);
    }

    // One change_log row per written group.
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

    // Move resets entered_at; deleted/unknown target -> Invalid; same folder no-op.
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

    /// AppStorage whose relocate fails the first N times and counts every call.
    struct FlakyRelocate {
        inner: crate::placement::AppStorage,
        fail_left: std::sync::atomic::AtomicU32,
        calls: std::sync::atomic::AtomicU32,
    }

    impl crate::placement::Placement for FlakyRelocate {
        fn part_path(&self, h: &BlobHash) -> Result<std::path::PathBuf> {
            self.inner.part_path(h)
        }
        fn commit(&self, part: &Path, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.commit(part, t)
        }
        fn adopt(&self, src: &Path, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.adopt(src, t)
        }
        fn relocate(&self, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            use std::sync::atomic::Ordering::SeqCst;
            self.calls.fetch_add(1, SeqCst);
            if self.fail_left.load(SeqCst) > 0 {
                self.fail_left.fetch_sub(1, SeqCst);
                return Err(CoreError::Io { path: "x".into(), msg: "relocate failed (test)".into() });
            }
            self.inner.relocate(t)
        }
        fn to_bin(&self, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.to_bin(t)
        }
        fn from_bin(&self, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.from_bin(t)
        }
        fn purge(&self, t: &crate::placement::FileTarget) -> Result<()> {
            self.inner.purge(t)
        }
        fn available_space(&self) -> Result<u64> {
            self.inner.available_space()
        }
    }

    // A move or rename whose relocate failed is retried with the same arguments: the retry must
    // call relocate again instead of returning at the "nothing changed" check.
    #[test]
    fn identical_retry_after_a_failed_relocate_relocates() {
        use std::sync::atomic::Ordering::SeqCst;
        let mut r = Rig::new();
        let id = r.add("a");
        let fid = r.run(|c, x| create_folder(c, x, "Work", Retention::Keep)).unwrap();
        let flaky = FlakyRelocate { inner: r.pl.clone(), fail_left: 1.into(), calls: 0.into() };
        let ctx = Ctx { self_id: p(), now: r.now(), placement: &flaky };
        assert!(move_item(&mut r.db.conn, &ctx, id, fid).is_err(), "first try: relocate fails after the commit");
        assert_eq!(flaky.calls.load(SeqCst), 1);
        assert_eq!(r.rec(id).location.val.folder, fid, "the record change stands");
        move_item(&mut r.db.conn, &ctx, id, fid).unwrap();
        assert_eq!(flaky.calls.load(SeqCst), 2, "the identical retry relocated");
        assert!(r.path(id).is_some());
        // Same for rename.
        flaky.fail_left.store(1, SeqCst);
        assert!(rename_item(&mut r.db.conn, &ctx, id, "b").is_err());
        let calls = flaky.calls.load(SeqCst);
        rename_item(&mut r.db.conn, &ctx, id, "b").unwrap();
        assert_eq!(flaky.calls.load(SeqCst), calls + 1, "rename retry relocated");
    }

    // Delete of a pinned item writes pinned = false + deleted in one op.
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

    // A PC edit gets a new hash. Expiry does not wait for any receipt (0.2.0): the edit keeps
    // the item's due time, and the new hash simply has no receipt from the peer yet.
    #[test]
    fn content_change_does_not_hold_the_expiry() {
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
        assert_eq!(rec.content.val.kind, MediaKind::Image, "an edit never changes the kind");
        assert_eq!(rec.content.val.thumb, None);
        assert_eq!(&log_rows(&r.db.conn)[before..], &[row("item", "content"), row("receipt", "all")]);
        assert_eq!(*r.actions().first().unwrap(), ActivityAction::Edited);
        // The peer has no receipt for the new bytes, and the item still expires on time.
        assert!(!store::peer_has_receipt(&r.db.conn, rec.content.val.hash, f()).unwrap());
        // Same bytes again: no-op.
        let rec_before = r.rec(id);
        let n = log_rows(&r.db.conn).len();
        r.run(|c, x| content_changed(c, x, id, &path)).unwrap();
        assert_eq!(r.rec(id), rec_before);
        assert_eq!(log_rows(&r.db.conn).len(), n);
        // Ten days later it expired on time and the sweep bins it.
        r.clock.advance(10 * DAY_MS);
        assert!(matches!(state(&r, r.now()), ItemState::Expired { .. }));
        assert_eq!(r.sweep().to_bin, vec![id]);
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

    // 0.2.0: expiry never reads receipts. Through the whole stack: never paired, paired with no
    // receipt, a receipt from a stranger: the item is binned at its due time in every case.
    #[test]
    fn items_expire_on_time_whatever_the_receipts_say() {
        let dir = tempfile::tempdir().unwrap();
        let pl = crate::placement::AppStorage::new(dir.path().join("s")).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        store::ensure_self(&db.conn, p(), "PC", Role::Pc).unwrap();
        let t = T0;
        let src = dir.path().join("a.jpg");
        std::fs::write(&src, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]).unwrap();
        let ctx = Ctx { self_id: p(), now: t, placement: &pl };
        let id = add_local_file(&mut db.conn, &ctx, &NewLocalFile { src, name: "a.jpg".into(), folder: None, captured_at: t, geo: None, thumb: None, how: AddKind::Added, ntfs: None }, None).unwrap();
        let hash = store::get_item(&db.conn, id).unwrap().unwrap().0.content.val.hash;
        let state = |db: &Db, t| bin::derive_state(&db.conn, &store::get_item(&db.conn, id).unwrap().unwrap().0, t).unwrap();
        // Never paired: live before due, expired at due (it used to wait for ever).
        assert!(matches!(state(&db, t + 24 * HOUR_MS - 1), ItemState::Live { .. }));
        assert_eq!(state(&db, t + 24 * HOUR_MS), ItemState::Expired { binned_at: t + 24 * HOUR_MS });
        // A receipt from a device that was never paired changes nothing.
        store::insert_receipt(&db.conn, &ReceiptRecord { hash, device: f(), received_at: 1 }, t).unwrap();
        assert_eq!(state(&db, t + 24 * HOUR_MS), ItemState::Expired { binned_at: t + 24 * HOUR_MS });
        // Paired: still the same state, receipt or not.
        let peer = DeviceRow { id: f(), name: "Phone".into(), is_self: false, role: Role::Phone, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec![] };
        pairing_completed(&mut db.conn, &Ctx { now: t, ..ctx }, &peer).unwrap();
        assert_eq!(state(&db, t + 24 * HOUR_MS), ItemState::Expired { binned_at: t + 24 * HOUR_MS });
        // A year late: the sweep bins it at once; the Bin clock starts then.
        let late = t + 365 * DAY_MS;
        let rep = bin::sweep(&mut db.conn, &pl, p(), late).unwrap();
        assert_eq!(rep.to_bin, vec![id]);
        assert!(rep.purged.is_empty());
        // The new phone (re-paired with another key) has never confirmed any file: it is the
        // "waiting" and "never reached" logic that notices, not the expiry.
        let new_phone = DeviceRow { id: dev(0xCC), ..peer.clone() };
        pairing_completed(&mut db.conn, &Ctx { now: late, ..ctx }, &new_phone).unwrap();
        assert_eq!(bin::missing_on(&db.conn, hash).unwrap(), vec!["Phone".to_string()]);
    }

    // O1/O4/O7 for a file that is not a photo or video.
    #[test]
    fn any_file_type_is_added_renamed_and_edited_as_a_plain_file() {
        let mut r = Rig::new();
        let now = r.now();
        let mk = |name: &str, src: &Path, thumb: bool| NewLocalFile { src: src.to_path_buf(), name: name.into(), folder: None, captured_at: now, geo: None, thumb: thumb.then(|| src.to_path_buf()), how: AddKind::Added, ntfs: None };
        let pdf = r.dir.path().join("src-report");
        std::fs::write(&pdf, b"%PDF-1.7 body").unwrap();
        // A thumbnail passed for a File is not stored.
        let thumb = Thumb { hash: BlobHash([9; 32]), size: 5 };
        let id = r.run(|c, x| add_local_file(c, x, &mk("Report.PDF", &pdf, true), Some(thumb))).unwrap();
        let rec = r.rec(id);
        assert_eq!(rec.content.val.kind, MediaKind::File);
        assert_eq!(rec.content.val.ext, "pdf");
        assert_eq!(rec.name.val, "Report.pdf");
        assert_eq!(rec.content.val.thumb, None, "no thumbnail for a File");
        assert!(r.path(id).unwrap().exists());
        // No extension at all, even with JPEG bytes inside: a plain file, not promoted.
        let jpeg = r.write_jpeg("makefile-body");
        let id2 = r.run(|c, x| add_local_file(c, x, &mk("Makefile", &jpeg, false), None)).unwrap();
        let rec2 = r.rec(id2);
        assert_eq!((rec2.content.val.kind, rec2.content.val.ext.as_str(), rec2.name.val.as_str()), (MediaKind::File, "", "Makefile"));
        assert_eq!(r.path(id2).unwrap().file_name().unwrap().to_string_lossy(), id2.0.hyphenated().to_string(), "no dot on a file with no extension");
        // Rename keeps the (empty) extension; a result that is a refused name is refused.
        r.run(|c, x| rename_item(c, x, id2, "Notes")).unwrap();
        assert_eq!(r.rec(id2).name.val, "Notes");
        // ... and it cannot take a photo/video name: the file would be placed under it.
        for bad in ["holiday.jpg", "clip.MP4"] {
            assert!(matches!(r.run(|c, x| rename_item(c, x, id2, bad)), Err(CoreError::NotMedia(_))), "{bad}");
        }
        assert_eq!(r.rec(id2).name.val, "Notes");
        let ini = r.dir.path().join("src-ini");
        std::fs::write(&ini, b"[x]").unwrap();
        let id3 = r.run(|c, x| add_local_file(c, x, &mk("settings.ini", &ini, false), None)).unwrap();
        assert!(matches!(r.run(|c, x| rename_item(c, x, id3, "desktop")), Err(CoreError::Refused(_))));
        assert_eq!(r.rec(id3).name.val, "settings.ini");
        // Edit: a File stays a File whatever the new bytes look like.
        let path = r.path(id).unwrap();
        std::fs::write(&path, [0xFF, 0xD8, 0xFF, 0xE0, 7]).unwrap();
        r.run(|c, x| content_changed(c, x, id, &path)).unwrap();
        assert_eq!(r.rec(id).content.val.kind, MediaKind::File);
        // A zero-byte file is a file.
        let empty = r.dir.path().join("src-empty");
        std::fs::write(&empty, b"").unwrap();
        let id4 = r.run(|c, x| add_local_file(c, x, &mk("empty.txt", &empty, false), None)).unwrap();
        assert_eq!(r.rec(id4).content.val.size, 0);
    }

    // The always-refused names, before anything is read or written.
    #[test]
    fn refused_names_never_become_items() {
        let mut r = Rig::new();
        let src = r.dir.path().join("src-x");
        std::fs::write(&src, b"anything").unwrap();
        let now = r.now();
        let mk = |name: &str| NewLocalFile { src: src.clone(), name: name.into(), folder: None, captured_at: now, geo: None, thumb: None, how: AddKind::Added, ntfs: None };
        for name in ["x.lnk", "X.URL", "a.scf", "b.library-ms", "c.searchConnector-ms", "desktop.ini", "Desktop.Ini.", "autorun.inf ", "x.lnk::$DATA", "x.LNK. "] {
            let e = r.run(|c, x| add_local_file(c, x, &mk(name), None)).unwrap_err();
            assert!(matches!(&e, CoreError::Refused(t) if t == names::REFUSED_TEXT), "{name}: {e}");
            assert_eq!(e.kind(), "refused");
        }
        assert_eq!(store::count_items(&r.db.conn).unwrap(), 0);
        assert_eq!(store::head_seq(&r.db.conn).unwrap(), 0, "nothing logged");
        assert!(std::fs::read_dir(r.pl.root.join("items")).unwrap().next().is_none());
        // A missing source with a refused name is still "refused", not an io error: no read happens.
        let gone = r.dir.path().join("gone.lnk");
        assert!(matches!(r.run(|c, x| add_local_file(c, x, &mk_at(&gone, "gone.lnk", now), None)), Err(CoreError::Refused(_))));
    }

    fn mk_at(src: &Path, name: &str, now: i64) -> NewLocalFile {
        NewLocalFile { src: src.to_path_buf(), name: name.into(), folder: None, captured_at: now, geo: None, thumb: None, how: AddKind::Added, ntfs: None }
    }

    // A file above Settings.max_blob_bytes is refused here, once, with a one-line reason.
    #[test]
    fn a_file_over_the_size_limit_is_refused_and_leaves_no_trace() {
        let mut r = Rig::new();
        store::save_settings(&r.db.conn, &Settings { max_blob_bytes: 1024 * 1024 * 1024, ..Settings::default() }).unwrap();
        let src = r.dir.path().join("big.bin");
        let f = std::fs::File::create(&src).unwrap();
        f.set_len(1024 * 1024 * 1024 + 1).unwrap();
        drop(f);
        let now = r.now();
        let e = r.run(|c, x| add_local_file(c, x, &mk_at(&src, "big.bin", now), None)).unwrap_err();
        assert!(matches!(&e, CoreError::Refused(t) if t == "TOO LARGE - over 1 GB"), "{e}");
        assert_eq!(store::count_items(&r.db.conn).unwrap(), 0);
        assert_eq!(too_large_text(16 * 1024 * 1024 * 1024), "TOO LARGE - over 16 GB");
        assert_eq!(too_large_text(500 * 1024 * 1024), "TOO LARGE - over 500 MB");
    }

    // Deleting a folder bins its items (files follow), refuses default folders.
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

    // O10/O11: one phone per PC, new peer = snapshot, unpair trims log and cursor, files stay.
    #[test]
    fn pairing_and_unpairing() {
        let mut r = Rig::new(); // f() is already paired
        let id = r.add("a");
        store::set_pulled(&r.db.conn, f(), 5).unwrap();
        assert!(store::head_seq(&r.db.conn).unwrap() > 0);
        let new_phone = DeviceRow { id: dev(0xCC), name: "New phone".into(), is_self: false, role: Role::Phone, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec!["10.0.0.5:5000".into()] };
        r.run(|c, x| pairing_completed(c, x, &new_phone)).unwrap();
        assert!(!store::is_paired(&r.db.conn, f()).unwrap(), "previous phone unpaired first");
        assert!(store::is_paired(&r.db.conn, dev(0xCC)).unwrap());
        assert_eq!(store::get_cursor(&r.db.conn, f()).unwrap(), None);
        assert_eq!(store::get_cursor(&r.db.conn, dev(0xCC)).unwrap(), None, "new peer starts with a full snapshot");
        assert!(store::changes_since(&r.db.conn, 0, 100, dev(9)).unwrap().0.is_empty(), "no peer left to keep rows for: log cleared");
        assert_eq!(store::log_floor(&r.db.conn).unwrap(), store::head_seq(&r.db.conn).unwrap());
        let acts = r.actions();
        assert!(acts.contains(&ActivityAction::Paired) && acts.contains(&ActivityAction::Unpaired));
        assert_eq!(r.lfs(id), LocalFileState::Present, "files stay");
        // A stale cursor for the new key is dropped on pairing.
        store::set_pulled(&r.db.conn, dev(0xCC), 9).unwrap();
        r.run(|c, x| pairing_completed(c, x, &new_phone)).unwrap();
        assert_eq!(store::get_cursor(&r.db.conn, dev(0xCC)).unwrap(), None);
        // Explicit unpair.
        r.run(|c, x| unpair(c, x, dev(0xCC), false)).unwrap();
        assert!(store::paired_peer(&r.db.conn).unwrap().is_none());
        assert!(matches!(r.run(|c, x| unpair(c, x, dev(0xDD), false)), Err(CoreError::NotFound { .. })));
        assert_eq!(r.lfs(id), LocalFileState::Present);
    }

    fn pc_row(id: DeviceId, name: &str) -> DeviceRow {
        DeviceRow { id, name: name.into(), is_self: false, role: Role::Pc, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec![format!("10.0.0.{}:4000", id.0[0])] }
    }

    // O10 phone side: a second PC is linked beside the first. Nothing is unpaired, nothing owed.
    #[test]
    fn phone_links_second_pc_keeps_first() {
        let mut r = Rig::new(); // f() is already paired
        store::set_pulled(&r.db.conn, f(), 5).unwrap();
        let other = pc_row(dev(0xCC), "Other PC");
        r.run(|c, x| pairing_completed_phone(c, x, &other)).unwrap();
        assert!(store::is_paired(&r.db.conn, f()).unwrap(), "the first PC stays linked");
        assert!(store::is_paired(&r.db.conn, dev(0xCC)).unwrap());
        assert_eq!(store::paired_peers(&r.db.conn).unwrap().len(), 2);
        assert_eq!(store::get_cursor(&r.db.conn, f()).unwrap(), Some((5, 0)), "its position is untouched");
        assert_eq!(store::get_cursor(&r.db.conn, dev(0xCC)).unwrap(), None, "the new PC starts with a full snapshot");
        assert!(store::pending_unpairs(&r.db.conn).unwrap().is_empty(), "no notice: nobody was replaced");
        assert!(!r.actions().contains(&ActivityAction::Unpaired));
        // The PC side keeps its one-phone rule.
        let mut pc = Rig::new();
        pc.run(|c, x| pairing_completed(c, x, &other)).unwrap();
        assert!(!store::is_paired(&pc.db.conn, f()).unwrap());
    }

    // O10 phone side: the limit is checked inside the transaction. The PC that was turned away
    // already saved its link, so it is stored as removed and owed a notice.
    #[test]
    fn over_limit_in_tx_owes_notice() {
        let mut r = Rig::new(); // f() is the first of MAX_LINKED
        for n in 1..MAX_LINKED as u8 {
            r.run(|c, x| pairing_completed_phone(c, x, &pc_row(dev(0xC0 + n), "PC"))).unwrap();
        }
        assert_eq!(store::paired_peers(&r.db.conn).unwrap().len(), MAX_LINKED as usize);
        let extra = pc_row(dev(0xEE), "One too many");
        let paired_events = r.actions().iter().filter(|a| **a == ActivityAction::Paired).count();
        assert!(matches!(r.run(|c, x| pairing_completed_phone(c, x, &extra)), Err(CoreError::TooManyDevices)));
        assert!(!store::is_paired(&r.db.conn, extra.id).unwrap());
        assert_eq!(store::paired_peers(&r.db.conn).unwrap().len(), MAX_LINKED as usize, "the others are untouched");
        let row = store::get_device(&r.db.conn, extra.id).unwrap().expect("kept as a removed device");
        assert!(row.paired_at.is_some() && row.unpaired_at.is_some());
        let owed = store::pending_unpairs(&r.db.conn).unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].peer_id().unwrap(), extra.id);
        assert_eq!(owed[0].addresses, extra.addresses);
        assert_eq!(r.actions().iter().filter(|a| **a == ActivityAction::Paired).count(), paired_events, "no Linked row for it");
        // Room again after one is unlinked, and linking the owed PC then voids its notice.
        r.run(|c, x| unpair(c, x, f(), false)).unwrap();
        r.run(|c, x| pairing_completed_phone(c, x, &extra)).unwrap();
        assert!(store::is_paired(&r.db.conn, extra.id).unwrap());
        assert!(store::pending_unpairs(&r.db.conn).unwrap().is_empty(), "linked again: the notice is void");
    }

    // A PC that is linked already links again (new scan after a reinstall): not counted twice,
    // name and addresses refreshed, a fresh snapshot.
    #[test]
    fn relink_same_pc_not_counted() {
        let mut r = Rig::new();
        for n in 1..MAX_LINKED as u8 {
            r.run(|c, x| pairing_completed_phone(c, x, &pc_row(dev(0xC0 + n), "PC"))).unwrap();
        }
        assert_eq!(store::paired_peers(&r.db.conn).unwrap().len(), MAX_LINKED as usize);
        store::set_pulled(&r.db.conn, dev(0xC1), 7).unwrap();
        let again = DeviceRow { name: "Renamed".into(), addresses: vec!["10.9.9.9:1".into()], ..pc_row(dev(0xC1), "PC") };
        r.run(|c, x| pairing_completed_phone(c, x, &again)).unwrap();
        let list = store::paired_peers(&r.db.conn).unwrap();
        assert_eq!(list.len(), MAX_LINKED as usize);
        let row = list.iter().find(|d| d.id == dev(0xC1)).unwrap();
        assert_eq!((row.name.as_str(), row.addresses.clone()), ("Renamed", vec!["10.9.9.9:1".to_string()]));
        assert_eq!(store::get_cursor(&r.db.conn, dev(0xC1)).unwrap(), None, "starts over with a snapshot");
        assert!(store::pending_unpairs(&r.db.conn).unwrap().is_empty());
    }

    // O11: unlinking one PC leaves the log rows another PC has not had yet; the last one out empties it.
    #[test]
    fn unpair_keeps_rows_other_pc_needs() {
        let mut r = Rig::new(); // f() is already paired
        let other = pc_row(dev(0xCC), "Other PC");
        r.run(|c, x| pairing_completed_phone(c, x, &other)).unwrap();
        for n in ["a", "b", "c"] {
            r.add(n);
        }
        let head = store::head_seq(&r.db.conn).unwrap();
        assert!(head > 2);
        store::set_pulled(&r.db.conn, f(), 0).unwrap();
        store::set_acked(&r.db.conn, f(), head).unwrap();
        store::set_pulled(&r.db.conn, other.id, 0).unwrap();
        store::set_acked(&r.db.conn, other.id, 1).unwrap();
        r.run(|c, x| unpair(c, x, f(), true)).unwrap();
        assert_eq!(store::log_floor(&r.db.conn).unwrap(), 1, "only what the other PC has acked is trimmed");
        let (keys, _) = store::changes_since(&r.db.conn, 1, 100, dev(9)).unwrap();
        assert!(!keys.is_empty(), "the rows the other PC still needs are there");
        let unpaired = crate::activity::list(&r.db.conn, None, 500).unwrap().into_iter().find(|a| a.action == ActivityAction::Unpaired).unwrap();
        assert_eq!(unpaired.detail, "by the other device");
        r.run(|c, x| unpair(c, x, other.id, false)).unwrap();
        assert!(store::changes_since(&r.db.conn, 0, 100, dev(9)).unwrap().0.is_empty());
        assert_eq!(store::log_floor(&r.db.conn).unwrap(), head);
        let last = crate::activity::list(&r.db.conn, None, 500).unwrap().into_iter().find(|a| a.action == ActivityAction::Unpaired && a.device == Some(other.id)).unwrap();
        assert_eq!(last.detail, "");
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

    // O6b: Delete forever writes the purged group once for the whole call, removes the file, queues the
    // thumbnail and logs one row; a repeat does nothing; every other op treats the item as gone.
    #[test]
    fn purge_items_deletes_forever_and_is_idempotent() {
        let mut r = Rig::new();
        let id = r.add("gone");
        let held = r.path(id).unwrap();
        // Not in the Bin: refused, nothing written.
        let head = store::head_seq(&r.db.conn).unwrap();
        assert!(matches!(r.run(|c, x| purge_items(c, x, &[id])), Err(CoreError::Invalid(_))));
        assert_eq!(store::head_seq(&r.db.conn).unwrap(), head);
        assert!(r.rec(id).purged.is_none());
        // Deleted: now it can go.
        r.run(|c, x| delete_item(c, x, id)).unwrap();
        let in_bin = r.path(id).unwrap();
        assert!(in_bin.exists());
        r.clock.advance(5_000);
        let rows_before = log_rows(&r.db.conn).len();
        assert_eq!(r.run(|c, x| purge_items(c, x, &[id])).unwrap(), 1);
        let purged = r.rec(id).purged.expect("purged is set");
        assert_eq!((purged.v.device, purged.val), (p(), r.now()));
        assert!(r.rec(id).name.val.is_empty() && r.rec(id).geo.is_none() && r.rec(id).content.val.thumb.is_none(), "no name, location or thumbnail is kept");
        assert!(purged.v.lamport > r.rec(id).deleted.v.lamport, "a new write, after the delete");
        assert_eq!(groups(&r.db.conn, "item").last().map(String::as_str), Some("purged"));
        assert_eq!(log_rows(&r.db.conn).len(), rows_before + 1);
        assert!(!in_bin.exists() && !held.exists(), "no copy is left anywhere");
        assert!(r.path(id).is_none());
        assert_eq!(r.lfs(id), LocalFileState::Purged);
        let row = activity::list(&r.db.conn, None, 1).unwrap().remove(0);
        assert_eq!((row.action, row.device, row.item, row.detail.as_str()), (ActivityAction::DeletedForever, Some(p()), Some(id), "Inbox"));
        // Again: nothing written, nothing counted. An id that never existed is NotFound.
        let head = store::head_seq(&r.db.conn).unwrap();
        assert_eq!(r.run(|c, x| purge_items(c, x, &[id])).unwrap(), 0);
        assert_eq!(store::head_seq(&r.db.conn).unwrap(), head);
        assert!(matches!(r.run(|c, x| purge_items(c, x, &[ItemId(Uuid::from_u128(1))])), Err(CoreError::NotFound { .. })));
        // Gone for every other op, and never restorable.
        assert!(matches!(r.run(|c, x| restore_item(c, x, id)), Err(CoreError::NotFound { .. })));
        assert!(matches!(r.run(|c, x| set_pinned(c, x, id, true)), Err(CoreError::NotFound { .. })));
        assert!(matches!(r.run(|c, x| move_item(c, x, id, KEEP_ID)), Err(CoreError::NotFound { .. })));
        assert!(matches!(r.run(|c, x| rename_item(c, x, id, "x")), Err(CoreError::NotFound { .. })));
        assert!(matches!(r.run(|c, x| delete_item(c, x, id)), Err(CoreError::NotFound { .. })));
        assert!(r.rec(id).deleted.val.is_some() && r.rec(id).purged.is_some());
    }

    // Several items in one call share one version; one that is not in the Bin stops the whole call.
    #[test]
    fn purge_items_takes_one_version_and_all_or_nothing() {
        let mut r = Rig::new();
        let ids: Vec<ItemId> = ["a", "b", "c"].iter().map(|n| r.add(n)).collect();
        let live = r.add("live");
        for id in &ids {
            r.run(|c, x| delete_item(c, x, *id)).unwrap();
        }
        // The live item in the list: nothing is purged, not even the ones before it.
        let mut with_live = ids.clone();
        with_live.push(live);
        assert!(matches!(r.run(|c, x| purge_items(c, x, &with_live)), Err(CoreError::Invalid(_))));
        assert!(ids.iter().all(|id| r.rec(*id).purged.is_none()), "rolled back as a whole");
        assert_eq!(r.run(|c, x| purge_items(c, x, &ids)).unwrap(), 3);
        let versions: Vec<Version> = ids.iter().map(|id| r.rec(*id).purged.unwrap().v).collect();
        assert!(versions.windows(2).all(|w| w[0] == w[1]), "one write, one version: {versions:?}");
        assert_eq!(groups(&r.db.conn, "item").iter().filter(|g| *g == "purged").count(), 3);
        assert!(r.rec(live).purged.is_none() && r.path(live).unwrap().exists(), "the live item is untouched");
    }

    // An expired item (not deleted) is in the Bin too and can be deleted forever.
    #[test]
    fn an_expired_item_can_be_deleted_forever() {
        let mut r = Rig::new();
        let id = r.add("old");
        r.clock.advance(24 * HOUR_MS);
        r.sweep();
        assert_eq!(r.lfs(id), LocalFileState::Binned);
        assert_eq!(r.run(|c, x| purge_items(c, x, &[id])).unwrap(), 1);
        assert_eq!(r.lfs(id), LocalFileState::Purged);
        assert!(r.rec(id).deleted.val.is_none(), "expiry stays derived; the purge is its own group");
    }

    // The thumbnail is queued; one a live item still uses stays on disk.
    #[test]
    fn purging_queues_the_thumbnail_unless_a_live_item_uses_it() {
        let mut r = Rig::new();
        let th = Thumb { hash: hash(0x44), size: 10 };
        let add = |r: &mut Rig, name: &str| {
            let src = r.write_jpeg(name);
            let now = r.now();
            r.run(|c, x| {
                add_local_file(c, x, &NewLocalFile { src, name: format!("{name}.jpg"), folder: None, captured_at: now, geo: None, thumb: None, how: AddKind::Added, ntfs: None }, Some(th.clone()))
            })
            .unwrap()
        };
        let (a, b) = (add(&mut r, "one"), add(&mut r, "two"));
        r.run(|c, x| delete_item(c, x, a)).unwrap();
        r.run(|c, x| purge_items(c, x, &[a])).unwrap();
        assert!(store::thumbs_to_delete(&r.db.conn).unwrap().is_empty(), "item two still uses this thumbnail");
        r.run(|c, x| delete_item(c, x, b)).unwrap();
        r.run(|c, x| purge_items(c, x, &[b])).unwrap();
        assert_eq!(store::thumbs_to_delete(&r.db.conn).unwrap(), vec![th.hash], "now nobody does");
        store::thumb_deleted(&r.db.conn, th.hash).unwrap();
        assert!(store::thumbs_to_delete(&r.db.conn).unwrap().is_empty());
    }

    // Views that count or list items leave a deleted-forever one out, even before the sweep ran.
    #[test]
    fn a_purged_item_is_in_no_count_or_list() {
        let mut r = Rig::new();
        let kept = r.add("kept");
        let gone = r.add("gone");
        r.run(|c, x| delete_item(c, x, gone)).unwrap();
        r.run(|c, x| purge_items(c, x, &[gone])).unwrap();
        let now = r.now();
        let inbox = |r: &Rig| folders::folder_views(&r.db.conn, now, false).unwrap().into_iter().find(|f| f.id == INBOX_ID).unwrap().count;
        assert_eq!(inbox(&r), 1);
        assert_eq!(store::items_page(&r.db.conn, None, None, 10).unwrap().into_iter().map(|(i, _)| i.id).collect::<Vec<_>>(), vec![kept]);
        assert_eq!(store::item_ids_in_folder(&r.db.conn, INBOX_ID).unwrap(), vec![kept]);
        assert!(bin::bin_ids(&r.db.conn, now).unwrap().is_empty());
        assert_eq!(folders::retention_preview(&r.db.conn, INBOX_ID, Retention::Secs { secs: 1 }, now + 5_000).unwrap(), 1);
        // A purged record that is still Pending on this device (the sweep has not run): not waiting, not picked.
        let mut late = item(7, f(), v(3, f()));
        late.purged = Some(Versioned { v: v(9, f()), val: T0 });
        store::insert_item(&r.db.conn, &late, LocalFileState::Pending, T0).unwrap();
        assert_eq!(store::pending_counts(&r.db.conn, &[f()]).unwrap(), (0, vec![0]));
        assert!(!store::pending_held_by(&r.db.conn, f()).unwrap());
        assert!(store::items_in_state(&r.db.conn, &[LocalFileState::Pending], 10).unwrap().is_empty());
        assert!(store::items_missing_receipts(&r.db.conn, &[f()]).unwrap().iter().all(|(rec, _)| rec.id != late.id));
    }

    // A result for a download that was running when the item was deleted forever is not an error, and marks nothing.
    #[test]
    fn download_results_for_a_purged_item_mark_nothing() {
        let mut r = Rig::new();
        let mut it = item(5, f(), v(3, f()));
        it.deleted.val = Some(T0);
        it.purged = Some(Versioned { v: v(9, f()), val: T0 });
        store::insert_item(&r.db.conn, &it, LocalFileState::Pending, T0).unwrap();
        assert!(!r.run(|c, x| blob_lost(c, x, it.id, it.content.val.hash)).unwrap());
        assert!(!r.run(|c, x| blob_refused(c, x, it.id, it.content.val.hash, "no")).unwrap());
        assert_eq!(r.lfs(it.id), LocalFileState::Pending);
    }
}
