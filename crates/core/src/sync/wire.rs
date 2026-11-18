//! Control-stream messages (ALPN localtugboat/1, StreamKind::Control, one stream per
//! connection, opened by the dialer = phone). Framing: tugboat-net `frame` (u32 BE length
//! + JSON). JSON tag field "t", snake_case names. Owner: sync builder.
//!
//! Session order (details and failure handling in core-design.md "Sync session"):
//! 1. dialer -> Hello; acceptor -> Hello or Refuse (then close VERSION_MISMATCH / NOT_PAIRED).
//! 2. Both: observe_lamport(peer.max_lamport) before any write (PLAN lamport catch-up).
//! 3. Both send Pull{from: Seq(cursor)} or Pull{from: Snapshot{after: None}} for a new peer.
//!    Answers: Changes pages until more = false. Each page applied in one transaction with
//!    the cursor update.
//! 4. When both directions are caught up: Digest. Two digests are compared only when they
//!    cover the same change-log prefixes (each side's head equals the other's cursor), so live
//!    traffic during the compare never looks like a difference. Equal -> Synced. Different ->
//!    List exchange -> Fetch -> Records -> a new Digest; different again after two heals ->
//!    Event::Error {kind: "reconcile"} and activity SyncError, retried on the next connect.
//! 5. Live: a local change sends Notify{head}; the peer answers with Pull.
//! 6. Unpair: send Unpair, then close(UNPAIRED).
//!    Blob downloads run on their own streams (tugboat-net `blob`), not on this one.

use serde::{Deserialize, Serialize};

use crate::model::*;

/// Sync protocol version, sent in Hello. 2 since 0.2.0: records may be `MediaKind::File` with any
/// safe extension, and expiry no longer waits for receipts. A 0.1.x peer (proto 1) would drop
/// those records as Protocol errors and its digest would never match, so the handshake refuses
/// a mismatch on BOTH sides ("Update Tugboat on the <device with the lower number>") before any
/// record moves. The ALPN stays `localtugboat/1` on purpose: an ALPN change would make an old
/// peer fail the TLS handshake and show no message at all. (PLAN says "bump localtugboat/N";
/// this deviation is recorded in notes/core-design.md section 12.)
pub const PROTO: u32 = 2;

/// Max records per Changes / Records page, keys per Fetch, entries per List.
/// Keeps every frame far below tugboat-net frame::MAX_FRAME (8 MiB): a full item record
/// is under 1 KiB of JSON.
pub const PAGE: u32 = 500;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    Hello {
        proto: u32,
        app_version: String,
        device_name: String,
        role: Role,
        /// Highest lamport this device has seen or issued.
        max_lamport: u64,
    },
    Refuse {
        reason: Refusal,
    },
    /// Ask for the peer's changes.
    Pull {
        from: PullFrom,
        limit: u32,
    },
    Changes {
        records: Vec<Record>,
        /// Pass back as `from` for the next page.
        next: PullFrom,
        more: bool,
        /// Sender's change_log head when the page was built. After the last page of a
        /// snapshot, the puller stores this as its cursor (changes made during the snapshot
        /// have higher seqs and come with the next Pull).
        head: u64,
    },
    /// "I have new changes up to head" (live mode).
    Notify {
        head: u64,
    },
    Digest {
        /// 64 hex chars (digest::compute).
        hash: String,
        count: u64,
        /// The sender's change_log head when the digest was computed.
        head: u64,
        /// The sender's cursor on the receiver (last seq it pulled from the receiver).
        cursor: u64,
        /// Heal generation: bumped by every list exchange, so a digest of the state before a
        /// heal is never compared with one after it.
        gen: u8,
    },
    ListRequest {
        after: Option<EntityKey>,
        limit: u32,
    },
    List {
        entries: Vec<ListEntry>,
        more: bool,
    },
    Fetch {
        keys: Vec<EntityKey>,
    },
    Records {
        records: Vec<Record>,
    },
    Unpair,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum PullFrom {
    /// Changes with seq > since, from the change log.
    Seq { since: u64 },
    /// Full snapshot in EntityKey order, after the given key (None = from the start).
    /// Used when the puller has no cursor for this peer (new peer, PLAN "full snapshot both
    /// ways, tombstones included").
    Snapshot { after: Option<EntityKey> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Refusal {
    /// "Update Tugboat on the <update>": the side with the lower proto must update.
    VersionMismatch { ours: u32, theirs: u32, app_version: String, update: Role },
    NotPaired,
}

/// One digest/list line: the key and the versions of its groups in the record's fixed
/// group order (items: name, content, placement, pinned, deleted; folders: name, retention,
/// deleted; places: name, area, target, deleted; receipts: none).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEntry {
    pub key: EntityKey,
    pub versions: Vec<Version>,
}
