//! Protocol-level tests: two `Session`s pumped against each other over in-memory message queues
//! and real SQLite files, no network. Fast and deterministic. Scenarios S19, S20 plus the
//! sync rules in engine.rs (cursor, snapshot restart, digest comparability, bad records).

use std::collections::VecDeque;

use tugboat_core::{
    db::Db,
    ops,
    ops::Ctx,
    store,
    sync::{
        digest,
        engine::{Phase, Session, Step},
        wire::{Msg, PullFrom},
    },
    AppStorage, DeviceId, DeviceRow, Retention, Role,
};

const T0: i64 = 1_790_000_000_000;

struct Side {
    db: Db,
    id: DeviceId,
    sess: Session,
    inbox: VecDeque<Msg>,
    steps: Vec<Step>,
    /// Every message this side sent, in order.
    sent: Vec<Msg>,
    store: AppStorage,
    _dir: tempfile::TempDir,
    dir_path: std::path::PathBuf,
}

fn key(n: u8) -> DeviceId {
    DeviceId([n; 32])
}

fn side(role: Role, me: u8, peer: u8) -> Side {
    let dir = tempfile::tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let db = Db::open(&dir.path().join("data")).unwrap();
    store::ensure_self(&db.conn, key(me), if role == Role::Pc { "pc" } else { "phone" }, role).unwrap();
    let peer_row = DeviceRow {
        id: key(peer),
        name: "peer".into(),
        is_self: false,
        role: if role == Role::Pc { Role::Phone } else { Role::Pc },
        paired_at: None,
        unpaired_at: None,
        last_seen_at: None,
        last_synced_at: None,
        addresses: vec![],
    };
    store::set_paired(&db.conn, &peer_row, T0).unwrap();
    let storage = AppStorage::new(dir.path().join("storage")).unwrap();
    Side { db, id: key(me), sess: Session::new(key(me), key(peer), role, "t"), inbox: VecDeque::new(), steps: vec![], sent: vec![], store: storage, _dir: dir, dir_path }
}

impl Side {
    fn folder(&mut self, name: &str) -> tugboat_core::FolderId {
        let ctx = Ctx { self_id: self.id, now: T0, placement: &self.store };
        ops::create_folder(&mut self.db.conn, &ctx, name, Retention::Keep).unwrap()
    }

    fn rename(&mut self, id: tugboat_core::FolderId, name: &str) {
        let ctx = Ctx { self_id: self.id, now: T0, placement: &self.store };
        ops::rename_folder(&mut self.db.conn, &ctx, id, name).unwrap();
    }

    fn folder_name(&self, id: tugboat_core::FolderId) -> String {
        store::get_folder(&self.db.conn, id).unwrap().unwrap().name.val
    }

    fn log_rows(&self) -> i64 {
        self.db.conn.query_row("SELECT count(*) FROM change_log", [], |r| r.get(0)).unwrap()
    }
}

/// Run one connection: `phone` dials, `pc` accepts, until both inboxes are empty.
fn pump(phone: &mut Side, pc: &mut Side) {
    pump_with(phone, pc, false);
}

/// What the runner does after a step that wrote to the log (a delete-vs-keep rewrite, or records
/// relayed for other devices): the live session sends a Notify with the new head.
fn live_notify(side: &mut Side, step: &tugboat_core::sync::engine::Step) -> Option<Msg> {
    if step.relayed || step.wrote_local {
        side.sess.local_change(&side.db.conn).unwrap()
    } else {
        None
    }
}

/// `drop_digests`: Digest messages are swallowed, so each side has sent one but compared none.
fn pump_with(phone: &mut Side, pc: &mut Side, drop_digests: bool) {
    let hello = phone.sess.hello(&phone.db.conn).unwrap();
    phone.sent.push(hello.clone());
    pc.inbox.push_back(hello);
    let mut guard = 0;
    while !phone.inbox.is_empty() || !pc.inbox.is_empty() {
        guard += 1;
        assert!(guard < 10_000, "the protocol did not settle");
        if let Some(m) = pc.inbox.pop_front() {
            let step = pc.sess.handle(&mut pc.db.conn, m, T0).unwrap();
            pc.sent.extend(step.send.iter().cloned());
            phone.inbox.extend(step.send.iter().filter(|m| !(drop_digests && matches!(m, Msg::Digest { .. }))).cloned());
            if let Some(n) = live_notify(pc, &step) {
                pc.sent.push(n.clone());
                phone.inbox.push_back(n);
            }
            pc.steps.push(step);
        }
        if let Some(m) = phone.inbox.pop_front() {
            let step = phone.sess.handle(&mut phone.db.conn, m, T0).unwrap();
            phone.sent.extend(step.send.iter().cloned());
            pc.inbox.extend(step.send.iter().filter(|m| !(drop_digests && matches!(m, Msg::Digest { .. }))).cloned());
            if let Some(n) = live_notify(phone, &step) {
                phone.sent.push(n.clone());
                pc.inbox.push_back(n);
            }
            phone.steps.push(step);
        }
    }
}

