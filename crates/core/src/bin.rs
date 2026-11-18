//! Bin and purge: makes the files this device holds match the derived state. Owner: logic.
//!
//! PLAN: "Expired and deleted items sit in the Bin for 7 days (setting), then that device
//! deletes its copy of the file. The small record stays forever as a tombstone."
//!
//! `sweep` rules, per item, with state = expiry::item_state(..) and local = LocalFileState:
//! | state      | local            | action                                              | activity |
//! |------------|------------------|-----------------------------------------------------|----------|
//! | in Bin     | Present          | placement.to_bin, local = Binned, file_ref = Bin path | Expired (state Expired) / Binned (Deleted by a peer version) / none (Deleted by self: logged at delete time) |
//! | in Bin, now >= purge_at | Binned (a Present file is binned first: bin_days >= 1) | placement.purge, local = Purged, file_ref deleted | Purged |
//! | in Bin     | Pending          | local = Purged (never download Bin items)           | none |
//! | not in Bin | Binned           | placement.from_bin, local = Present                 | Returned |
//! | not in Bin | Purged           | local = Pending (sync will fetch)                   | none |
//! | otherwise  | -                | nothing                                             | - |
//! - THE BIN CLOCK IS LOCAL. purge_at = (when THIS device moved the file into its Bin, i.e. the
//!   `local_state_at` of a Binned item) + bin_days. It is never derived from synced times
//!   (`entered_at`, `deleted_at`, receipt time): those are other devices' clocks and old values
//!   (an un-Keep of a 10-day-old item, a retention cut, a delete made while this device was
//!   away) would otherwise purge a file with zero Bin time. A file therefore always gets the
//!   full Bin days on every device that holds it, counted from the moment it entered that
//!   device's Bin.
//! - A placement error on one item is recorded (Event::Error via the report) and the sweep
//!   continues with the next item; the item's local state is NOT changed, so the next sweep
//!   retries. Never mark Purged unless the file is really gone.
//! - A Pending item that still has a `file_ref` holds the OLD content (its content changed
//!   on the peer): when such an item is in the Bin, the stale file is purged first.
//! - Also trims activity older than activity::KEEP_DAYS.
//! - Runs: at Core open, after every applied sync page, after every local op that can move
//!   an item in or out of the Bin, and at `next_due` (the facade schedules it; minimum every
//!   60 s as a safety net against clock jumps).

use std::collections::HashMap;

use rusqlite::{Connection, TransactionBehavior};

use crate::{
    activity,
    expiry::{self, ExpiryCtx},
    folders, model::*,
    placement::{FileTarget, Placement},
    store, CoreError, Result,
};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SweepReport {
    pub to_bin: Vec<ItemId>,
    pub returned: Vec<ItemId>,
    pub purged: Vec<ItemId>,
    /// Items that became Pending (need a download).
    pub want: Vec<ItemId>,
    /// (item, error text) for placement failures; the facade emits Event::Error for each.
    pub errors: Vec<(ItemId, String)>,
    /// Earliest future time a state changes by time alone; None = nothing pending.
    pub next_due: Option<i64>,
}

/// When this device purges the file of an item that is in the Bin. A Binned file counts from
/// the moment it entered this device's Bin (`state_at`); a file still Present counts from now,
/// because it is about to enter the Bin. Synced times never enter into it (module doc).
pub fn local_purge_at(lfs: LocalFileState, state_at: i64, now: i64, bin_days: u32) -> i64 {
    let start = if lfs == LocalFileState::Binned { state_at } else { now };
    start.saturating_add(bin_days as i64 * crate::clock::DAY_MS)
}

/// Derived state of an item given the folder table (preloaded by the caller for batches).
pub fn derive_state_in(c: &Connection, folders: &HashMap<FolderId, FolderRecord>, rec: &ItemRecord, now: i64) -> Result<ItemState> {
    let receipt = store::peer_receipt_seen_at(c, rec.content.val.hash)?;
    Ok(expiry::item_state(
        rec,
        ExpiryCtx { folder: folders.get(&rec.location.val.folder), peer_receipt_seen_at: receipt, now_ms: now },
    ))
}

/// Derived state of one item (loads its folder and receipt).
pub fn derive_state(c: &Connection, rec: &ItemRecord, now: i64) -> Result<ItemState> {
    let folder = store::get_folder(c, rec.location.val.folder)?;
    let receipt = store::peer_receipt_seen_at(c, rec.content.val.hash)?;
    Ok(expiry::item_state(rec, ExpiryCtx { folder: folder.as_ref(), peer_receipt_seen_at: receipt, now_ms: now }))
}

