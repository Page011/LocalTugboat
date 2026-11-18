//! Sync engine: the per-connection protocol state machine. Owner: sync builder.
//!
//! `Session::handle` is a function of (store, one incoming message) -> `Step` (messages to
//! send plus effects), so the protocol is testable without a network. `sync::net` drives it
//! over a tugboat-net connection: reads frames, calls `handle`, writes the answers, runs
//! the sweep and the blob fetch queue.
//!
//! Rules the session enforces (each has a test in tests/ or below):
//! S1 Handshake first: any message before Hello -> Protocol error, close PROTOCOL.
//! S2 proto != ours -> send Refuse{VersionMismatch{.., update: side with lower proto}}, close
//!    VERSION_MISMATCH, status Refused "Update Tugboat on the <side>", activity SyncError.
//!    Same on the receiving side of a Refuse. Never a silent no-sync (PLAN Network).
//! S3 Lamport catch-up: store::observe_lamport(peer max_lamport) before any local write.
//! S4 Cursor None -> Pull{Snapshot{after: None}}; else Pull{Seq{since: last_seq_pulled}}.
//!    On receiving a Pull{Seq{since}}: store::set_acked(peer, since) and trim_log(since)
//!    (the peer has everything up to since). A since above our head means we were restored
//!    from an older backup: the puller is told to restart from a snapshot, because new local
//!    changes would otherwise reuse seq numbers the peer's cursor is already past.
//! S5 Each received Changes/Records page is applied in one transaction, with the cursor update
//!    inside it, so a crash never leaves the cursor ahead of applied data. A record that fails
//!    validation is skipped, logged (activity SyncError) and reported; it never blocks the page.
//! S6 After applying, the runner sweeps (bin::sweep) and queues Pending items for download.
//! S7 Digest when both directions are caught up, until one comparison is equal. Digests carry
//!    (head, cursor); two are compared only when each side's head equals the other's cursor,
//!    i.e. they describe the same state, so concurrent live traffic is never a "difference".
//!    A real mismatch: list exchange + Fetch (heal generation +1), then a fresh Digest; a
//!    second real mismatch -> `Step.errors` kind "reconcile" + activity SyncError; the session
//!    stays up (live sync continues).
//! S8 Live: local change -> Notify{head}; on Notify with head above our cursor -> Pull. At
//!    most one Pull in flight; a Notify meanwhile sets `pull_again`.
//! S12 Unpair message -> `Step.unpair`; the runner unpairs and closes UNPAIRED.

use rusqlite::{Connection, TransactionBehavior};

use crate::{
    activity, merge,
    model::*,
    store,
    sync::{
        digest,
        wire::{ListEntry, Msg, PullFrom, Refusal, PAGE, PROTO},
    },
    CoreError, Result,
};
use tugboat_net::close;

/// What the session wants done after handling a message.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Step {
    pub send: Vec<Msg>,
    /// Items that changed (for Event::ItemsChanged).
    pub changed_items: Vec<ItemId>,
    pub folders_changed: bool,
    pub places_changed: bool,
    /// Blob downloads to queue.
    pub want: Vec<ItemId>,
    /// Close the connection with this net close code and reason.
    pub close: Option<(u32, String)>,
    /// The session reached Synced (equal digest).
    pub synced: bool,
    /// Records were applied (any page).
    pub applied: usize,
    /// A local write happened (M5 rewrite): send Notify.
    pub wrote_local: bool,
    /// Activity rows were written.
    pub activity: bool,
    /// The handshake was refused: the one-line text for the status.
    pub refused: Option<String>,
    /// The peer told us it unpaired (or we must treat it so).
    pub unpair: bool,
    /// (kind, message) to surface as Event::Error.
    pub errors: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    AwaitHello,
    CatchingUp,
    Live,
    Closed,
}