fn pair_up() -> (Side, Side) {
    (side(Role::Phone, 0xBB, 0xAA), side(Role::Pc, 0xAA, 0xBB))
}

fn synced(s: &Side) -> bool {
    s.steps.iter().any(|st| st.synced)
}

fn same_state(a: &Side, b: &Side) -> bool {
    digest::compute(&a.db.conn).unwrap() == digest::compute(&b.db.conn).unwrap()
}

// Two devices with different histories converge in one pump, and both report an equal digest.
#[test]
fn first_connect_exchanges_snapshots_and_reaches_equal_digests() {
    let (mut phone, mut pc) = pair_up();
    let f1 = phone.folder("From phone");
    let f2 = pc.folder("From PC");
    pump(&mut phone, &mut pc);
    assert_eq!(phone.folder_name(f2), "From PC");
    assert_eq!(pc.folder_name(f1), "From phone");
    assert!(same_state(&phone, &pc));
    assert!(synced(&phone) && synced(&pc), "both sides saw an equal digest");
    assert_eq!(phone.sess.phase, Phase::Live);
    // The cursor of a finished snapshot is the sender's head, so a second connect pulls by seq.
    assert!(store::get_cursor(&phone.db.conn, key(0xAA)).unwrap().is_some());
}

// S19: a device restored from a backup has a low lamport. The handshake raises it before its
// first write, so its next edit beats everything the other device holds.
#[test]
fn s19_handshake_raises_a_restored_lamport_before_its_first_write() {
    let (mut phone, mut pc) = pair_up();
    let f = phone.folder("Work");
    pump(&mut phone, &mut pc);
    // Back up the PC, then the phone edits many times.
    let backup_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(backup_dir.path().join("data")).unwrap();
    let bak = backup_dir.path().join("data").join("tugboat.db");
    pc.db.conn.execute_batch(&format!("VACUUM INTO '{}'", bak.display().to_string().replace('\'', "''"))).unwrap();
    for n in 0..20 {
        phone.rename(f, &format!("v{n}"));
    }
    let high = store::max_lamport(&phone.db.conn).unwrap();
    assert!(high >= 20);

    // The restored PC: same key, the old database.
    let mut pc2 = side(Role::Pc, 0xAA, 0xBB);
    pc2.db = Db::open(&backup_dir.path().join("data")).unwrap();
    assert!(store::max_lamport(&pc2.db.conn).unwrap() < high, "restored database is behind");

    // Only the Hello is handled: the lamport is up before anything else happens.
    let hello = phone.sess.hello(&phone.db.conn).unwrap();
    pc2.sess.handle(&mut pc2.db.conn, hello, T0).unwrap();
    assert!(store::max_lamport(&pc2.db.conn).unwrap() >= high, "raised by the handshake");

    // Its first write now has a version above everything the phone holds and wins.
    pc2.rename(f, "restored PC wins");
    let mut phone2 = phone;
    phone2.sess = Session::new(key(0xBB), key(0xAA), Role::Phone, "t");
    pc2.sess = Session::new(key(0xAA), key(0xBB), Role::Pc, "t");
    pump(&mut phone2, &mut pc2);
    assert_eq!(phone2.folder_name(f), "restored PC wins");
    assert_eq!(pc2.folder_name(f), "restored PC wins");
    assert!(same_state(&phone2, &pc2));
    let _ = (&pc2.dir_path, &pc.dir_path);
}

