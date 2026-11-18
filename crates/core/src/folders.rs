//! Folder records and the default folders. Owner: logic builder.
//!
//! Rules:
//! F1 Default folders have fixed ids (model::INBOX_ID, KEEP_ID, UNSORTED_ID) and GENESIS
//!    versions, so both devices create identical records and never duplicate them:
//!    Inbox (kind inbox, 24 h = 86_400 s), Keep (kind keep, Keep), Unsorted (kind unsorted, Keep).
//! F2 Default folders cannot be deleted; their kind never changes. Inbox and Unsorted
//!    retention can be changed (M4 UI); renaming is allowed (names are display only).
//! F3 A folder created in Explorer starts as Keep (PLAN). A folder created in the app takes
//!    the retention the user chose (default Keep).
//! F4 Folder names: `names::clean_folder_name`; two folders may share a name (disk names
//!    get " (2)", PLAN Storage), so uniqueness is not enforced in the record.
//! F5 Deleting a folder: deleted = Some(now) on the folder, and each item in it that is not
//!    already in the Bin is deleted in the same transaction (PLAN: "deleted subfolder = its
//!    items to the Bin"). Items moved into it later by the other device are treated as
//!    Unsorted (expiry E2) until moved.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::{bin, expiry, model::*, store, Result};

pub const INBOX_RETENTION_SECS: u64 = 24 * 60 * 60;

fn default_record(id: FolderId, kind: FolderKind, name: &str, retention: Retention) -> FolderRecord {
    FolderRecord {
        id,
        kind,
        name: Versioned { v: GENESIS, val: name.to_string() },
        retention: Versioned { v: GENESIS, val: retention },
        deleted: Versioned { v: GENESIS, val: None },
    }
}

/// The three default folder records (F1).
pub fn default_folders() -> Vec<FolderRecord> {
    vec![
        default_record(INBOX_ID, FolderKind::Inbox, "Inbox", Retention::Secs { secs: INBOX_RETENTION_SECS }),
        default_record(KEEP_ID, FolderKind::Keep, "Keep", Retention::Keep),
        default_record(UNSORTED_ID, FolderKind::Unsorted, "Unsorted", Retention::Keep),
    ]
}

/// True for the folders F2 protects.
pub fn is_default(id: FolderId) -> bool {
    id == INBOX_ID || id == KEEP_ID || id == UNSORTED_ID
}

/// Insert any missing default folder (F1). Idempotent; called at every open.
pub fn ensure_defaults(c: &Connection) -> Result<()> {
    for f in default_folders() {
        if store::get_folder(c, f.id)?.is_none() {
            store::put_folder(c, &f)?;
        }
    }
    Ok(())
}

/// Folder for an item: the record if it exists and is not deleted, else the Unsorted record
/// (E2's rule, used by the views and the PC placement to choose the directory).
pub fn effective_folder(c: &Connection, id: FolderId) -> Result<FolderRecord> {
    if let Some(f) = store::get_folder(c, id)? {
        if f.deleted.val.is_none() {
            return Ok(f);
        }
    }
    match store::get_folder(c, UNSORTED_ID)? {
        Some(f) => Ok(f),
        // ensure_defaults runs at open, so this is a corrupted db; fall back to the built-in.
        None => Ok(default_folders().remove(2)),
    }
}

fn sort_key(f: &FolderRecord) -> (u8, String) {
    let rank = match f.kind {
        FolderKind::Inbox => 0,
        FolderKind::Keep => 1,
        FolderKind::Unsorted => 2,
        FolderKind::User => 3,
    };
    (rank, f.name.val.to_lowercase())
}

