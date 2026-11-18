//! Data model shared by every core module and (as JSON) by the wire protocol and the UIs.
//! Types here are the contract: a new field changes the wire format and the generated UI
//! types.
//!
//! Field groups: each synced record is merged per field group, each group
//! carrying its own `Version = (lamport, device)`. The higher version wins; versions compare
//! by lamport first, then by device key bytes (total order, so both devices pick the same
//! winner).

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

// ---------- ids ----------

/// Ed25519 public key of a device (its iroh endpoint id). JSON: 64 lowercase hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, type = "string"))]
pub struct DeviceId(pub [u8; 32]);

/// BLAKE3 hash of a file's bytes. JSON: 64 lowercase hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, type = "string"))]
pub struct BlobHash(pub [u8; 32]);

/// UUIDv7 (time-ordered) item id, generated from the injected clock. JSON: hyphenated string.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, type = "string"))]
pub struct ItemId(pub Uuid);

/// Folder id. UUIDv7, except the three default folders, which have fixed ids (see
/// `INBOX_ID`..) so both devices create the SAME records independently.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, type = "string"))]
pub struct FolderId(pub Uuid);

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, type = "string"))]
pub struct PlaceId(pub Uuid);

/// UUIDv7 built from the injected clock (never `Uuid::now_v7`, which reads the system clock).
/// Counter bits are 0, so everything below the millisecond is random: two ids made in the
/// same millisecond are different.
pub fn uuid_v7_at(now_ms: i64) -> Uuid {
    let ms = now_ms.max(0) as u64;
    Uuid::new_v7(uuid::Timestamp::from_unix_time(ms / 1000, ((ms % 1000) * 1_000_000) as u32, 0, 0))
}

impl ItemId {
    pub fn new(now_ms: i64) -> ItemId {
        ItemId(uuid_v7_at(now_ms))
    }
}

impl FolderId {
    pub fn new(now_ms: i64) -> FolderId {
        FolderId(uuid_v7_at(now_ms))
    }
}

impl PlaceId {
    pub fn new(now_ms: i64) -> PlaceId {
        PlaceId(uuid_v7_at(now_ms))
    }
}

/// Fixed ids of the default folders (ids below 0x100 are reserved for these).
pub const INBOX_ID: FolderId = FolderId(Uuid::from_u128(1));
pub const KEEP_ID: FolderId = FolderId(Uuid::from_u128(2));
pub const UNSORTED_ID: FolderId = FolderId(Uuid::from_u128(3));

/// Version of the default folders' initial field groups: lamport 0, all-zero device.
/// Any real edit (lamport >= 1) beats it; two fresh devices hold identical records.
pub const GENESIS: Version = Version { lamport: 0, device: DeviceId([0; 32]) };

fn hex32<S: Serializer>(b: &[u8; 32], s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&hex::encode(b))
}

fn unhex32<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<[u8; 32], D::Error> {
    let s = String::deserialize(d)?;
    let v = hex::decode(&s).map_err(serde::de::Error::custom)?;
    v.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes (64 hex chars)"))
}

macro_rules! hex_id {
    ($t:ident) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                hex32(&self.0, s)
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                unhex32(d).map($t)
            }
        }
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($t), &hex::encode(self.0)[..10])
            }
        }
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }
    };
}
hex_id!(DeviceId);
hex_id!(BlobHash);

// ---------- versions ----------

/// Field-group version. Ordering: lamport, then device bytes. Derived Ord follows field
/// order, which is exactly that rule; do not reorder the fields.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct Version {
    #[serde(rename = "l")]
    pub lamport: u64,
    #[serde(rename = "d")]
    pub device: DeviceId,
}

/// A field-group value with its version.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Versioned<T> {
    pub v: Version,
    pub val: T,
}

// ---------- enums ----------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Role {
    Pc,
    Phone,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum FolderKind {
    Inbox,
    Keep,
    Unsorted,
    User,
}

/// Folder retention. DB: `retention_secs` NULL = Keep.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Retention {
    /// Never expires.
    Keep,
    /// Expires `secs` after `folder_entered_at`. Always > 0.
    Secs { secs: u64 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum MediaKind {
    Image,
    Video,
    /// Any other file (0.2.0: any file type syncs). No thumbnail, no photo/video content check.
    File,
}

/// Local-only: what this device physically holds for an item. Never synced.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum LocalFileState {
    /// Wanted, not held yet (download queued).
    Pending,
    /// Held in the library / app storage.
    Present,
    /// Held in this device's Bin area.
    Binned,
    /// Not held and not wanted (purged after Bin days, or arrived already in the Bin).
    Purged,
    /// Wanted, but the peer answered "not found". Soft: asked again on every connect and every few
    /// minutes, because the answer is often momentary (renamed in Explorer, drive briefly away).
    Lost,
    /// The received file failed the type check (extension/magic) or is an always-refused name;
    /// deleted or never downloaded, never stored.
    Refused,
}

