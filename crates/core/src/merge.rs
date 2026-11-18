//! Merge rules. Owner: logic builder. Pure functions first, then `apply_remote`, the single
//! entry point the sync engine calls for every record it receives.
//!
//! Rules (PLAN Sync model; tables with worked examples in notes/core-design.md "Merge"):
//! M1 Per group: the higher `Version` wins (lamport, then device bytes). Equal versions =
//!    same write; keep local. Values are never compared to pick a winner.
//! M2 Groups merge independently: a PC edit (content) and a phone move (placement) made
//!    while apart both survive.
//! M3 Immutable fields (id, captured_at, origin, geo, folder kind) come from whichever copy
//!    arrived first; a differing remote copy is a protocol error, logged, local kept.
//! M4 Receipts are a grow-only set: insert if absent, never removed.
//! M5 Delete vs Keep: if after merging an item has deleted = Some AND pinned = true, the
//!    later intent (by version) wins and the merging device rewrites the loser group with a
//!    fresh local version: pinned.v > deleted.v -> deleted := None; otherwise pinned := false.
//!    Activity `Conflict` is logged. Both devices compute the same winner from the same
//!    versions; their rewrites carry the same value, so they converge.
//! M6 Conflict logging (PLAN question 4: "the loser is logged in activity"): when a group's
//!    values differ and this device has an unacked local change to that group (change_log seq
//!    greater than our_seq_acked for the peer), the change was concurrent: log `Conflict` with the
//!    winning and losing values, whichever side won. Non-concurrent overwrites are not conflicts.
//! M7 Every remote version passes through `store::observe_lamport` before any local write.
//! M8 New record: inserted as received; items get LocalFileState::Pending (the Bin sweep
//!    turns it into Purged if the item is already in the Bin).
//! M9 Remote-applied changes are not written to change_log; M5 rewrites are (they are local).
//!
//! Also at the trust boundary (a paired peer is still validated, PLAN Security): a remote
//! record with an out-of-range or unsafe field is a `Protocol` error and is not stored.

use rusqlite::Connection;

use crate::{activity, model::*, names, store, CoreError, Result};

/// Which side a group merge keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Local,
    Remote,
}

/// M1: `Remote` only if remote.v > local.v.
pub fn pick<T>(local: &Versioned<T>, remote: &Versioned<T>) -> Pick {
    if remote.v > local.v {
        Pick::Remote
    } else {
        Pick::Local
    }
}

/// Item field group names in a fixed order (for loops, change_log and conflict detail).
pub const ITEM_GROUPS: [&str; 5] = [group::NAME, group::CONTENT, group::PLACEMENT, group::PINNED, group::DELETED];

#[derive(Debug, Clone, PartialEq)]
pub struct ItemMerge {
    pub merged: ItemRecord,
    /// Groups taken from remote (values or versions changed locally).
    pub taken: Vec<&'static str>,
    /// Groups where both values differed (candidates for M6 conflict logging).
    pub differing: Vec<&'static str>,
}

/// M1 for one group. Pushes onto `taken` when remote wins, onto `differing` when the two
/// writes are different AND their values differ.
fn merge_group<T: Clone + PartialEq>(
    name: &'static str,
    local: &Versioned<T>,
    remote: &Versioned<T>,
    taken: &mut Vec<&'static str>,
    differing: &mut Vec<&'static str>,
) -> Versioned<T> {
    if local.v != remote.v && local.val != remote.val {
        differing.push(name);
    }
    match pick(local, remote) {
        Pick::Remote => {
            taken.push(name);
            remote.clone()
        }
        Pick::Local => local.clone(),
    }
}

/// M1-M3 for items. Immutable-field mismatch -> Err(Protocol).
pub fn merge_item(local: &ItemRecord, remote: &ItemRecord) -> Result<ItemMerge> {
    if local.id != remote.id {
        return Err(CoreError::Protocol("merge of two different items".into()));
    }
    if local.captured_at != remote.captured_at || local.origin != remote.origin || local.geo != remote.geo {
        return Err(CoreError::Protocol(format!("item {} differs in captured_at, origin or location", local.id.0)));
    }
    let (mut taken, mut differing) = (Vec::new(), Vec::new());
    let merged = ItemRecord {
        id: local.id,
        captured_at: local.captured_at,
        origin: local.origin,
        geo: local.geo,
        name: merge_group(group::NAME, &local.name, &remote.name, &mut taken, &mut differing),
        content: merge_group(group::CONTENT, &local.content, &remote.content, &mut taken, &mut differing),
        location: merge_group(group::PLACEMENT, &local.location, &remote.location, &mut taken, &mut differing),
        pinned: merge_group(group::PINNED, &local.pinned, &remote.pinned, &mut taken, &mut differing),
        deleted: merge_group(group::DELETED, &local.deleted, &remote.deleted, &mut taken, &mut differing),
    };
    Ok(ItemMerge { merged, taken, differing })
}

/// M5 decision for one record (pure). None = no conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteKeep {
    /// Keep was the later intent: clear `deleted`.
    KeepWins,
    /// Delete was the later intent: clear `pinned`.
    DeleteWins,
}

pub fn delete_vs_keep(r: &ItemRecord) -> Option<DeleteKeep> {
    if r.deleted.val.is_none() || !r.pinned.val {
        return None;
    }
    // Equal versions cannot come from two real writes; delete wins the tie so a user's
    // delete is never undone by a stale pin.
    Some(if r.pinned.v > r.deleted.v { DeleteKeep::KeepWins } else { DeleteKeep::DeleteWins })
}

#[derive(Debug, Clone, PartialEq)]
pub struct FolderMerge {
    pub merged: FolderRecord,
    pub taken: Vec<&'static str>,
    pub differing: Vec<&'static str>,
}