/// Live folders with item counts (not in the Bin; `hide_refused` leaves out files this device refused:
/// the PC lists them under Not synced instead), Inbox first, then Keep, Unsorted, then
/// user folders by name (case-insensitive).
pub fn folder_views(c: &Connection, now: i64, hide_refused: bool) -> Result<Vec<FolderView>> {
    let folders = store::all_folders(c)?;
    let map: HashMap<FolderId, FolderRecord> = folders.iter().map(|f| (f.id, f.clone())).collect();
    let mut counts: HashMap<FolderId, u32> = HashMap::new();
    // Items effectively deleted are in the Bin whatever the rest says: only items that are not
    // deleted, or that carry a pin (which may outrank an older delete), can be outside it.
    for id in store::item_ids_where(c, "deleted_at IS NULL OR pinned = 1")? {
        let Some((rec, lfs)) = store::get_item(c, id)? else { continue };
        if hide_refused && lfs == LocalFileState::Refused {
            continue;
        }
        let state = bin::derive_state_in(c, &map, &rec, now)?;
        if !expiry::in_bin(state) {
            // E2: an item in a deleted or unknown folder shows under Unsorted.
            let shown = match map.get(&rec.location.val.folder) {
                Some(f) if f.deleted.val.is_none() => f.id,
                _ => UNSORTED_ID,
            };
            *counts.entry(shown).or_insert(0) += 1;
        }
    }
    let mut live: Vec<&FolderRecord> = folders.iter().filter(|f| f.deleted.val.is_none()).collect();
    live.sort_by_key(|f| sort_key(f));
    Ok(live
        .into_iter()
        .map(|f| FolderView { id: f.id, kind: f.kind, name: crate::names::display_name(&f.name.val, ""), retention: f.retention.val, count: counts.get(&f.id).copied().unwrap_or(0) })
        .collect())
}