// S4 (engine rule): a puller whose cursor is ahead of the sender's log (the sender was restored)
// is told to restart from a snapshot instead of silently missing every new change.
#[test]
fn a_cursor_ahead_of_the_senders_log_restarts_from_a_snapshot() {
    let (mut phone, mut pc) = pair_up();
    let f = pc.folder("A");
    for n in 0..5 {
        pc.rename(f, &format!("a{n}"));
    }
    pump(&mut phone, &mut pc);
    let (cursor, _) = store::get_cursor(&phone.db.conn, key(0xAA)).unwrap().unwrap();
    assert!(cursor >= 6);

    // A fresh PC database with the same identity and a SHORT log, and one new edit.
    let mut pc2 = side(Role::Pc, 0xAA, 0xBB);
    let g = pc2.folder("B");
    assert!(store::head_seq(&pc2.db.conn).unwrap() < cursor);
    let mut phone2 = phone;
    phone2.sess = Session::new(key(0xBB), key(0xAA), Role::Phone, "t");
    pump(&mut phone2, &mut pc2);
    assert_eq!(phone2.folder_name(g), "B", "the change under the old cursor number still arrived");
    assert!(same_state(&phone2, &pc2));
    let restart = pc2.sent.iter().any(|m| matches!(m, Msg::Changes { records, next: PullFrom::Snapshot { after: None }, more: true, .. } if records.is_empty()));
    assert!(restart, "the PC answered the stale cursor with a snapshot restart: {:?}", pc2.sent.iter().take(4).collect::<Vec<_>>());
}

/// The sender with its Hello handled, so it answers a Pull.
fn after_hello() -> (Side, Side) {
    let (phone, mut pc) = pair_up();
    let hello = phone.sess.hello(&phone.db.conn).unwrap();
    pc.sess.handle(&mut pc.db.conn, hello, T0).unwrap();
    (phone, pc)
}

fn is_snapshot_restart(m: &Msg) -> bool {
    matches!(m, Msg::Changes { records, next: PullFrom::Snapshot { after: None }, more: true, .. } if records.is_empty())
}

// A cursor below the log floor means the rows it needs were trimmed: restart from a snapshot, never
// an incremental page that would skip them. A cursor at the floor is still incremental.
#[test]
fn pull_below_floor_restarts_snapshot() {
    let (_phone, mut pc) = after_hello();
    for n in 0..4 {
        pc.folder(&format!("f{n}"));
    }
    store::set_log_floor(&pc.db.conn, 3).unwrap();
    let pull = |since| Msg::Pull { from: PullFrom::Seq { since }, limit: 10 };
    let step = pc.sess.handle(&mut pc.db.conn, pull(2), T0).unwrap();
    assert!(matches!(step.send.as_slice(), [m] if is_snapshot_restart(m)), "{:?}", step.send);
    assert_eq!(store::get_cursor(&pc.db.conn, key(0xBB)).unwrap(), None, "a restart acks nothing");
    let step = pc.sess.handle(&mut pc.db.conn, pull(3), T0).unwrap();
    assert!(matches!(step.send.as_slice(), [Msg::Changes { next: PullFrom::Seq { .. }, .. }, ..]), "at the floor the page is incremental: {:?}", step.send);
}

#[test]
fn pull_above_head_still_restarts() {
    let (_phone, mut pc) = after_hello();
    pc.folder("x");
    let head = store::head_seq(&pc.db.conn).unwrap();
    let step = pc.sess.handle(&mut pc.db.conn, Msg::Pull { from: PullFrom::Seq { since: head + 5 }, limit: 10 }, T0).unwrap();
    assert!(matches!(step.send.as_slice(), [m] if is_snapshot_restart(m)), "{:?}", step.send);
}

// A phone that was away past the retention converges by a snapshot, with no lost update.
#[test]
fn a_cursor_below_the_floor_converges_through_a_snapshot() {
    let (mut phone, mut pc) = pair_up();
    let f = pc.folder("A");
    pump(&mut phone, &mut pc);
    let (cursor, _) = store::get_cursor(&phone.db.conn, key(0xAA)).unwrap().unwrap();
    pc.rename(f, "B");
    let head = store::head_seq(&pc.db.conn).unwrap();
    assert!(cursor < head);
    pc.db.conn.execute("DELETE FROM change_log", []).unwrap();
    store::set_log_floor(&pc.db.conn, head).unwrap();
    pc.sent.clear();
    phone.sess = Session::new(key(0xBB), key(0xAA), Role::Phone, "t");
    pc.sess = Session::new(key(0xAA), key(0xBB), Role::Pc, "t");
    pump(&mut phone, &mut pc);
    assert!(pc.sent.iter().any(is_snapshot_restart), "the trimmed range was not skipped");
    assert_eq!(phone.folder_name(f), "B", "the update made in the trimmed range arrived");
    assert!(same_state(&phone, &pc));
    // The next connect is incremental again.
    pc.sent.clear();
    phone.sess = Session::new(key(0xBB), key(0xAA), Role::Phone, "t");
    pc.sess = Session::new(key(0xAA), key(0xBB), Role::Pc, "t");
    pump(&mut phone, &mut pc);
    assert!(!pc.sent.iter().any(is_snapshot_restart));
}