/// What a placement needs to know about an item: effective folder (E2) and current path.
pub fn file_target(c: &Connection, rec: &ItemRecord) -> Result<FileTarget> {
    let folder = folders::effective_folder(c, rec.location.val.folder)?;
    let current = store::get_file_ref(c, rec.id)?.map(|r| std::path::PathBuf::from(r.path));
    Ok(FileTarget {
        item: rec.id,
        hash: rec.content.val.hash,
        name: rec.name.val.clone(),
        ext: rec.content.val.ext.clone(),
        captured_at: rec.captured_at,
        folder: folder.id,
        folder_kind: folder.kind,
        folder_name: folder.name.val,
        current,
    })
}

/// Keep `file_ref` in step with a new path. Bin files carry no NTFS identity.
fn set_path(c: &Connection, item: ItemId, path: &std::path::Path, keep_ntfs: bool) -> Result<()> {
    let old = store::get_file_ref(c, item)?;
    let (volume_serial, file_id) = match (&old, keep_ntfs) {
        (Some(o), true) => (o.volume_serial, o.file_id),
        _ => (None, None),
    };
    store::put_file_ref(c, &store::FileRef { item, path: path.to_string_lossy().into_owned(), volume_serial, file_id })
}

struct Env<'a> {
    placement: &'a dyn Placement,
    self_id: DeviceId,
    now: i64,
    bin_days: u32,
    folders: HashMap<FolderId, FolderRecord>,
}

fn load_env<'a>(c: &Connection, placement: &'a dyn Placement, self_id: DeviceId, now: i64) -> Result<Env<'a>> {
    Ok(Env {
        placement,
        self_id,
        now,
        bin_days: store::load_settings(c)?.bin_days,
        folders: store::all_folders(c)?.into_iter().map(|f| (f.id, f)).collect(),
    })
}

/// One item, rules in the module table. Placement and logic errors are returned; the item's
/// local state is unchanged when the file move failed. `quiet` = no Returned/Expired/Binned
/// activity (the op that caused the move already logged what the user did).
fn sweep_one(c: &mut Connection, env: &Env<'_>, id: ItemId, quiet: bool, rep: &mut SweepReport) -> Result<()> {
    let Some((rec, lfs)) = store::get_item(c, id)? else { return Ok(()) };
    let state = derive_state_in(c, &env.folders, &rec, env.now)?;
    let inbin = expiry::in_bin(state);
    let state_at = store::local_state_at(c, id)?.unwrap_or(env.now);
    let purge_at = local_purge_at(lfs, state_at, env.now, env.bin_days);
    let target = file_target(c, &rec)?;
    let deleted_by_peer = rec.deleted.v.device != env.self_id;
    let mut state_after = lfs;
    // Start of the Bin clock for the next-due calculation below.
    let mut bin_start = if lfs == LocalFileState::Binned { state_at } else { env.now };

    match lfs {
        LocalFileState::Present | LocalFileState::Binned if inbin => {
            if env.now >= purge_at {
                // Purge: the file goes, the record stays as a tombstone.
                env.placement.purge(&target)?;
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                store::set_local_state(&tx, id, LocalFileState::Purged, env.now)?;
                store::delete_file_ref(&tx, id)?;
                activity::log(&tx, env.now, Some(env.self_id), Some(id), ActivityAction::Purged, &target.folder_name)?;
                tx.commit()?;
                rep.purged.push(id);
                state_after = LocalFileState::Purged;
            } else if lfs == LocalFileState::Present {
                let path = env.placement.to_bin(&target)?;
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                store::set_local_state(&tx, id, LocalFileState::Binned, env.now)?;
                set_path(&tx, id, &path, false)?;
                if !quiet {
                    match state {
                        ItemState::Expired { .. } => {
                            activity::log(&tx, env.now, Some(env.self_id), Some(id), ActivityAction::Expired, &target.folder_name)?;
                        }
                        ItemState::Deleted { .. } if deleted_by_peer => {
                            activity::log(&tx, env.now, Some(rec.deleted.v.device), Some(id), ActivityAction::Binned, "deleted on the other device")?;
                        }
                        _ => {}
                    }
                }
                tx.commit()?;
                rep.to_bin.push(id);
                state_after = LocalFileState::Binned;
                bin_start = env.now;
            }
        }
        LocalFileState::Pending if inbin => {
            // Never download Bin items. A file_ref here means the held file is the old content.
            if store::get_file_ref(c, id)?.is_some() {
                env.placement.purge(&target)?;
            }
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            store::set_local_state(&tx, id, LocalFileState::Purged, env.now)?;
            store::delete_file_ref(&tx, id)?;
            tx.commit()?;
            state_after = LocalFileState::Purged;
        }
        LocalFileState::Binned if store::get_file_ref(c, id)?.is_none() => {
            // Not in the Bin any more, but the file is gone from this device (deleted in Explorer,
            // see Core::local_files_missing): there is nothing to move back, so fetch it.
            store::set_local_state(c, id, LocalFileState::Pending, env.now)?;
            rep.want.push(id);
            state_after = LocalFileState::Pending;
        }
        LocalFileState::Binned => {
            // Not in the Bin any more (a synced Keep or move undid the expiry).
            let path = env.placement.from_bin(&target)?;
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            store::set_local_state(&tx, id, LocalFileState::Present, env.now)?;
            set_path(&tx, id, &path, false)?;
            if !quiet {
                activity::log(&tx, env.now, Some(env.self_id), Some(id), ActivityAction::Returned, &target.folder_name)?;
            }
            tx.commit()?;
            rep.returned.push(id);
            state_after = LocalFileState::Present;
        }
        LocalFileState::Purged if !inbin => {
            store::set_local_state(c, id, LocalFileState::Pending, env.now)?;
            rep.want.push(id);
            state_after = LocalFileState::Pending;
        }
        _ => {}
    }

    // Next time-driven change, for items that still hold a file or will.
    if !matches!(state_after, LocalFileState::Purged | LocalFileState::Lost | LocalFileState::Refused) {
        let next = match state {
            ItemState::Live { expires_at } => Some(expires_at),
            s if expiry::in_bin(s) => Some(bin_start.saturating_add(env.bin_days as i64 * crate::clock::DAY_MS)),
            _ => None,
        };
        if let Some(t) = next {
            if t > env.now {
                rep.next_due = Some(rep.next_due.map_or(t, |d| d.min(t)));
            }
        }
    }
    Ok(())
}

