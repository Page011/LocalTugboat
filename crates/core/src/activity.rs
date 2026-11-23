//! Activity history (local only): captured, received, moved (from, to), kept, expired, binned,
//! restored, purged, conflict lost, pairing, unpairing, paused for missing files - with time and
//! device.

use rusqlite::{params, Connection};
use uuid::Uuid;

use crate::{model::*, CoreError, Result};

/// Activity rows are kept for 30 days.
pub const KEEP_DAYS: i64 = 30;

/// Longest stored detail text (chars); longer text is cut, never rejected.
pub const MAX_DETAIL_CHARS: usize = 300;

/// Append one row.
pub fn log(c: &Connection, at: i64, device: Option<DeviceId>, item: Option<ItemId>, action: ActivityAction, detail: &str) -> Result<i64> {
    let detail: String = detail.chars().take(MAX_DETAIL_CHARS).collect();
    c.execute(
        "INSERT INTO activity (at, device_id, item_id, action, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            at,
            device.map(|d| d.0.to_vec()),
            item.map(|i| i.0.as_bytes().to_vec()),
            action_str(action),
            detail
        ],
    )?;
    Ok(c.last_insert_rowid())
}

fn parse_action(s: &str) -> Result<ActivityAction> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| CoreError::Invalid(format!("unknown activity action '{s}' in the database")))
}