/// Protocol state for one connection.
#[derive(Debug)]
pub struct Session {
    pub self_id: DeviceId,
    pub peer: DeviceId,
    pub role: Role,
    pub phase: Phase,
    /// Our protocol number (wire::PROTO; tests override it to prove the refusal).
    pub proto: u32,
    pub app_version: String,
    /// Our outstanding Pull, if any (S8 coalescing).
    pub pull_in_flight: Option<PullFrom>,
    pub our_caught_up: bool,
    pub peer_caught_up: bool,
    /// True once an equal digest was seen this session.
    pub synced: bool,
    pull_again: bool,
    last_notified: u64,
    snapshot_head: Option<u64>,
    /// Digests we sent at the current heal generation, by the (head, cursor) they describe.
    sent: Vec<((u64, u64), (String, u64))>,
    /// The peer's latest digest at the current generation: (head, cursor, hash, count).
    peer_digest: Option<(u64, u64, String, u64)>,
    gen: u8,
    heal_rounds: u8,
    gave_up: bool,
    healing: bool,
    list_theirs: Vec<ListEntry>,
    fetch_outstanding: u32,
}

fn role_label(r: Role) -> &'static str {
    match r {
        Role::Pc => "PC",
        Role::Phone => "phone",
    }
}

/// One-line text of a refusal, the same on both devices.
pub fn refusal_text(r: &Refusal) -> String {
    match r {
        Refusal::VersionMismatch { update, .. } => format!("Update Tugboat on the {}", role_label(*update)),
        Refusal::NotPaired => "NOT PAIRED - pair the devices again".to_string(),
    }
}

fn proto_err(msg: impl Into<String>) -> CoreError {
    CoreError::Protocol(msg.into())
}

impl Session {
    /// New session. The dialer (phone) sends `hello` first; the acceptor answers it.
    pub fn new(self_id: DeviceId, peer: DeviceId, role: Role, app_version: &str) -> Session {
        Session {
            self_id,
            peer,
            role,
            phase: Phase::AwaitHello,
            proto: PROTO,
            app_version: app_version.to_string(),
            pull_in_flight: None,
            our_caught_up: false,
            peer_caught_up: false,
            synced: false,
            pull_again: false,
            last_notified: 0,
            snapshot_head: None,
            sent: Vec::new(),
            peer_digest: None,
            gen: 0,
            heal_rounds: 0,
            gave_up: false,
            healing: false,
            list_theirs: Vec::new(),
            fetch_outstanding: 0,
        }
    }

    fn is_dialer(&self) -> bool {
        self.role == Role::Phone
    }

    /// Our Hello (reads max_lamport and the self device name from the store).
    pub fn hello(&self, c: &Connection) -> Result<Msg> {
        let me = store::self_device(c)?;
        Ok(Msg::Hello {
            proto: self.proto,
            app_version: self.app_version.clone(),
            device_name: me.name,
            role: self.role,
            max_lamport: store::max_lamport(c)?,
        })
    }

    /// The Pull for the current cursor (S4).
    fn pull_msg(&mut self, c: &Connection) -> Result<Msg> {
        let from = match store::get_cursor(c, self.peer)? {
            Some((since, _)) => PullFrom::Seq { since },
            None => PullFrom::Snapshot { after: None },
        };
        self.pull_in_flight = Some(from.clone());
        Ok(Msg::Pull { from, limit: PAGE })
    }