/// M1-M3 for folders (groups name, retention, deleted; kind immutable).
pub fn merge_folder(local: &FolderRecord, remote: &FolderRecord) -> Result<FolderMerge> {
    if local.id != remote.id {
        return Err(CoreError::Protocol("merge of two different folders".into()));
    }
    if local.kind != remote.kind {
        return Err(CoreError::Protocol(format!("folder {} differs in kind", local.id.0)));
    }
    let (mut taken, mut differing) = (Vec::new(), Vec::new());
    let merged = FolderRecord {
        id: local.id,
        kind: local.kind,
        name: merge_group(group::NAME, &local.name, &remote.name, &mut taken, &mut differing),
        retention: merge_group(group::RETENTION, &local.retention, &remote.retention, &mut taken, &mut differing),
        deleted: merge_group(group::DELETED, &local.deleted, &remote.deleted, &mut taken, &mut differing),
    };
    Ok(FolderMerge { merged, taken, differing })
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceMerge {
    pub merged: PlaceRecord,
    pub taken: Vec<&'static str>,
    pub differing: Vec<&'static str>,
}

/// M1-M2 for places (groups name, area, target, deleted).
pub fn merge_place(local: &PlaceRecord, remote: &PlaceRecord) -> Result<PlaceMerge> {
    if local.id != remote.id {
        return Err(CoreError::Protocol("merge of two different places".into()));
    }
    let (mut taken, mut differing) = (Vec::new(), Vec::new());
    let merged = PlaceRecord {
        id: local.id,
        name: merge_group(group::NAME, &local.name, &remote.name, &mut taken, &mut differing),
        area: merge_group(group::AREA, &local.area, &remote.area, &mut taken, &mut differing),
        target: merge_group(group::TARGET, &local.target, &remote.target, &mut taken, &mut differing),
        deleted: merge_group(group::DELETED, &local.deleted, &remote.deleted, &mut taken, &mut differing),
    };
    Ok(PlaceMerge { merged, taken, differing })
}

/// What applying one remote record did, for events and the fetch queue.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Applied {
    /// Some if anything changed locally.
    pub changed: Option<EntityKey>,
    /// Item content hash changed or the item is new: the fetch queue must look at it.
    pub content_changed: bool,
    /// Number of Conflict activity rows written.
    pub conflicts: u32,
    /// M5 rewrote a group (a new local change exists: send Notify).
    pub wrote_local: bool,
}

// ---------- validation of remote records (trust boundary) ----------

/// Longest accepted display name (chars). Real names are far shorter; this bounds storage.
const MAX_REMOTE_NAME_CHARS: usize = 1024;

fn proto(msg: impl Into<String>) -> CoreError {
    CoreError::Protocol(msg.into())
}

fn check_name(what: &str, name: &str) -> Result<()> {
    if name.chars().count() > MAX_REMOTE_NAME_CHARS {
        return Err(proto(format!("{what} name is longer than {MAX_REMOTE_NAME_CHARS} characters")));
    }
    Ok(())
}

/// Every version in a remote record is bounded, so none can pin the local clock (M7).
fn check_lamports(vs: &[Version]) -> Result<()> {
    if vs.iter().any(|v| v.lamport > store::MAX_LAMPORT) {
        return Err(proto("version out of range"));
    }
    Ok(())
}

fn validate_remote(record: &Record) -> Result<()> {
    match record {
        Record::Item(r) => {
            check_name("item", &r.name.val)?;
            // The extension is used to build paths: only safe ones pass (empty, or 1-16 lowercase
            // a-z 0-9 - _). A record may carry any file type now, but kind and extension must agree
            // (a File with a photo extension would skip the content check, a photo with a document
            // extension would get a thumbnail). A record whose NAME or extension is one of the
            // always-refused ones is still ACCEPTED as a record (both devices' digests must stay
            // equal) and never downloaded (sync::net::fetch_item).
            let ext = &r.content.val.ext;
            if !names::is_safe_ext(ext) {
                return Err(proto(format!("item {} has extension '{}' which cannot be used", r.id.0, ext)));
            }
            match (r.content.val.kind, names::ext_family(ext)) {
                (MediaKind::File, None) => {}
                (MediaKind::Image | MediaKind::Video, Some(fam)) if fam.kind() == r.content.val.kind => {}
                _ => return Err(proto(format!("item {} has kind {:?} and extension '{}' which do not belong together", r.id.0, r.content.val.kind, ext))),
            }
            if r.content.val.size > i64::MAX as u64 || r.content.val.thumb.as_ref().is_some_and(|t| t.size > i64::MAX as u64) {
                return Err(proto("item size out of range"));
            }
            check_lamports(&[r.name.v, r.content.v, r.location.v, r.pinned.v, r.deleted.v])?;
        }
        Record::Folder(r) => {
            check_name("folder", &r.name.val)?;
            check_lamports(&[r.name.v, r.retention.v, r.deleted.v])?;
            // The Keep folder keeps everything: an expiring retention would delete unpinned Keep items.
            if (r.id == KEEP_ID || r.kind == FolderKind::Keep) && r.retention.val != Retention::Keep {
                return Err(proto("the Keep folder keeps everything"));
            }
            if matches!(r.retention.val, Retention::Secs { secs: 0 }) {
                return Err(proto("folder retention of 0 seconds"));
            }
            if let Retention::Secs { secs } = r.retention.val {
                if secs > i64::MAX as u64 {
                    return Err(proto("folder retention out of range"));
                }
            }
        }
        Record::Place(r) => {
            check_name("place", &r.name.val)?;
            check_lamports(&[r.name.v, r.area.v, r.target.v, r.deleted.v])?;
            let a = r.area.val;
            let ok = a.lat.is_finite()
                && a.lon.is_finite()
                && a.radius_m.is_finite()
                && (-90.0..=90.0).contains(&a.lat)
                && (-180.0..=180.0).contains(&a.lon)
                && a.radius_m > 0.0;
            if !ok {
                return Err(proto("place area out of range"));
            }
        }
        Record::Receipt(_) => {}
    }
    Ok(())
}

