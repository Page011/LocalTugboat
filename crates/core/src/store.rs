//! Typed table access. The only module (with `activity`) that runs SQL.
//! Every function takes `&rusqlite::Connection` (a `Transaction` derefs to it), so callers
//! group writes into one transaction. No function here applies a rule: rules live in
//! merge/expiry/bin/ops and call these.

use rusqlite::{params, types::Type, Connection, OptionalExtension, Row};
use uuid::Uuid;

use crate::{model::*, CoreError, Result};

// ---------- column helpers ----------

fn to_i(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

fn to_u(v: i64) -> u64 {
    v.max(0) as u64
}

fn bad(idx: usize, ty: Type, msg: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, ty, msg.to_string().into())
}

fn b32(r: &Row, idx: usize) -> rusqlite::Result<[u8; 32]> {
    let v: Vec<u8> = r.get(idx)?;
    v.try_into().map_err(|_| bad(idx, Type::Blob, "expected 32 bytes"))
}

fn b16(r: &Row, idx: usize) -> rusqlite::Result<[u8; 16]> {
    let v: Vec<u8> = r.get(idx)?;
    v.try_into().map_err(|_| bad(idx, Type::Blob, "expected 16 bytes"))
}

fn opt_b32(r: &Row, idx: usize) -> rusqlite::Result<Option<[u8; 32]>> {
    let v: Option<Vec<u8>> = r.get(idx)?;
    match v {
        None => Ok(None),
        Some(v) => v.try_into().map(Some).map_err(|_| bad(idx, Type::Blob, "expected 32 bytes")),
    }
}

fn version(r: &Row, lamport_idx: usize, dev_idx: usize) -> rusqlite::Result<Version> {
    Ok(Version { lamport: to_u(r.get(lamport_idx)?), device: DeviceId(b32(r, dev_idx)?) })
}

fn media_str(k: MediaKind) -> &'static str {
    match k {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::File => "file",
    }
}

fn media_parse(idx: usize, s: &str) -> rusqlite::Result<MediaKind> {
    match s {
        "image" => Ok(MediaKind::Image),
        "video" => Ok(MediaKind::Video),
        "file" => Ok(MediaKind::File),
        _ => Err(bad(idx, Type::Text, "unknown media kind")),
    }
}

pub fn lfs_str(s: LocalFileState) -> &'static str {
    match s {
        LocalFileState::Pending => "pending",
        LocalFileState::Present => "present",
        LocalFileState::Binned => "binned",
        LocalFileState::Purged => "purged",
        LocalFileState::Lost => "lost",
        LocalFileState::Refused => "refused",
    }
}

fn lfs_parse(idx: usize, s: &str) -> rusqlite::Result<LocalFileState> {
    Ok(match s {
        "pending" => LocalFileState::Pending,
        "present" => LocalFileState::Present,
        "binned" => LocalFileState::Binned,
        "purged" => LocalFileState::Purged,
        "lost" => LocalFileState::Lost,
        "refused" => LocalFileState::Refused,
        _ => return Err(bad(idx, Type::Text, "unknown local file state")),
    })
}

fn folder_kind_str(k: FolderKind) -> &'static str {
    match k {
        FolderKind::Inbox => "inbox",
        FolderKind::Keep => "keep",
        FolderKind::Unsorted => "unsorted",
        FolderKind::User => "user",
    }
}

fn folder_kind_parse(idx: usize, s: &str) -> rusqlite::Result<FolderKind> {
    Ok(match s {
        "inbox" => FolderKind::Inbox,
        "keep" => FolderKind::Keep,
        "unsorted" => FolderKind::Unsorted,
        "user" => FolderKind::User,
        _ => return Err(bad(idx, Type::Text, "unknown folder kind")),
    })
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::Pc => "pc",
        Role::Phone => "phone",
    }
}

fn role_parse(idx: usize, s: &str) -> rusqlite::Result<Role> {
    match s {
        "pc" => Ok(Role::Pc),
        "phone" => Ok(Role::Phone),
        _ => Err(bad(idx, Type::Text, "unknown role")),
    }
}

fn retention_to_sql(r: Retention) -> Option<i64> {
    match r {
        Retention::Keep => None,
        Retention::Secs { secs } => Some(to_i(secs)),
    }
}

fn retention_from_sql(v: Option<i64>) -> Retention {
    match v {
        Some(s) if s > 0 => Retention::Secs { secs: s as u64 },
        _ => Retention::Keep,
    }
}

// ---------- lamport (a local edit sets lamport to max(seen) + 1) ----------

/// Highest lamport seen or issued by this device.
pub fn max_lamport(c: &Connection) -> Result<u64> {
    let v: i64 = c.query_row("SELECT value FROM lamport WHERE id = 1", [], |r| r.get(0))?;
    Ok(to_u(v))
}

/// Largest lamport this device accepts from a peer. Far above any real edit count; a value near
/// i64::MAX would pin the clock so `next_version` (value + 1) fails and every local write with it.
pub const MAX_LAMPORT: u64 = 1 << 62;

/// Raise the stored lamport to `seen` if higher (every remote version and the handshake's
/// max_lamport go through this BEFORE any local write in the same session). A value above
/// `MAX_LAMPORT` is a protocol error and writes nothing.
pub fn observe_lamport(c: &Connection, seen: u64) -> Result<()> {
    if seen > MAX_LAMPORT {
        return Err(CoreError::Protocol("version out of range".into()));
    }
    c.execute("UPDATE lamport SET value = MAX(value, ?1) WHERE id = 1", [to_i(seen)])?;
    Ok(())
}

/// New version for a local write: lamport = max + 1 (persisted), device = self.
pub fn next_version(c: &Connection, self_id: DeviceId) -> Result<Version> {
    let v: i64 = c.query_row("UPDATE lamport SET value = value + 1 WHERE id = 1 RETURNING value", [], |r| r.get(0))?;
    Ok(Version { lamport: to_u(v), device: self_id })
}

// ---------- devices ----------

fn device_from_row(r: &Row) -> rusqlite::Result<DeviceRow> {
    let role: String = r.get(3)?;
    let addrs: String = r.get(8)?;
    Ok(DeviceRow {
        id: DeviceId(b32(r, 0)?),
        name: r.get(1)?,
        is_self: r.get::<_, i64>(2)? == 1,
        role: role_parse(3, &role)?,
        paired_at: r.get(4)?,
        unpaired_at: r.get(5)?,
        last_seen_at: r.get(6)?,
        last_synced_at: r.get(7)?,
        addresses: serde_json::from_str(&addrs).map_err(|_| bad(8, Type::Text, "addresses is not a JSON array"))?,
    })
}

const DEVICE_COLS: &str = "id, name, is_self, role, paired_at, unpaired_at, last_seen_at, last_synced_at, addresses";

/// Insert the self row if missing; update name if changed. Errors if a different self key
/// is already stored (key changed under an existing db = new identity: caller decides).
pub fn ensure_self(c: &Connection, id: DeviceId, name: &str, role: Role) -> Result<()> {
    let existing: Option<Vec<u8>> = c.query_row("SELECT id FROM device WHERE is_self = 1", [], |r| r.get(0)).optional()?;
    match existing {
        Some(e) if e != id.0 => Err(CoreError::Invalid(format!(
            "this database belongs to device {}, not {}",
            hex::encode(&e[..e.len().min(5)]),
            &hex::encode(id.0)[..10]
        ))),
        Some(_) => {
            c.execute("UPDATE device SET name = ?2, role = ?3 WHERE id = ?1", params![&id.0[..], name, role_str(role)])?;
            Ok(())
        }
        None => {
            c.execute(
                "INSERT INTO device (id, name, is_self, role) VALUES (?1, ?2, 1, ?3)",
                params![&id.0[..], name, role_str(role)],
            )?;
            Ok(())
        }
    }
}

pub fn self_device(c: &Connection) -> Result<DeviceRow> {
    c.query_row(&format!("SELECT {DEVICE_COLS} FROM device WHERE is_self = 1"), [], device_from_row)
        .optional()?
        .ok_or(CoreError::NotFound { what: "self device", id: "-".into() })
}

pub fn get_device(c: &Connection, id: DeviceId) -> Result<Option<DeviceRow>> {
    Ok(c.query_row(&format!("SELECT {DEVICE_COLS} FROM device WHERE id = ?1"), [&id.0[..]], device_from_row).optional()?)
}

