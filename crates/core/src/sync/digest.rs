//! Reconcile digest. Owner: sync builder.
//!
//! PLAN: "both sides compare a hash of their sorted list of (id, field versions); a mismatch
//! triggers a full list exchange. This catches a lost change, a restored backup, or a cursor
//! that went wrong".
//!
//! Definition (both sides must produce identical bytes for identical state):
//! - Iterate every synced record (folders, places, items, receipts; tombstones included) in
//!   EntityKey order (`store::records_after` pages).
//! - For each, feed BLAKE3: entity tag byte (folder 1, place 2, item 3, receipt 4), the key
//!   bytes (16-byte uuid; receipts: 32-byte hash then 32-byte device), then for each group
//!   version in `wire::ListEntry` order: lamport as u64 BE (8 bytes) + device (32 bytes).
//! - count = number of records. Result: (hex digest, count).
//!   Local-only state (local_file_state, file_ref, activity, cursors) is never included.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::{model::*, store, sync::wire::ListEntry, Result};

/// Records read per store page while hashing.
const SCAN_PAGE: u32 = 500;

/// Identity of a record.
pub fn key_of(r: &Record) -> EntityKey {
    match r {
        Record::Item(i) => EntityKey::Item { id: i.id },
        Record::Folder(f) => EntityKey::Folder { id: f.id },
        Record::Place(p) => EntityKey::Place { id: p.id },
        Record::Receipt(x) => EntityKey::Receipt { hash: x.hash, device: x.device },
    }
}

/// Digest of all synced records: (64 hex chars, count).
pub fn compute(c: &Connection) -> Result<(String, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut count = 0u64;
    let mut after: Option<EntityKey> = None;
    loop {
        let page = store::records_after(c, after.as_ref(), SCAN_PAGE)?;
        if page.is_empty() {
            break;
        }
        for r in &page {
            let e = list_entry(r);
            feed_key(&mut hasher, &e.key);
            for v in &e.versions {
                hasher.update(&v.lamport.to_be_bytes());
                hasher.update(&v.device.0);
            }
            count += 1;
        }
        after = page.last().map(key_of);
    }
    Ok((hasher.finalize().to_hex().to_string(), count))
}

fn feed_key(h: &mut blake3::Hasher, key: &EntityKey) {
    match key {
        EntityKey::Folder { id } => {
            h.update(&[1]);
            h.update(id.0.as_bytes());
        }
        EntityKey::Place { id } => {
            h.update(&[2]);
            h.update(id.0.as_bytes());
        }
        EntityKey::Item { id } => {
            h.update(&[3]);
            h.update(id.0.as_bytes());
        }
        EntityKey::Receipt { hash, device } => {
            h.update(&[4]);
            h.update(&hash.0);
            h.update(&device.0);
        }
    }
}

/// The ListEntry of one record (key + group versions in the fixed order).
pub fn list_entry(r: &Record) -> ListEntry {
    let versions = match r {
        Record::Item(i) => vec![i.name.v, i.content.v, i.location.v, i.pinned.v, i.deleted.v],
        Record::Folder(f) => vec![f.name.v, f.retention.v, f.deleted.v],
        Record::Place(p) => vec![p.name.v, p.area.v, p.target.v, p.deleted.v],
        Record::Receipt(_) => Vec::new(),
    };
    ListEntry { key: key_of(r), versions }
}

/// Every ListEntry this device holds, in key order (the list exchange diff input).
pub fn all_entries(c: &Connection) -> Result<Vec<ListEntry>> {
    let mut out = Vec::new();
    let mut after: Option<EntityKey> = None;
    loop {
        let page = store::records_after(c, after.as_ref(), SCAN_PAGE)?;
        if page.is_empty() {
            return Ok(out);
        }
        after = page.last().map(key_of);
        out.extend(page.iter().map(list_entry));
    }
}

