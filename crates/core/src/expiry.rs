//! Derived expiry. Owner: logic builder. Pure: no DB, no clock.
//!
//! PLAN Folders and expiry (Decisions log 2026-11-18, "drop the expiry hold"):
//! - "An item expires at folder_entered_at + retention. Moving an item to another folder
//!   resets folder_entered_at to now. A kept (pinned) item never expires."
//! - Items expire ON TIME, whether or not the other device has the file. Before 0.2.0 an item
//!   waited ("Waiting for PC") until a receipt from the other device arrived; that hold is gone.
//!   Receipts stay as sync bookkeeping only (the "waiting" count, "never reached the PC").
//! - "Expiry is worked out on each device from these synced fields; no device sends
//!   'expired' to the other."
//!
//! Truth table (first matching row wins; full table with examples in core-design.md):
//! | # | effective deleted (E1) | pinned | retention (E2) | now >= due | state |
//! |---|---|---|---|---|---|
//! | 1 | yes | -   | -    | -   | Deleted{binned_at = deleted_at} |
//! | 2 | no  | yes | -    | -   | Kept |
//! | 3 | no  | no  | Keep | -   | FolderKeep |
//! | 4 | no  | no  | secs | no  | Live{expires_at = due} |
//! | 5 | no  | no  | secs | yes | Expired{binned_at = due} |
//! due = location.entered_at + secs * 1000.
//! E1 effective deleted = deleted.val.is_some() && !(pinned.val && pinned.v > deleted.v)
//!    (M5's rule, so the view is right even before the M5 rewrite has run).
//! E2 retention = the item's folder's retention if that folder record exists and is not
//!    deleted; otherwise Keep (an unknown or deleted folder is treated as Unsorted/Keep, so
//!    an item never falls into a short retention by accident).
//! `binned_at` (rows 1 and 5) is when the item LOGICALLY entered the Bin, built from synced
//! times; it is for display and ordering only. When a file is purged is decided by this
//! device's own Bin clock (bin.rs `local_purge_at`), never by `binned_at`: an old synced time
//! must not purge a file with no Bin time.

use crate::model::*;

/// The inputs `item_state` needs besides the record.
#[derive(Debug, Clone, Copy)]
pub struct ExpiryCtx<'a> {
    /// The item's folder record, if known locally.
    pub folder: Option<&'a FolderRecord>,
    pub now_ms: i64,
}

/// Truth table above.
pub fn item_state(item: &ItemRecord, ctx: ExpiryCtx<'_>) -> ItemState {
    if effectively_deleted(item) {
        // effectively_deleted implies deleted.val is Some.
        return ItemState::Deleted { binned_at: item.deleted.val.unwrap_or(0) };
    }
    if item.pinned.val {
        return ItemState::Kept;
    }
    match effective_retention(ctx.folder) {
        Retention::Keep => ItemState::FolderKeep,
        Retention::Secs { secs } => {
            let due = item.location.val.entered_at.saturating_add(secs_to_ms(secs));
            if ctx.now_ms >= due {
                ItemState::Expired { binned_at: due }
            } else {
                ItemState::Live { expires_at: due }
            }
        }
    }
}

fn secs_to_ms(secs: u64) -> i64 {
    (secs.min(i64::MAX as u64 / 1000) as i64) * 1000
}

/// E2.
pub fn effective_retention(folder: Option<&FolderRecord>) -> Retention {
    match folder {
        Some(f) if f.deleted.val.is_none() => f.retention.val,
        _ => Retention::Keep,
    }
}

/// E1.
pub fn effectively_deleted(item: &ItemRecord) -> bool {
    item.deleted.val.is_some() && !(item.pinned.val && item.pinned.v > item.deleted.v)
}