// The phone relays what a PC sent into its own log, tagged with that PC. The PC is not sent its own
// records back, yet its cursor still ends at the phone's head, so the two digests describe one state.
#[test]
fn final_page_cursor_is_head_with_exclusion() {
    let (mut phone, mut pc) = pair_up();
    let f = pc.folder("From PC");
    pump(&mut phone, &mut pc);
    let head = store::head_seq(&phone.db.conn).unwrap();
    assert!(head > 0, "the phone relayed the PC's records into its log");
    assert!(synced(&phone) && synced(&pc));
    assert_eq!(store::get_cursor(&pc.db.conn, key(0xBB)).unwrap().unwrap().0, head, "the cursor is the head although every row came from this PC");
    assert_eq!(store::changes_since(&phone.db.conn, 0, 100, key(0xAA)).unwrap().0.len(), 0, "nothing in the log is owed to the PC");

    // A later edit on the PC: the phone takes it, relays it, and sends nothing back.
    phone.sent.clear();
    phone.sess = Session::new(key(0xBB), key(0xAA), Role::Phone, "t");
    pc.sess = Session::new(key(0xAA), key(0xBB), Role::Pc, "t");
    pc.rename(f, "renamed on the PC");
    pump(&mut phone, &mut pc);
    assert_eq!(phone.folder_name(f), "renamed on the PC");
    assert!(!phone.sent.iter().any(|m| matches!(m, Msg::Changes { records, .. } if !records.is_empty())), "no record went back to its sender");
    assert!(same_state(&phone, &pc));
    assert_eq!(store::get_cursor(&pc.db.conn, key(0xBB)).unwrap().unwrap().0, store::head_seq(&phone.db.conn).unwrap());
}

// The phone names the PC to update; the PC (and a phone that is the one to update) keep the plain text.
#[test]
fn refusal_names_the_pc_on_the_phone() {
    let (mut phone, mut pc) = pair_up();
    phone.sess.peer_name = Some("WORK-PC".into());
    phone.sess.proto = 3;
    let hello = phone.sess.hello(&phone.db.conn).unwrap();
    let step = pc.sess.handle(&mut pc.db.conn, hello, T0).unwrap();
    assert_eq!(step.refused.as_deref(), Some("Update Tugboat on the PC"), "the PC keeps its wording");
    let back = phone.sess.handle(&mut phone.db.conn, step.send[0].clone(), T0).unwrap();
    assert_eq!(back.refused.as_deref(), Some("Update Tugboat on WORK-PC"));

    // The phone is the old one: it says so about itself, whatever the PC is called.
    let (mut phone, mut pc) = pair_up();
    phone.sess.peer_name = Some("WORK-PC".into());
    pc.sess.proto = 3;
    let hello = phone.sess.hello(&phone.db.conn).unwrap();
    let step = pc.sess.handle(&mut pc.db.conn, hello, T0).unwrap();
    assert_eq!(step.refused.as_deref(), Some("Update Tugboat on the phone"));
    let back = phone.sess.handle(&mut phone.db.conn, step.send[0].clone(), T0).unwrap();
    assert_eq!(back.refused.as_deref(), Some("Update Tugboat on the phone"));
}

// S20: the sender's log is trimmed up to the peer's Pull.since; nothing newer is touched.
#[test]
fn s20_log_is_trimmed_once_the_peer_has_pulled_it() {
    let (mut phone, mut pc) = pair_up();
    for n in 0..5 {
        pc.folder(&format!("f{n}"));
    }
    let before = pc.log_rows();
    assert!(before >= 5);
    pump(&mut phone, &mut pc);
    // First connect = snapshot: the PC's log is still there (the phone never asked "since").
    assert_eq!(pc.log_rows(), before);
    phone.sess = Session::new(key(0xBB), key(0xAA), Role::Phone, "t");
    pc.sess = Session::new(key(0xAA), key(0xBB), Role::Pc, "t");
    let extra = pc.folder("one more");
    pump(&mut phone, &mut pc);
    assert!(phone.folder_name(extra) == "one more");
    // The second connect pulled by seq since the snapshot head: everything up to it is gone.
    assert!(pc.log_rows() <= 3, "trimmed: {} rows left", pc.log_rows());
    // Another connect with nothing new trims the rest.
    phone.sess = Session::new(key(0xBB), key(0xAA), Role::Phone, "t");
    pc.sess = Session::new(key(0xAA), key(0xBB), Role::Pc, "t");
    pump(&mut phone, &mut pc);
    assert_eq!(pc.log_rows(), 0, "the peer has everything: log empty");
    assert!(same_state(&phone, &pc));
}