/// Candidate items: everything that may need a file action. Purged items are only candidates
/// when they could have left the Bin (not user-deleted, or carrying a pin), so the 100k
/// tombstones of a long-lived library are not re-derived every minute.
const SWEEP_CANDIDATES: &str =
    "local_file_state IN ('present', 'binned', 'pending') OR (local_file_state = 'purged' AND (deleted_at IS NULL OR pinned = 1))";

/// One full pass (rules above) inside one transaction per item.
pub fn sweep(c: &mut Connection, placement: &dyn Placement, self_id: DeviceId, now: i64) -> Result<SweepReport> {
    let env = load_env(c, placement, self_id, now)?;
    let mut rep = SweepReport::default();
    for id in store::item_ids_where(c, SWEEP_CANDIDATES)? {
        match sweep_one(c, &env, id, false, &mut rep) {
            Ok(()) => {}
            // A database failure is not an item problem: stop and report it.
            Err(e @ CoreError::Db(_)) => return Err(e),
            Err(e) => rep.errors.push((id, e.to_string())),
        }
    }
    activity::trim(c, now)?;
    Ok(rep)
}

/// Sweep only `ids` (the items a sync page touched), with the same rules and the same activity
/// rows as `sweep`. Added by the sync builder: a full sweep per received page is O(library)
/// and made a first sync of a big library quadratic.
pub fn sweep_items(c: &mut Connection, placement: &dyn Placement, self_id: DeviceId, ids: &[ItemId], now: i64) -> Result<SweepReport> {
    let env = load_env(c, placement, self_id, now)?;
    let mut rep = SweepReport::default();
    for id in ids {
        match sweep_one(c, &env, *id, false, &mut rep) {
            Ok(()) => {}
            Err(e @ CoreError::Db(_)) => return Err(e),
            Err(e) => rep.errors.push((*id, e.to_string())),
        }
    }
    Ok(rep)
}

/// Sweep one item right after a local op moved it in or out of the Bin. Returns the first
/// placement error (the record change stands; the next full sweep retries).
pub fn sweep_item(c: &mut Connection, placement: &dyn Placement, self_id: DeviceId, id: ItemId, now: i64) -> Result<()> {
    let env = load_env(c, placement, self_id, now)?;
    sweep_one(c, &env, id, true, &mut SweepReport::default())
}