/// True for Deleted and Expired.
pub fn in_bin(state: ItemState) -> bool {
    matches!(state, ItemState::Deleted { .. } | ItemState::Expired { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::HOUR_MS;
    use crate::store::test_support::*;

    fn folder(retention: Retention) -> FolderRecord {
        FolderRecord {
            id: INBOX_ID,
            kind: FolderKind::Inbox,
            name: Versioned { v: GENESIS, val: "Inbox".into() },
            retention: Versioned { v: GENESIS, val: retention },
            deleted: Versioned { v: GENESIS, val: None },
        }
    }

    const H24: Retention = Retention::Secs { secs: 24 * 3600 };

    fn st(item: &ItemRecord, f: Option<&FolderRecord>, now: i64) -> ItemState {
        item_state(item, ExpiryCtx { folder: f, now_ms: now })
    }

    // L11: truth table rows 1-5.
    #[test]
    fn truth_table_rows() {
        let fo = folder(H24);
        let mut it = item(1, p(), v(1, p()));
        it.location.val.entered_at = T0;
        let due = T0 + 24 * HOUR_MS;
        // Rows 4 and 5 are split by the boundary: due - 1 ms / due.
        assert_eq!(st(&it, Some(&fo), due - 1), ItemState::Live { expires_at: due });
        assert_eq!(st(&it, Some(&fo), due), ItemState::Expired { binned_at: due });
        // Expiry never waits for the other device: a thousand hours late, still binned at due.
        assert_eq!(st(&it, Some(&fo), due + 1000 * HOUR_MS), ItemState::Expired { binned_at: due });
        // Row 3: Keep folder.
        assert_eq!(st(&it, Some(&folder(Retention::Keep)), due + HOUR_MS), ItemState::FolderKeep);
        // Row 2: pinned beats retention.
        let mut pinned = it.clone();
        pinned.pinned.val = true;
        assert_eq!(st(&pinned, Some(&fo), due + HOUR_MS), ItemState::Kept);
        // Row 1: deleted wins over everything (older pin).
        let mut del = pinned.clone();
        del.deleted = Versioned { v: v(5, p()), val: Some(T0 + 77) };
        del.pinned = Versioned { v: v(3, p()), val: true };
        assert_eq!(st(&del, Some(&fo), T0), ItemState::Deleted { binned_at: T0 + 77 });
    }

    // L11: E1 (pin newer than delete = not deleted before M5 runs).
    #[test]
    fn e1_pin_newer_than_delete_means_kept() {
        let mut it = item(1, p(), v(1, p()));
        it.deleted = Versioned { v: v(6, p()), val: Some(T0) };
        it.pinned = Versioned { v: v(8, f()), val: true };
        assert!(!effectively_deleted(&it));
        assert_eq!(st(&it, Some(&folder(H24)), T0), ItemState::Kept);
        // Pin older than delete: deleted.
        it.pinned.v = v(5, f());
        assert!(effectively_deleted(&it));
        // Equal versions: delete wins (strictly greater pin needed).
        it.pinned.v = v(6, p());
        assert!(effectively_deleted(&it));
        // Pin newer but pinned = false: still deleted.
        it.pinned = Versioned { v: v(9, f()), val: false };
        assert!(effectively_deleted(&it));
        // Not deleted at all.
        it.deleted.val = None;
        assert!(!effectively_deleted(&it));
    }

    // L11: E2 unknown folder and deleted folder -> Keep, never a surprise expiry.
    #[test]
    fn e2_unknown_and_deleted_folder_are_keep() {
        let it = item(1, p(), v(1, p()));
        assert_eq!(effective_retention(None), Retention::Keep);
        assert_eq!(st(&it, None, T0 + 1000 * HOUR_MS), ItemState::FolderKeep);
        let mut gone = folder(H24);
        gone.deleted = Versioned { v: v(2, f()), val: Some(T0) };
        assert_eq!(effective_retention(Some(&gone)), Retention::Keep);
        assert_eq!(st(&it, Some(&gone), T0 + 1000 * HOUR_MS), ItemState::FolderKeep);
        assert_eq!(effective_retention(Some(&folder(H24))), H24);
    }

    // A device that was away binned_at = due (the display value; the purge clock is local, see bin.rs).
    #[test]
    fn late_device_binned_at_is_the_due_time() {
        let fo = folder(H24);
        let mut it = item(1, p(), v(1, p()));
        it.location.val.entered_at = T0;
        let due = T0 + 24 * HOUR_MS;
        assert_eq!(st(&it, Some(&fo), due + 3 * 24 * HOUR_MS), ItemState::Expired { binned_at: due });
        assert_eq!(st(&it, Some(&fo), due), ItemState::Expired { binned_at: due });
    }

    #[test]
    fn bin_helper() {
        for s in [ItemState::Kept, ItemState::FolderKeep, ItemState::Live { expires_at: 5 }] {
            assert!(!in_bin(s));
        }
        assert!(in_bin(ItemState::Expired { binned_at: 0 }) && in_bin(ItemState::Deleted { binned_at: 0 }));
    }

    #[test]
    fn huge_retention_does_not_overflow() {
        let fo = folder(Retention::Secs { secs: u64::MAX });
        let it = item(1, p(), v(1, p()));
        assert!(matches!(st(&it, Some(&fo), T0), ItemState::Live { .. }));
    }

    // Design doc dated example: entered 10:00, Inbox 24 h, Bin 7 days.
    #[test]
    fn design_doc_example() {
        let fo = folder(H24);
        let mut it = item(1, p(), v(1, p()));
        let entered = T0;
        it.location.val.entered_at = entered;
        assert!(matches!(st(&it, Some(&fo), entered + 24 * HOUR_MS - 60_000), ItemState::Live { .. }));
        let s = st(&it, Some(&fo), entered + 24 * HOUR_MS);
        assert_eq!(s, ItemState::Expired { binned_at: entered + 24 * HOUR_MS });
    }
}