/// Newest first; keyset paged on id (`before` = last id of the previous page).
pub fn list(c: &Connection, before: Option<i64>, limit: u32) -> Result<Vec<ActivityEntry>> {
    let mut st = c.prepare(
        "SELECT id, at, device_id, item_id, action, detail FROM activity
         WHERE (?1 IS NULL OR id < ?1) ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = st.query_map(params![before, limit.clamp(1, 500) as i64], |r| {
        let dev: Option<Vec<u8>> = r.get(2)?;
        let item: Option<Vec<u8>> = r.get(3)?;
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, dev, item, r.get::<_, String>(4)?, r.get::<_, String>(5)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, at, dev, item, action, detail) = row?;
        out.push(ActivityEntry {
            id,
            at,
            device: dev.and_then(|d| <[u8; 32]>::try_from(d).ok()).map(DeviceId),
            item: item.and_then(|i| <[u8; 16]>::try_from(i).ok()).map(|b| ItemId(Uuid::from_bytes(b))),
            action: parse_action(&action)?,
            detail,
        });
    }
    Ok(out)
}

/// The Activity screen's text: rows about one item say which one ("IMG_4.jpg - Inbox", moves
/// "IMG_4.jpg: Inbox -> Keep"), because four "Deleted - Inbox" rows cannot be told apart. Read
/// time, so a rename shows the current name and the stored detail stays as logged. Rename rows
/// already carry both names.
pub fn name_items(c: &Connection, mut rows: Vec<ActivityEntry>) -> Result<Vec<ActivityEntry>> {
    for r in &mut rows {
        let Some(id) = r.item.filter(|_| r.action != ActivityAction::Renamed) else { continue };
        let Some((rec, _)) = crate::store::get_item(c, id)? else { continue };
        // A deleted-forever item has no name left: its rows keep the detail they were logged with.
        if rec.name.val.is_empty() {
            continue;
        }
        let name = rec.name.val;
        r.detail = match (r.action, r.detail.is_empty()) {
            (_, true) => name,
            (ActivityAction::Moved, false) => format!("{name}: {}", r.detail),
            (_, false) => format!("{name} - {}", r.detail),
        };
    }
    Ok(rows)
}

/// Delete rows with at < now - KEEP_DAYS. Returns rows deleted.
pub fn trim(c: &Connection, now: i64) -> Result<usize> {
    let cutoff = now - KEEP_DAYS * crate::clock::DAY_MS;
    Ok(c.execute("DELETE FROM activity WHERE at < ?1", [cutoff])?)
}

/// Stable snake_case text stored in activity.action (same as serde).
pub fn action_str(a: ActivityAction) -> &'static str {
    match a {
        ActivityAction::Captured => "captured",
        ActivityAction::Added => "added",
        ActivityAction::Received => "received",
        ActivityAction::Moved => "moved",
        ActivityAction::Renamed => "renamed",
        ActivityAction::Kept => "kept",
        ActivityAction::Unkept => "unkept",
        ActivityAction::Edited => "edited",
        ActivityAction::Deleted => "deleted",
        ActivityAction::Binned => "binned",
        ActivityAction::Expired => "expired",
        ActivityAction::Restored => "restored",
        ActivityAction::Returned => "returned",
        ActivityAction::Purged => "purged",
        ActivityAction::DeletedForever => "deleted_forever",
        ActivityAction::Refused => "refused",
        ActivityAction::Conflict => "conflict",
        ActivityAction::Lost => "lost",
        ActivityAction::FolderCreated => "folder_created",
        ActivityAction::FolderRenamed => "folder_renamed",
        ActivityAction::FolderDeleted => "folder_deleted",
        ActivityAction::RetentionChanged => "retention_changed",
        ActivityAction::Paired => "paired",
        ActivityAction::Unpaired => "unpaired",
        ActivityAction::Paused => "paused",
        ActivityAction::Resumed => "resumed",
        ActivityAction::SyncError => "sync_error",
        ActivityAction::Upgraded => "upgraded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::DAY_MS;
    use crate::db::Db;
    use crate::store::test_support::*;

    const ALL: [ActivityAction; 27] = [
        ActivityAction::Captured,
        ActivityAction::Added,
        ActivityAction::Received,
        ActivityAction::Moved,
        ActivityAction::Renamed,
        ActivityAction::Kept,
        ActivityAction::Unkept,
        ActivityAction::Edited,
        ActivityAction::Deleted,
        ActivityAction::Binned,
        ActivityAction::Expired,
        ActivityAction::Restored,
        ActivityAction::Returned,
        ActivityAction::Purged,
        ActivityAction::Refused,
        ActivityAction::Conflict,
        ActivityAction::Lost,
        ActivityAction::FolderCreated,
        ActivityAction::FolderRenamed,
        ActivityAction::FolderDeleted,
        ActivityAction::RetentionChanged,
        ActivityAction::Paired,
        ActivityAction::Unpaired,
        ActivityAction::Paused,
        ActivityAction::Resumed,
        ActivityAction::SyncError,
        ActivityAction::Upgraded,
    ];

    // The stored text must equal the serde name, or listing would fail to parse old rows.
    #[test]
    fn action_str_matches_serde_for_every_variant() {
        for a in ALL {
            let serde_name = serde_json::to_value(a).unwrap();
            assert_eq!(serde_name.as_str().unwrap(), action_str(a));
            assert_eq!(parse_action(action_str(a)).unwrap(), a);
        }
        assert!(parse_action("nope").is_err());
    }

    // Trim at the 30-day boundary; paging.
    #[test]
    fn trim_boundary_and_paging() {
        let db = Db::open_in_memory().unwrap();
        let now = T0 + 100 * DAY_MS;
        let cutoff = now - KEEP_DAYS * DAY_MS;
        log(&db.conn, cutoff - 1, None, None, ActivityAction::Paired, "too old").unwrap();
        log(&db.conn, cutoff, Some(p()), None, ActivityAction::Moved, "exactly 30 days").unwrap();
        log(&db.conn, now, Some(f()), Some(ItemId(Uuid::from_u128(1))), ActivityAction::Kept, "new").unwrap();
        assert_eq!(trim(&db.conn, now).unwrap(), 1);
        let rows = list(&db.conn, None, 10).unwrap();
        assert_eq!(rows.iter().map(|r| r.detail.as_str()).collect::<Vec<_>>(), vec!["new", "exactly 30 days"]);
        assert_eq!(rows[0].device, Some(f()));
        assert_eq!(rows[0].item, Some(ItemId(Uuid::from_u128(1))));
        assert_eq!(rows[1].action, ActivityAction::Moved);
        assert_eq!(trim(&db.conn, now).unwrap(), 0, "second trim is a no-op");

        for i in 0..5 {
            log(&db.conn, now + i, None, None, ActivityAction::Added, &format!("n{i}")).unwrap();
        }
        let p1 = list(&db.conn, None, 3).unwrap();
        assert_eq!(p1.iter().map(|r| r.detail.as_str()).collect::<Vec<_>>(), vec!["n4", "n3", "n2"]);
        let p2 = list(&db.conn, Some(p1.last().unwrap().id), 3).unwrap();
        assert_eq!(p2.iter().map(|r| r.detail.as_str()).collect::<Vec<_>>(), vec!["n1", "n0", "new"]);
        let p3 = list(&db.conn, Some(p2.last().unwrap().id), 3).unwrap();
        assert_eq!(p3.len(), 1);
    }

    #[test]
    fn long_detail_is_cut_not_rejected() {
        let db = Db::open_in_memory().unwrap();
        let long = "x".repeat(MAX_DETAIL_CHARS + 50);
        log(&db.conn, T0, None, None, ActivityAction::Conflict, &long).unwrap();
        assert_eq!(list(&db.conn, None, 1).unwrap()[0].detail.chars().count(), MAX_DETAIL_CHARS);
    }

    #[test]
    fn rows_about_an_item_name_it() {
        let mut r = crate::store::test_support::Rig::new();
        let id = r.add("holiday");
        let (n, d) = ("holiday.jpg", &r.db.conn);
        log(d, T0, None, Some(id), ActivityAction::Deleted, "Inbox").unwrap();
        log(d, T0, None, Some(id), ActivityAction::Moved, "Inbox -> Keep").unwrap();
        log(d, T0, None, Some(id), ActivityAction::Renamed, "a.jpg -> b.jpg").unwrap();
        log(d, T0, None, Some(id), ActivityAction::Lost, "").unwrap();
        log(d, T0, None, None, ActivityAction::Paused, "LIBRARY FOLDER NOT FOUND").unwrap();
        let rows = name_items(d, list(d, None, 10).unwrap()).unwrap();
        let text: Vec<&str> = rows.iter().take(5).map(|r| r.detail.as_str()).collect();
        // Newest first (the add's own row is below these five).
        assert_eq!(text, ["LIBRARY FOLDER NOT FOUND", n, "a.jpg -> b.jpg", &format!("{n}: Inbox -> Keep"), &format!("{n} - Inbox")]);
    }
}