// S1 (rule): a message before Hello is a protocol error (S1), never processed.
#[test]
fn a_message_before_hello_is_a_protocol_error() {
    let (_phone, mut pc) = pair_up();
    let err = pc.sess.handle(&mut pc.db.conn, Msg::Pull { from: PullFrom::Seq { since: 0 }, limit: 10 }, T0).unwrap_err();
    assert_eq!(err.kind(), "protocol");
    let err = pc.sess.handle(&mut pc.db.conn, Msg::Notify { head: 1 }, T0).unwrap_err();
    assert_eq!(err.kind(), "protocol");
}

// S2 (rule): another protocol number is refused with the same one-line text on both sides, and no
// record is exchanged.
#[test]
fn version_mismatch_is_refused_on_both_sides_without_exchanging_records() {
    let (mut phone, mut pc) = pair_up();
    phone.sess.proto = 3;
    phone.folder("secret");
    let hello = phone.sess.hello(&phone.db.conn).unwrap();
    let step = pc.sess.handle(&mut pc.db.conn, hello, T0).unwrap();
    assert_eq!(step.refused.as_deref(), Some("Update Tugboat on the PC"), "the PC is the lower protocol");
    assert!(matches!(step.send.as_slice(), [Msg::Refuse { .. }]), "only the refusal is sent: {:?}", step.send);
    assert_eq!(step.close.as_ref().map(|c| c.0), Some(tugboat_net::close::VERSION_MISMATCH));
    let back = phone.sess.handle(&mut phone.db.conn, step.send[0].clone(), T0).unwrap();
    assert_eq!(back.refused.as_deref(), Some("Update Tugboat on the PC"));
    assert_eq!(pc.sess.phase, Phase::Closed);
    assert!(pc.sess.handle(&mut pc.db.conn, Msg::Notify { head: 1 }, T0).is_err());
    assert_eq!(digest::compute(&pc.db.conn).unwrap().1, 3, "the PC holds only its default folders");
}

// S5 (rule): a record that fails validation is skipped and reported; the rest of the page is
// applied and the cursor still advances.
#[test]
fn a_bad_record_is_skipped_reported_and_does_not_block_the_page() {
    let (mut phone, mut pc) = pair_up();
    let good = phone.folder("good");
    // A folder record claiming a zero-length name is fine; an item with a path-like extension is not.
    let bad = {
        let ctx = Ctx { self_id: phone.id, now: T0, placement: &phone.store };
        let src = phone.dir_path.join("x.jpg");
        std::fs::write(&src, [0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0]).unwrap();
        let id = ops::add_local_file(
            &mut phone.db.conn,
            &ctx,
            &ops::NewLocalFile { src, name: "x.jpg".into(), folder: None, captured_at: T0, geo: None, thumb: None, how: ops::AddKind::Captured, ntfs: None },
            None,
        )
        .unwrap();
        phone.db.conn.execute_batch(&format!("UPDATE item SET ext = '../exe' WHERE id = x'{}'", hex::encode(id.0.as_bytes()))).unwrap();
        id
    };
    pump(&mut phone, &mut pc);
    assert_eq!(pc.folder_name(good), "good", "the good record in the same page was applied");
    assert!(store::get_item(&pc.db.conn, bad).unwrap().is_none(), "the bad one was not stored");
    let errs: Vec<_> = pc.steps.iter().flat_map(|s| s.errors.iter()).collect();
    assert!(errs.iter().any(|(k, _)| k == "protocol"), "{errs:?}");
    assert!(store::get_cursor(&pc.db.conn, key(0xBB)).unwrap().is_some(), "cursor advanced");
    // The mismatch that remains is reported, not hidden.
    assert!(pc.steps.iter().flat_map(|s| s.errors.iter()).any(|(k, _)| k == "reconcile") || phone.steps.iter().flat_map(|s| s.errors.iter()).any(|(k, _)| k == "reconcile"));
}