/// Items currently in the Bin, newest binned_at first, keyset paged by item id.
/// Items past their purge time are not listed (the file is gone; only the tombstone remains).
pub fn bin_page(c: &Connection, after: Option<ItemId>, limit: u32, now: i64) -> Result<Vec<(ItemRecord, ItemState, LocalFileState)>> {
    let bin_days = store::load_settings(c)?.bin_days;
    let folders: HashMap<FolderId, FolderRecord> = store::all_folders(c)?.into_iter().map(|f| (f.id, f)).collect();
    let mut rows: Vec<(ItemRecord, ItemState, LocalFileState)> = Vec::new();
    // Purged-and-still-in-Bin items are the common tombstones; skip them in SQL.
    for id in store::item_ids_where(c, "local_file_state != 'purged'")? {
        let Some((rec, lfs)) = store::get_item(c, id)? else { continue };
        let state = derive_state_in(c, &folders, &rec, now)?;
        let state_at = store::local_state_at(c, id)?.unwrap_or(now);
        if expiry::in_bin(state) && now < local_purge_at(lfs, state_at, now, bin_days) {
            rows.push((rec, state, lfs));
        }
    }
    let binned_at = |s: &ItemState| match s {
        ItemState::Deleted { binned_at } | ItemState::Expired { binned_at } => *binned_at,
        _ => 0,
    };
    rows.sort_by(|a, b| binned_at(&b.1).cmp(&binned_at(&a.1)).then_with(|| b.0.id.cmp(&a.0.id)));
    let start = match after {
        Some(a) => rows.iter().position(|r| r.0.id == a).map(|i| i + 1).unwrap_or(rows.len()),
        None => 0,
    };
    Ok(rows.into_iter().skip(start).take(limit.clamp(1, 500) as usize).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, DAY_MS, HOUR_MS};
    use crate::ops;
    use crate::store::test_support::*;

    // L14: expiry -> Bin -> purge at exactly 7 days (not 6.99), tombstone stays (L15).
    #[test]
    fn expire_bin_purge_timeline_and_tombstone() {
        let mut r = Rig::new();
        let id = r.add("a");
        assert_eq!(r.lfs(id), LocalFileState::Present);
        let held = r.path(id).unwrap();
        assert!(held.exists());
        // No receipt yet: Waiting, nothing happens however late.
        r.clock.advance(10 * DAY_MS);
        let rep = r.sweep();
        assert!(rep.to_bin.is_empty() && rep.purged.is_empty());
        assert_eq!(r.lfs(id), LocalFileState::Present);
        // Receipt arrives late: binned at the receipt time (seen_at), not at due.
        let seen = r.clock.now_ms();
        r.confirm(id);
        let rep = r.sweep();
        assert_eq!(rep.to_bin, vec![id]);
        assert_eq!(r.lfs(id), LocalFileState::Binned);
        let bin_path = r.path(id).unwrap();
        assert!(bin_path != held && bin_path.exists() && !held.exists(), "file moved to the Bin area");
        assert!(r.actions().contains(&ActivityAction::Expired));
        // Not yet purged one ms before 7 days after seen_at (L13).
        r.clock.set(seen + 7 * DAY_MS - 1);
        let rep = r.sweep();
        assert!(rep.purged.is_empty(), "6.99 days: still in the Bin");
        assert!(bin_path.exists());
        assert_eq!(rep.next_due, Some(seen + 7 * DAY_MS), "the purge is the next time-driven change");
        r.clock.set(seen + 7 * DAY_MS);
        let rep = r.sweep();
        assert_eq!(rep.purged, vec![id]);
        assert_eq!(r.lfs(id), LocalFileState::Purged);
        assert!(!bin_path.exists() && r.path(id).is_none());
        assert!(r.actions().contains(&ActivityAction::Purged));
        // L15: the record stays as a tombstone, unchanged by the purge.
        assert_eq!(store::count_items(&r.db.conn).unwrap(), 1);
        let (rec, _) = store::get_item(&r.db.conn, id).unwrap().unwrap();
        assert!(rec.deleted.val.is_none(), "expiry is derived, nothing was written to the record");
        assert!(matches!(derive_state(&r.db.conn, &rec, r.clock.now_ms()).unwrap(), ItemState::Expired { .. }));
        // Sweeping again does nothing (idempotent).
        let rep = r.sweep();
        assert_eq!(rep, SweepReport::default());
    }

    #[test]
    fn next_due_points_at_the_earliest_future_change() {
        let mut r = Rig::new();
        let a = r.add("a");
        r.confirm(a);
        let rep = r.sweep();
        assert_eq!(rep.next_due, Some(T0 + 24 * HOUR_MS), "Live item expires at due");
        r.clock.set(T0 + 24 * HOUR_MS);
        let rep = r.sweep();
        assert_eq!(rep.to_bin, vec![a]);
        assert_eq!(rep.next_due, Some(T0 + 24 * HOUR_MS + 7 * DAY_MS), "then the purge time");
    }

    // L14: delete by self is logged once at delete time; sweep moves the file without a second row.
    #[test]
    fn user_delete_moves_file_without_duplicate_activity() {
        let mut r = Rig::new();
        let id = r.add("a");
        r.run(|c, x| ops::delete_item(c, x, id)).unwrap();
        // The op itself already moved the file.
        assert_eq!(r.lfs(id), LocalFileState::Binned);
        let rep = r.sweep();
        assert!(rep.to_bin.is_empty());
        let n = r.actions().iter().filter(|a| **a == ActivityAction::Deleted).count();
        assert_eq!(n, 1);
        assert!(!r.actions().contains(&ActivityAction::Binned));
    }

    #[test]
    fn deleted_by_peer_logs_binned() {
        let mut r = Rig::new();
        let id = r.add("a");
        let mut rec = store::get_item(&r.db.conn, id).unwrap().unwrap().0;
        rec.deleted = Versioned { v: v(50, f()), val: Some(T0 + 1) };
        store::update_item(&r.db.conn, &rec).unwrap();
        let rep = r.sweep();
        assert_eq!(rep.to_bin, vec![id]);
        assert!(r.actions().contains(&ActivityAction::Binned));
    }

    // L14: Binned + not in Bin -> Returned (E8: pinned on the other device while apart).
    #[test]
    fn synced_keep_brings_an_expired_item_back() {
        let mut r = Rig::new();
        let id = r.add("a");
        r.confirm(id);
        r.clock.set(T0 + 25 * HOUR_MS);
        assert_eq!(r.sweep().to_bin, vec![id]);
        let mut rec = store::get_item(&r.db.conn, id).unwrap().unwrap().0;
        rec.pinned = Versioned { v: v(60, f()), val: true };
        store::update_item(&r.db.conn, &rec).unwrap();
        let rep = r.sweep();
        assert_eq!(rep.returned, vec![id]);
        assert_eq!(r.lfs(id), LocalFileState::Present);
        assert!(r.path(id).unwrap().exists());
        assert!(r.actions().contains(&ActivityAction::Returned));
        assert!(!r.sweep().returned.contains(&id));
    }

    // L14: Pending in Bin -> Purged (never download); Purged + not in Bin -> Pending (want).
    #[test]
    fn pending_in_bin_is_purged_and_purged_outside_bin_is_wanted() {
        let mut r = Rig::new();
        // Arrives already deleted.
        let mut deleted = item(7, f(), v(3, f()));
        deleted.deleted = Versioned { v: v(4, f()), val: Some(T0) };
        store::insert_item(&r.db.conn, &deleted, LocalFileState::Pending, T0).unwrap();
        // Arrives live.
        let live = item(8, f(), v(3, f()));
        store::insert_item(&r.db.conn, &live, LocalFileState::Pending, T0).unwrap();
        let rep = r.sweep();
        assert_eq!(r.lfs(deleted.id), LocalFileState::Purged, "L9: never queued, never downloaded");
        assert_eq!(r.lfs(live.id), LocalFileState::Pending);
        assert!(rep.want.is_empty(), "want = transitions only");
        // Pin arrives on the purged one: it must be fetched again.
        let mut back = deleted.clone();
        back.pinned = Versioned { v: v(9, f()), val: true };
        back.deleted = Versioned { v: v(10, f()), val: None };
        store::update_item(&r.db.conn, &back).unwrap();
        let rep = r.sweep();
        assert_eq!(rep.want, vec![deleted.id]);
        assert_eq!(r.lfs(deleted.id), LocalFileState::Pending);
    }

    #[test]
    fn stale_file_of_a_pending_bin_item_is_purged() {
        let mut r = Rig::new();
        let id = r.add("a");
        let path = r.path(id).unwrap();
        // The peer edited the content (-> Pending, old file still referenced), then deleted the item.
        store::set_local_state(&r.db.conn, id, LocalFileState::Pending, T0).unwrap();
        let mut rec = store::get_item(&r.db.conn, id).unwrap().unwrap().0;
        rec.deleted = Versioned { v: v(50, f()), val: Some(T0) };
        store::update_item(&r.db.conn, &rec).unwrap();
        r.sweep();
        assert_eq!(r.lfs(id), LocalFileState::Purged);
        assert!(!path.exists() && r.path(id).is_none(), "no stale file left behind");
    }

    // L14: a placement error leaves the state unchanged and is reported; the next sweep retries.
    #[test]
    fn placement_error_leaves_state_and_is_retried() {
        let mut r = Rig::new();
        let a = r.add("a");
        let b = r.add("b");
        r.confirm(a);
        r.confirm(b);
        r.clock.set(T0 + 25 * HOUR_MS);
        // Break item a: its file vanished from disk.
        let held = r.path(a).unwrap();
        let hidden = held.with_extension("moved");
        std::fs::rename(&held, &hidden).unwrap();
        let rep = r.sweep();
        assert_eq!(rep.errors.len(), 1);
        assert_eq!(rep.errors[0].0, a);
        assert_eq!(r.lfs(a), LocalFileState::Present, "state unchanged on failure");
        assert_eq!(r.lfs(b), LocalFileState::Binned, "other items still processed");
        assert_eq!(rep.to_bin, vec![b]);
        // File returns: the next sweep succeeds.
        std::fs::rename(&hidden, &held).unwrap();
        let rep = r.sweep();
        assert!(rep.errors.is_empty());
        assert_eq!(rep.to_bin, vec![a]);
    }

    #[test]
    fn purge_failure_never_marks_purged() {
        let mut r = Rig::new();
        let a = r.add("a");
        r.confirm(a);
        r.clock.set(T0 + 25 * HOUR_MS);
        r.sweep();
        assert_eq!(r.lfs(a), LocalFileState::Binned);
        // Make the Bin file undeletable: replace it with a non-empty directory.
        let bin_path = r.path(a).unwrap();
        std::fs::remove_file(&bin_path).unwrap();
        std::fs::create_dir(&bin_path).unwrap();
        std::fs::write(bin_path.join("x"), b"x").unwrap();
        r.clock.set(T0 + 25 * HOUR_MS + 8 * DAY_MS);
        let rep = r.sweep();
        assert_eq!(rep.errors.len(), 1, "{rep:?}");
        assert_eq!(r.lfs(a), LocalFileState::Binned, "not Purged while the file is still there");
    }

    // L16: restore: deleted, expired, purged -> Pending, not in Bin -> Invalid.
    #[test]
    fn restore_gives_fresh_retention_and_refetches_purged() {
        let mut r = Rig::new();
        let a = r.add("a"); // will be deleted
        let b = r.add("b"); // will expire
        let ec = r.add("c"); // will expire and be purged
        r.confirm(b);
        r.confirm(ec);
        r.run(|c, x| ops::delete_item(c, x, a)).unwrap();
        r.clock.set(T0 + 25 * HOUR_MS);
        r.sweep();
        r.clock.set(T0 + 25 * HOUR_MS + 8 * DAY_MS);
        r.sweep();
        assert_eq!(r.lfs(ec), LocalFileState::Purged);
        let now = r.clock.now_ms();
        // b was purged too by now; add a fresh pair for the "Binned" cases.
        let d = r.add("d");
        r.confirm(d);
        r.clock.advance(25 * HOUR_MS);
        r.sweep();
        assert_eq!(r.lfs(d), LocalFileState::Binned);
        let now2 = r.clock.now_ms();
        assert!(now2 > now);
        // Restore the Binned expired one: file comes back, fresh retention from now.
        r.run(|c, x| ops::restore_item(c, x, d)).unwrap();
        assert_eq!(r.lfs(d), LocalFileState::Present);
        assert!(r.path(d).unwrap().exists());
        let rec = store::get_item(&r.db.conn, d).unwrap().unwrap().0;
        assert_eq!(rec.location.val.entered_at, now2);
        assert!(matches!(derive_state(&r.db.conn, &rec, now2).unwrap(), ItemState::Live { expires_at } if expires_at == now2 + 24 * HOUR_MS));
        // Restore the purged deleted one: record un-deleted, entered_at reset, file wanted again.
        r.run(|c, x| ops::restore_item(c, x, a)).unwrap();
        let rec = store::get_item(&r.db.conn, a).unwrap().unwrap().0;
        assert_eq!(rec.deleted.val, None);
        assert_eq!(rec.location.val.entered_at, now2);
        assert_eq!(r.lfs(a), LocalFileState::Pending);
        // Purged and expired: Pending too.
        r.run(|c, x| ops::restore_item(c, x, ec)).unwrap();
        assert_eq!(r.lfs(ec), LocalFileState::Pending);
        // Not in the Bin any more: Invalid.
        assert!(matches!(r.run(|c, x| ops::restore_item(c, x, d)), Err(CoreError::Invalid(_))));
        assert!(matches!(r.run(|c, x| ops::restore_item(c, x, ItemId(uuid::Uuid::from_u128(1)))), Err(CoreError::NotFound { .. })));
        assert!(r.actions().contains(&ActivityAction::Restored));
        let _ = b;
    }

    #[test]
    fn bin_page_lists_binned_items_newest_first_and_hides_purged() {
        let mut r = Rig::new();
        let ids: Vec<ItemId> = ["a", "b", "c"].iter().map(|n| r.add(n)).collect();
        for (i, id) in ids.iter().enumerate() {
            r.clock.advance(HOUR_MS);
            let _ = i;
            r.run(|c, x| ops::delete_item(c, x, *id)).unwrap();
        }
        let page = bin_page(&r.db.conn, None, 2, r.clock.now_ms()).unwrap();
        assert_eq!(page.iter().map(|p| p.0.id).collect::<Vec<_>>(), vec![ids[2], ids[1]], "newest binned first");
        let page2 = bin_page(&r.db.conn, Some(ids[1]), 2, r.clock.now_ms()).unwrap();
        assert_eq!(page2.iter().map(|p| p.0.id).collect::<Vec<_>>(), vec![ids[0]]);
        assert!(matches!(page[0].1, ItemState::Deleted { .. }));
        // After the Bin days: gone from the list, tombstone remains.
        r.clock.advance(8 * DAY_MS);
        r.sweep();
        assert!(bin_page(&r.db.conn, None, 10, r.clock.now_ms()).unwrap().is_empty());
        assert_eq!(store::count_items(&r.db.conn).unwrap(), 3);
    }

    // A file deleted in Explorer (Core::local_files_missing): listed in the Bin, restorable by
    // fetching from the other device, purged (nothing to delete) after the Bin days.
    #[test]
    fn a_file_lost_locally_is_listed_in_the_bin_and_restore_fetches_it() {
        let mut r = Rig::new();
        let id = r.add("a");
        let other = r.add("b");
        // What Core::local_files_missing does, in the same order.
        store::set_local_state(&r.db.conn, id, LocalFileState::Binned, r.now()).unwrap();
        store::delete_file_ref(&r.db.conn, id).unwrap();
        r.run(|c, x| ops::delete_item(c, x, id)).unwrap();
        let listed = |r: &Rig| bin_page(&r.db.conn, None, 10, r.now()).unwrap().into_iter().map(|p| p.0.id).collect::<Vec<_>>();
        assert_eq!(listed(&r), vec![id]);
        assert!(r.sweep().errors.is_empty());
        assert_eq!(listed(&r), vec![id], "a sweep keeps it listed");
        // Restore: no file to move back, so it is wanted from the other device.
        r.run(|c, x| ops::restore_item(c, x, id)).unwrap();
        assert_eq!(r.lfs(id), LocalFileState::Pending);
        assert_eq!(r.lfs(other), LocalFileState::Present);
        assert!(listed(&r).is_empty());
    }

    #[test]
    fn a_file_lost_locally_leaves_the_bin_after_the_bin_days() {
        let mut r = Rig::new();
        let id = r.add("a");
        store::set_local_state(&r.db.conn, id, LocalFileState::Binned, r.now()).unwrap();
        store::delete_file_ref(&r.db.conn, id).unwrap();
        r.run(|c, x| ops::delete_item(c, x, id)).unwrap();
        r.clock.advance(8 * DAY_MS);
        let rep = r.sweep();
        assert_eq!(rep.purged, vec![id]);
        assert!(bin_page(&r.db.conn, None, 10, r.now()).unwrap().is_empty());
    }

    #[test]
    fn bin_days_setting_is_honoured_and_activity_is_trimmed() {
        let mut r = Rig::new();
        let s = Settings { bin_days: 1, ..Settings::default() };
        store::save_settings(&r.db.conn, &s).unwrap();
        let a = r.add("a");
        r.run(|c, x| ops::delete_item(c, x, a)).unwrap();
        r.clock.advance(DAY_MS - 1);
        assert!(r.sweep().purged.is_empty());
        r.clock.advance(1);
        assert_eq!(r.sweep().purged, vec![a]);
        // Old activity goes with the sweep.
        r.clock.advance(31 * DAY_MS);
        r.sweep();
        assert!(activity::list(&r.db.conn, None, 100).unwrap().is_empty());
    }

    // Blocker 1 (verifier): the Bin clock is local. A device that was away when an item expired
    // gives the file the full Bin days from the moment it enters ITS Bin, not from the synced due time.
    #[test]
    fn late_sweep_bins_first_and_counts_bin_days_from_binning() {
        let mut r = Rig::new();
        let id = r.add("a");
        r.confirm(id);
        // The device slept 30 days: expiry was due 29 days ago.
        r.clock.set(T0 + 30 * DAY_MS);
        let rep = r.sweep();
        assert_eq!(rep.to_bin, vec![id]);
        assert!(rep.purged.is_empty(), "never purged in the same sweep that bins it");
        assert_eq!(r.lfs(id), LocalFileState::Binned);
        let binned = r.clock.now_ms();
        assert_eq!(rep.next_due, Some(binned + 7 * DAY_MS), "next due follows the local Bin clock");
        let rec = store::get_item(&r.db.conn, id).unwrap().unwrap().0;
        let listed = |r: &Rig| bin_page(&r.db.conn, None, 10, r.clock.now_ms()).unwrap().iter().any(|p| p.0.id == rec.id);
        assert!(listed(&r), "listed in the Bin, restorable");
        r.clock.set(binned + 7 * DAY_MS - 1);
        assert!(r.sweep().purged.is_empty());
        assert!(listed(&r));
        r.clock.set(binned + 7 * DAY_MS);
        assert_eq!(r.sweep().purged, vec![id]);
        assert!(!listed(&r), "past the local purge time it is no longer listed");
    }

    // Verifier claim: a delete made by a device with a wrong clock, or while this device was away,
    // must not shorten or stretch this device's Bin time (deleted_at is another clock's value).
    #[test]
    fn remote_delete_times_do_not_decide_the_bin_time() {
        for (label, deleted_at) in [("100 days old", T0 - 100 * DAY_MS), ("100 days in the future", T0 + 100 * DAY_MS)] {
            let mut r = Rig::new();
            let id = r.add("a");
            let mut rec = store::get_item(&r.db.conn, id).unwrap().unwrap().0;
            rec.deleted = Versioned { v: v(50, f()), val: Some(deleted_at) };
            store::update_item(&r.db.conn, &rec).unwrap();
            let rep = r.sweep();
            assert_eq!(rep.to_bin, vec![id], "{label}: binned, not purged on the spot");
            assert!(rep.purged.is_empty(), "{label}");
            r.clock.advance(7 * DAY_MS - 1);
            assert!(r.sweep().purged.is_empty(), "{label}: still in the Bin at 6.99 days");
            r.clock.advance(1);
            assert_eq!(r.sweep().purged, vec![id], "{label}: purged exactly Bin days after it entered this Bin");
        }
    }

    // Leaving the Bin and coming back restarts the Bin clock.
    #[test]
    fn a_second_binning_gets_a_fresh_bin_clock() {
        let mut r = Rig::new();
        let id = r.add("a");
        r.confirm(id);
        r.clock.set(T0 + 25 * HOUR_MS);
        r.sweep();
        r.clock.advance(6 * DAY_MS);
        // Kept on the other device: back out of the Bin.
        let mut rec = store::get_item(&r.db.conn, id).unwrap().unwrap().0;
        rec.pinned = Versioned { v: v(60, f()), val: true };
        store::update_item(&r.db.conn, &rec).unwrap();
        assert_eq!(r.sweep().returned, vec![id]);
        // Un-kept a day later: binned again, with the full Bin days from now.
        r.clock.advance(DAY_MS);
        rec.pinned = Versioned { v: v(61, f()), val: false };
        store::update_item(&r.db.conn, &rec).unwrap();
        assert_eq!(r.sweep().to_bin, vec![id]);
        let binned = r.clock.now_ms();
        r.clock.set(binned + 7 * DAY_MS - 1);
        assert!(r.sweep().purged.is_empty());
        r.clock.set(binned + 7 * DAY_MS);
        assert_eq!(r.sweep().purged, vec![id]);
    }

    // Verifier claim (needs Nigel's decision, so it is pinned here, not "fixed"): receipts are per
    // file hash, forever. PLAN: "'Confirmed' means a receipt for that file's hash from a device
    // this one is or was paired with." So a NEW item with the same bytes as a long-purged one (the
    // user drops the same file in again) counts the old receipt and starts its expiry clock at once,
    // even if the other device never got this new item. This test records that behaviour; if
    // Nigel wants receipts to be per item generation, this test is the one to change.
    #[test]
    fn an_old_receipt_confirms_a_new_item_with_the_same_bytes() {
        let mut r = Rig::new();
        let old = r.add("a");
        r.confirm(old);
        r.run(|c, x| ops::delete_item(c, x, old)).unwrap();
        r.clock.advance(8 * DAY_MS);
        assert_eq!(r.sweep().purged, vec![old]);
        // The same bytes come back as a new item, and the other device is away.
        let mut again = r.rec(old);
        again.id = ItemId(uuid::Uuid::from_u128(0xA11CE));
        again.deleted = Versioned { v: again.deleted.v, val: None };
        again.location.val.entered_at = r.clock.now_ms();
        store::insert_item(&r.db.conn, &again, LocalFileState::Present, r.clock.now_ms()).unwrap();
        let state = derive_state(&r.db.conn, &again, r.clock.now_ms()).unwrap();
        assert!(matches!(state, ItemState::Live { .. }), "not Waiting: the old receipt for these bytes still counts, got {state:?}");
    }
}