// ---------- records (synced) ----------

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Geo {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: f64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Thumb {
    pub hash: BlobHash,
    pub size: u64,
}

/// Item "content" group: a PC edit changes all of it at once.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Content {
    pub hash: BlobHash,
    pub size: u64,
    pub kind: MediaKind,
    /// Lowercase extension without the dot (`names::is_safe_ext`); may be empty (a file with no
    /// extension). Photos and videos carry one of the media extensions, every other kind never does.
    pub ext: String,
    pub thumb: Option<Thumb>,
}

/// Item "placement" group (named Location to avoid a clash with the Placement trait).
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Location {
    pub folder: FolderId,
    /// Unix ms on the device that made the move. Expiry = entered_at + retention.
    pub entered_at: i64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ItemRecord {
    pub id: ItemId,
    // Immutable after creation (written once, no version):
    pub captured_at: i64,
    pub origin: DeviceId,
    pub geo: Option<Geo>,
    // Field groups:
    pub name: Versioned<String>,
    pub content: Versioned<Content>,
    pub location: Versioned<Location>,
    pub pinned: Versioned<bool>,
    /// Some(unix ms) = user-deleted (in the Bin). None = not deleted.
    pub deleted: Versioned<Option<i64>>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct FolderRecord {
    pub id: FolderId,
    /// Immutable.
    pub kind: FolderKind,
    pub name: Versioned<String>,
    pub retention: Versioned<Retention>,
    pub deleted: Versioned<Option<i64>>,
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Area {
    pub lat: f64,
    pub lon: f64,
    pub radius_m: f64,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct PlaceRecord {
    pub id: PlaceId,
    pub name: Versioned<String>,
    pub area: Versioned<Area>,
    pub target: Versioned<FolderId>,
    pub deleted: Versioned<Option<i64>>,
}

/// "device holds (or held) the verified file with this hash". Grow-only set; no version.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ReceiptRecord {
    pub hash: BlobHash,
    pub device: DeviceId,
    /// Unix ms on `device` when it got the file.
    pub received_at: i64,
}

/// One synced record, as sent on the wire (whole record, every group with its version).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // a short-lived wire value; boxing the item would touch every match
pub enum Record {
    Item(ItemRecord),
    Folder(FolderRecord),
    Place(PlaceRecord),
    Receipt(ReceiptRecord),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Folder,
    Place,
    Item,
    Receipt,
}

/// Identity of a synced record. Sort order (kind, id bytes) is the digest/list order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum EntityKey {
    Folder { id: FolderId },
    Place { id: PlaceId },
    Item { id: ItemId },
    Receipt { hash: BlobHash, device: DeviceId },
}

/// Field-group names as stored in change_log.field_group.
pub mod group {
    pub const NAME: &str = "name";
    pub const CONTENT: &str = "content";
    pub const PLACEMENT: &str = "placement";
    pub const PINNED: &str = "pinned";
    pub const DELETED: &str = "deleted";
    pub const RETENTION: &str = "retention";
    pub const AREA: &str = "area";
    pub const TARGET: &str = "target";
    /// Receipts and record creation.
    pub const ALL: &str = "all";
}

// ---------- local rows ----------

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct DeviceRow {
    pub id: DeviceId,
    pub name: String,
    pub is_self: bool,
    pub role: Role,
    pub paired_at: Option<i64>,
    pub unpaired_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    /// Last-known "ip:port" addresses (local only, dialed first by the phone).
    pub addresses: Vec<String>,
}

/// Local settings (`setting` table). Business rules are settings, not constants.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Settings {
    /// Days an item stays in the Bin before this device deletes its copy. Default 7.
    pub bin_days: u32,
    /// Phone: stop downloading below this many free bytes. Default 2 GiB.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub storage_guard_bytes: u64,
    /// PC rescan: pause when more than this many files are missing. Default 20.
    pub missing_pause_files: u32,
    /// PC rescan: pause when more than this percent of the library is missing. Default 10.
    pub missing_pause_pct: u32,
    /// PC UDP port. Default 47300.
    pub udp_port: u16,
    /// Largest accepted blob. Default 16 GiB.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub max_blob_bytes: u64,
    /// Phone: keep the connection in the background. Default true.
    pub stay_connected: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bin_days: 7,
            storage_guard_bytes: 2 * 1024 * 1024 * 1024,
            missing_pause_files: 20,
            missing_pause_pct: 10,
            udp_port: 47300,
            max_blob_bytes: 16 * 1024 * 1024 * 1024,
            stay_connected: true,
        }
    }
}