    /// Handle one incoming message (rules S1-S8, S12).
    pub fn handle(&mut self, c: &mut Connection, msg: Msg, now: i64) -> Result<Step> {
        let mut step = Step::default();
        if self.phase == Phase::Closed {
            return Err(proto_err("message after the session was closed"));
        }
        // An Unpair may come first: a phone that unpaired while away dials only to say so.
        // The connection is authenticated and passed the paired-key gate, like any other.
        if self.phase == Phase::AwaitHello && !matches!(msg, Msg::Hello { .. } | Msg::Refuse { .. } | Msg::Unpair) {
            // S1.
            return Err(proto_err("message before hello"));
        }
        match msg {
            Msg::Hello { proto, app_version, device_name: _, role, max_lamport } => {
                self.on_hello(c, &mut step, proto, &app_version, role, max_lamport, now)?
            }
            Msg::Refuse { reason } => {
                let text = refusal_text(&reason);
                activity::log(c, now, Some(self.peer), None, ActivityAction::SyncError, &text)?;
                step.activity = true;
                let code = match reason {
                    Refusal::VersionMismatch { .. } => close::VERSION_MISMATCH,
                    Refusal::NotPaired => close::NOT_PAIRED,
                };
                if matches!(reason, Refusal::NotPaired) {
                    step.unpair = true;
                }
                step.refused = Some(text.clone());
                step.close = Some((code, text));
                self.phase = Phase::Closed;
            }
            Msg::Pull { from, limit } => self.on_pull(c, &mut step, from, limit, now)?,
            Msg::Changes { records, next, more, head } => self.on_changes(c, &mut step, records, next, more, head, now)?,
            Msg::Notify { head } => {
                let have = store::get_cursor(c, self.peer)?.map(|x| x.0);
                if have.is_none_or(|h| head > h) {
                    if self.pull_in_flight.is_some() {
                        self.pull_again = true;
                    } else {
                        let m = self.pull_msg(c)?;
                        step.send.push(m);
                    }
                }
            }
            Msg::Digest { hash, count, head, cursor, gen } => {
                if gen > self.gen {
                    // The peer found a real difference and is healing: do the same.
                    self.start_heal(gen, &mut step);
                } else if gen == self.gen {
                    self.peer_digest = Some((head, cursor, hash, count));
                    self.compare(c, &mut step, now)?;
                }
                // gen < ours: a digest from before a heal, no longer meaningful.
            }
            Msg::ListRequest { after, limit } => {
                let limit = limit.clamp(1, PAGE);
                let page = store::records_after(c, after.as_ref(), limit)?;
                let more = match page.last() {
                    Some(last) if page.len() == limit as usize => !store::records_after(c, Some(&digest::key_of(last)), 1)?.is_empty(),
                    _ => false,
                };
                step.send.push(Msg::List { entries: page.iter().map(digest::list_entry).collect(), more });
            }
            Msg::List { entries, more } => self.on_list(c, &mut step, entries, more, now)?,
            Msg::Fetch { keys } => {
                if keys.len() > PAGE as usize {
                    return Err(proto_err(format!("fetch of {} keys, the limit is {PAGE}", keys.len())));
                }
                let mut records = Vec::with_capacity(keys.len());
                for k in &keys {
                    if let Some(r) = store::get_record(c, k)? {
                        records.push(r);
                    }
                }
                step.send.push(Msg::Records { records });
            }
            Msg::Records { records } => {
                if self.fetch_outstanding == 0 {
                    return Err(proto_err("records nobody asked for"));
                }
                self.apply_page(c, &mut step, &records, None, now)?;
                self.fetch_outstanding -= 1;
                if self.fetch_outstanding == 0 {
                    self.finish_heal(c, &mut step, now)?;
                }
            }
            Msg::Unpair => {
                step.unpair = true;
                step.close = Some((close::UNPAIRED, "unpaired by the other device".into()));
                self.phase = Phase::Closed;
            }
        }
        Ok(step)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_hello(&mut self, c: &mut Connection, step: &mut Step, proto: u32, app_version: &str, role: Role, max_lamport: u64, now: i64) -> Result<()> {
        if self.phase != Phase::AwaitHello {
            return Err(proto_err("second hello"));
        }
        if role == self.role {
            return Err(proto_err(format!("peer role is also {}", role_label(role))));
        }
        if proto != self.proto {
            // S2: the side with the lower protocol number is the one to update.
            let update = if self.proto < proto { self.role } else { role };
            let reason = Refusal::VersionMismatch { ours: self.proto, theirs: proto, app_version: self.app_version.clone(), update };
            let text = refusal_text(&reason);
            activity::log(c, now, Some(self.peer), None, ActivityAction::SyncError, &format!("{text} (protocol {} here, {proto} there, app {app_version})", self.proto))?;
            step.activity = true;
            step.send.push(Msg::Refuse { reason });
            step.refused = Some(text.clone());
            step.close = Some((close::VERSION_MISMATCH, text));
            self.phase = Phase::Closed;
            return Ok(());
        }
        // S3: before any local write of this session.
        store::observe_lamport(c, max_lamport)?;
        let addrs = store::get_device(c, self.peer)?.map(|d| d.addresses).unwrap_or_default();
        store::set_last_seen(c, self.peer, now, &addrs)?;
        if !self.is_dialer() {
            let h = self.hello(c)?;
            step.send.push(h);
        }
        self.phase = Phase::CatchingUp;
        let p = self.pull_msg(c)?;
        step.send.push(p);
        Ok(())
    }

    fn on_pull(&mut self, c: &mut Connection, step: &mut Step, from: PullFrom, limit: u32, now: i64) -> Result<()> {
        let limit = limit.clamp(1, PAGE);
        let head = store::head_seq(c)?;
        let (records, next, more) = match from {
            PullFrom::Seq { since } => {
                if since > head {
                    // S4: the peer's cursor is ahead of our log (we were restored from an older
                    // backup). Our next local changes would reuse seq numbers it is already past.
                    step.send.push(Msg::Changes { records: Vec::new(), next: PullFrom::Snapshot { after: None }, more: true, head });
                    return Ok(());
                }
                store::set_acked(c, self.peer, since)?;
                store::trim_log(c, since)?;
                let (keys, last) = store::changes_since(c, since, limit)?;
                let mut records = Vec::with_capacity(keys.len());
                for k in &keys {
                    if let Some(r) = store::get_record(c, k)? {
                        records.push(r);
                    }
                }
                let more = !store::changes_since(c, last, 1)?.0.is_empty();
                (records, PullFrom::Seq { since: last }, more)
            }
            PullFrom::Snapshot { after } => {
                let records = store::records_after(c, after.as_ref(), limit)?;
                let last = records.last().map(digest::key_of).or(after);
                let more = match records.last() {
                    Some(l) if records.len() == limit as usize => !store::records_after(c, Some(&digest::key_of(l)), 1)?.is_empty(),
                    _ => false,
                };
                (records, PullFrom::Snapshot { after: last }, more)
            }
        };
        step.send.push(Msg::Changes { records, next, more, head });
        if !more {
            self.peer_caught_up = true;
            self.digest_due(c, step, now)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn on_changes(&mut self, c: &mut Connection, step: &mut Step, records: Vec<Record>, next: PullFrom, more: bool, head: u64, now: i64) -> Result<()> {
        if self.pull_in_flight.take().is_none() {
            return Err(proto_err("changes without a pull"));
        }
        if records.len() > PAGE as usize {
            return Err(proto_err(format!("page of {} records, the limit is {PAGE}", records.len())));
        }
        // The first snapshot page fixes the cursor the finished snapshot will leave behind:
        // changes made while it streams have higher seqs and come with the next Pull.
        if let PullFrom::Snapshot { .. } = next {
            self.snapshot_head.get_or_insert(head);
        }
        let cursor = match &next {
            // A final page means everything up to the sender's head was delivered (rows below
            // it may have been trimmed): the cursor is then the head, so (head, cursor) pairs match.
            PullFrom::Seq { since } => Some(if more { *since } else { (*since).max(head) }),
            PullFrom::Snapshot { .. } if !more => self.snapshot_head.take(),
            PullFrom::Snapshot { .. } => None,
        };
        self.apply_page(c, step, &records, cursor, now)?;
        if more {
            self.pull_in_flight = Some(next.clone());
            step.send.push(Msg::Pull { from: next, limit: PAGE });
            return Ok(());
        }
        self.our_caught_up = true;
        if self.pull_again {
            self.pull_again = false;
            let m = self.pull_msg(c)?;
            step.send.push(m);
        }
        self.digest_due(c, step, now)
    }

    /// Apply records in one transaction (S5), optionally moving the cursor in the same one.
    fn apply_page(&mut self, c: &mut Connection, step: &mut Step, records: &[Record], cursor: Option<u64>, now: i64) -> Result<()> {
        let mut tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for r in records {
            let sp = tx.savepoint()?;
            match merge::apply_remote(&sp, self.self_id, self.peer, r, now) {
                Ok(a) => {
                    // A new receipt changes what the UI shows for every item with that hash (the waiting count, "NOT ON PC" in the Bin); expiry itself never reads it.
                    let by_receipt = match a.changed {
                        Some(EntityKey::Receipt { hash, .. }) => store::item_ids_where(&sp, &format!("blob_hash = x'{}'", hex::encode(hash.0)))?,
                        _ => Vec::new(),
                    };
                    sp.commit()?;
                    step.applied += 1;
                    step.wrote_local |= a.wrote_local;
                    if a.conflicts > 0 {
                        step.activity = true;
                    }
                    step.changed_items.extend(by_receipt);
                    match a.changed {
                        Some(EntityKey::Item { id }) => {
                            step.changed_items.push(id);
                            if a.content_changed {
                                step.want.push(id);
                            }
                        }
                        Some(EntityKey::Folder { .. }) => step.folders_changed = true,
                        Some(EntityKey::Place { .. }) => step.places_changed = true,
                        Some(EntityKey::Receipt { .. }) | None => {}
                    }
                }
                // A record the peer should not have sent: dropped, reported, never fatal.
                Err(e @ (CoreError::Protocol(_) | CoreError::Invalid(_) | CoreError::Name(_) | CoreError::NotMedia(_))) => {
                    drop(sp); // rolls back this record only
                    let what = digest::list_entry(r).key;
                    let text = format!("skipped a bad record from the other device: {e}");
                    activity::log(&tx, now, Some(self.peer), None, ActivityAction::SyncError, &text)?;
                    tracing::warn!(?what, "{text}");
                    step.activity = true;
                    step.errors.push((e.kind().to_string(), text));
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(since) = cursor {
            store::set_pulled(&tx, self.peer, since)?;
        }
        tx.commit()?;
        step.changed_items.sort();
        step.changed_items.dedup();
        step.want.sort();
        step.want.dedup();
        Ok(())
    }

    /// Send our digest when due (S7): both directions caught up, nothing in flight, no equal
    /// comparison yet, and the state differs from the last digest we sent.
    fn digest_due(&mut self, c: &Connection, step: &mut Step, now: i64) -> Result<()> {
        if self.synced || self.gave_up || self.healing || !self.our_caught_up || !self.peer_caught_up || self.pull_in_flight.is_some() {
            return Ok(());
        }
        let head = store::head_seq(c)?;
        let cursor = store::get_cursor(c, self.peer)?.map(|x| x.0).unwrap_or(0);
        if self.sent.last().is_some_and(|(pair, _)| *pair == (head, cursor)) {
            return Ok(());
        }
        let (hash, count) = digest::compute(c)?;
        self.sent.push(((head, cursor), (hash.clone(), count)));
        if self.sent.len() > 16 {
            self.sent.remove(0);
        }
        step.send.push(Msg::Digest { hash, count, head, cursor, gen: self.gen });
        self.compare(c, step, now)
    }

    /// The driver calls this when the session has been quiet for a tick: live traffic stopped,
    /// so a digest sent now describes a state the peer can match.
    pub fn quiet_tick(&mut self, c: &Connection, now: i64) -> Result<Step> {
        let mut step = Step::default();
        self.digest_due(c, &mut step, now)?;
        Ok(step)
    }

    /// Compare the peer's digest with ours when they describe the same state: our head is the
    /// peer's cursor and our cursor is the peer's head. Anything else is "not yet comparable",
    /// never a mismatch.
    fn compare(&mut self, c: &Connection, step: &mut Step, now: i64) -> Result<()> {
        let Some((peer_head, peer_cursor, hash, count)) = self.peer_digest.clone() else { return Ok(()) };
        let Some((_, mine)) = self.sent.iter().find(|(pair, _)| *pair == (peer_cursor, peer_head)) else { return Ok(()) };
        let mine = mine.clone();
        self.peer_digest = None;
        if mine == (hash, count) {
            self.synced = true;
            self.phase = Phase::Live;
            self.heal_rounds = 0;
            step.synced = true;
            return Ok(());
        }
        if self.heal_rounds >= 2 {
            // Two list exchanges did not make the devices equal: report it, never hide it.
            self.gave_up = true;
            self.phase = Phase::Live;
            let text = format!(
                "the two devices still differ after a full compare ({} records here, {count} there); will retry on the next connect",
                mine.1
            );
            activity::log(c, now, Some(self.peer), None, ActivityAction::SyncError, &text)?;
            step.activity = true;
            step.errors.push(("reconcile".into(), text));
            return Ok(());
        }
        self.start_heal(self.gen.wrapping_add(1), step);
        Ok(())
    }

    /// A real difference: list everything and fetch what the other side has newer.
    fn start_heal(&mut self, gen: u8, step: &mut Step) {
        if self.healing || self.gave_up || self.synced {
            return;
        }
        self.gen = gen;
        self.heal_rounds += 1;
        self.healing = true;
        self.sent.clear();
        self.peer_digest = None;
        self.list_theirs.clear();
        self.fetch_outstanding = 0;
        step.send.push(Msg::ListRequest { after: None, limit: PAGE });
    }

    fn on_list(&mut self, c: &mut Connection, step: &mut Step, entries: Vec<ListEntry>, more: bool, now: i64) -> Result<()> {
        if !self.healing {
            return Err(proto_err("list nobody asked for"));
        }
        if entries.len() > PAGE as usize {
            return Err(proto_err(format!("list of {} entries, the limit is {PAGE}", entries.len())));
        }
        let last = entries.last().map(|e| e.key);
        self.list_theirs.extend(entries);
        if more {
            let Some(after) = last else { return Err(proto_err("empty list page claims more")) };
            step.send.push(Msg::ListRequest { after: Some(after), limit: PAGE });
            return Ok(());
        }
        let ours = digest::all_entries(c)?;
        let theirs = std::mem::take(&mut self.list_theirs);
        let keys = digest::keys_to_fetch(&ours, &theirs);
        for chunk in keys.chunks(PAGE as usize) {
            step.send.push(Msg::Fetch { keys: chunk.to_vec() });
            self.fetch_outstanding += 1;
        }
        if self.fetch_outstanding == 0 {
            self.finish_heal(c, step, now)?;
        }
        Ok(())
    }

    /// The list exchange is done and its records are applied: send a fresh digest.
    fn finish_heal(&mut self, c: &Connection, step: &mut Step, now: i64) -> Result<()> {
        self.healing = false;
        self.digest_due(c, step, now)
    }

    /// A local change happened (S8): the Notify to send, if the session is past the handshake.
    pub fn local_change(&mut self, c: &Connection) -> Result<Option<Msg>> {
        if matches!(self.phase, Phase::AwaitHello | Phase::Closed) {
            return Ok(None);
        }
        let head = store::head_seq(c)?;
        if head > self.last_notified {
            self.last_notified = head;
            return Ok(Some(Msg::Notify { head }));
        }
        Ok(None)
    }
}