/// Upsert a peer as paired (paired_at = now, unpaired_at = NULL).
pub fn set_paired(c: &Connection, peer: &DeviceRow, now: i64) -> Result<()> {
    if peer.is_self {
        return Err(CoreError::Invalid("cannot pair with this device itself".into()));
    }
    let addrs = serde_json::to_string(&peer.addresses).map_err(|e| CoreError::Invalid(e.to_string()))?;
    c.execute(
        "INSERT INTO device (id, name, is_self, role, paired_at, unpaired_at, last_seen_at, addresses)
         VALUES (?1, ?2, 0, ?3, ?4, NULL, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, role = excluded.role,
           paired_at = excluded.paired_at, unpaired_at = NULL, addresses = excluded.addresses",
        params![&peer.id.0[..], peer.name, role_str(peer.role), now, peer.last_seen_at, addrs],
    )?;
    Ok(())
}

/// unpaired_at = now. The row stays: its receipts still count ("is or was paired").
pub fn set_unpaired(c: &Connection, peer: DeviceId, now: i64) -> Result<()> {
    let n = c.execute("UPDATE device SET unpaired_at = ?2 WHERE id = ?1 AND is_self = 0", params![&peer.0[..], now])?;
    if n == 0 {
        return Err(CoreError::NotFound { what: "device", id: hex::encode(peer.0) });
    }
    Ok(())
}

/// The currently paired peer (paired_at set, unpaired_at NULL), if any. v1 has at most one.
pub fn paired_peer(c: &Connection) -> Result<Option<DeviceRow>> {
    Ok(c.query_row(
        &format!(
            "SELECT {DEVICE_COLS} FROM device WHERE is_self = 0 AND paired_at IS NOT NULL AND unpaired_at IS NULL
             ORDER BY paired_at DESC LIMIT 1"
        ),
        [],
        device_from_row,
    )
    .optional()?)
}

/// Every currently paired peer, oldest link first (first stored first when linked in the same
/// millisecond). A phone holds up to `MAX_LINKED`.
pub fn paired_peers(c: &Connection) -> Result<Vec<DeviceRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {DEVICE_COLS} FROM device WHERE is_self = 0 AND paired_at IS NOT NULL AND unpaired_at IS NULL
         ORDER BY paired_at, rowid"
    ))?;
    let rows = st.query_map([], device_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// How many peers other than `id` are currently paired (the limit check never counts the
/// device that is linking again).
pub fn count_paired_except(c: &Connection, id: DeviceId) -> Result<u32> {
    let n: i64 = c.query_row(
        "SELECT count(*) FROM device WHERE id <> ?1 AND is_self = 0 AND paired_at IS NOT NULL AND unpaired_at IS NULL",
        [&id.0[..]],
        |r| r.get(0),
    )?;
    Ok(n as u32)
}

/// True if `id` is currently paired (the net AccessGate).
pub fn is_paired(c: &Connection, id: DeviceId) -> Result<bool> {
    let n: i64 = c.query_row(
        "SELECT count(*) FROM device WHERE id = ?1 AND is_self = 0 AND paired_at IS NOT NULL AND unpaired_at IS NULL",
        [&id.0[..]],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Move `last_seen_at` of a peer to `now` without touching its stored addresses (the session
/// ended; the hello already stored the addresses). Never moves the time backwards.
pub fn touch_last_seen(c: &Connection, peer: DeviceId, now: i64) -> Result<()> {
    c.execute(
        "UPDATE device SET last_seen_at = MAX(COALESCE(last_seen_at, 0), ?2) WHERE id = ?1 AND is_self = 0",
        params![&peer.0[..], now],
    )?;
    Ok(())
}

/// Move a peer's `last_synced_at` to `at`, never backwards.
pub fn set_last_synced(c: &Connection, peer: DeviceId, at: i64) -> Result<()> {
    c.execute(
        "UPDATE device SET last_synced_at = MAX(COALESCE(last_synced_at, 0), ?2) WHERE id = ?1 AND is_self = 0",
        params![&peer.0[..], at],
    )?;
    Ok(())
}

pub fn set_last_seen(c: &Connection, peer: DeviceId, now: i64, addresses: &[String]) -> Result<()> {
    let addrs = serde_json::to_string(addresses).map_err(|e| CoreError::Invalid(e.to_string()))?;
    c.execute(
        "UPDATE device SET last_seen_at = ?2, addresses = ?3 WHERE id = ?1 AND is_self = 0",
        params![&peer.0[..], now, addrs],
    )?;
    Ok(())
}

// ---------- records ----------

const ITEM_COLS: &str = "id, captured_at, origin_device, lat, lon, accuracy_m, name, name_lamport, name_dev, \
    blob_hash, size, media_kind, ext, thumb_hash, thumb_size, content_lamport, content_dev, \
    folder_id, folder_entered_at, placement_lamport, placement_dev, pinned, pinned_lamport, pinned_dev, \
    deleted_at, deleted_lamport, deleted_dev, purged_at, purged_lamport, purged_dev, local_file_state";

fn item_from_row(r: &Row) -> rusqlite::Result<(ItemRecord, LocalFileState)> {
    let lat: Option<f64> = r.get(3)?;
    let lon: Option<f64> = r.get(4)?;
    let acc: Option<f64> = r.get(5)?;
    let geo = match (lat, lon) {
        (Some(lat), Some(lon)) => Some(Geo { lat, lon, accuracy_m: acc.unwrap_or(0.0) }),
        _ => None,
    };
    let thumb = match (opt_b32(r, 13)?, r.get::<_, Option<i64>>(14)?) {
        (Some(h), Some(sz)) => Some(Thumb { hash: BlobHash(h), size: to_u(sz) }),
        _ => None,
    };
    let kind: String = r.get(11)?;
    let lfs: String = r.get(30)?;
    let purged = match r.get::<_, Option<i64>>(27)? {
        Some(at) => Some(Versioned { v: version(r, 28, 29)?, val: at }),
        None => None,
    };
    let rec = ItemRecord {
        id: ItemId(Uuid::from_bytes(b16(r, 0)?)),
        captured_at: r.get(1)?,
        origin: DeviceId(b32(r, 2)?),
        geo,
        name: Versioned { v: version(r, 7, 8)?, val: r.get(6)? },
        content: Versioned {
            v: version(r, 15, 16)?,
            val: Content {
                hash: BlobHash(b32(r, 9)?),
                size: to_u(r.get(10)?),
                kind: media_parse(11, &kind)?,
                ext: r.get(12)?,
                thumb,
            },
        },
        location: Versioned {
            v: version(r, 19, 20)?,
            val: Location { folder: FolderId(Uuid::from_bytes(b16(r, 17)?)), entered_at: r.get(18)? },
        },
        pinned: Versioned { v: version(r, 22, 23)?, val: r.get::<_, i64>(21)? == 1 },
        deleted: Versioned { v: version(r, 25, 26)?, val: r.get(24)? },
        purged,
    };
    Ok((rec, lfs_parse(30, &lfs)?))
}

/// Items held here and not deleted (nor purged) that some device in `devices` has no receipt for
/// (for the item's current hash), each with one flag per device (true = that device has none). One
/// pass that also reads the whole row, so the caller needs no second lookup per item.
pub fn items_missing_receipts(c: &Connection, devices: &[DeviceId]) -> Result<Vec<(ItemRecord, Vec<bool>)>> {
    if devices.is_empty() {
        return Ok(Vec::new());
    }
    // The ids are 32-byte keys from our own table, written as hex literals like the other fixed fragments here.
    let flags: Vec<String> = devices
        .iter()
        .enumerate()
        .map(|(i, d)| format!("NOT EXISTS (SELECT 1 FROM receipt r WHERE r.blob_hash = item.blob_hash AND r.device_id = x'{}') AS m{i}", hex::encode(d.0)))
        .collect();
    let names: Vec<String> = (0..devices.len()).map(|i| format!("m{i}")).collect();
    let sql = format!(
        "SELECT * FROM (SELECT {ITEM_COLS}, {} FROM item WHERE local_file_state = 'present' AND deleted_at IS NULL AND purged_at IS NULL) WHERE {}",
        flags.join(", "),
        names.join(" OR ")
    );
    let mut st = c.prepare(&sql)?;
    let rows = st
        .query_map([], |r| {
            let (rec, _) = item_from_row(r)?;
            let missing = (0..devices.len()).map(|i| r.get::<_, bool>(31 + i)).collect::<rusqlite::Result<Vec<bool>>>()?;
            Ok((rec, missing))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Item record plus its local file state.
pub fn get_item(c: &Connection, id: ItemId) -> Result<Option<(ItemRecord, LocalFileState)>> {
    Ok(c.query_row(&format!("SELECT {ITEM_COLS} FROM item WHERE id = ?1"), [id.0.as_bytes().as_slice()], item_from_row).optional()?)
}

/// Insert a new item with all groups. Fails if the id exists.
pub fn insert_item(c: &Connection, r: &ItemRecord, local: LocalFileState, now: i64) -> Result<()> {
    let (lat, lon, acc) = match r.geo {
        Some(g) => (Some(g.lat), Some(g.lon), Some(g.accuracy_m)),
        None => (None, None, None),
    };
    let (th, ts) = match &r.content.val.thumb {
        Some(t) => (Some(t.hash.0.to_vec()), Some(to_i(t.size))),
        None => (None, None),
    };
    c.execute(
        &format!(
            "INSERT INTO item ({ITEM_COLS}, local_state_at) VALUES
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21,
              ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32)"
        ),
        params![
            r.id.0.as_bytes().as_slice(),
            r.captured_at,
            &r.origin.0[..],
            lat,
            lon,
            acc,
            r.name.val,
            to_i(r.name.v.lamport),
            &r.name.v.device.0[..],
            &r.content.val.hash.0[..],
            to_i(r.content.val.size),
            media_str(r.content.val.kind),
            r.content.val.ext,
            th,
            ts,
            to_i(r.content.v.lamport),
            &r.content.v.device.0[..],
            r.location.val.folder.0.as_bytes().as_slice(),
            r.location.val.entered_at,
            to_i(r.location.v.lamport),
            &r.location.v.device.0[..],
            r.pinned.val as i64,
            to_i(r.pinned.v.lamport),
            &r.pinned.v.device.0[..],
            r.deleted.val,
            to_i(r.deleted.v.lamport),
            &r.deleted.v.device.0[..],
            r.purged.as_ref().map(|p| p.val),
            r.purged.as_ref().map(|p| to_i(p.v.lamport)),
            r.purged.as_ref().map(|p| p.v.device.0.to_vec()),
            lfs_str(local),
            now,
        ],
    )?;
    Ok(())
}

/// Overwrite all field groups (values + versions) of an existing item. Local columns untouched. The
/// saved location is written too: it only ever changes when a purge scrubs the record.
pub fn update_item(c: &Connection, r: &ItemRecord) -> Result<()> {
    let (th, ts) = match &r.content.val.thumb {
        Some(t) => (Some(t.hash.0.to_vec()), Some(to_i(t.size))),
        None => (None, None),
    };
    let (lat, lon, acc) = match r.geo {
        Some(g) => (Some(g.lat), Some(g.lon), Some(g.accuracy_m)),
        None => (None, None, None),
    };
    let n = c.execute(
        "UPDATE item SET
           lat = ?26, lon = ?27, accuracy_m = ?28,
           name = ?2, name_lamport = ?3, name_dev = ?4,
           blob_hash = ?5, size = ?6, media_kind = ?7, ext = ?8, thumb_hash = ?9, thumb_size = ?10,
           content_lamport = ?11, content_dev = ?12,
           folder_id = ?13, folder_entered_at = ?14, placement_lamport = ?15, placement_dev = ?16,
           pinned = ?17, pinned_lamport = ?18, pinned_dev = ?19,
           deleted_at = ?20, deleted_lamport = ?21, deleted_dev = ?22,
           purged_at = ?23, purged_lamport = ?24, purged_dev = ?25
         WHERE id = ?1",
        params![
            r.id.0.as_bytes().as_slice(),
            r.name.val,
            to_i(r.name.v.lamport),
            &r.name.v.device.0[..],
            &r.content.val.hash.0[..],
            to_i(r.content.val.size),
            media_str(r.content.val.kind),
            r.content.val.ext,
            th,
            ts,
            to_i(r.content.v.lamport),
            &r.content.v.device.0[..],
            r.location.val.folder.0.as_bytes().as_slice(),
            r.location.val.entered_at,
            to_i(r.location.v.lamport),
            &r.location.v.device.0[..],
            r.pinned.val as i64,
            to_i(r.pinned.v.lamport),
            &r.pinned.v.device.0[..],
            r.deleted.val,
            to_i(r.deleted.v.lamport),
            &r.deleted.v.device.0[..],
            r.purged.as_ref().map(|p| p.val),
            r.purged.as_ref().map(|p| to_i(p.v.lamport)),
            r.purged.as_ref().map(|p| p.v.device.0.to_vec()),
            lat,
            lon,
            acc,
        ],
    )?;
    if n == 0 {
        return Err(CoreError::NotFound { what: "item", id: r.id.0.to_string() });
    }
    Ok(())
}

pub fn set_local_state(c: &Connection, id: ItemId, s: LocalFileState, now: i64) -> Result<()> {
    let n = c.execute(
        "UPDATE item SET local_file_state = ?2, local_state_at = ?3 WHERE id = ?1",
        params![id.0.as_bytes().as_slice(), lfs_str(s), now],
    )?;
    if n == 0 {
        return Err(CoreError::NotFound { what: "item", id: id.0.to_string() });
    }
    Ok(())
}

/// When this device last changed the item's local file state (unix ms, this device's clock).
/// For a Binned item that is the moment its file entered THIS device's Bin: the Bin clock.
pub fn local_state_at(c: &Connection, id: ItemId) -> Result<Option<i64>> {
    Ok(c.query_row("SELECT local_state_at FROM item WHERE id = ?1", [id.0.as_bytes().as_slice()], |r| r.get(0)).optional()?)
}

/// Queue a thumbnail for deletion (see `thumb_trash` in the v4 schema). The file goes once no live
/// item uses it.
pub fn trash_thumb(c: &Connection, hash: BlobHash) -> Result<()> {
    c.execute("INSERT OR IGNORE INTO thumb_trash (hash) VALUES (?1)", [&hash.0[..]])?;
    Ok(())
}

/// Thumbnails waiting for deletion that no live item uses: the files to remove now. Queued hashes a
/// live item still uses are dropped from the queue.
pub fn thumbs_to_delete(c: &Connection) -> Result<Vec<BlobHash>> {
    c.execute(
        "DELETE FROM thumb_trash WHERE EXISTS (SELECT 1 FROM item WHERE thumb_hash = thumb_trash.hash AND purged_at IS NULL)",
        [],
    )?;
    let mut st = c.prepare("SELECT hash FROM thumb_trash")?;
    let rows = st.query_map([], |r| Ok(BlobHash(b32(r, 0)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The thumbnail file is gone (or was never there): take it off the queue.
pub fn thumb_deleted(c: &Connection, hash: BlobHash) -> Result<()> {
    c.execute("DELETE FROM thumb_trash WHERE hash = ?1", [&hash.0[..]])?;
    Ok(())
}

/// Does an item other than `except` still want a download of `hash` (Pending or Lost, not deleted
/// forever)? Then the partial file for that hash is kept.
pub fn wanted_by_other(c: &Connection, hash: BlobHash, except: ItemId) -> Result<bool> {
    let n: i64 = c.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM item WHERE blob_hash = ?1 AND id != ?2 AND purged_at IS NULL AND local_file_state IN ('pending', 'lost') LIMIT 1)",
        rusqlite::params![&hash.0[..], except.0.as_bytes().as_slice()],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Items whose content hash is `hash` and whose local state is Present or Binned
/// (blob source for the sender; dedupe on receive).
pub fn items_holding(c: &Connection, hash: BlobHash) -> Result<Vec<ItemId>> {
    let mut st = c.prepare("SELECT id FROM item WHERE blob_hash = ?1 AND local_file_state IN ('present', 'binned') AND purged_at IS NULL ORDER BY id")?;
    let rows = st.query_map([&hash.0[..]], |r| Ok(ItemId(Uuid::from_bytes(b16(r, 0)?))))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Ids of items in the given local states, oldest captured first, at most `limit`.
pub fn items_in_state(c: &Connection, states: &[LocalFileState], limit: u32) -> Result<Vec<ItemId>> {
    Ok(items_in_state_after(c, states, None, limit, None)?.into_iter().map(|(_, id)| id).collect())
}

/// The device `?4` can serve this item's file: it wrote the content, or it has a receipt for the
/// hash. A `NULL` holder means no filter. The receipt key (blob_hash, device_id) makes the
/// lookup an index hit.
const HELD_BY: &str = "(?4 IS NULL OR content_dev = ?4 OR EXISTS (SELECT 1 FROM receipt r WHERE r.blob_hash = item.blob_hash AND r.device_id = ?4))";

/// Same order, as (captured_at, id), starting after `after` (a keyset cursor taken from the last
/// row of the previous page), so a caller can walk past rows it skips. With `holder`, only items
/// whose file that device holds.
pub fn items_in_state_after(
    c: &Connection,
    states: &[LocalFileState],
    after: Option<(i64, ItemId)>,
    limit: u32,
    holder: Option<DeviceId>,
) -> Result<Vec<(i64, ItemId)>> {
    if states.is_empty() {
        return Ok(Vec::new());
    }
    // The strings come from lfs_str (fixed set), never from outside, so inlining is safe.
    let list = states.iter().map(|s| format!("'{}'", lfs_str(*s))).collect::<Vec<_>>().join(", ");
    let mut st = c.prepare(&format!(
        "SELECT captured_at, id FROM item WHERE local_file_state IN ({list}) AND purged_at IS NULL AND {HELD_BY}
           AND (?2 IS NULL OR captured_at > ?2 OR (captured_at = ?2 AND id > ?3)) ORDER BY captured_at, id LIMIT ?1"
    ))?;
    let (cap, id): (Option<i64>, Option<Vec<u8>>) = match after {
        Some((cap, id)) => (Some(cap), Some(id.0.as_bytes().to_vec())),
        None => (None, None),
    };
    let holder = holder.map(|d| d.0.to_vec());
    let rows = st.query_map(params![limit as i64, cap, id, holder], |r| Ok((r.get::<_, i64>(0)?, ItemId(Uuid::from_bytes(b16(r, 1)?)))))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// True if any Pending item here is one `peer` can serve (a download still waiting on that device).
pub fn pending_held_by(c: &Connection, peer: DeviceId) -> Result<bool> {
    let n: i64 = c.query_row(
        &format!("SELECT count(*) FROM (SELECT 1 FROM item WHERE local_file_state = 'pending' AND purged_at IS NULL AND {HELD_BY} LIMIT 1)"),
        params![0, 0, 0, &peer.0[..]],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Pending items here, and how many of them each of `peers` can serve (in the order given).
/// One scan for all devices: the status reads it after every session message.
pub fn pending_counts(c: &Connection, peers: &[DeviceId]) -> Result<(u32, Vec<u32>)> {
    let sums: String = peers
        .iter()
        .map(|d| {
            let h = hex::encode(d.0);
            format!(", COALESCE(SUM(content_dev = x'{h}' OR EXISTS (SELECT 1 FROM receipt r WHERE r.blob_hash = item.blob_hash AND r.device_id = x'{h}')), 0)")
        })
        .collect();
    let sql = format!("SELECT count(*){sums} FROM item WHERE local_file_state = 'pending' AND purged_at IS NULL");
    Ok(c.query_row(&sql, [], |r| {
        let all: i64 = r.get(0)?;
        let per = (0..peers.len()).map(|i| r.get::<_, i64>(i + 1).map(|n| n as u32)).collect::<rusqlite::Result<Vec<u32>>>()?;
        Ok((all as u32, per))
    })?)
}

/// Ids of items matching a raw WHERE fragment of fixed text (no user input), for the sweep
/// and the views: candidates are narrowed here, rules are applied by the callers.
pub fn item_ids_where(c: &Connection, fixed_where: &str) -> Result<Vec<ItemId>> {
    let mut st = c.prepare(&format!("SELECT id FROM item WHERE {fixed_where} ORDER BY captured_at, id"))?;
    let rows = st.query_map([], |r| Ok(ItemId(Uuid::from_bytes(b16(r, 0)?))))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Ids of items whose placement folder is `folder` (any state).
pub fn item_ids_in_folder(c: &Connection, folder: FolderId) -> Result<Vec<ItemId>> {
    let mut st = c.prepare("SELECT id FROM item WHERE folder_id = ?1 AND purged_at IS NULL ORDER BY captured_at, id")?;
    let rows = st.query_map([folder.0.as_bytes().as_slice()], |r| Ok(ItemId(Uuid::from_bytes(b16(r, 0)?))))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Total number of item records (tombstones included).
pub fn count_items(c: &Connection) -> Result<u64> {
    let n: i64 = c.query_row("SELECT count(*) FROM item", [], |r| r.get(0))?;
    Ok(to_u(n))
}

/// One page of items for a view (sort captured_at DESC, id DESC; keyset paging on `after`).
/// The bin flag is applied by the caller with `expiry::item_state`, so this returns records;
/// see bin::page for the Bin view.
pub fn items_page(c: &Connection, folder: Option<FolderId>, after: Option<ItemId>, limit: u32) -> Result<Vec<(ItemRecord, LocalFileState)>> {
    let f = folder.map(|f| f.0.as_bytes().to_vec());
    let a = after.map(|a| a.0.as_bytes().to_vec());
    let mut st = c.prepare(&format!(
        "SELECT {ITEM_COLS} FROM item
         WHERE purged_at IS NULL AND (?1 IS NULL OR folder_id = ?1)
           AND (?2 IS NULL OR (captured_at, id) < (SELECT captured_at, id FROM item WHERE id = ?2))
         ORDER BY captured_at DESC, id DESC LIMIT ?3"
    ))?;
    let rows = st.query_map(params![f, a, limit.clamp(1, 500) as i64], item_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

const FOLDER_COLS: &str = "id, kind, name, name_lamport, name_dev, retention_secs, retention_lamport, retention_dev, \
    deleted_at, deleted_lamport, deleted_dev";

fn folder_from_row(r: &Row) -> rusqlite::Result<FolderRecord> {
    let kind: String = r.get(1)?;
    Ok(FolderRecord {
        id: FolderId(Uuid::from_bytes(b16(r, 0)?)),
        kind: folder_kind_parse(1, &kind)?,
        name: Versioned { v: version(r, 3, 4)?, val: r.get(2)? },
        retention: Versioned { v: version(r, 6, 7)?, val: retention_from_sql(r.get(5)?) },
        deleted: Versioned { v: version(r, 9, 10)?, val: r.get(8)? },
    })
}

pub fn get_folder(c: &Connection, id: FolderId) -> Result<Option<FolderRecord>> {
    Ok(c.query_row(&format!("SELECT {FOLDER_COLS} FROM folder WHERE id = ?1"), [id.0.as_bytes().as_slice()], folder_from_row).optional()?)
}

pub fn all_folders(c: &Connection) -> Result<Vec<FolderRecord>> {
    let mut st = c.prepare(&format!("SELECT {FOLDER_COLS} FROM folder ORDER BY id"))?;
    let rows = st.query_map([], folder_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Insert or overwrite all groups of a folder.
pub fn put_folder(c: &Connection, r: &FolderRecord) -> Result<()> {
    c.execute(
        &format!("INSERT OR REPLACE INTO folder ({FOLDER_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
        params![
            r.id.0.as_bytes().as_slice(),
            folder_kind_str(r.kind),
            r.name.val,
            to_i(r.name.v.lamport),
            &r.name.v.device.0[..],
            retention_to_sql(r.retention.val),
            to_i(r.retention.v.lamport),
            &r.retention.v.device.0[..],
            r.deleted.val,
            to_i(r.deleted.v.lamport),
            &r.deleted.v.device.0[..],
        ],
    )?;
    Ok(())
}

const PLACE_COLS: &str = "id, name, name_lamport, name_dev, lat, lon, radius_m, area_lamport, area_dev, \
    folder_id, target_lamport, target_dev, deleted_at, deleted_lamport, deleted_dev";

fn place_from_row(r: &Row) -> rusqlite::Result<PlaceRecord> {
    Ok(PlaceRecord {
        id: PlaceId(Uuid::from_bytes(b16(r, 0)?)),
        name: Versioned { v: version(r, 2, 3)?, val: r.get(1)? },
        area: Versioned { v: version(r, 7, 8)?, val: Area { lat: r.get(4)?, lon: r.get(5)?, radius_m: r.get(6)? } },
        target: Versioned { v: version(r, 10, 11)?, val: FolderId(Uuid::from_bytes(b16(r, 9)?)) },
        deleted: Versioned { v: version(r, 13, 14)?, val: r.get(12)? },
    })
}

pub fn get_place(c: &Connection, id: PlaceId) -> Result<Option<PlaceRecord>> {
    Ok(c.query_row(&format!("SELECT {PLACE_COLS} FROM place WHERE id = ?1"), [id.0.as_bytes().as_slice()], place_from_row).optional()?)
}

pub fn all_places(c: &Connection) -> Result<Vec<PlaceRecord>> {
    let mut st = c.prepare(&format!("SELECT {PLACE_COLS} FROM place ORDER BY id"))?;
    let rows = st.query_map([], place_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn put_place(c: &Connection, r: &PlaceRecord) -> Result<()> {
    c.execute(
        &format!("INSERT OR REPLACE INTO place ({PLACE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"),
        params![
            r.id.0.as_bytes().as_slice(),
            r.name.val,
            to_i(r.name.v.lamport),
            &r.name.v.device.0[..],
            r.area.val.lat,
            r.area.val.lon,
            r.area.val.radius_m,
            to_i(r.area.v.lamport),
            &r.area.v.device.0[..],
            r.target.val.0.as_bytes().as_slice(),
            to_i(r.target.v.lamport),
            &r.target.v.device.0[..],
            r.deleted.val,
            to_i(r.deleted.v.lamport),
            &r.deleted.v.device.0[..],
        ],
    )?;
    Ok(())
}

fn receipt_from_row(r: &Row) -> rusqlite::Result<ReceiptRecord> {
    Ok(ReceiptRecord { hash: BlobHash(b32(r, 0)?), device: DeviceId(b32(r, 1)?), received_at: r.get(2)? })
}

pub fn get_receipt(c: &Connection, hash: BlobHash, device: DeviceId) -> Result<Option<ReceiptRecord>> {
    Ok(c.query_row(
        "SELECT blob_hash, device_id, received_at FROM receipt WHERE blob_hash = ?1 AND device_id = ?2",
        params![&hash.0[..], &device.0[..]],
        receipt_from_row,
    )
    .optional()?)
}

/// Load any record by key (sync: Fetch, Changes pages).
pub fn get_record(c: &Connection, key: &EntityKey) -> Result<Option<Record>> {
    Ok(match key {
        EntityKey::Folder { id } => get_folder(c, *id)?.map(Record::Folder),
        EntityKey::Place { id } => get_place(c, *id)?.map(Record::Place),
        EntityKey::Item { id } => get_item(c, *id)?.map(|(r, _)| Record::Item(r)),
        EntityKey::Receipt { hash, device } => get_receipt(c, *hash, *device)?.map(Record::Receipt),
    })
}

fn rank(k: &EntityKey) -> u8 {
    match k {
        EntityKey::Folder { .. } => 0,
        EntityKey::Place { .. } => 1,
        EntityKey::Item { .. } => 2,
        EntityKey::Receipt { .. } => 3,
    }
}

/// Records in EntityKey order strictly after `after`, at most `limit` (snapshot pages,
/// digest, list exchange). Order: folders, places, items, receipts; by id bytes within.
pub fn records_after(c: &Connection, after: Option<&EntityKey>, limit: u32) -> Result<Vec<Record>> {
    let mut out: Vec<Record> = Vec::new();
    for kind in 0u8..4 {
        let remaining = (limit as usize).saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        // Lower bound for this kind: strictly after `after` if it is of this kind, nothing
        // if `after` is of a later kind, everything if it is of an earlier kind.
        let lo: Option<&EntityKey> = match after {
            Some(k) if rank(k) == kind => Some(k),
            Some(k) if rank(k) > kind => continue,
            _ => None,
        };
        let lim = remaining as i64;
        match kind {
            0 => {
                let lo_id = match lo {
                    Some(EntityKey::Folder { id }) => id.0.as_bytes().to_vec(),
                    _ => Vec::new(),
                };
                let mut st = c.prepare(&format!("SELECT {FOLDER_COLS} FROM folder WHERE id > ?1 ORDER BY id LIMIT ?2"))?;
                for r in st.query_map(params![lo_id, lim], folder_from_row)? {
                    out.push(Record::Folder(r?));
                }
            }
            1 => {
                let lo_id = match lo {
                    Some(EntityKey::Place { id }) => id.0.as_bytes().to_vec(),
                    _ => Vec::new(),
                };
                let mut st = c.prepare(&format!("SELECT {PLACE_COLS} FROM place WHERE id > ?1 ORDER BY id LIMIT ?2"))?;
                for r in st.query_map(params![lo_id, lim], place_from_row)? {
                    out.push(Record::Place(r?));
                }
            }
            2 => {
                let lo_id = match lo {
                    Some(EntityKey::Item { id }) => id.0.as_bytes().to_vec(),
                    _ => Vec::new(),
                };
                let mut st = c.prepare(&format!("SELECT {ITEM_COLS} FROM item WHERE id > ?1 ORDER BY id LIMIT ?2"))?;
                for r in st.query_map(params![lo_id, lim], item_from_row)? {
                    out.push(Record::Item(r?.0));
                }
            }
            _ => {
                let (h, d) = match lo {
                    Some(EntityKey::Receipt { hash, device }) => (hash.0.to_vec(), device.0.to_vec()),
                    _ => (Vec::new(), Vec::new()),
                };
                let mut st = c.prepare(
                    "SELECT blob_hash, device_id, received_at FROM receipt
                     WHERE (blob_hash, device_id) > (?1, ?2) ORDER BY blob_hash, device_id LIMIT ?3",
                )?;
                for r in st.query_map(params![h, d, lim], receipt_from_row)? {
                    out.push(Record::Receipt(r?));
                }
            }
        }
    }
    Ok(out)
}

// ---------- receipts ----------

/// Insert if absent (grow-only). Returns true if new. `seen_at` = local now.
pub fn insert_receipt(c: &Connection, r: &ReceiptRecord, seen_at: i64) -> Result<bool> {
    let n = c.execute(
        "INSERT OR IGNORE INTO receipt (blob_hash, device_id, received_at, seen_at) VALUES (?1, ?2, ?3, ?4)",
        params![&r.hash.0[..], &r.device.0[..], r.received_at, seen_at],
    )?;
    Ok(n == 1)
}

/// True if `peer` has confirmed holding the file with `hash` (a receipt row from that device).
/// Receipts are per file hash, forever: a NEW item with the same bytes as an old one counts the
/// old receipt. Expiry never reads this (0.2.0: items expire on time); it only feeds the
/// "waiting" count and the "never reached the other device" note.
pub fn peer_has_receipt(c: &Connection, hash: BlobHash, peer: DeviceId) -> Result<bool> {
    let n: i64 = c.query_row(
        "SELECT count(*) FROM receipt WHERE blob_hash = ?1 AND device_id = ?2",
        params![&hash.0[..], &peer.0[..]],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

// ---------- change log and cursors ----------

fn key_parts(key: &EntityKey) -> (&'static str, Vec<u8>) {
    match key {
        EntityKey::Item { id } => ("item", id.0.as_bytes().to_vec()),
        EntityKey::Folder { id } => ("folder", id.0.as_bytes().to_vec()),
        EntityKey::Place { id } => ("place", id.0.as_bytes().to_vec()),
        EntityKey::Receipt { hash, device } => {
            let mut v = hash.0.to_vec();
            v.extend_from_slice(&device.0);
            ("receipt", v)
        }
    }
}

fn key_from_parts(idx: usize, entity: &str, id: &[u8]) -> rusqlite::Result<EntityKey> {
    let u = |b: &[u8]| -> rusqlite::Result<Uuid> {
        Ok(Uuid::from_bytes(b.try_into().map_err(|_| bad(idx, Type::Blob, "expected 16 bytes"))?))
    };
    Ok(match entity {
        "item" => EntityKey::Item { id: ItemId(u(id)?) },
        "folder" => EntityKey::Folder { id: FolderId(u(id)?) },
        "place" => EntityKey::Place { id: PlaceId(u(id)?) },
        "receipt" => {
            if id.len() != 64 {
                return Err(bad(idx, Type::Blob, "expected 64 bytes"));
            }
            EntityKey::Receipt {
                hash: BlobHash(id[..32].try_into().map_err(|_| bad(idx, Type::Blob, "hash"))?),
                device: DeviceId(id[32..].try_into().map_err(|_| bad(idx, Type::Blob, "device"))?),
            }
        }
        _ => return Err(bad(idx, Type::Text, "unknown entity")),
    })
}

/// Append one change_log row written on this device (src NULL); returns its seq.
pub fn log_change(c: &Connection, key: &EntityKey, field_group: &str, lamport: u64) -> Result<u64> {
    let (entity, id) = key_parts(key);
    c.execute(
        "INSERT INTO change_log (entity, entity_id, field_group, lamport) VALUES (?1, ?2, ?3, ?4)",
        params![entity, id, field_group, to_i(lamport)],
    )?;
    Ok(to_u(c.last_insert_rowid()))
}

/// Append one change_log row for a change taken from `src` (the phone relaying one PC's change
/// to the others). That device never gets the row back.
pub fn log_change_from(c: &Connection, key: &EntityKey, field_group: &str, lamport: u64, src: DeviceId) -> Result<u64> {
    let (entity, id) = key_parts(key);
    c.execute(
        "INSERT INTO change_log (entity, entity_id, field_group, lamport, src) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![entity, id, field_group, to_i(lamport), &src.0[..]],
    )?;
    Ok(to_u(c.last_insert_rowid()))
}

/// Distinct keys changed after `since` that were not taken from `exclude`, in seq order, at most
/// `limit`, with the highest seq covered (the next `since`). Returns (keys, last_seq).
///
/// A key is ordered by its LAST change, so a key changed again after the page ends is
/// returned by a later page, never lost. Empty result: last_seq = since.
pub fn changes_since(c: &Connection, since: u64, limit: u32, exclude: DeviceId) -> Result<(Vec<EntityKey>, u64)> {
    let mut st = c.prepare(
        "SELECT entity, entity_id, MAX(seq) AS m FROM change_log
          WHERE seq > ?1 AND (src IS NULL OR src <> ?3)
          GROUP BY entity, entity_id ORDER BY m LIMIT ?2",
    )?;
    let rows = st.query_map(params![to_i(since), limit as i64, &exclude.0[..]], |r| {
        let e: String = r.get(0)?;
        let id: Vec<u8> = r.get(1)?;
        let m: i64 = r.get(2)?;
        Ok((key_from_parts(1, &e, &id)?, to_u(m)))
    })?;
    let mut keys = Vec::new();
    let mut last = since;
    for r in rows {
        let (k, m) = r?;
        keys.push(k);
        last = m;
    }
    Ok((keys, last))
}

/// Highest seq in the change log (0 if empty; AUTOINCREMENT keeps it monotonic after trims).
pub fn head_seq(c: &Connection) -> Result<u64> {
    let v: Option<i64> = c
        .query_row("SELECT seq FROM sqlite_sequence WHERE name = 'change_log'", [], |r| r.get(0))
        .optional()?;
    Ok(v.map(to_u).unwrap_or(0))
}

/// Highest seq of a change to (key, group) above `after` that the peer has not seen, if any.
/// Used by merge conflict detection: a change the peer has not acked = concurrent. Rows taken
/// from the peer itself are its own writes coming back, never concurrent with it.
pub fn unacked_local_change(c: &Connection, key: &EntityKey, field_group: &str, after: u64, peer: DeviceId) -> Result<Option<u64>> {
    let (entity, id) = key_parts(key);
    let v: Option<i64> = c.query_row(
        "SELECT MAX(seq) FROM change_log
          WHERE entity = ?1 AND entity_id = ?2 AND field_group = ?3 AND seq > ?4 AND (src IS NULL OR src <> ?5)",
        params![entity, id, field_group, to_i(after), &peer.0[..]],
        |r| r.get(0),
    )?;
    Ok(v.map(to_u))
}

const LOG_FLOOR: &str = "log_floor";

/// Every change_log row with seq <= this may be gone. A peer whose cursor is below it must
/// restart from a snapshot. Missing = 0 (a database that never trimmed).
pub fn log_floor(c: &Connection) -> Result<u64> {
    match get_value(c, LOG_FLOOR)? {
        None => Ok(0),
        Some(v) => v.parse::<u64>().map_err(|e| CoreError::Invalid(format!("setting '{LOG_FLOOR}' has a bad value: {e}"))),
    }
}

/// Raise the floor. It never moves back, so a stale caller cannot make a trimmed range look held.
pub fn set_log_floor(c: &Connection, floor: u64) -> Result<()> {
    let floor = floor.max(log_floor(c)?);
    set_value(c, LOG_FLOOR, &floor.to_string())
}

/// A linked peer unseen this long stops holding the log back; it starts over from a snapshot
/// when it returns.
pub const STALE_PEER_MS: i64 = 60 * crate::clock::DAY_MS;

/// Beyond this many rows the oldest go, whoever still needs them (they start over from a snapshot).
pub const MAX_LOG_ROWS: i64 = 200_000;

/// Drop the change_log rows every live linked peer has acked, and move the floor up to match.
/// Returns the new floor.
///
/// A peer with no cursor row counts as acked 0: it is still on its first snapshot and will pull
/// from the head that snapshot started at, so every row above that must stay. Time (not
/// distance) retires a peer, so one that is linking now is never dropped. With no live peer
/// the whole log goes.
pub fn trim_log_for_peers(c: &Connection, now: i64) -> Result<u64> {
    trim_log_capped(c, now, MAX_LOG_ROWS)
}

fn trim_log_capped(c: &Connection, now: i64, max_rows: i64) -> Result<u64> {
    let floor = log_floor(c)?;
    let head = head_seq(c)?;
    let min: Option<i64> = c.query_row(
        "SELECT MIN(COALESCE(pc.our_seq_acked, 0)) FROM device d
           LEFT JOIN peer_cursor pc ON pc.peer_id = d.id
          WHERE d.is_self = 0 AND d.paired_at IS NOT NULL AND d.unpaired_at IS NULL
            AND COALESCE(d.last_seen_at, d.paired_at) >= ?1",
        [now - STALE_PEER_MS],
        |r| r.get(0),
    )?;
    let mut target = min.map(to_u).unwrap_or(head);
    let cap: Option<i64> = c
        .query_row("SELECT seq FROM change_log ORDER BY seq DESC LIMIT 1 OFFSET ?1", [max_rows], |r| r.get(0))
        .optional()?;
    if let Some(s) = cap {
        target = target.max(to_u(s));
    }
    let target = target.max(floor).min(head);
    if target > floor {
        // Floor first: if we stop between the two, the floor is too high (a stale peer costs a
        // snapshot), never too low (an incremental pull that silently skips the deleted rows).
        set_log_floor(c, target)?;
        c.execute("DELETE FROM change_log WHERE seq <= ?1", [to_i(target)])?;
    }
    Ok(target)
}

/// (last_seq_pulled, our_seq_acked) for the peer; None = new peer (full snapshot).
pub fn get_cursor(c: &Connection, peer: DeviceId) -> Result<Option<(u64, u64)>> {
    Ok(c.query_row(
        "SELECT last_seq_pulled, our_seq_acked FROM peer_cursor WHERE peer_id = ?1",
        [&peer.0[..]],
        |r| Ok((to_u(r.get(0)?), to_u(r.get(1)?))),
    )
    .optional()?)
}

pub fn set_pulled(c: &Connection, peer: DeviceId, last_seq_pulled: u64) -> Result<()> {
    c.execute(
        "INSERT INTO peer_cursor (peer_id, last_seq_pulled, our_seq_acked) VALUES (?1, ?2, 0)
         ON CONFLICT(peer_id) DO UPDATE SET last_seq_pulled = excluded.last_seq_pulled",
        params![&peer.0[..], to_i(last_seq_pulled)],
    )?;
    Ok(())
}

/// Record that the peer has our changes up to `our_seq_acked`. Updates an existing cursor
/// only: a peer without a cursor is still on its first snapshot, and creating a row here
/// would make it look like a caught-up peer (the engine would skip the snapshot).
pub fn set_acked(c: &Connection, peer: DeviceId, our_seq_acked: u64) -> Result<()> {
    c.execute(
        "UPDATE peer_cursor SET our_seq_acked = MAX(our_seq_acked, ?2) WHERE peer_id = ?1",
        params![&peer.0[..], to_i(our_seq_acked)],
    )?;
    Ok(())
}

pub fn delete_cursor(c: &Connection, peer: DeviceId) -> Result<()> {
    c.execute("DELETE FROM peer_cursor WHERE peer_id = ?1", [&peer.0[..]])?;
    Ok(())
}

// ---------- file refs ----------

/// Where this device holds the item's file (library or Bin path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRef {
    pub item: ItemId,
    pub path: String,
    pub volume_serial: Option<u64>,
    pub file_id: Option<[u8; 16]>,
    /// PC: modified time (unix ms) of the held file when the watcher last recorded it. None =
    /// not recorded yet (a new download, or a library from before 0.2.0).
    pub mtime_ms: Option<i64>,
}

fn file_ref_from_row(r: &Row) -> rusqlite::Result<FileRef> {
    let vol: Option<i64> = r.get(2)?;
    let fid: Option<Vec<u8>> = r.get(3)?;
    Ok(FileRef {
        item: ItemId(Uuid::from_bytes(b16(r, 0)?)),
        path: r.get(1)?,
        // The volume serial is a u32/u64 bit pattern; it round-trips through i64 unchanged.
        volume_serial: vol.map(|v| v as u64),
        file_id: match fid {
            None => None,
            Some(v) => Some(v.try_into().map_err(|_| bad(3, Type::Blob, "expected 16 bytes"))?),
        },
        mtime_ms: r.get(4)?,
    })
}

pub fn get_file_ref(c: &Connection, item: ItemId) -> Result<Option<FileRef>> {
    Ok(c.query_row(
        "SELECT item_id, path, volume_serial, file_id, mtime_ms FROM file_ref WHERE item_id = ?1",
        [item.0.as_bytes().as_slice()],
        file_ref_from_row,
    )
    .optional()?)
}

/// Upsert on item_id. If the NTFS identity is already recorded for another item, that item
/// loses only the identity (its path stays); a REPLACE would delete its whole row.
pub fn put_file_ref(c: &Connection, r: &FileRef) -> Result<()> {
    if let (Some(vol), Some(fid)) = (r.volume_serial, r.file_id) {
        c.execute(
            "UPDATE file_ref SET volume_serial = NULL, file_id = NULL WHERE volume_serial = ?1 AND file_id = ?2 AND item_id <> ?3",
            params![vol as i64, &fid[..], r.item.0.as_bytes().as_slice()],
        )?;
    }
    c.execute(
        "INSERT INTO file_ref (item_id, path, volume_serial, file_id, mtime_ms) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(item_id) DO UPDATE SET path = excluded.path, volume_serial = excluded.volume_serial, file_id = excluded.file_id, mtime_ms = excluded.mtime_ms",
        params![
            r.item.0.as_bytes().as_slice(),
            r.path,
            r.volume_serial.map(|v| v as i64),
            r.file_id.map(|f| f.to_vec()),
            r.mtime_ms,
        ],
    )?;
    Ok(())
}

pub fn delete_file_ref(c: &Connection, item: ItemId) -> Result<()> {
    c.execute("DELETE FROM file_ref WHERE item_id = ?1", [item.0.as_bytes().as_slice()])?;
    Ok(())
}

/// One file this device holds in its library (state Present), with what the PC watcher needs
/// to match it against the disk: path, NTFS identity, the record's hash, size, name and folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldFile {
    pub item: ItemId,
    pub path: String,
    pub volume_serial: Option<u64>,
    pub file_id: Option<[u8; 16]>,
    pub hash: BlobHash,
    pub size: u64,
    pub name: String,
    pub ext: String,
    pub folder: FolderId,
    pub captured_at: i64,
    /// `file_ref.mtime_ms` (see `FileRef`).
    pub mtime_ms: Option<i64>,
}

/// Every Present item that has a file_ref (one SQL pass; the PC rescan's view of "what we hold").
pub fn held_files(c: &Connection) -> Result<Vec<HeldFile>> {
    let mut st = c.prepare(
        "SELECT f.item_id, f.path, f.volume_serial, f.file_id, i.blob_hash, i.size, i.name, i.ext, i.folder_id, i.captured_at, f.mtime_ms
         FROM file_ref f JOIN item i ON i.id = f.item_id
         WHERE i.local_file_state = 'present'",
    )?;
    let rows = st.query_map([], |r| {
        let vol: Option<i64> = r.get(2)?;
        let fid: Option<Vec<u8>> = r.get(3)?;
        let size: i64 = r.get(5)?;
        Ok(HeldFile {
            item: ItemId(Uuid::from_bytes(b16(r, 0)?)),
            path: r.get(1)?,
            volume_serial: vol.map(|v| v as u64),
            file_id: match fid {
                None => None,
                Some(v) => Some(v.try_into().map_err(|_| bad(3, Type::Blob, "expected 16 bytes"))?),
            },
            hash: BlobHash(b32(r, 4)?),
            size: size.max(0) as u64,
            name: r.get(6)?,
            ext: r.get(7)?,
            folder: FolderId(Uuid::from_bytes(b16(r, 8)?)),
            captured_at: r.get(9)?,
            mtime_ms: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record the NTFS identity of the file an item holds (the watcher saw it at its current path).
/// Clears the same identity on any other item first: the unique index must never fail a scan.
pub fn set_file_identity(c: &Connection, item: ItemId, volume_serial: u64, file_id: [u8; 16]) -> Result<()> {
    c.execute(
        "UPDATE file_ref SET volume_serial = NULL, file_id = NULL WHERE volume_serial = ?1 AND file_id = ?2 AND item_id <> ?3",
        params![volume_serial as i64, &file_id[..], item.0.as_bytes().as_slice()],
    )?;
    let n = c.execute(
        "UPDATE file_ref SET volume_serial = ?2, file_id = ?3 WHERE item_id = ?1",
        params![item.0.as_bytes().as_slice(), volume_serial as i64, &file_id[..]],
    )?;
    if n == 0 {
        return Err(CoreError::NotFound { what: "file_ref", id: item.0.to_string() });
    }
    Ok(())
}

/// Record the modified time of the file an item holds (the PC watcher saw it at its current
/// path). Errors when the item holds no file_ref, so a caller never believes it stamped nothing.
pub fn set_file_mtime(c: &Connection, item: ItemId, mtime_ms: i64) -> Result<()> {
    let n = c.execute("UPDATE file_ref SET mtime_ms = ?2 WHERE item_id = ?1", params![item.0.as_bytes().as_slice(), mtime_ms])?;
    if n == 0 {
        return Err(CoreError::NotFound { what: "file_ref", id: item.0.to_string() });
    }
    Ok(())
}

/// PC move matching: item whose file has this NTFS identity.
pub fn item_by_file_id(c: &Connection, volume_serial: u64, file_id: [u8; 16]) -> Result<Option<ItemId>> {
    Ok(c.query_row(
        "SELECT item_id FROM file_ref WHERE volume_serial = ?1 AND file_id = ?2",
        params![volume_serial as i64, &file_id[..]],
        |r| Ok(ItemId(Uuid::from_bytes(b16(r, 0)?))),
    )
    .optional()?)
}

pub fn item_by_path(c: &Connection, path: &str) -> Result<Option<ItemId>> {
    Ok(c.query_row("SELECT item_id FROM file_ref WHERE path = ?1", [path], |r| Ok(ItemId(Uuid::from_bytes(b16(r, 0)?))))
        .optional()?)
}

// ---------- settings ----------

/// Range checks shared by load and save. Messages name the key.
fn validate_settings(s: &Settings) -> Result<()> {
    fn range(key: &str, v: u64, lo: u64, hi: u64) -> Result<()> {
        if v < lo || v > hi {
            return Err(CoreError::Invalid(format!("setting '{key}' must be {lo}..={hi}, got {v}")));
        }
        Ok(())
    }
    range("bin_days", s.bin_days as u64, 1, 365)?;
    range("udp_port", s.udp_port as u64, 1024, 65535)?;
    range("missing_pause_pct", s.missing_pause_pct as u64, 1, 100)?;
    range("missing_pause_files", s.missing_pause_files as u64, 1, u32::MAX as u64)?;
    range("max_blob_bytes", s.max_blob_bytes, 1, u64::MAX)?;
    Ok(())
}

/// All settings; missing keys take `Settings::default()` values. A stored value that does
/// not parse is an error naming the key (never silently defaulted).
pub fn load_settings(c: &Connection) -> Result<Settings> {
    let defaults = serde_json::to_value(Settings::default()).map_err(|e| CoreError::Invalid(e.to_string()))?;
    let mut map = match defaults.clone() {
        serde_json::Value::Object(m) => m,
        _ => return Err(CoreError::Invalid("settings are not an object".into())),
    };
    let mut st = c.prepare("SELECT key, value FROM setting")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (k, v) = row?;
        // The table also holds free-form values (get_value); only known keys are settings.
        if !map.contains_key(&k) {
            continue;
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&v).map_err(|e| CoreError::Invalid(format!("setting '{k}' is not valid JSON: {e}")))?;
        let mut probe = defaults.clone();
        probe[&k] = parsed.clone();
        serde_json::from_value::<Settings>(probe)
            .map_err(|e| CoreError::Invalid(format!("setting '{k}' has a bad value: {e}")))?;
        map.insert(k, parsed);
    }
    let s: Settings = serde_json::from_value(serde_json::Value::Object(map)).map_err(|e| CoreError::Invalid(e.to_string()))?;
    validate_settings(&s)?;
    Ok(s)
}

/// Validate (bin_days 1..=365, udp_port 1024..=65535, percents 1..=100, max_blob > 0) and store.
pub fn save_settings(c: &Connection, s: &Settings) -> Result<()> {
    validate_settings(s)?;
    let v = serde_json::to_value(s).map_err(|e| CoreError::Invalid(e.to_string()))?;
    if let serde_json::Value::Object(m) = v {
        for (k, val) in m {
            set_value(c, &k, &val.to_string())?;
        }
    }
    Ok(())
}

/// Free-form local values (desktop: "library_path", "library_id"). JSON text.
pub fn get_value(c: &Connection, key: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| r.get(0)).optional()?)
}

pub fn set_value(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute(
        "INSERT INTO setting (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ---------- pending unpair (phone) ----------

const PENDING_UNPAIR: &str = "pending_unpair";

/// An unpair this device made while the other device could not be told. An unpair removes
/// the key on both devices, so the phone keeps this until it has delivered the
/// news to the PC (or the PC refuses it as unknown, which is the same result).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingUnpair {
    /// Hex of the peer's public key.
    pub peer: String,
    /// Last known "ip:port" addresses of the peer.
    pub addresses: Vec<String>,
}

impl PendingUnpair {
    pub fn peer_id(&self) -> Result<DeviceId> {
        let raw = hex::decode(&self.peer).map_err(|e| CoreError::Invalid(format!("pending unpair has a bad key: {e}")))?;
        let key: [u8; 32] = raw.try_into().map_err(|_| CoreError::Invalid("pending unpair key is not 32 bytes".into()))?;
        Ok(DeviceId(key))
    }
}

/// Every notice still owed, oldest first. The value is a JSON list; a 0.2.x database holds one
/// bare object, which reads as a list of one.
pub fn pending_unpairs(c: &Connection) -> Result<Vec<PendingUnpair>> {
    let Some(text) = get_value(c, PENDING_UNPAIR)? else { return Ok(Vec::new()) };
    let bad = |e: serde_json::Error| CoreError::Invalid(format!("setting '{PENDING_UNPAIR}' has a bad value: {e}"));
    let v: serde_json::Value = serde_json::from_str(&text).map_err(bad)?;
    match v {
        serde_json::Value::Array(_) => serde_json::from_value(v).map_err(bad),
        _ => Ok(vec![serde_json::from_value(v).map_err(bad)?]),
    }
}

fn save_pending_unpairs(c: &Connection, list: &[PendingUnpair]) -> Result<()> {
    if list.is_empty() {
        c.execute("DELETE FROM setting WHERE key = ?1", [PENDING_UNPAIR])?;
        return Ok(());
    }
    let text = serde_json::to_string(list).map_err(|e| CoreError::Invalid(format!("pending unpair: {e}")))?;
    set_value(c, PENDING_UNPAIR, &text)
}

/// Owe `p.peer` a notice. A second notice for the same peer replaces the first (new addresses).
pub fn add_pending_unpair(c: &Connection, p: &PendingUnpair) -> Result<()> {
    let mut list = pending_unpairs(c)?;
    match list.iter_mut().find(|x| x.peer == p.peer) {
        Some(x) => *x = p.clone(),
        None => list.push(p.clone()),
    }
    save_pending_unpairs(c, &list)
}

/// The notice for `peer` is delivered or void. Other notices stay.
pub fn remove_pending_unpair(c: &Connection, peer: DeviceId) -> Result<()> {
    let mut list = pending_unpairs(c)?;
    let before = list.len();
    list.retain(|x| !x.peer_id().is_ok_and(|id| id == peer));
    if list.len() != before {
        save_pending_unpairs(c, &list)?;
    }
    Ok(())
}

pub fn is_pending_unpair(c: &Connection, peer: DeviceId) -> Result<bool> {
    Ok(pending_unpairs(c)?.iter().any(|x| x.peer_id().is_ok_and(|id| id == peer)))
}

// ---------- test support (shared by the logic modules' tests) ----------

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::db::Db;

    pub const T0: i64 = 1_790_000_000_000; // 2026-09-21, an arbitrary fixed start

    pub fn dev(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }

    pub fn p() -> DeviceId {
        dev(0xAA)
    }

    pub fn f() -> DeviceId {
        dev(0xBB)
    }

    pub fn hash(n: u8) -> BlobHash {
        BlobHash([n; 32])
    }

    pub fn v(lamport: u64, d: DeviceId) -> Version {
        Version { lamport, device: d }
    }

    /// In-memory db with `me` as the self device and `peer` paired (paired_at = T0).
    pub fn db_with_peer(me: DeviceId, peer: DeviceId) -> Db {
        let db = Db::open_in_memory().unwrap();
        let (my_role, peer_role) = if me == p() { (Role::Pc, Role::Phone) } else { (Role::Phone, Role::Pc) };
        ensure_self(&db.conn, me, "me", my_role).unwrap();
        set_paired(
            &db.conn,
            &DeviceRow { id: peer, name: "peer".into(), is_self: false, role: peer_role, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec![] },
            T0,
        )
        .unwrap();
        db
    }

    /// A PC-side core (self = p) paired with phone f, on a real AppStorage in a temp dir,
    /// with a clock the test controls. Used by the bin, ops and folders tests.
    pub struct Rig {
        pub db: Db,
        pub pl: crate::placement::AppStorage,
        pub clock: crate::clock::TestClock,
        pub dir: tempfile::TempDir,
    }

    impl Rig {
        pub fn new() -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let pl = crate::placement::AppStorage::new(dir.path().join("store")).unwrap();
            Rig { db: db_with_peer(p(), f()), pl, clock: crate::clock::TestClock::new(T0), dir }
        }

        /// Run an op with a ctx at the current test time (disjoint borrows of db and placement).
        pub fn run<T>(&mut self, f: impl FnOnce(&mut Connection, &crate::ops::Ctx<'_>) -> T) -> T {
            let ctx = crate::ops::Ctx { self_id: p(), now: self.now(), placement: &self.pl };
            f(&mut self.db.conn, &ctx)
        }

        pub fn now(&self) -> i64 {
            use crate::clock::Clock;
            self.clock.now_ms()
        }

        /// A file on disk with JPEG magic and distinct content per `name`.
        pub fn write_jpeg(&self, name: &str) -> std::path::PathBuf {
            let src = self.dir.path().join(format!("src-{name}"));
            let mut body = vec![0xFF, 0xD8, 0xFF, 0xE0];
            body.extend_from_slice(name.as_bytes());
            std::fs::write(&src, &body).unwrap();
            src
        }

        /// Add a real JPEG through the add path; returns its id.
        pub fn add(&mut self, name: &str) -> ItemId {
            let src = self.write_jpeg(name);
            let now = self.now();
            let ctx = crate::ops::Ctx { self_id: p(), now, placement: &self.pl };
            crate::ops::add_local_file(
                &mut self.db.conn,
                &ctx,
                &crate::ops::NewLocalFile {
                    src,
                    name: format!("{name}.jpg"),
                    folder: None,
                    captured_at: now,
                    geo: None,
                    thumb: None,
                    how: crate::ops::AddKind::Captured,
                    ntfs: None,
                },
                None,
            )
            .unwrap()
        }

        /// The peer confirms it holds the file of `id` (receipt seen now).
        pub fn confirm(&self, id: ItemId) {
            let (rec, _) = get_item(&self.db.conn, id).unwrap().unwrap();
            insert_receipt(&self.db.conn, &ReceiptRecord { hash: rec.content.val.hash, device: f(), received_at: self.now() }, self.now()).unwrap();
        }

        pub fn sweep(&mut self) -> crate::bin::SweepReport {
            let now = self.now();
            crate::bin::sweep(&mut self.db.conn, &self.pl, p(), now).unwrap()
        }

        pub fn lfs(&self, id: ItemId) -> LocalFileState {
            get_item(&self.db.conn, id).unwrap().unwrap().1
        }

        pub fn rec(&self, id: ItemId) -> ItemRecord {
            get_item(&self.db.conn, id).unwrap().unwrap().0
        }

        pub fn path(&self, id: ItemId) -> Option<std::path::PathBuf> {
            get_file_ref(&self.db.conn, id).unwrap().map(|r| r.path.into())
        }

        pub fn actions(&self) -> Vec<ActivityAction> {
            crate::activity::list(&self.db.conn, None, 500).unwrap().into_iter().map(|a| a.action).collect()
        }
    }

    /// Insert an item into a database still at schema v1-v3, the columns those versions have: the
    /// current `insert_item` also writes the purge columns of v4.
    pub fn insert_item_before_v4(c: &Connection, r: &ItemRecord, local: LocalFileState, now: i64) {
        c.execute(
            "INSERT INTO item (id, captured_at, origin_device, name, name_lamport, name_dev, blob_hash, size, media_kind, ext,
                content_lamport, content_dev, folder_id, folder_entered_at, placement_lamport, placement_dev,
                pinned, pinned_lamport, pinned_dev, deleted_at, deleted_lamport, deleted_dev, local_file_state, local_state_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
            params![
                r.id.0.as_bytes().as_slice(),
                r.captured_at,
                &r.origin.0[..],
                r.name.val,
                to_i(r.name.v.lamport),
                &r.name.v.device.0[..],
                &r.content.val.hash.0[..],
                to_i(r.content.val.size),
                media_str(r.content.val.kind),
                r.content.val.ext,
                to_i(r.content.v.lamport),
                &r.content.v.device.0[..],
                r.location.val.folder.0.as_bytes().as_slice(),
                r.location.val.entered_at,
                to_i(r.location.v.lamport),
                &r.location.v.device.0[..],
                r.pinned.val as i64,
                to_i(r.pinned.v.lamport),
                &r.pinned.v.device.0[..],
                r.deleted.val,
                to_i(r.deleted.v.lamport),
                &r.deleted.v.device.0[..],
                lfs_str(local),
                now,
            ],
        )
        .unwrap();
    }

    pub fn item(n: u8, origin: DeviceId, ver: Version) -> ItemRecord {
        ItemRecord {
            id: ItemId(Uuid::from_u128(0x1000 + n as u128)),
            captured_at: T0 + n as i64,
            origin,
            geo: None,
            name: Versioned { v: ver, val: format!("IMG_{n}.jpg") },
            content: Versioned {
                v: ver,
                val: Content { hash: hash(n), size: 100, kind: MediaKind::Image, ext: "jpg".into(), thumb: None },
            },
            location: Versioned { v: ver, val: Location { folder: INBOX_ID, entered_at: T0 } },
            pinned: Versioned { v: ver, val: false },
            deleted: Versioned { v: ver, val: None },
            purged: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::db::Db;

    // Lamport = max + 1, persisted across reopen; observe raises, never lowers.
    #[test]
    fn lamport_next_observe_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Db::open(dir.path()).unwrap();
            assert_eq!(max_lamport(&db.conn).unwrap(), 0);
            assert_eq!(next_version(&db.conn, p()).unwrap(), v(1, p()));
            assert_eq!(next_version(&db.conn, p()).unwrap(), v(2, p()));
            observe_lamport(&db.conn, 10).unwrap();
            assert_eq!(max_lamport(&db.conn).unwrap(), 10);
            observe_lamport(&db.conn, 4).unwrap();
            assert_eq!(max_lamport(&db.conn).unwrap(), 10, "observe never lowers");
            assert_eq!(next_version(&db.conn, p()).unwrap().lamport, 11);
        }
        let db = Db::open(dir.path()).unwrap();
        assert_eq!(max_lamport(&db.conn).unwrap(), 11);
        assert_eq!(next_version(&db.conn, p()).unwrap().lamport, 12);
    }

    #[test]
    fn self_device_rules() {
        let db = Db::open_in_memory().unwrap();
        ensure_self(&db.conn, p(), "PC", Role::Pc).unwrap();
        ensure_self(&db.conn, p(), "PC renamed", Role::Pc).unwrap();
        assert_eq!(self_device(&db.conn).unwrap().name, "PC renamed");
        assert!(ensure_self(&db.conn, f(), "x", Role::Phone).is_err(), "other key under same db is refused");
        let me = DeviceRow { id: p(), name: "me".into(), is_self: true, role: Role::Pc, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec![] };
        assert!(set_paired(&db.conn, &me, T0).is_err(), "cannot pair with self");
    }

    #[test]
    fn pairing_rows() {
        let db = Db::open_in_memory().unwrap();
        ensure_self(&db.conn, p(), "PC", Role::Pc).unwrap();
        assert!(paired_peer(&db.conn).unwrap().is_none());
        let peer = DeviceRow { id: f(), name: "Phone".into(), is_self: false, role: Role::Phone, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec!["10.0.0.2:5000".into()] };
        set_paired(&db.conn, &peer, T0).unwrap();
        assert!(is_paired(&db.conn, f()).unwrap());
        assert!(!is_paired(&db.conn, dev(1)).unwrap());
        assert!(!is_paired(&db.conn, p()).unwrap(), "self is not a peer");
        let got = paired_peer(&db.conn).unwrap().unwrap();
        assert_eq!(got.addresses, vec!["10.0.0.2:5000".to_string()]);
        assert_eq!(got.paired_at, Some(T0));
        set_last_seen(&db.conn, f(), T0 + 5, &["10.0.0.9:1".to_string()]).unwrap();
        assert_eq!(get_device(&db.conn, f()).unwrap().unwrap().last_seen_at, Some(T0 + 5));
        set_unpaired(&db.conn, f(), T0 + 10).unwrap();
        assert!(!is_paired(&db.conn, f()).unwrap());
        assert!(paired_peer(&db.conn).unwrap().is_none());
        assert!(set_unpaired(&db.conn, dev(9), T0).is_err());
        // Re-pair clears unpaired_at.
        set_paired(&db.conn, &peer, T0 + 20).unwrap();
        assert!(is_paired(&db.conn, f()).unwrap());
    }

    #[test]
    fn item_roundtrip_and_update() {
        let db = db_with_peer(p(), f());
        let mut it = item(1, p(), v(3, p()));
        it.geo = Some(Geo { lat: -30.5, lon: 151.6, accuracy_m: 12.0 });
        it.content.val.thumb = Some(Thumb { hash: hash(77), size: 5 });
        insert_item(&db.conn, &it, LocalFileState::Present, T0).unwrap();
        let (got, lfs) = get_item(&db.conn, it.id).unwrap().unwrap();
        assert_eq!(got, it);
        assert_eq!(lfs, LocalFileState::Present);
        assert!(insert_item(&db.conn, &it, LocalFileState::Present, T0).is_err(), "duplicate id fails");
        let mut it2 = it.clone();
        it2.name = Versioned { v: v(9, f()), val: "new.jpg".into() };
        it2.pinned = Versioned { v: v(9, f()), val: true };
        it2.deleted = Versioned { v: v(10, f()), val: Some(T0 + 1) };
        it2.content.val.thumb = None;
        update_item(&db.conn, &it2).unwrap();
        let (got, lfs) = get_item(&db.conn, it.id).unwrap().unwrap();
        assert_eq!(got, it2);
        assert_eq!(lfs, LocalFileState::Present, "local state untouched by update_item");
        set_local_state(&db.conn, it.id, LocalFileState::Binned, T0 + 2).unwrap();
        assert_eq!(get_item(&db.conn, it.id).unwrap().unwrap().1, LocalFileState::Binned);
        assert!(set_local_state(&db.conn, ItemId(Uuid::from_u128(5)), LocalFileState::Binned, 0).is_err());
        assert_eq!(items_holding(&db.conn, hash(1)).unwrap(), vec![it.id]);
        set_local_state(&db.conn, it.id, LocalFileState::Purged, T0).unwrap();
        assert!(items_holding(&db.conn, hash(1)).unwrap().is_empty());
        assert_eq!(items_in_state(&db.conn, &[LocalFileState::Purged], 10).unwrap(), vec![it.id]);
        assert!(items_in_state(&db.conn, &[], 10).unwrap().is_empty());
    }

    #[test]
    fn items_paging_is_keyset_newest_first() {
        let db = db_with_peer(p(), f());
        for n in 1..=7u8 {
            insert_item(&db.conn, &item(n, p(), v(1, p())), LocalFileState::Present, T0).unwrap();
        }
        let page1 = items_page(&db.conn, None, None, 3).unwrap();
        let ids1: Vec<_> = page1.iter().map(|(r, _)| r.captured_at - T0).collect();
        assert_eq!(ids1, vec![7, 6, 5]);
        let after = page1.last().unwrap().0.id;
        let page2 = items_page(&db.conn, None, Some(after), 3).unwrap();
        assert_eq!(page2.iter().map(|(r, _)| r.captured_at - T0).collect::<Vec<_>>(), vec![4, 3, 2]);
        let page3 = items_page(&db.conn, Some(INBOX_ID), Some(page2.last().unwrap().0.id), 3).unwrap();
        assert_eq!(page3.len(), 1);
        assert!(items_page(&db.conn, Some(KEEP_ID), None, 3).unwrap().is_empty());
    }

    #[test]
    fn folder_and_place_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let fr = FolderRecord {
            id: FolderId(Uuid::from_u128(0x500)),
            kind: FolderKind::User,
            name: Versioned { v: v(2, p()), val: "Work".into() },
            retention: Versioned { v: v(3, p()), val: Retention::Secs { secs: 3600 } },
            deleted: Versioned { v: v(4, f()), val: Some(T0) },
        };
        put_folder(&db.conn, &fr).unwrap();
        assert_eq!(get_folder(&db.conn, fr.id).unwrap().unwrap(), fr);
        assert_eq!(all_folders(&db.conn).unwrap().len(), 4);
        let pl = PlaceRecord {
            id: PlaceId(Uuid::from_u128(0x600)),
            name: Versioned { v: v(2, p()), val: "Site".into() },
            area: Versioned { v: v(3, p()), val: Area { lat: -30.0, lon: 151.0, radius_m: 150.0 } },
            target: Versioned { v: v(4, f()), val: fr.id },
            deleted: Versioned { v: v(5, f()), val: None },
        };
        put_place(&db.conn, &pl).unwrap();
        assert_eq!(get_place(&db.conn, pl.id).unwrap().unwrap(), pl);
        assert_eq!(all_places(&db.conn).unwrap(), vec![pl]);
    }

    // Receipts grow-only; duplicate -> false. A receipt counts for ONE named device.
    #[test]
    fn receipts_grow_only_and_are_matched_per_device() {
        let db = Db::open_in_memory().unwrap();
        ensure_self(&db.conn, p(), "PC", Role::Pc).unwrap();
        let rec = |d: DeviceId, at| ReceiptRecord { hash: hash(1), device: d, received_at: at };
        assert!(insert_receipt(&db.conn, &rec(f(), 5), T0 + 1).unwrap());
        assert!(!insert_receipt(&db.conn, &rec(f(), 6), T0 + 2).unwrap(), "duplicate is ignored");
        assert_eq!(get_receipt(&db.conn, hash(1), f()).unwrap().unwrap().received_at, 5, "first copy kept");
        assert!(peer_has_receipt(&db.conn, hash(1), f()).unwrap());
        assert!(!peer_has_receipt(&db.conn, hash(1), dev(9)).unwrap(), "another device's receipt does not count");
        assert!(!peer_has_receipt(&db.conn, hash(2), f()).unwrap(), "another file does not count");
        // Own receipt is a receipt of the own device only.
        insert_receipt(&db.conn, &rec(p(), 5), T0 + 3).unwrap();
        assert!(peer_has_receipt(&db.conn, hash(1), p()).unwrap());
        assert!(!peer_has_receipt(&db.conn, hash(3), p()).unwrap());
    }

    #[test]
    fn file_mtime_is_stored_per_file_ref_and_listed() {
        let db = Db::open_in_memory().unwrap();
        let it = item(1, p(), v(1, p()));
        insert_item(&db.conn, &it, LocalFileState::Present, T0).unwrap();
        assert!(matches!(set_file_mtime(&db.conn, it.id, 5), Err(CoreError::NotFound { .. })), "no file_ref yet");
        let fr = FileRef { item: it.id, path: "C:/lib/a.jpg".into(), volume_serial: None, file_id: None, mtime_ms: None };
        put_file_ref(&db.conn, &fr).unwrap();
        assert_eq!(held_files(&db.conn).unwrap()[0].mtime_ms, None);
        set_file_mtime(&db.conn, it.id, 1_234_567).unwrap();
        assert_eq!(get_file_ref(&db.conn, it.id).unwrap().unwrap().mtime_ms, Some(1_234_567));
        assert_eq!(held_files(&db.conn).unwrap()[0].mtime_ms, Some(1_234_567));
        // put_file_ref writes exactly what it is given (callers carry the old value on a move).
        put_file_ref(&db.conn, &FileRef { mtime_ms: Some(9), ..fr.clone() }).unwrap();
        assert_eq!(get_file_ref(&db.conn, it.id).unwrap().unwrap().mtime_ms, Some(9));
        put_file_ref(&db.conn, &fr).unwrap();
        assert_eq!(get_file_ref(&db.conn, it.id).unwrap().unwrap().mtime_ms, None);
    }

    #[test]
    fn file_kind_roundtrips_through_the_item_table() {
        let db = Db::open_in_memory().unwrap();
        let mut it = item(2, p(), v(1, p()));
        it.content.val.kind = MediaKind::File;
        it.content.val.ext = String::new();
        it.name.val = "Makefile".into();
        insert_item(&db.conn, &it, LocalFileState::Pending, T0).unwrap();
        let (got, _) = get_item(&db.conn, it.id).unwrap().unwrap();
        assert_eq!(got.content.val.kind, MediaKind::File);
        assert_eq!(got.content.val.ext, "");
        assert_eq!(got, it);
    }

    // Change_log rows, dedupe, paging, trim, head, unacked.
    #[test]
    fn change_log_dedupes_pages_and_trims() {
        let db = db_with_peer(p(), f());
        let a = EntityKey::Item { id: ItemId(Uuid::from_u128(1)) };
        let b = EntityKey::Folder { id: FolderId(Uuid::from_u128(2)) };
        let r = EntityKey::Receipt { hash: hash(1), device: f() };
        assert_eq!(head_seq(&db.conn).unwrap(), 0);
        assert_eq!(log_change(&db.conn, &a, group::NAME, 1).unwrap(), 1);
        assert_eq!(log_change(&db.conn, &b, group::NAME, 2).unwrap(), 2);
        assert_eq!(log_change(&db.conn, &a, group::PINNED, 3).unwrap(), 3);
        assert_eq!(log_change(&db.conn, &r, group::ALL, 4).unwrap(), 4);
        assert_eq!(head_seq(&db.conn).unwrap(), 4);
        let (keys, last) = changes_since(&db.conn, 0, 100, f()).unwrap();
        assert_eq!(keys, vec![b, a, r], "ordered by last change, one entry per key");
        assert_eq!(last, 4);
        // Paged: the page ends after b; `a` (last change seq 3) comes on the next page.
        let (keys, last) = changes_since(&db.conn, 0, 1, f()).unwrap();
        assert_eq!((keys, last), (vec![b], 2));
        let (keys, last) = changes_since(&db.conn, 2, 10, f()).unwrap();
        assert_eq!((keys, last), (vec![a, r], 4));
        let (keys, last) = changes_since(&db.conn, 4, 10, f()).unwrap();
        assert!(keys.is_empty());
        assert_eq!(last, 4, "nothing new: since is returned");
        // unacked
        assert_eq!(unacked_local_change(&db.conn, &a, group::PINNED, 0, f()).unwrap(), Some(3));
        assert_eq!(unacked_local_change(&db.conn, &a, group::PINNED, 3, f()).unwrap(), None);
        assert_eq!(unacked_local_change(&db.conn, &a, group::DELETED, 0, f()).unwrap(), None);
        // trim keeps the sequence monotonic.
        set_pulled(&db.conn, f(), 0).unwrap();
        set_acked(&db.conn, f(), 3).unwrap();
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 3);
        assert_eq!(changes_since(&db.conn, 0, 10, f()).unwrap().0, vec![r]);
        set_unpaired(&db.conn, f(), T0).unwrap();
        trim_log_for_peers(&db.conn, T0).unwrap();
        assert_eq!(head_seq(&db.conn).unwrap(), 4, "head survives a trim");
        assert_eq!(log_floor(&db.conn).unwrap(), 4);
        assert_eq!(log_change(&db.conn, &a, group::NAME, 5).unwrap(), 5);
    }

    fn key_n(n: u128) -> EntityKey {
        EntityKey::Item { id: ItemId(Uuid::from_u128(n)) }
    }

    /// `n` change_log rows for distinct items, written here.
    fn log_rows(c: &Connection, n: u128) {
        for i in 1..=n {
            log_change(c, &key_n(i), group::NAME, i as u64).unwrap();
        }
    }

    /// A paired PC row with a cursor acked up to `acked`, last seen at `seen`.
    fn peer_acked(c: &Connection, id: DeviceId, acked: u64, seen: i64) {
        let row = DeviceRow { id, name: "pc".into(), is_self: false, role: Role::Pc, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec![] };
        set_paired(c, &row, T0).unwrap();
        set_last_seen(c, id, seen, &[]).unwrap();
        set_pulled(c, id, 0).unwrap();
        set_acked(c, id, acked).unwrap();
    }

    #[test]
    fn changes_since_excludes_the_puller() {
        let db = Db::open_in_memory().unwrap();
        let (a, b, c) = (key_n(1), key_n(2), key_n(3));
        log_change(&db.conn, &a, group::NAME, 1).unwrap(); // 1, written here
        log_change_from(&db.conn, &b, group::NAME, 2, f()).unwrap(); // 2, taken from f
        log_change_from(&db.conn, &c, group::NAME, 3, dev(3)).unwrap(); // 3, taken from dev 3
        log_change_from(&db.conn, &a, group::PINNED, 4, f()).unwrap(); // 4, a again, from f
        // f never gets its own changes back; a is still owed to it through the row written here.
        assert_eq!(changes_since(&db.conn, 0, 10, f()).unwrap(), (vec![a, c], 3));
        // dev 3 gets everything except what it sent; a's newest row is from f, so it is owed.
        assert_eq!(changes_since(&db.conn, 0, 10, dev(3)).unwrap(), (vec![b, a], 4));
        // Somebody else gets all of it, in order of last change.
        assert_eq!(changes_since(&db.conn, 0, 10, dev(9)).unwrap(), (vec![b, c, a], 4));
        // A cursor past the only row not from f: nothing for f.
        assert_eq!(changes_since(&db.conn, 3, 10, f()).unwrap(), (vec![], 3));
    }

    #[test]
    fn changes_since_more_probe_uses_exclusion() {
        let db = Db::open_in_memory().unwrap();
        log_change(&db.conn, &key_n(1), group::NAME, 1).unwrap();
        log_change_from(&db.conn, &key_n(2), group::NAME, 2, f()).unwrap();
        let (keys, last) = changes_since(&db.conn, 0, 1, f()).unwrap();
        assert_eq!((keys.len(), last), (1, 1));
        assert!(changes_since(&db.conn, last, 1, f()).unwrap().0.is_empty(), "nothing more for f, so the page is final");
        assert_eq!(changes_since(&db.conn, last, 1, dev(9)).unwrap().0, vec![key_n(2)], "another puller has more");
    }

    #[test]
    fn unacked_local_change_ignores_rows_from_that_peer() {
        let db = Db::open_in_memory().unwrap();
        let a = key_n(1);
        log_change_from(&db.conn, &a, group::NAME, 1, f()).unwrap(); // 1: f's own write relayed
        assert_eq!(unacked_local_change(&db.conn, &a, group::NAME, 0, f()).unwrap(), None, "f's write is not concurrent with f");
        assert_eq!(unacked_local_change(&db.conn, &a, group::NAME, 0, dev(3)).unwrap(), Some(1), "but it is a change dev 3 has not seen");
        log_change(&db.conn, &a, group::NAME, 2).unwrap(); // 2: written here
        assert_eq!(unacked_local_change(&db.conn, &a, group::NAME, 0, f()).unwrap(), Some(2));
        assert_eq!(unacked_local_change(&db.conn, &a, group::NAME, 2, f()).unwrap(), None);
    }

    #[test]
    fn trim_keeps_rows_a_live_peer_has_not_acked() {
        let db = Db::open_in_memory().unwrap();
        ensure_self(&db.conn, p(), "me", Role::Phone).unwrap();
        log_rows(&db.conn, 10);
        peer_acked(&db.conn, dev(1), 8, T0);
        peer_acked(&db.conn, dev(2), 4, T0);
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 4);
        assert_eq!(log_floor(&db.conn).unwrap(), 4);
        assert_eq!(changes_since(&db.conn, 0, 100, dev(9)).unwrap().0.len(), 6, "rows 5..=10 stay for the slower peer");
        set_acked(&db.conn, dev(2), 9).unwrap();
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 8, "now the other peer is the slowest");
        assert_eq!(changes_since(&db.conn, 0, 100, dev(9)).unwrap().0.len(), 2);
    }

    #[test]
    fn cursorless_peer_pins_the_log() {
        let db = Db::open_in_memory().unwrap();
        log_rows(&db.conn, 10);
        peer_acked(&db.conn, dev(1), 10, T0);
        // A PC that is linking now: paired, no cursor yet. Its first snapshot is still running.
        set_paired(&db.conn, &DeviceRow { id: dev(2), name: "new".into(), is_self: false, role: Role::Pc, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec![] }, T0).unwrap();
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 0);
        assert_eq!(log_floor(&db.conn).unwrap(), 0);
        assert_eq!(changes_since(&db.conn, 0, 100, dev(9)).unwrap().0.len(), 10, "nothing trimmed");
        // Once it has a cursor the log can follow its acks again.
        set_pulled(&db.conn, dev(2), 0).unwrap();
        set_acked(&db.conn, dev(2), 6).unwrap();
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 6);
    }

    #[test]
    fn trim_ignores_peer_unseen_60_days() {
        let db = Db::open_in_memory().unwrap();
        log_rows(&db.conn, 10);
        peer_acked(&db.conn, dev(1), 10, T0);
        peer_acked(&db.conn, dev(2), 0, T0 - 61 * crate::clock::DAY_MS);
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 10, "the long-absent peer no longer holds the log back");
        // Seen 59 days ago it still counts.
        let db = Db::open_in_memory().unwrap();
        log_rows(&db.conn, 10);
        peer_acked(&db.conn, dev(1), 10, T0);
        peer_acked(&db.conn, dev(2), 0, T0 - 59 * crate::clock::DAY_MS);
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 0);
        // Every linked peer long gone: nobody to keep rows for.
        let db = Db::open_in_memory().unwrap();
        log_rows(&db.conn, 10);
        peer_acked(&db.conn, dev(1), 3, T0 - 90 * crate::clock::DAY_MS);
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 10);
    }

    #[test]
    fn trim_cap_at_max_rows() {
        let db = Db::open_in_memory().unwrap();
        log_rows(&db.conn, 10);
        peer_acked(&db.conn, dev(1), 0, T0);
        assert_eq!(trim_log_capped(&db.conn, T0, 3).unwrap(), 7, "the newest 3 rows stay whoever still needs the rest");
        assert_eq!(log_floor(&db.conn).unwrap(), 7);
        assert_eq!(changes_since(&db.conn, 0, 100, dev(9)).unwrap().0.len(), 3);
        // Under the cap nothing extra goes.
        assert_eq!(trim_log_capped(&db.conn, T0, 3).unwrap(), 7);
    }

    #[test]
    fn trim_never_moves_floor_back() {
        let db = Db::open_in_memory().unwrap();
        log_rows(&db.conn, 10);
        peer_acked(&db.conn, dev(1), 2, T0);
        set_log_floor(&db.conn, 8).unwrap();
        set_log_floor(&db.conn, 3).unwrap();
        assert_eq!(log_floor(&db.conn).unwrap(), 8, "a lower floor is ignored");
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 8, "an old ack does not lower it either");
        assert_eq!(log_floor(&db.conn).unwrap(), 8);
    }

    #[test]
    fn trim_raises_the_floor_even_when_the_delete_fails() {
        let db = Db::open_in_memory().unwrap();
        log_rows(&db.conn, 10);
        peer_acked(&db.conn, dev(1), 6, T0);
        db.conn
            .execute_batch("CREATE TEMP TRIGGER no_trim BEFORE DELETE ON change_log BEGIN SELECT RAISE(ABORT, 'disk full'); END")
            .unwrap();
        assert!(trim_log_for_peers(&db.conn, T0).is_err());
        // Rows may still be there, but the floor must not be below any row that went.
        assert_eq!(log_floor(&db.conn).unwrap(), 6, "an interrupted trim leaves the floor high, never low");
    }

    #[test]
    fn trim_with_no_peer_empties_log_floor_head() {
        let db = Db::open_in_memory().unwrap();
        log_rows(&db.conn, 5);
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 5);
        assert!(changes_since(&db.conn, 0, 100, dev(9)).unwrap().0.is_empty());
        assert_eq!((log_floor(&db.conn).unwrap(), head_seq(&db.conn).unwrap()), (5, 5));
        assert_eq!(log_change(&db.conn, &key_n(1), group::NAME, 9).unwrap(), 6, "seqs keep counting");
        // Nothing logged and nothing linked: nothing to do.
        let db = Db::open_in_memory().unwrap();
        assert_eq!(trim_log_for_peers(&db.conn, T0).unwrap(), 0);
        assert_eq!(log_floor(&db.conn).unwrap(), 0);
    }

    #[test]
    fn pending_unpairs_reads_legacy_object() {
        let db = Db::open_in_memory().unwrap();
        assert!(pending_unpairs(&db.conn).unwrap().is_empty());
        assert!(!is_pending_unpair(&db.conn, f()).unwrap());
        // A 0.2.x database holds one bare object under the same key.
        set_value(&db.conn, "pending_unpair", &format!("{{\"peer\":\"{}\",\"addresses\":[\"10.0.0.9:4000\"]}}", hex::encode(f().0))).unwrap();
        let list = pending_unpairs(&db.conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].peer_id().unwrap(), f());
        assert_eq!(list[0].addresses, vec!["10.0.0.9:4000".to_string()]);
        assert!(is_pending_unpair(&db.conn, f()).unwrap());
        // The next write turns it into a list and keeps the old entry.
        add_pending_unpair(&db.conn, &PendingUnpair { peer: hex::encode(dev(3).0), addresses: vec![] }).unwrap();
        let list = pending_unpairs(&db.conn).unwrap();
        assert_eq!(list.iter().map(|x| x.peer_id().unwrap()).collect::<Vec<_>>(), vec![f(), dev(3)]);
        // Garbage is an error naming the key, never silently empty.
        set_value(&db.conn, "pending_unpair", "{not json").unwrap();
        assert!(pending_unpairs(&db.conn).unwrap_err().to_string().contains("pending_unpair"));
    }

    #[test]
    fn pending_unpairs_add_replace_remove() {
        let db = Db::open_in_memory().unwrap();
        let note = |d: DeviceId, addr: &str| PendingUnpair { peer: hex::encode(d.0), addresses: vec![addr.to_string()] };
        add_pending_unpair(&db.conn, &note(f(), "10.0.0.1:1")).unwrap();
        add_pending_unpair(&db.conn, &note(dev(3), "10.0.0.3:3")).unwrap();
        add_pending_unpair(&db.conn, &note(f(), "10.0.0.2:2")).unwrap();
        let list = pending_unpairs(&db.conn).unwrap();
        assert_eq!(list.len(), 2, "the same peer is replaced, not added twice");
        assert_eq!(list[0].addresses, vec!["10.0.0.2:2".to_string()]);
        assert!(is_pending_unpair(&db.conn, f()).unwrap() && is_pending_unpair(&db.conn, dev(3)).unwrap());
        remove_pending_unpair(&db.conn, f()).unwrap();
        assert!(!is_pending_unpair(&db.conn, f()).unwrap());
        assert!(is_pending_unpair(&db.conn, dev(3)).unwrap(), "the other notice stays");
        remove_pending_unpair(&db.conn, dev(9)).unwrap();
        assert_eq!(pending_unpairs(&db.conn).unwrap().len(), 1, "removing an unknown peer changes nothing");
        remove_pending_unpair(&db.conn, dev(3)).unwrap();
        assert!(pending_unpairs(&db.conn).unwrap().is_empty());
        assert_eq!(get_value(&db.conn, "pending_unpair").unwrap(), None, "an empty list leaves no row");
    }

    #[test]
    fn items_in_state_after_holder_filter() {
        let db = db_with_peer(p(), f());
        // 1: written by f. 2: written here, f has a receipt. 3: written here, no receipt.
        // 4: written by dev 3, f has a receipt for the same bytes as 2.
        let it1 = item(1, f(), v(1, f()));
        let it2 = item(2, p(), v(1, p()));
        let it3 = item(3, p(), v(1, p()));
        let mut it4 = item(4, dev(3), v(1, dev(3)));
        it4.content.val.hash = hash(2);
        for it in [&it1, &it2, &it3, &it4] {
            insert_item(&db.conn, it, LocalFileState::Pending, T0).unwrap();
        }
        insert_receipt(&db.conn, &ReceiptRecord { hash: hash(2), device: f(), received_at: 1 }, 1).unwrap();
        let ids = |holder| items_in_state_after(&db.conn, &[LocalFileState::Pending], None, 100, holder).unwrap().into_iter().map(|(_, i)| i).collect::<Vec<_>>();
        assert_eq!(ids(None), vec![it1.id, it2.id, it3.id, it4.id], "no holder: no filter");
        assert_eq!(ids(Some(f())), vec![it1.id, it2.id, it4.id], "content from f, or a receipt from f");
        assert_eq!(ids(Some(dev(3))), vec![it4.id], "only what dev 3 wrote");
        assert_eq!(ids(Some(dev(9))), Vec::<ItemId>::new());
        // The cursor still pages through the filtered rows.
        let after = Some((it1.captured_at, it1.id));
        let page = items_in_state_after(&db.conn, &[LocalFileState::Pending], after, 1, Some(f())).unwrap();
        assert_eq!(page.into_iter().map(|(_, i)| i).collect::<Vec<_>>(), vec![it2.id]);
        // pending_held_by is the same test, one answer.
        assert!(pending_held_by(&db.conn, f()).unwrap());
        assert!(!pending_held_by(&db.conn, dev(9)).unwrap());
        set_local_state(&db.conn, it1.id, LocalFileState::Present, T0).unwrap();
        set_local_state(&db.conn, it2.id, LocalFileState::Present, T0).unwrap();
        set_local_state(&db.conn, it4.id, LocalFileState::Present, T0).unwrap();
        assert!(!pending_held_by(&db.conn, f()).unwrap(), "only Pending items count");
    }

    #[test]
    fn paired_peers_and_the_link_count() {
        let db = Db::open_in_memory().unwrap();
        ensure_self(&db.conn, p(), "me", Role::Phone).unwrap();
        assert!(paired_peers(&db.conn).unwrap().is_empty());
        let row = |d: DeviceId| DeviceRow { id: d, name: format!("pc {}", d.0[0]), is_self: false, role: Role::Pc, paired_at: None, unpaired_at: None, last_seen_at: None, last_synced_at: None, addresses: vec![] };
        set_paired(&db.conn, &row(dev(2)), T0 + 5).unwrap();
        set_paired(&db.conn, &row(dev(1)), T0).unwrap();
        set_paired(&db.conn, &row(dev(3)), T0 + 9).unwrap();
        assert_eq!(paired_peers(&db.conn).unwrap().iter().map(|d| d.id).collect::<Vec<_>>(), vec![dev(1), dev(2), dev(3)], "oldest link first");
        assert_eq!(count_paired_except(&db.conn, dev(9)).unwrap(), 3);
        assert_eq!(count_paired_except(&db.conn, dev(2)).unwrap(), 2, "a peer linking again is not counted against itself");
        set_unpaired(&db.conn, dev(1), T0 + 10).unwrap();
        assert_eq!(paired_peers(&db.conn).unwrap().len(), 2);
        assert_eq!(count_paired_except(&db.conn, dev(9)).unwrap(), 2);
        // last synced only moves forward
        set_last_synced(&db.conn, dev(2), 50).unwrap();
        set_last_synced(&db.conn, dev(2), 40).unwrap();
        assert_eq!(get_device(&db.conn, dev(2)).unwrap().unwrap().last_synced_at, Some(50));
    }

    #[test]
    fn cursors_acked_needs_existing_row() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(get_cursor(&db.conn, f()).unwrap(), None);
        set_acked(&db.conn, f(), 5).unwrap();
        assert_eq!(get_cursor(&db.conn, f()).unwrap(), None, "no cursor created by an ack");
        set_pulled(&db.conn, f(), 7).unwrap();
        set_acked(&db.conn, f(), 5).unwrap();
        set_acked(&db.conn, f(), 3).unwrap();
        assert_eq!(get_cursor(&db.conn, f()).unwrap(), Some((7, 5)), "ack never moves backwards");
        set_pulled(&db.conn, f(), 9).unwrap();
        assert_eq!(get_cursor(&db.conn, f()).unwrap(), Some((9, 5)));
        delete_cursor(&db.conn, f()).unwrap();
        assert_eq!(get_cursor(&db.conn, f()).unwrap(), None);
    }

    #[test]
    fn records_after_orders_folders_places_items_receipts() {
        let db = db_with_peer(p(), f());
        for n in [3u8, 1, 2] {
            insert_item(&db.conn, &item(n, p(), v(1, p())), LocalFileState::Present, T0).unwrap();
        }
        insert_receipt(&db.conn, &ReceiptRecord { hash: hash(2), device: f(), received_at: 1 }, 1).unwrap();
        insert_receipt(&db.conn, &ReceiptRecord { hash: hash(1), device: p(), received_at: 1 }, 1).unwrap();
        put_place(
            &db.conn,
            &PlaceRecord {
                id: PlaceId(Uuid::from_u128(9)),
                name: Versioned { v: GENESIS, val: "x".into() },
                area: Versioned { v: GENESIS, val: Area { lat: 0.0, lon: 0.0, radius_m: 1.0 } },
                target: Versioned { v: GENESIS, val: INBOX_ID },
                deleted: Versioned { v: GENESIS, val: None },
            },
        )
        .unwrap();
        let all = records_after(&db.conn, None, 100).unwrap();
        let kinds: Vec<&str> = all
            .iter()
            .map(|r| match r {
                Record::Folder(_) => "F",
                Record::Place(_) => "P",
                Record::Item(_) => "I",
                Record::Receipt(_) => "R",
            })
            .collect();
        assert_eq!(kinds.join(""), "FFFPIIIRR");
        // Paging through one record at a time visits the same sequence.
        let mut seen = Vec::new();
        let mut after: Option<EntityKey> = None;
        loop {
            let page = records_after(&db.conn, after.as_ref(), 1).unwrap();
            let Some(rec) = page.into_iter().next() else { break };
            let key = match &rec {
                Record::Folder(x) => EntityKey::Folder { id: x.id },
                Record::Place(x) => EntityKey::Place { id: x.id },
                Record::Item(x) => EntityKey::Item { id: x.id },
                Record::Receipt(x) => EntityKey::Receipt { hash: x.hash, device: x.device },
            };
            assert_eq!(get_record(&db.conn, &key).unwrap().as_ref(), Some(&rec));
            if let Some(prev) = after {
                assert!(prev < key, "keys strictly increase");
            }
            seen.push(rec);
            after = Some(key);
        }
        assert_eq!(seen, all);
    }

    #[test]
    fn file_refs_and_ntfs_lookup() {
        let db = Db::open_in_memory().unwrap();
        let id = ItemId(Uuid::from_u128(1));
        let fr = FileRef { item: id, path: "C:\\lib\\a.jpg".into(), volume_serial: Some(0xDEAD_BEEF_u64), file_id: Some([7; 16]), mtime_ms: Some(42) };
        put_file_ref(&db.conn, &fr).unwrap();
        assert_eq!(get_file_ref(&db.conn, id).unwrap(), Some(fr.clone()));
        assert_eq!(item_by_file_id(&db.conn, 0xDEAD_BEEF, [7; 16]).unwrap(), Some(id));
        assert_eq!(item_by_file_id(&db.conn, 1, [7; 16]).unwrap(), None);
        assert_eq!(item_by_path(&db.conn, "C:\\lib\\a.jpg").unwrap(), Some(id));
        delete_file_ref(&db.conn, id).unwrap();
        assert_eq!(get_file_ref(&db.conn, id).unwrap(), None);
    }

    // An NTFS identity claimed by a second item must not delete the first item's whole row.
    #[test]
    fn put_file_ref_identity_conflict_keeps_the_other_path() {
        let db = Db::open_in_memory().unwrap();
        let (a, b) = (ItemId(Uuid::from_u128(1)), ItemId(Uuid::from_u128(2)));
        let fa = FileRef { item: a, path: "C:/lib/a.jpg".into(), volume_serial: Some(5), file_id: Some([9; 16]), mtime_ms: Some(1) };
        let fb = FileRef { item: b, path: "C:/lib/b.jpg".into(), volume_serial: Some(5), file_id: Some([9; 16]), mtime_ms: Some(2) };
        put_file_ref(&db.conn, &fa).unwrap();
        put_file_ref(&db.conn, &fb).unwrap();
        let got_a = get_file_ref(&db.conn, a).unwrap().expect("A keeps its row");
        assert_eq!(got_a.path, fa.path);
        assert_eq!((got_a.volume_serial, got_a.file_id), (None, None), "A lost only the identity");
        assert_eq!(got_a.mtime_ms, Some(1), "A keeps its modified time");
        assert_eq!(get_file_ref(&db.conn, b).unwrap(), Some(fb.clone()));
        assert_eq!(item_by_file_id(&db.conn, 5, [9; 16]).unwrap(), Some(b));
        // Same item again: plain update.
        let fb2 = FileRef { path: "C:/lib/b2.jpg".into(), ..fb };
        put_file_ref(&db.conn, &fb2).unwrap();
        assert_eq!(get_file_ref(&db.conn, b).unwrap(), Some(fb2));
    }

    // Settings defaults, validation, unparsable stored value names the key.
    #[test]
    fn settings_defaults_validation_and_bad_value() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(load_settings(&db.conn).unwrap(), Settings::default());
        let s = Settings { bin_days: 3, stay_connected: false, ..Settings::default() };
        save_settings(&db.conn, &s).unwrap();
        assert_eq!(load_settings(&db.conn).unwrap(), s);
        for bad_s in [
            Settings { bin_days: 0, ..Settings::default() },
            Settings { bin_days: 366, ..Settings::default() },
            Settings { udp_port: 80, ..Settings::default() },
            Settings { missing_pause_pct: 0, ..Settings::default() },
            Settings { missing_pause_pct: 101, ..Settings::default() },
            Settings { max_blob_bytes: 0, ..Settings::default() },
        ] {
            assert!(matches!(save_settings(&db.conn, &bad_s), Err(CoreError::Invalid(_))));
        }
        assert_eq!(load_settings(&db.conn).unwrap(), s, "rejected save changed nothing");
        // Free-form values share the table and are not settings.
        set_value(&db.conn, "library_path", "\"C:\\\\Local Tugboat\"").unwrap();
        assert_eq!(get_value(&db.conn, "library_path").unwrap().unwrap(), "\"C:\\\\Local Tugboat\"");
        assert_eq!(get_value(&db.conn, "nope").unwrap(), None);
        assert_eq!(load_settings(&db.conn).unwrap(), s);
        // Hand-edited garbage names the key.
        set_value(&db.conn, "bin_days", "\"seven\"").unwrap();
        let e = load_settings(&db.conn).unwrap_err().to_string();
        assert!(e.contains("bin_days"), "{e}");
        set_value(&db.conn, "bin_days", "{not json").unwrap();
        let e = load_settings(&db.conn).unwrap_err().to_string();
        assert!(e.contains("bin_days"), "{e}");
        set_value(&db.conn, "bin_days", "0").unwrap();
        let e = load_settings(&db.conn).unwrap_err().to_string();
        assert!(e.contains("bin_days"), "{e}");
    }
}