// ---------- views (what the UIs get) ----------

/// Derived expiry/Bin state of an item (computed by `expiry::item_state`, never stored).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ItemState {
    /// User-deleted; in the Bin since `binned_at`.
    Deleted { binned_at: i64 },
    /// Expired; in the Bin since `binned_at`.
    Expired { binned_at: i64 },
    /// Pinned (Keep on the item). Label "Kept".
    Kept,
    /// In a folder whose retention is Keep (or an unknown/deleted folder). Label "Keep".
    FolderKeep,
    /// Label "Expires in 5h".
    Live { expires_at: i64 },
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ItemView {
    pub id: ItemId,
    pub name: String,
    pub folder: FolderId,
    pub kind: MediaKind,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub size: u64,
    pub ext: String,
    pub captured_at: i64,
    pub origin: DeviceId,
    pub pinned: bool,
    pub state: ItemState,
    pub local: LocalFileState,
    /// Local path of the held file (library or Bin), when held.
    pub path: Option<String>,
    /// Local path of the thumbnail, when held. Always None for a File.
    pub thumb_path: Option<String>,
    /// In the Bin because it EXPIRED, and the paired device never confirmed holding this file:
    /// the item may now exist on this device only (shown as "NOT ON PC" / "NOT ON PHONE").
    pub never_reached_peer: bool,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FolderView {
    pub id: FolderId,
    pub kind: FolderKind,
    pub name: String,
    pub retention: Retention,
    /// Items not in the Bin.
    pub count: u32,
}

/// Paging: items sorted by (captured_at DESC, id DESC); `after` is the last item of the
/// previous page. `limit` is capped at 500.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ItemQuery {
    /// None = all folders.
    pub folder: Option<FolderId>,
    /// true = only items in the Bin; false = only items not in the Bin.
    pub bin: bool,
    pub after: Option<ItemId>,
    pub limit: u32,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Page<T> {
    pub rows: Vec<T>,
    /// Pass back as `after` for the next page; None = last page.
    pub next: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ActivityAction {
    /// Phone camera shot.
    Captured,
    /// File added locally: PC library drop, phone "Share > Tugboat".
    Added,
    /// File received from the other device and verified.
    Received,
    Moved,
    Renamed,
    Kept,
    Unkept,
    /// Content changed (PC edit).
    Edited,
    /// Deleted on this device (to the Bin).
    Deleted,
    /// Deleted on the other device (to the Bin here).
    Binned,
    Expired,
    Restored,
    /// Left the Bin because of a synced change (Keep or move on the other device).
    Returned,
    /// This device deleted its copy after Bin days.
    Purged,
    /// Received file failed the type check.
    Refused,
    /// Same field changed on both devices while apart; detail names winner and loser.
    Conflict,
    /// The other device did not have the file when asked (retried later; logged once).
    Lost,
    FolderCreated,
    FolderRenamed,
    FolderDeleted,
    RetentionChanged,
    Paired,
    Unpaired,
    /// PC library missing / bulk-missing pause.
    Paused,
    Resumed,
    /// Sync refused (version mismatch, not paired) or reconcile mismatch.
    SyncError,
    /// The app was updated and carried its data over from the old file names (phone, 0.1.1).
    Upgraded,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ActivityEntry {
    pub id: i64,
    pub at: i64,
    /// Device the change came from (self or the peer); None for local system events.
    pub device: Option<DeviceId>,
    pub item: Option<ItemId>,
    pub action: ActivityAction,
    /// Short human text, e.g. "Inbox -> Work", "kept on phone, delete on PC lost".
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dv(l: u64, b: u8) -> Version {
        Version { lamport: l, device: DeviceId([b; 32]) }
    }

    // Version order = lamport, then device bytes; GENESIS is the lowest.
    #[test]
    fn version_order() {
        assert!(dv(2, 0x01) > dv(1, 0xFF), "lamport first");
        assert!(dv(5, 0xBB) > dv(5, 0xAA), "then device bytes");
        assert_eq!(dv(5, 0xAA), dv(5, 0xAA));
        assert!(dv(0, 1) > GENESIS && dv(1, 0) > GENESIS);
        assert!(GENESIS <= dv(0, 0));
        // Device bytes compare lexicographically from the first byte.
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        a[0] = 1;
        b[31] = 0xFF;
        assert!(Version { lamport: 1, device: DeviceId(a) } > Version { lamport: 1, device: DeviceId(b) });
    }

    #[test]
    fn ids_serialise_as_hex_and_reject_bad_input() {
        let d = DeviceId([0xAB; 32]);
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
        assert_eq!(serde_json::from_str::<DeviceId>(&json).unwrap(), d);
        assert!(serde_json::from_str::<DeviceId>("\"abcd\"").is_err(), "wrong length");
        assert!(serde_json::from_str::<DeviceId>(&format!("\"{}\"", "zz".repeat(32))).is_err(), "not hex");
        assert!(serde_json::from_str::<BlobHash>("12").is_err());
        assert!(format!("{d:?}").len() < 40, "Debug is short");
    }

    #[test]
    fn version_wire_shape_and_record_roundtrip() {
        let v = dv(7, 0xBB);
        let json = serde_json::to_value(v).unwrap();
        assert_eq!(json["l"], 7);
        assert_eq!(json["d"], "bb".repeat(32));
        let rec = Record::Folder(FolderRecord {
            id: INBOX_ID,
            kind: FolderKind::Inbox,
            name: Versioned { v, val: "Inbox".into() },
            retention: Versioned { v, val: Retention::Secs { secs: 86_400 } },
            deleted: Versioned { v, val: None },
        });
        let text = serde_json::to_string(&rec).unwrap();
        assert!(text.contains("\"e\":\"folder\""), "{text}");
        assert_eq!(serde_json::from_str::<Record>(&text).unwrap(), rec);
        let key = EntityKey::Receipt { hash: BlobHash([1; 32]), device: DeviceId([2; 32]) };
        assert_eq!(serde_json::from_str::<EntityKey>(&serde_json::to_string(&key).unwrap()).unwrap(), key);
    }

    #[test]
    fn entity_keys_sort_folders_places_items_receipts() {
        let u = |n| uuid::Uuid::from_u128(n);
        let mut keys = [
            EntityKey::Receipt { hash: BlobHash([0; 32]), device: DeviceId([0; 32]) },
            EntityKey::Item { id: ItemId(u(1)) },
            EntityKey::Place { id: PlaceId(u(1)) },
            EntityKey::Folder { id: FolderId(u(9)) },
            EntityKey::Folder { id: FolderId(u(2)) },
        ];
        keys.sort();
        let kinds: Vec<&str> = keys
            .iter()
            .map(|k| match k {
                EntityKey::Folder { .. } => "F",
                EntityKey::Place { .. } => "P",
                EntityKey::Item { .. } => "I",
                EntityKey::Receipt { .. } => "R",
            })
            .collect();
        assert_eq!(kinds.join(""), "FFPIR");
        assert!(keys[0] < keys[1]);
    }

    // Ids come from the injected clock, are time ordered and unique within one millisecond.
    #[test]
    fn uuid_v7_follows_the_given_time() {
        let ms = 1_794_924_153_123_i64;
        let a = ItemId::new(ms);
        let b = ItemId::new(ms);
        assert_ne!(a, b, "random bits differ within a millisecond");
        assert_eq!(a.0.get_version_num(), 7);
        let (secs, nanos) = a.0.get_timestamp().unwrap().to_unix();
        assert_eq!(secs * 1000 + nanos as u64 / 1_000_000, ms as u64);
        assert!(ItemId::new(ms + 1) > ItemId::new(ms), "later time sorts later");
        // Negative time does not panic.
        let _ = FolderId::new(-5);
        assert!(PlaceId::new(ms).0.get_timestamp().is_some());
    }

    #[test]
    fn default_folder_ids_are_fixed() {
        assert_eq!(INBOX_ID.0.to_string(), "00000000-0000-0000-0000-000000000001");
        assert_eq!(KEEP_ID.0.to_string(), "00000000-0000-0000-0000-000000000002");
        assert_eq!(UNSORTED_ID.0.to_string(), "00000000-0000-0000-0000-000000000003");
    }
}