/// Keys the puller must Fetch: theirs not held locally, or any group version of theirs
/// higher than ours. Pure.
pub fn keys_to_fetch(ours: &[ListEntry], theirs: &[ListEntry]) -> Vec<EntityKey> {
    let held: HashMap<&EntityKey, &Vec<Version>> = ours.iter().map(|e| (&e.key, &e.versions)).collect();
    let mut out: Vec<EntityKey> = theirs
        .iter()
        .filter(|t| match held.get(&t.key) {
            None => true,
            Some(mine) => t.versions.iter().enumerate().any(|(i, tv)| mine.get(i).is_none_or(|mv| tv > mv)),
        })
        .map(|t| t.key)
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn dev(b: u8) -> DeviceId {
        DeviceId([b; 32])
    }

    fn v(l: u64, b: u8) -> Version {
        Version { lamport: l, device: dev(b) }
    }

    fn item_key(n: u128) -> EntityKey {
        EntityKey::Item { id: ItemId(uuid::Uuid::from_u128(n)) }
    }

    fn entry(n: u128, vs: &[Version]) -> ListEntry {
        ListEntry { key: item_key(n), versions: vs.to_vec() }
    }

    #[test]
    fn fetch_missing_and_newer_only() {
        let ours = vec![entry(1, &[v(1, 1), v(1, 1)]), entry(2, &[v(5, 1), v(5, 1)]), entry(4, &[v(2, 1), v(3, 1)])];
        let theirs = vec![
            entry(1, &[v(1, 1), v(1, 1)]), // equal: skip
            entry(2, &[v(5, 1), v(4, 9)]), // ours is newer or equal in each group: skip
            entry(3, &[v(1, 1), v(1, 1)]), // we lack it: fetch
            entry(4, &[v(2, 1), v(4, 2)]), // one group newer: fetch
        ];
        assert_eq!(keys_to_fetch(&ours, &theirs), vec![item_key(3), item_key(4)]);
        // Same lamport, higher device wins: theirs (5, dev 2) beats ours (5, dev 1).
        assert_eq!(keys_to_fetch(&[entry(1, &[v(5, 1)])], &[entry(1, &[v(5, 2)])]), vec![item_key(1)]);
        // Receipts carry no versions: held = nothing to fetch, missing = fetch.
        let rk = EntityKey::Receipt { hash: BlobHash([1; 32]), device: dev(2) };
        let r = ListEntry { key: rk, versions: vec![] };
        assert!(keys_to_fetch(std::slice::from_ref(&r), std::slice::from_ref(&r)).is_empty());
        assert_eq!(keys_to_fetch(&[], &[r]), vec![rk]);
    }

    // Two fresh databases hold identical default folders, so identical digests; any change shows.
    #[test]
    fn digest_is_stable_and_sensitive() {
        let a = Db::open_in_memory().unwrap();
        let b = Db::open_in_memory().unwrap();
        crate::folders::ensure_defaults(&a.conn).unwrap();
        crate::folders::ensure_defaults(&b.conn).unwrap();
        let (ha, ca) = compute(&a.conn).unwrap();
        assert_eq!(ca, 3);
        assert_eq!((ha.clone(), ca), compute(&b.conn).unwrap());
        assert_eq!(ha.len(), 64);
        // A version change in one group changes the digest.
        let mut f = store::get_folder(&b.conn, crate::model::KEEP_ID).unwrap().unwrap();
        f.name = Versioned { v: v(3, 7), val: "Keep2".into() };
        store::put_folder(&b.conn, &f).unwrap();
        assert_ne!(ha, compute(&b.conn).unwrap().0);
        // The digest is over versions, not values: same version, other text, same digest.
        let mut g = f.clone();
        g.name.val = "other text".into();
        store::put_folder(&b.conn, &g).unwrap();
        let (h1, _) = compute(&b.conn).unwrap();
        store::put_folder(&b.conn, &f).unwrap();
        assert_eq!(h1, compute(&b.conn).unwrap().0);
    }
}