fn max_lamport_of(record: &Record) -> u64 {
    match record {
        Record::Item(r) => [r.name.v, r.content.v, r.location.v, r.pinned.v, r.deleted.v].iter().map(|v| v.lamport).max().unwrap_or(0),
        Record::Folder(r) => [r.name.v, r.retention.v, r.deleted.v].iter().map(|v| v.lamport).max().unwrap_or(0),
        Record::Place(r) => [r.name.v, r.area.v, r.target.v, r.deleted.v].iter().map(|v| v.lamport).max().unwrap_or(0),
        Record::Receipt(_) => 0,
    }
}

// ---------- conflict text ----------

fn folder_label(c: &Connection, id: FolderId) -> String {
    match store::get_folder(c, id) {
        Ok(Some(f)) => f.name.val,
        _ => "unknown folder".to_string(),
    }
}

fn retention_label(r: Retention) -> String {
    match r {
        Retention::Keep => "Keep".to_string(),
        Retention::Secs { secs } if secs % 86_400 == 0 => format!("{} d", secs / 86_400),
        Retention::Secs { secs } if secs % 3_600 == 0 => format!("{} h", secs / 3_600),
        Retention::Secs { secs } => format!("{secs} s"),
    }
}

fn item_group_text(c: &Connection, r: &ItemRecord, g: &str) -> String {
    match g {
        group::NAME => format!("\"{}\"", r.name.val),
        group::CONTENT => format!("file {}", &hex::encode(r.content.val.hash.0)[..8]),
        group::PLACEMENT => format!("folder {}", folder_label(c, r.location.val.folder)),
        group::PINNED => (if r.pinned.val { "Keep on" } else { "Keep off" }).to_string(),
        _ => (if r.deleted.val.is_some() { "deleted" } else { "not deleted" }).to_string(),
    }
}

fn folder_group_text(f: &FolderRecord, g: &str) -> String {
    match g {
        group::NAME => format!("\"{}\"", f.name.val),
        group::RETENTION => format!("retention {}", retention_label(f.retention.val)),
        _ => (if f.deleted.val.is_some() { "deleted" } else { "not deleted" }).to_string(),
    }
}

fn place_group_text(c: &Connection, pl: &PlaceRecord, g: &str) -> String {
    match g {
        group::NAME => format!("\"{}\"", pl.name.val),
        group::AREA => format!("{:.0} m radius", pl.area.val.radius_m),
        group::TARGET => format!("folder {}", folder_label(c, pl.target.val)),
        _ => (if pl.deleted.val.is_some() { "deleted" } else { "not deleted" }).to_string(),
    }
}

