//! Reconcile digest.
//!
//! Both sides compare a hash of their sorted list of (id, field versions); a mismatch
//! triggers a full list exchange. This catches a lost change, a restored backup, or a cursor
//! that went wrong.
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
        // An item that was never purged carries GENESIS, below the version of any purge.
        Record::Item(i) => vec![i.name.v, i.content.v, i.location.v, i.pinned.v, i.deleted.v, i.purged.as_ref().map_or(GENESIS, |p| p.v)],
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

    // A purge is part of the entry and the digest: an item that was never purged carries GENESIS
    // last, a purged one carries the purge version, and both sides compute the same bytes.
    #[test]
    fn purged_is_part_of_the_list_entry_and_the_digest() {
        use crate::store::test_support::item;
        let plain = item(1, dev(1), v(2, 1));
        let mut purged = plain.clone();
        purged.purged = Some(Versioned { v: v(9, 7), val: 123 });
        let e_plain = list_entry(&Record::Item(plain.clone()));
        let e_purged = list_entry(&Record::Item(purged.clone()));
        assert_eq!(e_plain.versions.len(), 6, "name, content, placement, pinned, deleted, purged");
        assert_eq!(e_plain.versions[5], GENESIS);
        assert_eq!(e_purged.versions[5], v(9, 7));
        // The device that has not seen the purge asks for the record; the other side never asks back.
        assert_eq!(keys_to_fetch(std::slice::from_ref(&e_plain), std::slice::from_ref(&e_purged)), vec![e_purged.key]);
        assert!(keys_to_fetch(std::slice::from_ref(&e_purged), std::slice::from_ref(&e_plain)).is_empty());
        // A higher purge version replaces a lower one, so two concurrent purges converge.
        let mut later = purged.clone();
        later.purged = Some(Versioned { v: v(10, 3), val: 456 });
        let e_later = list_entry(&Record::Item(later));
        assert_eq!(keys_to_fetch(std::slice::from_ref(&e_purged), std::slice::from_ref(&e_later)), vec![e_later.key]);
        // The digest sees it: same record with and without the purge, and two purge versions.
        let digest_of = |it: &ItemRecord| {
            let d = Db::open_in_memory().unwrap();
            store::insert_item(&d.conn, it, LocalFileState::Present, 0).unwrap();
            compute(&d.conn).unwrap().0
        };
        let (h_plain, h_purged) = (digest_of(&plain), digest_of(&purged));
        assert_ne!(h_plain, h_purged);
        assert_eq!(h_purged, digest_of(&purged), "stable");
        let mut other_value = purged.clone();
        other_value.purged.as_mut().unwrap().val = 999;
        assert_eq!(h_purged, digest_of(&other_value), "versions count, not values");
        let mut other_version = purged.clone();
        other_version.purged = Some(Versioned { v: v(9, 8), val: 123 });
        assert_ne!(h_purged, digest_of(&other_version));
    }
}