// S7 (rule): a peer digest is compared only when it describes the same state as one we sent
// (our head is its cursor and our cursor is its head). Any other digest is "not yet comparable":
// it neither heals nor reports. A digest that does match and differs starts the list exchange.
#[test]
fn digests_are_compared_only_when_they_describe_the_same_state() {
    let (mut phone, mut pc) = pair_up();
    pc.folder("x");
    phone.folder("y");
    pump_with(&mut phone, &mut pc, true); // each side sent its digest; none was delivered
    assert!(!synced(&phone) && !synced(&pc));
    let head = store::head_seq(&phone.db.conn).unwrap();
    let (cursor, _) = store::get_cursor(&phone.db.conn, key(0xAA)).unwrap().unwrap();

    // A digest from a different moment (its cursor is not our head): ignored, nothing sent.
    let other = Msg::Digest { hash: "0".repeat(64), count: 1, head: cursor, cursor: head + 5, gen: 0 };
    let step = phone.sess.handle(&mut phone.db.conn, other, T0).unwrap();
    assert!(step.send.is_empty() && step.errors.is_empty() && !step.synced, "not comparable: {step:?}");

    // One that describes our state and has a different hash is a real difference: list exchange.
    let differs = Msg::Digest { hash: "0".repeat(64), count: 1, head: cursor, cursor: head, gen: 0 };
    let step = phone.sess.handle(&mut phone.db.conn, differs, T0).unwrap();
    assert!(matches!(step.send.as_slice(), [Msg::ListRequest { after: None, .. }]), "{step:?}");
}

// S7 (rule): the digest the other side really sent, for the same state, is equal: Synced.
#[test]
fn the_real_digest_of_an_equal_state_compares_equal() {
    let (mut phone, mut pc) = pair_up();
    pc.folder("x");
    phone.folder("y");
    pump_with(&mut phone, &mut pc, true);
    assert!(!synced(&phone));
    // The newest one: it describes the state the PC reached after the phone's relayed changes.
    let theirs = pc.sent.iter().rev().find(|m| matches!(m, Msg::Digest { .. })).cloned().expect("the PC sent a digest");
    let step = phone.sess.handle(&mut phone.db.conn, theirs, T0).unwrap();
    assert!(step.synced && step.send.is_empty() && step.errors.is_empty(), "{step:?}");
    assert_eq!(phone.sess.phase, Phase::Live);
}

// S12: the Unpair message ends the session and tells the runner to unpair (files stay).
#[test]
fn an_unpair_message_unpairs_and_closes() {
    let (mut phone, mut pc) = pair_up();
    pump(&mut phone, &mut pc);
    let step = pc.sess.handle(&mut pc.db.conn, Msg::Unpair, T0).unwrap();
    assert!(step.unpair);
    assert_eq!(step.close.as_ref().map(|c| c.0), Some(tugboat_net::close::UNPAIRED));
    assert_eq!(pc.sess.phase, Phase::Closed);
}

// Handshake: both ends being the same role is a protocol error, not a session.
#[test]
fn a_peer_with_the_same_role_is_a_protocol_error() {
    let (_phone, mut pc) = pair_up();
    let hello = Msg::Hello { proto: 1, app_version: "x".into(), device_name: "x".into(), role: Role::Pc, max_lamport: 0 };
    let err = pc.sess.handle(&mut pc.db.conn, hello, T0).unwrap_err();
    assert_eq!(err.kind(), "protocol");
}

// A paired peer's Hello cannot pin the lamport clock at its maximum: the session ends as a
// protocol error and the local clock is untouched, so local writes keep working.
#[test]
fn a_hello_with_an_absurd_lamport_is_a_protocol_error_and_leaves_the_clock_alone() {
    let (_phone, mut pc) = pair_up();
    let before = store::max_lamport(&pc.db.conn).unwrap();
    for max_lamport in [u64::MAX, i64::MAX as u64] {
        let hello = Msg::Hello { proto: tugboat_core::sync::wire::PROTO, app_version: "x".into(), device_name: "x".into(), role: Role::Phone, max_lamport };
        let err = pc.sess.handle(&mut pc.db.conn, hello, T0).unwrap_err();
        assert_eq!(err.kind(), "protocol", "{max_lamport}");
        pc.sess = Session::new(key(0xAA), key(0xBB), Role::Pc, "t");
    }
    assert_eq!(store::max_lamport(&pc.db.conn).unwrap(), before);
    assert_eq!(store::next_version(&pc.db.conn, pc.id).unwrap().lamport, before + 1, "local writes still work");
}