/// M6 for one record: log a Conflict row for each differing group where this device has a
/// local change the peer has not acked. No cursor = first snapshot with this peer: nothing
/// is concurrent yet (open issue 6 in the design doc).
#[allow(clippy::too_many_arguments)]
fn log_conflicts(
    c: &Connection,
    peer: DeviceId,
    key: &EntityKey,
    item: Option<ItemId>,
    differing: &[&'static str],
    taken: &[&'static str],
    text: &dyn Fn(&str, bool) -> String,
    now: i64,
) -> Result<u32> {
    let Some((_, acked)) = store::get_cursor(c, peer)? else {
        return Ok(0);
    };
    let mut n = 0;
    for g in differing {
        if store::unacked_local_change(c, key, g, acked)?.is_none() {
            continue;
        }
        let remote_won = taken.contains(g);
        let detail = format!("{g}: kept {}, lost {}", text(g, remote_won), text(g, !remote_won));
        activity::log(c, now, Some(peer), item, ActivityAction::Conflict, &detail)?;
        n += 1;
    }
    Ok(n)
}

// ---------- apply ----------

/// Apply one received record inside the caller's transaction, following M1-M9.
/// `peer` is the device the record came from (for activity rows and M6 `our_seq_acked`).
pub fn apply_remote(c: &Connection, self_id: DeviceId, peer: DeviceId, record: &Record, now: i64) -> Result<Applied> {
    validate_remote(record)?;
    // M7: before any local write (the M5 rewrite below takes the next version).
    store::observe_lamport(c, max_lamport_of(record))?;
    match record {
        Record::Receipt(r) => {
            let mut out = Applied::default();
            // M4: insert if absent. M9: not logged, the peer already has it.
            if store::insert_receipt(c, r, now)? {
                out.changed = Some(EntityKey::Receipt { hash: r.hash, device: r.device });
            }
            Ok(out)
        }
        Record::Item(r) => apply_item(c, self_id, peer, r, now),
        Record::Folder(r) => apply_folder(c, peer, r, now),
        Record::Place(r) => apply_place(c, peer, r, now),
    }
}

fn apply_item(c: &Connection, self_id: DeviceId, peer: DeviceId, remote: &ItemRecord, now: i64) -> Result<Applied> {
    let key = EntityKey::Item { id: remote.id };
    let mut out = Applied::default();
    let merged = match store::get_item(c, remote.id)? {
        None => {
            // M8: new record, inserted as received; the file is wanted until the sweep says otherwise.
            store::insert_item(c, remote, LocalFileState::Pending, now)?;
            out.changed = Some(key);
            out.content_changed = true;
            remote.clone()
        }
        Some((local, lfs)) => {
            let m = merge_item(&local, remote)?;
            out.conflicts += log_conflicts(
                c,
                peer,
                &key,
                Some(local.id),
                &m.differing,
                &m.taken,
                &|g, use_remote| item_group_text(c, if use_remote { remote } else { &local }, g),
                now,
            )?;
            if !m.taken.is_empty() {
                store::update_item(c, &m.merged)?;
                out.changed = Some(key);
                if m.taken.contains(&group::CONTENT) && local.content.val.hash != m.merged.content.val.hash {
                    out.content_changed = true;
                    // The held file is the old content. Wanted again, unless it was purged
                    // (then the sweep decides: in the Bin stays purged, else Pending).
                    if lfs != LocalFileState::Purged {
                        store::set_local_state(c, local.id, LocalFileState::Pending, now)?;
                    }
                }
            }
            m.merged
        }
    };
    // M5 on the merged record (also catches a new record that arrives both deleted and kept).
    if let Some(dk) = delete_vs_keep(&merged) {
        let nv = store::next_version(c, self_id)?;
        let mut fixed = merged.clone();
        let detail = match dk {
            DeleteKeep::KeepWins => {
                fixed.deleted = Versioned { v: nv, val: None };
                store::log_change(c, &key, group::DELETED, nv.lamport)?;
                "delete vs keep: Keep was later, delete lost"
            }
            DeleteKeep::DeleteWins => {
                fixed.pinned = Versioned { v: nv, val: false };
                store::log_change(c, &key, group::PINNED, nv.lamport)?;
                "delete vs keep: delete was later, Keep lost"
            }
        };
        store::update_item(c, &fixed)?;
        activity::log(c, now, Some(peer), Some(fixed.id), ActivityAction::Conflict, detail)?;
        out.conflicts += 1;
        out.wrote_local = true;
        out.changed = Some(key);
    }
    Ok(out)
}

fn apply_folder(c: &Connection, peer: DeviceId, remote: &FolderRecord, now: i64) -> Result<Applied> {
    let key = EntityKey::Folder { id: remote.id };
    let mut out = Applied::default();
    match store::get_folder(c, remote.id)? {
        None => {
            store::put_folder(c, remote)?;
            out.changed = Some(key);
        }
        Some(local) => {
            let m = merge_folder(&local, remote)?;
            out.conflicts += log_conflicts(
                c,
                peer,
                &key,
                None,
                &m.differing,
                &m.taken,
                &|g, use_remote| folder_group_text(if use_remote { remote } else { &local }, g),
                now,
            )?;
            if !m.taken.is_empty() {
                store::put_folder(c, &m.merged)?;
                out.changed = Some(key);
            }
        }
    }
    Ok(out)
}

fn apply_place(c: &Connection, peer: DeviceId, remote: &PlaceRecord, now: i64) -> Result<Applied> {
    let key = EntityKey::Place { id: remote.id };
    let mut out = Applied::default();
    match store::get_place(c, remote.id)? {
        None => {
            store::put_place(c, remote)?;
            out.changed = Some(key);
        }
        Some(local) => {
            let m = merge_place(&local, remote)?;
            out.conflicts += log_conflicts(
                c,
                peer,
                &key,
                None,
                &m.differing,
                &m.taken,
                &|g, use_remote| place_group_text(c, if use_remote { remote } else { &local }, g),
                now,
            )?;
            if !m.taken.is_empty() {
                store::put_place(c, &m.merged)?;
                out.changed = Some(key);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::store::test_support::*;
    use uuid::Uuid;

    fn vs<T>(ver: Version, val: T) -> Versioned<T> {
        Versioned { v: ver, val }
    }

    // L1/L2: pick rows E1-E4.
    #[test]
    fn pick_examples_e1_to_e4() {
        // E1: local (3,P), remote (5,F): remote.
        assert_eq!(pick(&vs(v(3, p()), "a"), &vs(v(5, f()), "b")), Pick::Remote);
        // E2: local (5,P), remote (3,F): local.
        assert_eq!(pick(&vs(v(5, p()), "a"), &vs(v(3, f()), "b")), Pick::Local);
        // E3: equal lamport, 0xBB > 0xAA: remote (from F) wins on P; local wins on F.
        assert_eq!(pick(&vs(v(5, p()), "a"), &vs(v(5, f()), "b")), Pick::Remote);
        assert_eq!(pick(&vs(v(5, f()), "b"), &vs(v(5, p()), "a")), Pick::Local);
        // E4: same write: local kept, nothing taken.
        assert_eq!(pick(&vs(v(5, p()), "a"), &vs(v(5, p()), "a")), Pick::Local);
        // Values never decide: a "bigger" value with an older version loses.
        assert_eq!(pick(&vs(v(5, p()), 1), &vs(v(4, f()), 999)), Pick::Local);
    }

    // L3: E5 content edit + move both survive.
    #[test]
    fn e5_content_edit_and_move_both_survive() {
        let mut local = item(1, f(), v(1, f()));
        local.content = vs(v(4, p()), Content { hash: hash(11), size: 5, kind: MediaKind::Image, ext: "jpg".into(), thumb: None });
        local.location = vs(v(2, f()), Location { folder: INBOX_ID, entered_at: T0 });
        let mut remote = item(1, f(), v(1, f()));
        remote.content = vs(v(1, f()), Content { hash: hash(10), size: 5, kind: MediaKind::Image, ext: "jpg".into(), thumb: None });
        remote.location = vs(v(4, f()), Location { folder: KEEP_ID, entered_at: T0 + 9 });
        let m = merge_item(&local, &remote).unwrap();
        assert_eq!(m.merged.content.val.hash, hash(11), "PC edit survives");
        assert_eq!(m.merged.location.val.folder, KEEP_ID, "phone move survives");
        assert_eq!(m.taken, vec![group::PLACEMENT]);
        assert_eq!(m.differing, vec![group::CONTENT, group::PLACEMENT]);
        // The other direction ends in the same record (M2 symmetry).
        let m2 = merge_item(&remote, &local).unwrap();
        assert_eq!(m2.merged, m.merged);
    }

    // Tiny deterministic generator (no rand dependency).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    fn random_version(g: &mut Lcg) -> Version {
        v(1 + g.next() % 4, if g.next().is_multiple_of(2) { p() } else { f() })
    }

    /// Every value is a function of its version, so equal versions always carry equal values
    /// (two devices never issue the same version for different writes).
    fn random_item(g: &mut Lcg) -> ItemRecord {
        let mut it = item(1, p(), v(1, p()));
        let (a, b, c, d, e) = (random_version(g), random_version(g), random_version(g), random_version(g), random_version(g));
        let tag = |x: Version| x.lamport * 256 + x.device.0[0] as u64;
        it.name = vs(a, format!("n{}", tag(a)));
        it.content = vs(b, Content { hash: BlobHash([tag(b) as u8; 32]), size: tag(b), kind: MediaKind::Image, ext: "jpg".into(), thumb: None });
        it.location = vs(c, Location { folder: FolderId(Uuid::from_u128(tag(c) as u128)), entered_at: tag(c) as i64 });
        it.pinned = vs(d, tag(d) % 2 == 0);
        it.deleted = vs(e, if tag(e) % 2 == 0 { Some(tag(e) as i64) } else { None });
        it
    }

    // L4: merge is commutative per group, associative, idempotent (random version pairs).
    #[test]
    fn merge_is_commutative_associative_idempotent() {
        let mut g = Lcg(42);
        for _ in 0..500 {
            let (a, b, c) = (random_item(&mut g), random_item(&mut g), random_item(&mut g));
            let ab = merge_item(&a, &b).unwrap().merged;
            let ba = merge_item(&b, &a).unwrap().merged;
            assert_eq!(ab, ba, "commutative");
            let ab_c = merge_item(&ab, &c).unwrap().merged;
            let bc = merge_item(&b, &c).unwrap().merged;
            let a_bc = merge_item(&a, &bc).unwrap().merged;
            assert_eq!(ab_c, a_bc, "associative");
            assert_eq!(merge_item(&a, &a).unwrap().merged, a, "idempotent");
            assert!(merge_item(&ab, &a).unwrap().taken.is_empty(), "a merged record takes nothing from its inputs");
        }
        // Same for folders and places.
        let fo = |ver: Version, name: &str, ret: Retention| FolderRecord {
            id: INBOX_ID,
            kind: FolderKind::Inbox,
            name: vs(ver, name.to_string()),
            retention: vs(ver, ret),
            deleted: vs(ver, None),
        };
        let (x, y) = (fo(v(3, p()), "x", Retention::Keep), fo(v(5, f()), "y", Retention::Secs { secs: 60 }));
        assert_eq!(merge_folder(&x, &y).unwrap().merged, merge_folder(&y, &x).unwrap().merged);
    }

    // L7: immutable mismatch -> Protocol error.
    #[test]
    fn immutable_mismatch_is_a_protocol_error() {
        let a = item(1, p(), v(1, p()));
        let mut b = a.clone();
        b.captured_at += 1;
        assert!(matches!(merge_item(&a, &b), Err(CoreError::Protocol(_))));
        let mut b = a.clone();
        b.origin = f();
        assert!(matches!(merge_item(&a, &b), Err(CoreError::Protocol(_))));
        let mut b = a.clone();
        b.geo = Some(Geo { lat: 1.0, lon: 2.0, accuracy_m: 3.0 });
        assert!(matches!(merge_item(&a, &b), Err(CoreError::Protocol(_))));
        let mut b = a.clone();
        b.id = ItemId(Uuid::from_u128(9));
        assert!(matches!(merge_item(&a, &b), Err(CoreError::Protocol(_))));
        let fa = FolderRecord { id: INBOX_ID, kind: FolderKind::Inbox, name: vs(GENESIS, "x".into()), retention: vs(GENESIS, Retention::Keep), deleted: vs(GENESIS, None) };
        let mut fb = fa.clone();
        fb.kind = FolderKind::Keep;
        assert!(matches!(merge_folder(&fa, &fb), Err(CoreError::Protocol(_))));
    }

    #[test]
    fn delete_vs_keep_decision() {
        let mut it = item(1, p(), v(1, p()));
        assert_eq!(delete_vs_keep(&it), None);
        it.pinned = vs(v(5, f()), true);
        assert_eq!(delete_vs_keep(&it), None, "pinned but not deleted");
        it.deleted = vs(v(6, p()), Some(T0));
        assert_eq!(delete_vs_keep(&it), Some(DeleteKeep::DeleteWins));
        it.pinned.v = v(8, f());
        assert_eq!(delete_vs_keep(&it), Some(DeleteKeep::KeepWins));
        it.pinned.v = v(6, p());
        assert_eq!(delete_vs_keep(&it), Some(DeleteKeep::DeleteWins), "tie: delete");
        it.pinned.val = false;
        assert_eq!(delete_vs_keep(&it), None);
    }

    /// Two devices with the same item at the base version.
    fn pair_with_item() -> (Db, Db, ItemRecord) {
        let base = item(1, p(), v(1, p()));
        let dp = db_with_peer(p(), f());
        let df = db_with_peer(f(), p());
        for d in [&dp, &df] {
            store::insert_item(&d.conn, &base, LocalFileState::Present, T0).unwrap();
            store::observe_lamport(&d.conn, 1).unwrap();
        }
        (dp, df, base)
    }

    fn apply(d: &Db, me: DeviceId, peer: DeviceId, r: &ItemRecord) -> Applied {
        apply_remote(&d.conn, me, peer, &Record::Item(r.clone()), T0 + 1000).unwrap()
    }

    fn conflicts(d: &Db) -> Vec<String> {
        activity::list(&d.conn, None, 100)
            .unwrap()
            .into_iter()
            .filter(|a| a.action == ActivityAction::Conflict)
            .map(|a| a.detail)
            .collect()
    }

    // L5: E7 (pin later than delete), both arrival orders, two dbs converge.
    #[test]
    fn e7_keep_after_delete_wins_on_both_devices() {
        let (dp, df, base) = pair_with_item();
        // P deleted at (6,P) (and cleared its pin); F pinned later at (8,F).
        let mut on_p = base.clone();
        on_p.deleted = vs(v(6, p()), Some(T0 + 5));
        on_p.pinned = vs(v(6, p()), false);
        let mut on_f = base.clone();
        on_f.pinned = vs(v(8, f()), true);
        store::update_item(&dp.conn, &on_p).unwrap();
        store::update_item(&df.conn, &on_f).unwrap();

        let a1 = apply(&dp, p(), f(), &on_f); // P receives F's pin
        let a2 = apply(&df, f(), p(), &on_p); // F receives P's delete
        assert!(a1.wrote_local && a2.wrote_local, "both rewrite deleted := None");
        assert_eq!(a1.conflicts, 1);
        // Second round: each receives the other's rewritten record.
        let rp = store::get_item(&dp.conn, base.id).unwrap().unwrap().0;
        let rf = store::get_item(&df.conn, base.id).unwrap().unwrap().0;
        assert_eq!(rp.deleted.val, None);
        assert_eq!(rf.deleted.val, None);
        assert!(rp.pinned.val && rf.pinned.val, "Keep survives on both");
        apply(&dp, p(), f(), &rf);
        apply(&df, f(), p(), &rp);
        let fp = store::get_item(&dp.conn, base.id).unwrap().unwrap().0;
        let ff = store::get_item(&df.conn, base.id).unwrap().unwrap().0;
        assert_eq!(fp, ff, "converged exactly, versions included");
        assert!(!conflicts(&dp).is_empty() && !conflicts(&df).is_empty(), "Conflict logged on both");
        // Further exchanges change nothing.
        assert_eq!(apply(&dp, p(), f(), &ff).changed, None);
        assert_eq!(apply(&df, f(), p(), &fp).changed, None);
    }

    // L5: E6 variant where delete is the later intent: pin loses, item deleted on both.
    #[test]
    fn delete_after_keep_wins_on_both_devices() {
        let (dp, df, base) = pair_with_item();
        let mut on_p = base.clone();
        on_p.deleted = vs(v(6, p()), Some(T0 + 5));
        let mut on_f = base.clone();
        on_f.pinned = vs(v(5, f()), true);
        store::update_item(&dp.conn, &on_p).unwrap();
        store::update_item(&df.conn, &on_f).unwrap();
        let a1 = apply(&dp, p(), f(), &on_f);
        let a2 = apply(&df, f(), p(), &on_p);
        assert!(a1.wrote_local && a2.wrote_local);
        let rp = store::get_item(&dp.conn, base.id).unwrap().unwrap().0;
        let rf = store::get_item(&df.conn, base.id).unwrap().unwrap().0;
        assert!(rp.deleted.val.is_some() && !rp.pinned.val);
        assert!(rf.deleted.val.is_some() && !rf.pinned.val);
        apply(&dp, p(), f(), &rf);
        apply(&df, f(), p(), &rp);
        assert_eq!(store::get_item(&dp.conn, base.id).unwrap().unwrap().0, store::get_item(&df.conn, base.id).unwrap().unwrap().0);
        assert!(conflicts(&dp).iter().any(|d| d.contains("delete was later")));
        // Max lamport only ever moved up past everything seen (M7).
        assert!(store::max_lamport(&dp.conn).unwrap() >= 7);
    }

    // L6: M6 concurrent change logs a Conflict; a non-concurrent overwrite logs nothing.
    #[test]
    fn m6_conflict_only_when_local_change_unacked() {
        let (dp, _df, base) = pair_with_item();
        let key = EntityKey::Item { id: base.id };
        // First sync done: cursor exists, everything so far acked.
        store::set_pulled(&dp.conn, f(), 0).unwrap();
        // P renames at (3,P) and logs the change (seq 1, unacked).
        let mut local = base.clone();
        local.name = vs(v(3, p()), "P name".into());
        store::update_item(&dp.conn, &local).unwrap();
        let seq = store::log_change(&dp.conn, &key, group::NAME, 3).unwrap();
        // F renamed concurrently at (5,F): F wins, but the conflict is still logged on P.
        let mut remote = base.clone();
        remote.name = vs(v(5, f()), "F name".into());
        let a = apply(&dp, p(), f(), &remote);
        assert_eq!(a.conflicts, 1);
        assert_eq!(conflicts(&dp), vec!["name: kept \"F name\", lost \"P name\"".to_string()]);
        // Local wins variant: F's concurrent rename is older.
        let mut older = base.clone();
        older.name = vs(v(2, f()), "old F".into());
        // (now local is "F name" (5,F); make a new unacked local change winning)
        let mut mine = store::get_item(&dp.conn, base.id).unwrap().unwrap().0;
        mine.name = vs(v(9, p()), "mine".into());
        store::update_item(&dp.conn, &mine).unwrap();
        store::log_change(&dp.conn, &key, group::NAME, 9).unwrap();
        assert_eq!(apply(&dp, p(), f(), &older).conflicts, 1, "logged even though local won");
        assert!(conflicts(&dp)[0].contains("kept \"mine\", lost \"old F\""));
        // Once the peer acked those changes, a plain newer remote write is NOT a conflict.
        store::set_acked(&dp.conn, f(), seq + 10).unwrap();
        let mut newer = base.clone();
        newer.name = vs(v(20, f()), "later".into());
        let before = conflicts(&dp).len();
        let a = apply(&dp, p(), f(), &newer);
        assert_eq!(a.conflicts, 0);
        assert_eq!(conflicts(&dp).len(), before);
        assert_eq!(store::get_item(&dp.conn, base.id).unwrap().unwrap().0.name.val, "later");
    }

    #[test]
    fn first_snapshot_with_a_new_peer_logs_no_conflicts() {
        let (dp, _df, base) = pair_with_item();
        let key = EntityKey::Item { id: base.id };
        let mut local = base.clone();
        local.name = vs(v(3, p()), "P".into());
        store::update_item(&dp.conn, &local).unwrap();
        store::log_change(&dp.conn, &key, group::NAME, 3).unwrap();
        let mut remote = base.clone();
        remote.name = vs(v(5, f()), "F".into());
        assert_eq!(store::get_cursor(&dp.conn, f()).unwrap(), None);
        assert_eq!(apply(&dp, p(), f(), &remote).conflicts, 0);
        assert_eq!(store::get_item(&dp.conn, base.id).unwrap().unwrap().0.name.val, "F", "merge itself still applies");
    }

    // L8 via apply_remote: receipts are grow-only; duplicate -> no change.
    #[test]
    fn receipts_via_apply_remote() {
        let d = db_with_peer(p(), f());
        let r = Record::Receipt(ReceiptRecord { hash: hash(1), device: f(), received_at: 77 });
        let a = apply_remote(&d.conn, p(), f(), &r, T0).unwrap();
        assert_eq!(a.changed, Some(EntityKey::Receipt { hash: hash(1), device: f() }));
        assert!(store::peer_has_receipt(&d.conn, hash(1), f()).unwrap());
        let a = apply_remote(&d.conn, p(), f(), &r, T0 + 5).unwrap();
        assert_eq!(a.changed, None, "duplicate");
        let seen: i64 = d.conn.query_row("SELECT seen_at FROM receipt", [], |r| r.get(0)).unwrap();
        assert_eq!(seen, T0, "seen_at not moved by a duplicate");
        assert_eq!(store::head_seq(&d.conn).unwrap(), 0, "M9: remote-applied changes are not logged");
    }

    // 0.2.0: any file type. validate_remote accepts a File with a safe extension (or none), keeps
    // kind and extension together, and still ACCEPTS a refused name (digests stay equal).
    #[test]
    fn validate_remote_for_any_file_type() {
        let rec = |kind: MediaKind, ext: &str, name: &str| {
            let mut it = item(3, f(), v(2, f()));
            it.content.val.kind = kind;
            it.content.val.ext = ext.into();
            it.name.val = name.into();
            Record::Item(it)
        };
        for (kind, ext, name) in [
            (MediaKind::File, "pdf", "report.pdf"),
            (MediaKind::File, "", "Makefile"),
            (MediaKind::File, "tar-gz", "x.tar-gz"),
            (MediaKind::File, "exe", "setup.exe"),
            (MediaKind::Image, "jpg", "a.jpg"),
            (MediaKind::Video, "mp4", "a.mp4"),
            // Accepted as records, refused at download time.
            (MediaKind::File, "lnk", "x.lnk"),
            (MediaKind::File, "", "desktop.ini"),
            (MediaKind::File, "", "AUTORUN.INF. "),
        ] {
            validate_remote(&rec(kind, ext, name)).unwrap_or_else(|e| panic!("{kind:?} {ext:?}: {e}"));
        }
        for (kind, ext) in [
            (MediaKind::File, "PDF"),
            (MediaKind::File, "pd f"),
            (MediaKind::File, "../x"),
            (MediaKind::File, "a.b"),
            (MediaKind::File, "jpg"),
            (MediaKind::File, "mp4"),
            (MediaKind::Image, "pdf"),
            (MediaKind::Image, ""),
            (MediaKind::Video, "jpg"),
            (MediaKind::Image, "mp4"),
        ] {
            assert!(validate_remote(&rec(kind, ext, "x")).is_err(), "{kind:?} with ext {ext:?} must be rejected");
        }
        assert!(validate_remote(&rec(MediaKind::File, &"x".repeat(17), "x")).is_err());
    }

    // L9: a new remote item is Pending; applying logs nothing (M9).
    #[test]
    fn new_remote_item_is_pending_and_not_logged() {
        let d = db_with_peer(p(), f());
        let it = item(2, f(), v(4, f()));
        let a = apply_remote(&d.conn, p(), f(), &Record::Item(it.clone()), T0).unwrap();
        assert!(a.content_changed);
        assert_eq!(a.changed, Some(EntityKey::Item { id: it.id }));
        assert_eq!(store::get_item(&d.conn, it.id).unwrap().unwrap().1, LocalFileState::Pending);
        assert_eq!(store::head_seq(&d.conn).unwrap(), 0);
        assert_eq!(store::max_lamport(&d.conn).unwrap(), 4, "M7: remote versions observed");
        // Same record again: nothing changes.
        let a = apply_remote(&d.conn, p(), f(), &Record::Item(it), T0).unwrap();
        assert_eq!(a.changed, None);
        assert!(!a.content_changed);
    }

    #[test]
    fn remote_content_change_makes_a_held_file_pending_again() {
        let (dp, _df, base) = pair_with_item();
        let mut remote = base.clone();
        remote.content = vs(v(5, f()), Content { hash: hash(99), size: 7, kind: MediaKind::Image, ext: "jpg".into(), thumb: None });
        let a = apply(&dp, p(), f(), &remote);
        assert!(a.content_changed);
        assert_eq!(store::get_item(&dp.conn, base.id).unwrap().unwrap().1, LocalFileState::Pending);
        // A rename alone leaves the held file alone.
        let (dp2, _d, base2) = pair_with_item();
        let mut renamed = base2.clone();
        renamed.name = vs(v(5, f()), "x.jpg".into());
        let a = apply(&dp2, p(), f(), &renamed);
        assert!(!a.content_changed);
        assert_eq!(store::get_item(&dp2.conn, base2.id).unwrap().unwrap().1, LocalFileState::Present);
    }

    #[test]
    fn folders_and_places_merge_per_group() {
        let d = db_with_peer(p(), f());
        // E9: retention Keep (GENESIS) vs 3 days (4,F).
        let mut inbox = store::get_folder(&d.conn, INBOX_ID).unwrap().unwrap();
        inbox.retention = vs(v(4, f()), Retention::Secs { secs: 3 * 86_400 });
        let a = apply_remote(&d.conn, p(), f(), &Record::Folder(inbox.clone()), T0).unwrap();
        assert_eq!(a.changed, Some(EntityKey::Folder { id: INBOX_ID }));
        assert_eq!(store::get_folder(&d.conn, INBOX_ID).unwrap().unwrap().retention.val, Retention::Secs { secs: 3 * 86_400 });
        // The same record again: no change. The default records themselves: no change.
        assert_eq!(apply_remote(&d.conn, p(), f(), &Record::Folder(inbox), T0).unwrap().changed, None);
        for fr in crate::folders::default_folders() {
            if fr.id != INBOX_ID {
                assert_eq!(apply_remote(&d.conn, p(), f(), &Record::Folder(fr), T0).unwrap().changed, None);
            }
        }
        // A new place arrives, then a rename merges without touching its area.
        let pl = PlaceRecord {
            id: PlaceId(Uuid::from_u128(0x800)),
            name: vs(v(2, f()), "Site".into()),
            area: vs(v(2, f()), Area { lat: -30.0, lon: 151.0, radius_m: 150.0 }),
            target: vs(v(2, f()), INBOX_ID),
            deleted: vs(v(2, f()), None),
        };
        apply_remote(&d.conn, p(), f(), &Record::Place(pl.clone()), T0).unwrap();
        let mut renamed = pl.clone();
        renamed.name = vs(v(6, f()), "Site 2".into());
        let a = apply_remote(&d.conn, p(), f(), &Record::Place(renamed), T0).unwrap();
        assert!(a.changed.is_some());
        let got = store::get_place(&d.conn, pl.id).unwrap().unwrap();
        assert_eq!(got.name.val, "Site 2");
        assert_eq!(got.area, pl.area);
    }

    // The Keep folder never expires: a remote record giving it a duration is refused, nothing stored.
    #[test]
    fn remote_keep_folder_with_expiring_retention_is_refused() {
        let d = db_with_peer(p(), f());
        let mut keep = store::get_folder(&d.conn, KEEP_ID).unwrap().unwrap();
        keep.retention = vs(v(4, f()), Retention::Secs { secs: 3 * 86_400 });
        assert!(matches!(apply_remote(&d.conn, p(), f(), &Record::Folder(keep), T0), Err(CoreError::Protocol(_))));
        assert_eq!(store::get_folder(&d.conn, KEEP_ID).unwrap().unwrap().retention.val, Retention::Keep);
    }

    // A version near u64::MAX must not pin the clock: refused, clock and next_version untouched.
    #[test]
    fn remote_versions_beyond_the_lamport_bound_are_refused() {
        let d = db_with_peer(p(), f());
        let before = store::max_lamport(&d.conn).unwrap();
        assert!(matches!(store::observe_lamport(&d.conn, u64::MAX), Err(CoreError::Protocol(_))));
        assert_eq!(store::max_lamport(&d.conn).unwrap(), before);
        let mut fo = store::get_folder(&d.conn, INBOX_ID).unwrap().unwrap();
        fo.name = vs(v(u64::MAX, f()), "x".into());
        assert!(matches!(apply_remote(&d.conn, p(), f(), &Record::Folder(fo), T0), Err(CoreError::Protocol(_))));
        let pl = PlaceRecord {
            id: PlaceId(Uuid::from_u128(0x902)),
            name: vs(v(u64::MAX, f()), "x".into()),
            area: vs(v(1, f()), Area { lat: 1.0, lon: 1.0, radius_m: 10.0 }),
            target: vs(v(1, f()), INBOX_ID),
            deleted: vs(v(1, f()), None),
        };
        assert!(matches!(apply_remote(&d.conn, p(), f(), &Record::Place(pl), T0), Err(CoreError::Protocol(_))));
        let mut it = item(7, f(), v(2, f()));
        it.name = vs(v(i64::MAX as u64, f()), "x.jpg".into());
        assert!(matches!(apply_remote(&d.conn, p(), f(), &Record::Item(it), T0), Err(CoreError::Protocol(_))));
        assert_eq!(store::max_lamport(&d.conn).unwrap(), before);
        assert_eq!(store::next_version(&d.conn, p()).unwrap().lamport, before + 1);
    }

    // Trust boundary: a paired peer's malformed record is refused, nothing stored.
    #[test]
    fn malformed_remote_records_are_refused() {
        let d = db_with_peer(p(), f());
        let mut bad_ext = item(3, f(), v(2, f()));
        bad_ext.content.val.ext = "exe".into();
        let mut path_ext = item(4, f(), v(2, f()));
        path_ext.content.val.ext = "jpg/../../x".into();
        let mut huge_name = item(5, f(), v(2, f()));
        huge_name.name.val = "x".repeat(5000);
        let mut upper = item(6, f(), v(2, f()));
        upper.content.val.ext = "JPG".into();
        for r in [bad_ext, path_ext, huge_name, upper] {
            let id = r.id;
            assert!(matches!(apply_remote(&d.conn, p(), f(), &Record::Item(r), T0), Err(CoreError::Protocol(_))));
            assert!(store::get_item(&d.conn, id).unwrap().is_none());
        }
        let zero = FolderRecord { id: FolderId(Uuid::from_u128(0x900)), kind: FolderKind::User, name: vs(v(1, f()), "x".into()), retention: vs(v(1, f()), Retention::Secs { secs: 0 }), deleted: vs(v(1, f()), None) };
        assert!(matches!(apply_remote(&d.conn, p(), f(), &Record::Folder(zero), T0), Err(CoreError::Protocol(_))));
        let bad_place = PlaceRecord {
            id: PlaceId(Uuid::from_u128(0x901)),
            name: vs(v(1, f()), "x".into()),
            area: vs(v(1, f()), Area { lat: 95.0, lon: 0.0, radius_m: 10.0 }),
            target: vs(v(1, f()), INBOX_ID),
            deleted: vs(v(1, f()), None),
        };
        assert!(matches!(apply_remote(&d.conn, p(), f(), &Record::Place(bad_place), T0), Err(CoreError::Protocol(_))));
        // Different immutable data under an existing id: Protocol, local kept.
        let (dp, _df, base) = pair_with_item();
        let mut forged = base.clone();
        forged.captured_at += 5;
        forged.name = vs(v(50, f()), "forged".into());
        assert!(matches!(apply_remote(&dp.conn, p(), f(), &Record::Item(forged), T0), Err(CoreError::Protocol(_))));
        assert_eq!(store::get_item(&dp.conn, base.id).unwrap().unwrap().0, base);
    }
}