/// Number of items that `set_retention(folder, new)` would move to the Bin at `now`
/// (M4 confirmation "This moves 132 items to the Bin").
pub fn retention_preview(c: &Connection, folder: FolderId, new: Retention, now: i64) -> Result<u32> {
    let mut map: HashMap<FolderId, FolderRecord> = store::all_folders(c)?.into_iter().map(|f| (f.id, f)).collect();
    let Some(current) = map.get(&folder).cloned() else { return Ok(0) };
    let mut changed = current;
    changed.retention.val = new;
    let mut n = 0;
    for id in store::item_ids_in_folder(c, folder)? {
        let Some((rec, _)) = store::get_item(c, id)? else { continue };
        if expiry::in_bin(bin::derive_state_in(c, &map, &rec, now)?) {
            continue;
        }
        let mut with_new = map.clone();
        with_new.insert(folder, changed.clone());
        if expiry::in_bin(bin::derive_state_in(c, &with_new, &rec, now)?) {
            n += 1;
        }
    }
    map.clear();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{HOUR_MS, DAY_MS};
    use crate::db::Db;
    use crate::ops::{self, Ctx};
    use crate::placement::AppStorage;
    use crate::store::test_support::*;

    // L21: default folders identical on two fresh dbs; idempotent.
    #[test]
    fn defaults_identical_on_two_fresh_dbs_and_idempotent() {
        let a = Db::open_in_memory().unwrap();
        let b = Db::open_in_memory().unwrap();
        let ra = store::all_folders(&a.conn).unwrap();
        let rb = store::all_folders(&b.conn).unwrap();
        assert_eq!(ra, rb, "same ids, versions and values on two fresh devices");
        assert_eq!(ra.len(), 3);
        let inbox = ra.iter().find(|f| f.id == INBOX_ID).unwrap();
        assert_eq!(inbox.retention.val, Retention::Secs { secs: 86_400 });
        assert_eq!(inbox.kind, FolderKind::Inbox);
        assert!(ra.iter().filter(|f| f.id != INBOX_ID).all(|f| f.retention.val == Retention::Keep));
        assert!(ra.iter().all(|f| f.name.v == GENESIS && f.retention.v == GENESIS && f.deleted.v == GENESIS));
        ensure_defaults(&a.conn).unwrap();
        ensure_defaults(&a.conn).unwrap();
        assert_eq!(store::all_folders(&a.conn).unwrap().len(), 3);
        // ensure_defaults never overwrites a user's edit.
        let mut edited = inbox.clone();
        edited.retention = Versioned { v: v(3, p()), val: Retention::Keep };
        store::put_folder(&a.conn, &edited).unwrap();
        ensure_defaults(&a.conn).unwrap();
        assert_eq!(store::get_folder(&a.conn, INBOX_ID).unwrap().unwrap().retention.val, Retention::Keep);
        // Nothing logged for the defaults: both devices have them already.
        assert_eq!(store::head_seq(&b.conn).unwrap(), 0);
    }

    #[test]
    fn effective_folder_falls_back_to_unsorted() {
        let mut db = db_with_peer(p(), f());
        let dir = tempfile::tempdir().unwrap();
        let pl = AppStorage::new(dir.path().to_path_buf()).unwrap();
        let ctx = Ctx { self_id: p(), now: T0, placement: &pl };
        assert_eq!(effective_folder(&db.conn, INBOX_ID).unwrap().id, INBOX_ID);
        assert_eq!(effective_folder(&db.conn, FolderId(uuid::Uuid::from_u128(0x4242))).unwrap().id, UNSORTED_ID);
        let id = ops::create_folder(&mut db.conn, &ctx, "Work", Retention::Keep).unwrap();
        assert_eq!(effective_folder(&db.conn, id).unwrap().id, id);
        ops::delete_folder(&mut db.conn, &ctx, id).unwrap();
        assert_eq!(effective_folder(&db.conn, id).unwrap().id, UNSORTED_ID);
    }

    // L21: views: order, counts exclude Bin items, items of a deleted folder show under Unsorted.
    #[test]
    fn views_order_and_counts() {
        let mut db = db_with_peer(p(), f());
        let dir = tempfile::tempdir().unwrap();
        let pl = AppStorage::new(dir.path().to_path_buf()).unwrap();
        let ctx = Ctx { self_id: p(), now: T0, placement: &pl };
        let z = ops::create_folder(&mut db.conn, &ctx, "zebra", Retention::Keep).unwrap();
        let a = ops::create_folder(&mut db.conn, &ctx, "Apple", Retention::Secs { secs: 3600 }).unwrap();
        for (n, folder) in [(1u8, INBOX_ID), (2, INBOX_ID), (3, a), (4, z)] {
            let mut it = item(n, p(), v(1, p()));
            it.location.val.folder = folder;
            it.location.val.entered_at = T0;
            store::insert_item(&db.conn, &it, LocalFileState::Pending, T0).unwrap();
        }
        // Item 2 is deleted: in the Bin, not counted.
        let mut it2 = store::get_item(&db.conn, ItemId(uuid::Uuid::from_u128(0x1002))).unwrap().unwrap().0;
        it2.deleted = Versioned { v: v(5, p()), val: Some(T0) };
        store::update_item(&db.conn, &it2).unwrap();
        let views = folder_views(&db.conn, T0, false).unwrap();
        let names: Vec<&str> = views.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["Inbox", "Keep", "Unsorted", "Apple", "zebra"]);
        let count = |n: &str| views.iter().find(|v| v.name == n).unwrap().count;
        assert_eq!((count("Inbox"), count("Apple"), count("zebra"), count("Keep")), (1, 1, 1, 0));
        // Deleting Apple deletes its item too; the folder disappears from the views.
        ops::delete_folder(&mut db.conn, &Ctx { now: T0 + 1, ..ctx }, a).unwrap();
        let views = folder_views(&db.conn, T0 + 1, false).unwrap();
        assert!(views.iter().all(|v| v.name != "Apple"));
        assert_eq!(views.iter().map(|v| v.count).sum::<u32>(), 2, "Inbox 1 + zebra 1");
        // An item that arrives pointing at a deleted folder is shown under Unsorted.
        let mut stray = item(9, f(), v(2, f()));
        stray.location.val.folder = a;
        store::insert_item(&db.conn, &stray, LocalFileState::Pending, T0).unwrap();
        let views = folder_views(&db.conn, T0 + 1, false).unwrap();
        assert_eq!(views.iter().find(|v| v.id == UNSORTED_ID).unwrap().count, 1);
    }

    // L21: preview counts what a retention change would bin; the change itself re-derives all items.
    #[test]
    fn retention_preview_and_change_rederive_all_items() {
        let mut db = db_with_peer(p(), f());
        let dir = tempfile::tempdir().unwrap();
        let pl = AppStorage::new(dir.path().to_path_buf()).unwrap();
        let ctx = Ctx { self_id: p(), now: T0, placement: &pl };
        let mut ids = Vec::new();
        for n in 1..=5u8 {
            let mut it = item(n, p(), v(1, p()));
            it.location.val.entered_at = T0 - (n as i64) * 10 * HOUR_MS; // 10 h, 20 h, ... 50 h old
            store::insert_item(&db.conn, &it, LocalFileState::Pending, T0).unwrap();
            // Peer confirmed every file.
            store::insert_receipt(&db.conn, &ReceiptRecord { hash: hash(n), device: f(), received_at: T0 }, T0 - 60 * HOUR_MS).unwrap();
            ids.push(it.id);
        }
        // Pinned item 5 never expires; item 4 is already past 24 h (40 h old) -> already in the Bin.
        let mut it5 = store::get_item(&db.conn, ids[4]).unwrap().unwrap().0;
        it5.pinned = Versioned { v: v(2, p()), val: true };
        store::update_item(&db.conn, &it5).unwrap();
        let now = T0;
        let state = |db: &Db, id| bin::derive_state(&db.conn, &store::get_item(&db.conn, id).unwrap().unwrap().0, now).unwrap();
        assert!(matches!(state(&db, ids[0]), ItemState::Live { .. }));
        assert!(expiry::in_bin(state(&db, ids[3])));
        assert_eq!(state(&db, ids[4]), ItemState::Kept);
        // Inbox 24 h -> 12 h: items 1 (10 h) stays, 2 (20 h) and 3 (30 h)... 3 is already out. Newly binned: 2.
        assert_eq!(retention_preview(&db.conn, INBOX_ID, Retention::Secs { secs: 12 * 3600 }, now).unwrap(), 1);
        assert_eq!(retention_preview(&db.conn, INBOX_ID, Retention::Secs { secs: 5 * 3600 }, now).unwrap(), 2, "items 1 and 2");
        assert_eq!(retention_preview(&db.conn, INBOX_ID, Retention::Keep, now).unwrap(), 0);
        assert_eq!(retention_preview(&db.conn, INBOX_ID, Retention::Secs { secs: 100 * 3600 }, now).unwrap(), 0);
        assert_eq!(retention_preview(&db.conn, FolderId(uuid::Uuid::from_u128(0x77)), Retention::Keep, now).unwrap(), 0);
        // Apply 5 h: the preview number matches what actually goes to the Bin.
        let before = (0..5).filter(|i| expiry::in_bin(state(&db, ids[*i]))).count();
        ops::set_retention(&mut db.conn, &Ctx { now, ..ctx }, INBOX_ID, Retention::Secs { secs: 5 * 3600 }).unwrap();
        let after = (0..5).filter(|i| expiry::in_bin(state(&db, ids[*i]))).count();
        assert_eq!(after - before, 2);
        // And back to Keep: everything not deleted leaves the Bin again, nothing else touched.
        ops::set_retention(&mut db.conn, &Ctx { now, ..ctx }, INBOX_ID, Retention::Keep).unwrap();
        assert!((0..5).all(|i| !expiry::in_bin(state(&db, ids[i]))));
        let _ = DAY_MS;
    }
}
