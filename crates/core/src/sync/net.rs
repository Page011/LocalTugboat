//! Network driver: endpoint start/stop, the accept and connect loops, and the per-connection
//! run loop that feeds `engine::Session` and runs the blob fetch queue. Owner: sync builder.
//!
//! One control stream per connection (opened by the phone). A reader task turns frames into
//! channel messages (frame reads are not cancel safe), so the single control task can select
//! over: incoming message, local change (Notify), download kick, download result, command
//! (unpair), a 500 ms ticker for back-off retries, and the 10 s hello deadline.
//! Blob streams are separate: the stream task serves the peer's requests, `fetch_item`
//! downloads ours (two at a time, resumable, retried with back-off).

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use rusqlite::Connection;
use tugboat_net::{
    blob::{fetch_blob, serve_blob, BlobSource},
    close,
    error::stream_err,
    frame::{read_frame, write_frame},
    iroh::endpoint::{RecvStream, SendStream},
    node::{AccessGate, Accepted, Node, NodeConfig, PeerConn, Role as NetRole, StreamKind},
    NetError, ALPN_PAIR, ALPN_SYNC,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

use crate::{
    bin,
    events::{Event, PairingEvent, SyncState},
    expiry,
    facade::{lock, Core, Inner, NetworkConfig},
    model::*,
    names, ops,
    sync::{
        engine::{Phase, Session, Step},
        pair,
        wire::Msg,
    },
    CoreError, Result,
};

/// The other side must send its Hello within this.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// One dial attempt (cached addresses, then mDNS).
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// Downloads running at once (design S9).
const MAX_PARALLEL_FETCH: usize = 2;
/// Longest wait between two tries of one download.
const MAX_FETCH_BACKOFF: Duration = Duration::from_secs(60);
/// A Lost item (the peer said "not found") is asked for again this long after the last try, and
/// at once on every new connection: the answer is often momentary.
const LOST_RETRY: Duration = Duration::from_secs(300);
/// Thumbnails are small; a bigger "thumbnail" is a hostile or broken record.
const MAX_THUMB_BYTES: u64 = 16 * 1024 * 1024;
/// A session shorter than this does not reset the reconnect back-off.
const STABLE_SESSION: Duration = Duration::from_secs(5);

pub(crate) struct NetState {
    pub node: Node,
    pub tasks: Vec<JoinHandle<()>>,
}

pub(crate) struct Active {
    pub gen: u64,
    pub conn: Arc<PeerConn>,
    pub cmd: mpsc::UnboundedSender<Cmd>,
}

pub(crate) enum Cmd {
    /// Tell the peer we unpaired, then close. The sender is answered with whether the notice
    /// was written and acknowledged by the transport.
    Unpair(oneshot::Sender<bool>),
}

/// NetError -> CoreError (pairing errors keep their code and one-line text).
pub(crate) fn net_err(e: NetError) -> CoreError {
    match e {
        NetError::Pairing(pe) => CoreError::Pairing { code: pair::pair_code(&pe), msg: pair::pair_text(&pe) },
        other => CoreError::Net { kind: other.kind().to_string(), msg: other.to_string() },
    }
}

fn short(e: &dyn std::fmt::Display) -> String {
    e.to_string().chars().take(100).collect()
}

struct Gate(Weak<Inner>);

impl AccessGate for Gate {
    fn is_paired(&self, peer: &[u8; 32]) -> std::result::Result<bool, String> {
        let Some(inner) = self.0.upgrade() else { return Ok(false) };
        // A peer we owe an unpair notice still passes the gate, or our own hook would refuse
        // the one connection that delivers it.
        let r = {
            let db = lock(&inner.db);
            crate::store::is_paired(&db.conn, DeviceId(*peer)).and_then(|p| {
                if p {
                    return Ok(true);
                }
                Ok(crate::store::get_pending_unpair(&db.conn)?.is_some_and(|u| u.peer_id().is_ok_and(|id| id.0 == *peer)))
            })
        };
        match r {
            Ok(paired) => Ok(paired),
            // Refusing is the safe answer, but a database failure must not look like an
            // ordinary "not paired" refusal (the other device deletes its pairing on that
            // code): say what happened and let the caller close without NOT_PAIRED.
            Err(e) => {
                Core { inner }.emit_error(e.kind(), &format!("could not check the paired device, connection refused: {e}"));
                Err(e.to_string())
            }
        }
    }
}

/// A connection path left the local network (live guard) or a dial/accept hit a non-local
/// address: one visible error and one activity row, never only a debug log line.
pub(crate) fn report_not_local(core: &Core, addr: &str, what: &str) {
    let text = format!("BLOCKED {what}: {addr} is not on the local network");
    core.emit_error("not_local", &text);
    match core.with_ctx(|c, ctx| crate::activity::log(c, ctx.now, None, None, ActivityAction::SyncError, &text)) {
        Ok(_) => core.emit(Event::ActivityAdded),
        Err(e) => core.emit_error(e.kind(), &format!("could not log a blocked connection: {e}")),
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ---------- start / stop ----------

pub(crate) async fn start(core: &Core, cfg: NetworkConfig) -> Result<()> {
    if lock(&core.inner.net).is_some() {
        return Err(CoreError::Invalid("the network is already started".into()));
    }
    let role = core.inner.cfg.role;
    let gate = Arc::new(Gate(Arc::downgrade(&core.inner)));
    let net_role = if role == Role::Pc { NetRole::Pc } else { NetRole::Phone };
    let node = Node::start(NodeConfig { bind: cfg.bind, role: net_role, mdns: cfg.mdns, secret_key: cfg.secret_key }, gate).await.map_err(net_err)?;
    if node.id() != core.inner.cfg.self_id.0 {
        node.shutdown().await;
        return Err(CoreError::Invalid("the secret key does not belong to this device's id".into()));
    }
    {
        let weak = Arc::downgrade(&core.inner);
        node.set_violation_handler(Arc::new(move |addr: String| {
            if let Some(inner) = weak.upgrade() {
                report_not_local(&Core { inner }, &addr, "connection closed, path");
            }
        }));
    }
    let mut tasks = vec![tokio::spawn(accept_loop(core.clone(), node.clone()))];
    if role == Role::Phone {
        tasks.push(tokio::spawn(connect_loop(core.clone(), node.clone())));
    }
    *lock(&core.inner.net) = Some(NetState { node, tasks });
    core.set_state(SyncState::Offline, None);
    Ok(())
}

pub(crate) async fn stop(core: &Core) {
    let ns = lock(&core.inner.net).take();
    let Some(ns) = ns else { return };
    for t in &ns.tasks {
        t.abort();
    }
    // An aborted notice task cannot reset its own flag.
    core.inner.notice_running.store(false, std::sync::atomic::Ordering::SeqCst);
    if let Some(a) = lock(&core.inner.active).take() {
        a.conn.close(close::NORMAL, "network stopped");
    }
    ns.node.shutdown().await;
    core.set_state(SyncState::Offline, None);
}

// ---------- accept (PC) and connect (phone) loops ----------

async fn accept_loop(core: Core, node: Node) {
    while let Some(r) = node.accept().await {
        match r {
            Ok(acc) => {
                tokio::spawn(dispatch(core.clone(), acc));
            }
            // A refused stranger is not an event for the user; a non-local path is.
            Err(NetError::NotLocal(addr)) => report_not_local(&core, &addr, "incoming connection refused, address"),
            Err(e) => tracing::debug!("incoming connection ended: {e}"),
        }
    }
}

async fn dispatch(core: Core, acc: Accepted) {
    if lock(&core.inner.paused).is_some() {
        acc.peer.close(close::NORMAL, "sync paused");
        return;
    }
    if core.inner.cfg.role != Role::Pc {
        acc.peer.close(close::PROTOCOL, "this device does not accept connections");
        return;
    }
    if acc.alpn == ALPN_PAIR {
        pair::handle_conn(core, acc.peer).await;
    } else if acc.alpn == ALPN_SYNC {
        run_session(core, Arc::new(acc.peer), false).await;
    } else {
        acc.peer.close(close::PROTOCOL, "unknown protocol");
    }
}

/// Phone: keep a connection to the paired PC. Back-off 1 s doubling to 30 s between tries.
async fn connect_loop(core: Core, node: Node) {
    let mut backoff = Duration::from_secs(1);
    // The last blocked address reported, so a retry loop does not repeat the same error.
    let mut reported_not_local: Option<String> = None;
    loop {
        let target = loop {
            let found = {
                let db = lock(&core.inner.db);
                crate::store::get_pending_unpair(&db.conn).and_then(|pending| Ok((pending, crate::store::paired_peer(&db.conn)?)))
            };
            match found {
                // Unpaired with nobody to sync with: delivering the notice is the only job.
                Ok((Some(u), None)) => break Target::Unpair(u),
                Ok((pending, Some(p))) => {
                    // Re-paired while the old PC was away: its notice goes out on its own task,
                    // so a PC that stays off never holds up the new one.
                    if let Some(u) = pending {
                        spawn_notice(&core, &node, u);
                    }
                    if lock(&core.inner.paused).is_none() {
                        break Target::Pc(p);
                    }
                }
                Ok((None, None)) => {}
                Err(e) => core.emit_error(e.kind(), &format!("could not read the paired device: {e}")),
            }
            core.inner.wake.notified().await;
        };
        let pc = match target {
            Target::Pc(p) => p,
            Target::Unpair(u) => {
                match deliver_unpair(&core, &node, &u).await {
                    Ok(()) => backoff = Duration::from_secs(1),
                    Err(e) => {
                        tracing::debug!("could not tell the PC about the unpair yet: {e}");
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = core.inner.wake.notified() => { backoff = Duration::from_secs(1); }
                        }
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
                continue;
            }
        };
        // A refusal stays on screen until a handshake succeeds; do not flip it to Connecting on every retry.
        if lock(&core.inner.status).state != Some(SyncState::Refused) {
            core.set_state(SyncState::Connecting, None);
        }
        let addrs: Vec<SocketAddr> = pc.addresses.iter().filter_map(|a| a.parse().ok()).collect();
        match node.dial(pc.id.0, &addrs, ALPN_SYNC, DIAL_TIMEOUT).await {
            Ok(conn) => {
                reported_not_local = None;
                let started = Instant::now();
                run_session(core.clone(), Arc::new(conn), true).await;
                if started.elapsed() >= STABLE_SESSION {
                    backoff = Duration::from_secs(1);
                }
            }
            Err(NetError::NotLocal(addr)) => {
                // The stored address is not on the local network: no retry can fix that.
                if reported_not_local.as_deref() != Some(addr.as_str()) {
                    report_not_local(&core, &addr, "not connecting, address");
                    reported_not_local = Some(addr);
                }
                if lock(&core.inner.status).state != Some(SyncState::Refused) {
                    core.set_state(SyncState::Offline, None);
                }
            }
            Err(e) => {
                tracing::debug!("PC not reachable: {e}");
                // The status says so ("PC not found"); a log line per retry is not an error event.
                let keep = lock(&core.inner.status).state == Some(SyncState::Refused);
                if !keep {
                    core.set_state(SyncState::Offline, None);
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = core.inner.wake.notified() => { backoff = Duration::from_secs(1); }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

// ---------- unpair ----------

enum Target {
    Pc(DeviceRow),
    Unpair(crate::store::PendingUnpair),
}

/// Phone: dial the PC we unpaired from while apart and tell it (one Unpair frame, no sync).
/// Delivered also when the PC refuses us as unknown: it no longer pairs this phone, which is
/// what the notice is for. On success the pending record is cleared and the status refreshed.
async fn deliver_unpair(core: &Core, node: &Node, pending: &crate::store::PendingUnpair) -> std::result::Result<(), NetError> {
    let peer = pending.peer_id().map_err(|e| NetError::Protocol(e.to_string()))?;
    let addrs: Vec<SocketAddr> = pending.addresses.iter().filter_map(|a| a.parse().ok()).collect();
    let conn = node.dial(peer.0, &addrs, ALPN_SYNC, DIAL_TIMEOUT).await?;
    let sent = async {
        let (mut tx, _rx) = conn.open_stream(StreamKind::Control).await?;
        write_frame(&mut tx, &Msg::Unpair).await?;
        finish_and_wait(&mut tx).await
    }
    .await;
    conn.close(close::UNPAIRED, "unpaired");
    // Not confirmed (stopped, timed out, failed): the notice stays and is tried again. A PC that
    // closed on us because it no longer pairs this phone counts as told.
    if !unpair_delivered(&sent) {
        return sent;
    }
    // Clear only the notice just delivered: a newer one for another PC stays.
    match core.with_ctx(|c, _| match crate::store::get_pending_unpair(c)? {
        Some(p) if p.peer == pending.peer => crate::store::clear_pending_unpair(c),
        _ => Ok(()),
    }) {
        Ok(()) => core.emit_status(),
        Err(e) => core.emit_error(e.kind(), &format!("could not clear the unpair notice: {e}")),
    }
    Ok(())
}

/// Phone: keep trying to tell a replaced PC about the unpair, beside the live link to the new one.
/// Ends when the notice is delivered or void (cleared, replaced) or the network stops.
fn spawn_notice(core: &Core, node: &Node, first: crate::store::PendingUnpair) {
    use std::sync::atomic::Ordering::SeqCst;
    if core.inner.notice_running.swap(true, SeqCst) {
        return;
    }
    let (task_core, node) = (core.clone(), node.clone());
    let handle = tokio::spawn(async move {
        let core = task_core;
        let mut pending = first;
        let mut backoff = Duration::from_secs(1);
        loop {
            if lock(&core.inner.net).is_none() {
                break;
            }
            match deliver_unpair(&core, &node, &pending).await {
                Ok(()) => break,
                Err(e) => {
                    tracing::debug!("could not tell the old PC about the unpair yet: {e}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
            // Void if it was cleared or replaced meanwhile.
            let still = lock(&core.inner.db);
            match crate::store::get_pending_unpair(&still.conn) {
                Ok(Some(p)) => pending = p,
                Ok(None) => break,
                Err(e) => {
                    drop(still);
                    core.emit_error(e.kind(), &format!("could not read the unpair notice: {e}"));
                    break;
                }
            }
        }
        core.inner.notice_running.store(false, SeqCst);
    });
    // The task belongs to this network instance: `stop` aborts it with the loops, so a restarted
    // network never inherits a task that holds the old, shut-down node.
    match lock(&core.inner.net).as_mut() {
        Some(ns) => ns.tasks.push(handle),
        None => {
            handle.abort();
            core.inner.notice_running.store(false, SeqCst);
        }
    }
}

/// Local side of an unpair (also run when the peer tells us it unpaired). Files stay.
/// Ok(true) = unpaired now, Ok(false) = nothing was paired. A failure is returned (and shown):
/// callers that are about to tell the peer must not do so when this device is still paired.
fn unpair_local(core: &Core, peer: DeviceId) -> Result<bool> {
    let r = core.with_ctx(|c, ctx| {
        if crate::store::is_paired(c, peer)? {
            ops::unpair(c, ctx, peer)?;
            Ok(true)
        } else {
            Ok(false)
        }
    });
    match &r {
        Ok(true) => {
            core.emit(Event::Pairing(PairingEvent::Unpaired { peer }));
            core.emit(Event::ActivityAdded);
            core.emit_status();
        }
        Ok(false) => {}
        Err(e) => core.emit_error(e.kind(), &format!("could not unpair: {e}")),
    }
    r
}

pub(crate) async fn unpair(core: &Core) -> Result<()> {
    let peer = {
        let db = lock(&core.inner.db);
        crate::store::paired_peer(&db.conn)?
    }
    .ok_or(CoreError::NotPaired)?;
    // If this device cannot unpair, say so and tell nobody: the peer would drop the pairing while
    // this device kept it, and the caller would see success.
    unpair_local(core, peer.id)?;
    let cmd = lock(&core.inner.active).as_ref().filter(|a| a.conn.peer() == peer.id.0).map(|a| a.cmd.clone());
    let mut delivered = false;
    if let Some(cmd) = cmd {
        let (dtx, drx) = oneshot::channel();
        if cmd.send(Cmd::Unpair(dtx)).is_ok() {
            delivered = matches!(tokio::time::timeout(Duration::from_secs(3), drx).await, Ok(Ok(true)));
        }
    }
    // A PC that was not told learns it when the phone next dials (the phone then gets
    // NOT_PAIRED and unpairs itself). The reverse needs help: a phone that unpaired while away
    // never dials the PC again, so it keeps a notice and delivers it once the PC is reachable.
    if !delivered && core.inner.cfg.role == Role::Phone {
        let pending = crate::store::PendingUnpair { peer: hex::encode(peer.id.0), addresses: peer.addresses.clone() };
        core.with_ctx(|c, _| crate::store::set_pending_unpair(c, &pending))?;
        core.emit_status();
        core.inner.wake.notify_one();
    }
    Ok(())
}

// ---------- one connection ----------

enum End {
    Clean,
    Net(NetError),
    Core(CoreError),
}

enum Flow {
    Continue,
    Stop(End),
}

pub(crate) async fn run_session(core: Core, conn: Arc<PeerConn>, dialer: bool) {
    let gen = core.inner.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    // A second connection from the same paired peer replaces the first (phone changed network).
    let old = lock(&core.inner.active).replace(Active { gen, conn: conn.clone(), cmd: cmd_tx });
    if let Some(old) = old {
        old.conn.close(close::NORMAL, "replaced by a newer connection");
    }
    let peer = DeviceId(conn.peer());
    let (end, refused) = drive(&core, &conn, dialer, cmd_rx).await;
    conn.close(close::NORMAL, "session ended");
    // "Last seen" is the last contact: written when the session ends too, not only at the hello,
    // so a long session does not leave the time at its start (Settings showed "never" / stale).
    if matches!(core.with_ctx(|c, _| crate::store::is_paired(c, peer)), Ok(true)) {
        if let Err(e) = core.with_ctx(|c, ctx| crate::store::touch_last_seen(c, peer, ctx.now)) {
            core.emit_error(e.kind(), &format!("could not save the last seen time: {e}"));
        }
    }
    let current = {
        let mut a = lock(&core.inner.active);
        let mine = a.as_ref().is_some_and(|x| x.gen == gen);
        if mine {
            *a = None;
        }
        mine
    };
    match end {
        End::Clean => {}
        End::Core(e) => core.emit_error(e.kind(), &e.to_string()),
        End::Net(NetError::ClosedByPeer { code, reason }) if code == close::NOT_PAIRED || code == close::UNPAIRED => {
            tracing::info!("the other device unpaired us: {reason}");
            // A failure is already shown by unpair_local; the pairing stays until a retry works.
            let _ = unpair_local(&core, peer);
        }
        End::Net(NetError::ClosedByPeer { code, reason }) if code == close::NORMAL => tracing::debug!("peer closed: {reason}"),
        End::Net(e @ (NetError::Timeout(_) | NetError::Stream(_))) => tracing::debug!("connection lost: {e}"),
        End::Net(e) => core.emit_error(e.kind(), &e.to_string()),
    }
    if current {
        match refused {
            Some(text) => core.set_state(SyncState::Refused, Some(text)),
            None => core.set_state(SyncState::Offline, None),
        }
    }
}

/// How `finish_and_wait` ended: what the transport said about the finished stream.
#[derive(Debug)]
enum StreamEnd {
    /// Every byte and the FIN were acknowledged.
    Acked,
    /// The peer sent STOP_SENDING: it may not have the whole frame.
    Stopped(u64),
    /// The stream or connection failed.
    Failed(NetError),
    /// No answer in time.
    TimedOut,
}

/// A frame counts as delivered only when the transport acknowledged all of it.
fn confirmed(end: StreamEnd) -> std::result::Result<(), NetError> {
    match end {
        StreamEnd::Acked => Ok(()),
        StreamEnd::Stopped(code) => Err(NetError::Stream(format!("the peer stopped reading (code {code})"))),
        StreamEnd::Failed(e) => Err(e),
        StreamEnd::TimedOut => Err(NetError::Timeout("the peer did not confirm it received the frame".into())),
    }
}

/// finish + wait until the peer has the bytes, so a following close cannot drop them. The result
/// says whether it did: callers that must know (the unpair notice) use it, the others may ignore it.
async fn finish_and_wait(tx: &mut SendStream) -> std::result::Result<(), NetError> {
    if let Err(e) = tx.finish() {
        return confirmed(StreamEnd::Failed(stream_err("finish", &e)));
    }
    confirmed(match tokio::time::timeout(Duration::from_secs(2), tx.stopped()).await {
        Err(_) => StreamEnd::TimedOut,
        Ok(Ok(None)) => StreamEnd::Acked,
        Ok(Ok(Some(code))) => StreamEnd::Stopped(code.into_inner()),
        Ok(Err(e)) => StreamEnd::Failed(stream_err("confirm delivery", &e)),
    })
}

/// The unpair notice reached the other device: the transport confirmed it, or the device closed
/// on us because it no longer pairs this one (the very thing the notice says).
fn unpair_delivered(r: &std::result::Result<(), NetError>) -> bool {
    match r {
        Ok(()) => true,
        Err(NetError::ClosedByPeer { code, .. }) => *code == close::NOT_PAIRED || *code == close::UNPAIRED,
        Err(_) => false,
    }
}

async fn stream_task(core: Core, conn: Arc<PeerConn>, mut control: Option<oneshot::Sender<(SendStream, RecvStream)>>) {
    let source = Arc::new(CoreBlobSource { core });
    loop {
        match conn.accept_stream().await {
            Ok((StreamKind::Blob, tx, rx)) => {
                let s = source.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_blob(tx, rx, &*s).await {
                        tracing::debug!("blob serve ended: {e}");
                    }
                });
            }
            Ok((StreamKind::Control, tx, rx)) => match control.take() {
                Some(c) => {
                    let _ = c.send((tx, rx));
                }
                None => {
                    conn.close(close::PROTOCOL, "a second control stream");
                    return;
                }
            },
            Ok((StreamKind::Pair, ..)) => {
                conn.close(close::PROTOCOL, "pairing stream on a sync connection");
                return;
            }
            Err(_) => return,
        }
    }
}

async fn drive(core: &Core, conn: &Arc<PeerConn>, dialer: bool, mut cmd_rx: mpsc::UnboundedReceiver<Cmd>) -> (End, Option<String>) {
    let inner = &core.inner;
    let peer = DeviceId(conn.peer());
    let paired = {
        let db = lock(&inner.db);
        crate::store::is_paired(&db.conn, peer)
    };
    match paired {
        Ok(true) => {}
        Ok(false) => {
            conn.close(close::NOT_PAIRED, "not paired");
            return (End::Clean, None);
        }
        // A database fault is not "not paired": NOT_PAIRED makes the healthy device delete its pairing.
        Err(e) => {
            conn.close(close::NORMAL, "could not check pairing");
            return (End::Core(e), None);
        }
    }
    core.set_state(SyncState::Syncing, None);

    let (ctl_tx, ctl_rx) = oneshot::channel();
    let _streams = AbortOnDrop(tokio::spawn(stream_task(core.clone(), conn.clone(), if dialer { None } else { Some(ctl_tx) })));
    let (mut tx, rx) = if dialer {
        match conn.open_stream(StreamKind::Control).await {
            Ok(p) => p,
            Err(e) => return (End::Net(e), None),
        }
    } else {
        match tokio::time::timeout(HELLO_TIMEOUT, ctl_rx).await {
            Ok(Ok(p)) => p,
            _ => {
                conn.close(close::PROTOCOL, "no control stream");
                return (End::Net(NetError::Timeout("the peer never opened the control stream".into())), None);
            }
        }
    };
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<std::result::Result<Option<Msg>, NetError>>();
    let _reader = AbortOnDrop(tokio::spawn(async move {
        let mut rx = rx;
        loop {
            let ev = read_frame::<Msg>(&mut rx).await;
            let last = !matches!(ev, Ok(Some(_)));
            if msg_tx.send(ev).is_err() || last {
                return;
            }
        }
    }));

    let mut sess = Session::new(inner.cfg.self_id, peer, inner.cfg.role, &inner.cfg.app_version);
    sess.proto = inner.proto.load(std::sync::atomic::Ordering::SeqCst);
    if dialer {
        let hello = {
            let db = lock(&inner.db);
            sess.hello(&db.conn)
        };
        let sent = match hello {
            Ok(h) => write_frame(&mut tx, &h).await.map_err(End::Net),
            Err(e) => Err(End::Core(e)),
        };
        if let Err(end) = sent {
            return (end, None);
        }
    }

    let (fetch_tx, mut fetch_rx) = mpsc::unbounded_channel::<FetchDone>();
    let mut fetcher = Fetcher::default();
    let mut refused: Option<String> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let hello_deadline = Instant::now() + HELLO_TIMEOUT;
    // True while messages, local changes or downloads happened since the last tick: a digest is
    // only sent into a quiet session, where the peer's state can match it (engine S7).
    let mut busy = false;

    loop {
        tokio::select! {
            ev = msg_rx.recv() => {
                busy = true;
                let msg = match ev {
                    Some(Ok(Some(m))) => m,
                    // The peer finished its control stream, or the reader is gone.
                    Some(Ok(None)) | None => return (End::Clean, refused),
                    Some(Err(e)) => return (End::Net(e), refused),
                };
                let now = core.now();
                let step = {
                    let mut db = lock(&inner.db);
                    sess.handle(&mut db.conn, msg, now)
                };
                match step {
                    Err(e) => {
                        conn.close(close::PROTOCOL, &short(&e));
                        return (End::Core(e), refused);
                    }
                    Ok(step) => {
                        if let Flow::Stop(end) = apply_step(core, conn, &sess, &mut tx, step, now, &mut refused).await {
                            return (end, refused);
                        }
                        schedule(core, conn, peer, &mut fetcher, &fetch_tx);
                        evaluate(core, &sess, &refused);
                    }
                }
            }
            _ = inner.local_changed.notified() => {
                busy = true;
                let m = {
                    let db = lock(&inner.db);
                    sess.local_change(&db.conn)
                };
                match m {
                    Ok(Some(m)) => {
                        if let Err(e) = write_frame(&mut tx, &m).await {
                            return (End::Net(e), refused);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => return (End::Core(e), refused),
                }
                // A new receipt or restore can change what is downloadable and the waiting count.
                schedule(core, conn, peer, &mut fetcher, &fetch_tx);
                evaluate(core, &sess, &refused);
            }
            _ = inner.kick.notified() => {
                schedule(core, conn, peer, &mut fetcher, &fetch_tx);
                evaluate(core, &sess, &refused);
            }
            Some(done) = fetch_rx.recv() => {
                busy = true;
                fetcher.finish(&done, inner.retry_base_ms.load(std::sync::atomic::Ordering::SeqCst));
                match done.outcome {
                    FetchOutcome::Done => {
                        inner.local_changed.notify_one();
                        core.emit(Event::ItemsChanged { ids: vec![done.id] });
                        core.emit(Event::ActivityAdded);
                    }
                    FetchOutcome::Retry { ref kind, ref message, connection_lost } => {
                        if !connection_lost && conn.is_open() {
                            core.emit_error(kind, &format!("download failed, will retry: {message}"));
                        }
                    }
                    FetchOutcome::Final { ref kind, ref message } => {
                        core.emit_error(kind, message);
                        core.emit(Event::ItemsChanged { ids: vec![done.id] });
                        core.emit(Event::ActivityAdded);
                    }
                    FetchOutcome::Dropped => {}
                }
                schedule(core, conn, peer, &mut fetcher, &fetch_tx);
                evaluate(core, &sess, &refused);
            }
            Some(Cmd::Unpair(done)) = cmd_rx.recv() => {
                let sent = async {
                    write_frame(&mut tx, &Msg::Unpair).await?;
                    finish_and_wait(&mut tx).await
                }
                .await;
                conn.close(close::UNPAIRED, "unpaired");
                // Reported as delivered only when the transport confirmed the whole frame.
                let delivered = unpair_delivered(&sent);
                let _ = done.send(delivered);
                return (if delivered { End::Clean } else { sent.err().map_or(End::Clean, End::Net) }, refused);
            }
            _ = ticker.tick() => {
                let guard_wait = std::time::Duration::from_millis(inner.guard_recheck_ms.load(std::sync::atomic::Ordering::SeqCst));
                let guard_due = inner.storage_low.load(std::sync::atomic::Ordering::SeqCst) && fetcher.last_schedule.elapsed() >= guard_wait;
                if fetcher.has_backoff() || fetcher.lost_due() || guard_due {
                    schedule(core, conn, peer, &mut fetcher, &fetch_tx);
                }
                if busy {
                    busy = false;
                } else if !sess.synced && sess.phase != Phase::AwaitHello && sess.phase != Phase::Closed {
                    let now = core.now();
                    let step = {
                        let db = lock(&inner.db);
                        sess.quiet_tick(&db.conn, now)
                    };
                    match step {
                        Err(e) => {
                            conn.close(close::PROTOCOL, &short(&e));
                            return (End::Core(e), refused);
                        }
                        Ok(step) if step.send.is_empty() && !step.synced => {}
                        Ok(step) => {
                            if let Flow::Stop(end) = apply_step(core, conn, &sess, &mut tx, step, now, &mut refused).await {
                                return (end, refused);
                            }
                            evaluate(core, &sess, &refused);
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(hello_deadline), if sess.phase == Phase::AwaitHello => {
                conn.close(close::PROTOCOL, "no hello");
                return (End::Net(NetError::Timeout("the peer sent no hello".into())), refused);
            }
        }
    }
}

/// Effects of one handled message: unpair, status, events, frames out, sweep.
async fn apply_step(core: &Core, conn: &Arc<PeerConn>, sess: &Session, tx: &mut SendStream, step: Step, now: i64, refused: &mut Option<String>) -> Flow {
    let peer = sess.peer;
    if step.unpair {
        // A failure is already shown by unpair_local. A phone retries on its next dial (this PC
        // answers NOT_PAIRED); a PC keeps the pairing until the user unpairs it there (an
        // app-level acknowledgement to the phone would need a wire change).
        let _ = unpair_local(core, peer);
    }
    if let Some(text) = &step.refused {
        *refused = Some(text.clone());
        core.set_state(SyncState::Refused, Some(text.clone()));
        core.emit_error("version_mismatch", text);
    }
    if !step.changed_items.is_empty() {
        core.emit(Event::ItemsChanged { ids: step.changed_items.clone() });
    }
    if step.folders_changed {
        core.emit(Event::FoldersChanged);
    }
    if step.places_changed {
        core.emit(Event::PlacesChanged);
    }
    if step.activity || step.applied > 0 {
        core.emit(Event::ActivityAdded);
    }
    for (kind, message) in &step.errors {
        core.emit_error(kind, message);
    }
    for m in &step.send {
        if let Err(e) = write_frame(tx, m).await {
            return Flow::Stop(End::Net(e));
        }
    }
    if let Some((code, reason)) = &step.close {
        // Best effort: the close below follows either way.
        let _ = finish_and_wait(tx).await;
        conn.close(*code, reason);
        return Flow::Stop(End::Clean);
    }
    if step.applied > 0 {
        // S6: held files follow the new state (Keep or move on the other device returns an
        // expired file; a delete moves one to the Bin). Items that became Pending are queued.
        // A changed folder (retention, delete) can expire items nobody touched: full sweep then.
        let swept = if step.folders_changed { core.sweep_and_emit().map(|_| ()) } else { core.sweep_changed(&step.changed_items) };
        if let Err(e) = swept {
            core.emit_error(e.kind(), &format!("sweep failed: {e}"));
        }
        if step.wrote_local {
            core.inner.local_changed.notify_one();
        }
    }
    if step.synced || (step.applied > 0 && sess.synced) {
        core.mark_synced(now);
    }
    Flow::Continue
}

/// Status from what the session knows: Synced only when the digest matched and nothing is
/// left to download.
fn evaluate(core: &Core, sess: &Session, refused: &Option<String>) {
    if refused.is_some() || sess.phase == Phase::AwaitHello {
        return;
    }
    let pending = {
        let db = lock(&core.inner.db);
        crate::store::items_in_state(&db.conn, &[LocalFileState::Pending], 1).map(|v| !v.is_empty()).unwrap_or(true)
    };
    core.set_state(if sess.synced && !pending { SyncState::Synced } else { SyncState::Syncing }, None);
}

// ---------- blob downloads ----------

struct FetchDone {
    id: ItemId,
    outcome: FetchOutcome,
}

enum FetchOutcome {
    Done,
    /// Try again later (network error, hash mismatch, peer's file changed).
    Retry { kind: String, message: String, connection_lost: bool },
    /// Recorded as Refused or Lost and reported to the user. A Lost item is asked for again
    /// later (`LOST_RETRY`, every new connection); a Refused one is not.
    Final { kind: String, message: String },
    /// Nothing to do any more (item gone, no longer Pending).
    Dropped,
}

struct Fetcher {
    in_flight: HashSet<ItemId>,
    /// When `schedule` last ran: a queue blocked by the storage guard looks again after a while.
    last_schedule: Instant,
    backoff: HashMap<ItemId, (u32, Instant)>,
    /// Lost items already asked for in this session -> earliest time to ask again.
    lost_tried: HashMap<ItemId, Instant>,
}

impl Default for Fetcher {
    fn default() -> Self {
        Fetcher {
            in_flight: HashSet::new(),
            last_schedule: Instant::now(),
            backoff: HashMap::new(),
            lost_tried: HashMap::new(),
        }
    }
}

impl Fetcher {
    fn has_backoff(&self) -> bool {
        !self.backoff.is_empty()
    }

    /// A Lost item is due for another try.
    fn lost_due(&self) -> bool {
        let now = Instant::now();
        self.lost_tried.values().any(|t| *t <= now)
    }

    fn finish(&mut self, d: &FetchDone, base_ms: u64) {
        self.in_flight.remove(&d.id);
        match &d.outcome {
            FetchOutcome::Retry { .. } => {
                let tries = self.backoff.get(&d.id).map(|b| b.0).unwrap_or(0) + 1;
                let wait = Duration::from_millis(base_ms.saturating_mul(1u64 << (tries - 1).min(16))).min(MAX_FETCH_BACKOFF);
                self.backoff.insert(d.id, (tries, Instant::now() + wait));
            }
            outcome => {
                self.backoff.remove(&d.id);
                if matches!(outcome, FetchOutcome::Done) {
                    self.lost_tried.remove(&d.id);
                }
            }
        }
    }
}

/// What the storage guard knows about the free space in one scheduling pass.
enum Space {
    /// The PC: no guard.
    Unlimited,
    /// Phone: bytes that may still be written before the guard is reached (free space, minus what
    /// the running downloads still have to write, minus the guard); `below` = free < guard now.
    Known { spare: u64, below: bool, free: u64, guard: u64 },
    /// `available_space()` failed: never read as "enough".
    Unknown(String),
}

/// The phone storage guard (R1, 0.2.0) for one `pick_downloads` pass: what it is allowed to look
/// at, and what it reports back.
struct GuardPass {
    /// Only the phone has a guard; the PC picks without one.
    phone: bool,
    placement: Arc<dyn crate::placement::Placement>,
    /// Set when the guard held an item back in this pass: why.
    held_back: Option<String>,
}

impl GuardPass {
    fn new(core: &Core) -> GuardPass {
        GuardPass { phone: core.inner.cfg.role == Role::Phone, placement: core.inner.placement.clone(), held_back: None }
    }

    /// No guard (tests that are not about the guard).
    #[cfg(test)]
    fn off(core: &Core) -> GuardPass {
        GuardPass { phone: false, ..GuardPass::new(core) }
    }
}

/// Rows read per page when looking for something to download.
const PICK_PAGE: u32 = 500;

/// Owned by a download task: while it lives its content hash is claimed core-wide, so no second
/// task (this session's or a later one's) writes the same `<hash>.part`. Dropping it, whatever
/// ended the task, frees the hash and wakes the scheduler: a new session may have skipped the
/// item only because this task still held it.
struct HashGuard {
    core: Core,
    hash: BlobHash,
}

impl Drop for HashGuard {
    fn drop(&mut self) {
        lock(&self.core.inner.fetching).remove(&self.hash);
        self.core.inner.kick.notify_one();
    }
}

/// Choose up to `slots` items to download: Pending oldest first, then Lost (asked at most once per
/// `LOST_RETRY`). It pages past every item it has to skip (in flight, in back-off, in the Bin, hash
/// already being fetched, held back by the phone storage guard), so a block of failing items never
/// starves the ones behind it. The hash of each pick is claimed in `fetching` before this returns.
/// On an error nothing stays claimed: no task would ever own those claims, so the items would be
/// skipped until the app restarts.
fn pick_downloads(
    c: &Connection,
    f: &Fetcher,
    fetching: &Mutex<HashMap<BlobHash, u64>>,
    slots: usize,
    now: i64,
    t: Instant,
    gp: &mut GuardPass,
) -> Result<Vec<(ItemId, BlobHash, bool)>> {
    let mut out: Vec<(ItemId, BlobHash, bool)> = Vec::new();
    match pick_into(c, f, fetching, slots, now, t, gp, &mut out) {
        Ok(()) => Ok(out),
        Err(e) => {
            let mut held = lock(fetching);
            for (_, hash, _) in &out {
                held.remove(hash);
            }
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn pick_into(
    c: &Connection,
    f: &Fetcher,
    fetching: &Mutex<HashMap<BlobHash, u64>>,
    slots: usize,
    now: i64,
    t: Instant,
    gp: &mut GuardPass,
    out: &mut Vec<(ItemId, BlobHash, bool)>,
) -> Result<()> {
    // Phone storage guard: how much may still be written in this pass.
    let mut space = Space::Unlimited;
    let mut max_blob = u64::MAX;
    if gp.phone {
        let settings = crate::store::load_settings(c)?;
        max_blob = settings.max_blob_bytes;
        let guard = settings.storage_guard_bytes;
        space = match gp.placement.available_space() {
            Ok(free) => {
                // Every live download task, whichever session started it: `fetching` is core-wide.
                let still_coming: u64 =
                    lock(fetching).iter().map(|(h, size)| size.saturating_sub(part_len(&*gp.placement, h))).fold(0u64, |a, b| a.saturating_add(b));
                Space::Known { spare: free.saturating_sub(still_coming).saturating_sub(guard), below: free < guard, free, guard }
            }
            Err(e) => Space::Unknown(e.to_string()),
        };
    }
    'states: for state in [LocalFileState::Pending, LocalFileState::Lost] {
        let mut after = None;
        // Items the storage guard holds back are not rare and not bounded like back-off or Bin
        // skips: with the phone low on space every row is held. Past one page of them the scan
        // of this state stops (0.2.0's window), so a big backlog is not walked on every pass
        // under the DB lock.
        let mut guard_skips: u32 = 0;
        'pages: loop {
            let page = crate::store::items_in_state_after(c, &[state], after, PICK_PAGE)?;
            for (captured_at, id) in &page {
                after = Some((*captured_at, *id));
                if out.len() >= slots {
                    break 'states;
                }
                let id = *id;
                if f.in_flight.contains(&id) || f.backoff.get(&id).is_some_and(|b| b.1 > t) {
                    continue;
                }
                // The retry deadline belongs to Lost items only: a Pending item (restored from the
                // Bin, or new content) that still carries one is downloaded at once.
                if state == LocalFileState::Lost && f.lost_tried.get(&id).is_some_and(|next| *next > t) {
                    continue;
                }
                let Some((rec, lfs)) = crate::store::get_item(c, id)? else { continue };
                let hash = rec.content.val.hash;
                // A Pending item that sits in the Bin is never downloaded: the sweep purges it.
                if expiry::in_bin(bin::derive_state(c, &rec, now)?) {
                    continue;
                }
                // A hash a task already holds is skipped before the guard looks at it, so a
                // duplicate never turns the storage-low flag on.
                if lock(fetching).contains_key(&hash) {
                    continue;
                }
                // A record over max_blob_bytes is never stored: fetch_item refuses it once (TOO
                // LARGE), so the space guard must not hold it back for ever behind a PHONE STORAGE
                // LOW banner.
                let size = rec.content.val.size;
                let mut need_now = 0u64;
                match &mut space {
                    Space::Unlimited => {}
                    _ if size > max_blob => {}
                    Space::Unknown(e) => {
                        gp.held_back.get_or_insert_with(|| format!("cannot read the free space: {e}"));
                        guard_skips += 1;
                        if guard_skips >= PICK_PAGE {
                            break 'pages;
                        }
                        continue;
                    }
                    Space::Known { below: true, free, guard, .. } => {
                        gp.held_back.get_or_insert_with(|| format!("{} MB free, the guard is {} MB", *free / (1024 * 1024), *guard / (1024 * 1024)));
                        guard_skips += 1;
                        if guard_skips >= PICK_PAGE {
                            break 'pages;
                        }
                        continue;
                    }
                    Space::Known { spare, guard, .. } => {
                        let need = size.saturating_sub(part_len(&*gp.placement, &hash));
                        if need > *spare {
                            gp.held_back.get_or_insert_with(|| format!("a {} MB file does not fit above the {} MB guard", need / (1024 * 1024), *guard / (1024 * 1024)));
                            guard_skips += 1;
                            if guard_skips >= PICK_PAGE {
                                break 'pages;
                            }
                            continue;
                        }
                        need_now = need;
                    }
                }
                // Two items with the same file share one part file: one download at a time per
                // hash, across sessions (a task outlives its session). The last check, the claim.
                match lock(fetching).entry(hash) {
                    std::collections::hash_map::Entry::Occupied(_) => continue,
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(size);
                    }
                }
                if let Space::Known { spare, .. } = &mut space {
                    *spare -= need_now;
                }
                out.push((id, hash, lfs == LocalFileState::Lost));
            }
            if page.len() < PICK_PAGE as usize {
                break;
            }
        }
    }
    Ok(())
}

/// Start downloads for Pending items, oldest first, up to `MAX_PARALLEL_FETCH`.
///
/// Phone storage guard (R1, 0.2.0): `Settings.storage_guard_bytes` is enforced here, on the phone
/// only. Nothing is downloaded while the free space is below the guard, and one item is skipped
/// when `free - bytes still to come of the downloads running - this item's remaining bytes` would
/// drop below the guard (a 3 GB video never eats the margin). A part file that already exists
/// counts as used, so a resumed download only needs the rest. When `available_space()` fails the
/// queue is blocked too and the error is shown: an unknown amount of space is never "enough".
/// A blocked queue is never silent: `SyncStatus.storage_low` is set, one activity row is written
/// when the guard turns on, and the queue looks again after `guard_recheck_ms` (30 s) and on kick.
fn schedule(core: &Core, conn: &Arc<PeerConn>, peer: DeviceId, f: &mut Fetcher, done_tx: &mpsc::UnboundedSender<FetchDone>) {
    if lock(&core.inner.paused).is_some() || f.in_flight.len() >= MAX_PARALLEL_FETCH || !conn.is_open() {
        return;
    }
    let now = core.now();
    let t = Instant::now();
    f.last_schedule = t;
    let slots = MAX_PARALLEL_FETCH - f.in_flight.len();
    let mut gp = GuardPass::new(core);
    let picked = {
        let db = lock(&core.inner.db);
        pick_downloads(&db.conn, f, &core.inner.fetching, slots, now, t, &mut gp)
    };
    let picked = match picked {
        Ok(c) => c,
        Err(e) => {
            core.emit_error(e.kind(), &format!("could not list downloads: {e}"));
            return;
        }
    };
    if gp.phone {
        set_storage_low(core, gp.held_back.take());
    }
    for (id, hash, was_lost) in picked {
        f.in_flight.insert(id);
        // Remember when each picked Lost item was asked, so it is not asked in a loop.
        if was_lost {
            f.lost_tried.insert(id, t + LOST_RETRY);
        }
        let guard = HashGuard { core: core.clone(), hash };
        let (core, conn, tx) = (core.clone(), conn.clone(), done_tx.clone());
        tokio::spawn(async move {
            let outcome = fetch_item(&core, &conn, peer, id, hash).await;
            let _ = tx.send(FetchDone { id, outcome });
            drop(guard);
        });
    }
}

/// Bytes of the in-progress part file of `hash` (0 when there is none).
fn part_len(placement: &dyn crate::placement::Placement, hash: &BlobHash) -> u64 {
    placement.part_path(hash).ok().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0)
}

/// The record a download task works on, or None (Dropped) when the item is no longer Pending or
/// Lost, or its content is no longer the hash the scheduler claimed. The claim covers `claimed`
/// only: downloading another hash would write a `.part` no claim protects. Dropping frees the
/// claim and the item is picked again under its current hash.
fn load_for_fetch(core: &Core, id: ItemId, claimed: BlobHash) -> Result<Option<(ItemRecord, Settings)>> {
    let db = lock(&core.inner.db);
    match (crate::store::get_item(&db.conn, id), crate::store::load_settings(&db.conn)) {
        (Ok(Some((rec, LocalFileState::Pending | LocalFileState::Lost))), Ok(s)) if rec.content.val.hash == claimed => Ok(Some((rec, s))),
        (Ok(_), Ok(_)) => Ok(None),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}

/// Turn the storage-guard flag on or off. On the OFF -> ON edge: one activity row and one error
/// event (never one per 30 s pass); every change refreshes the status the UIs show.
fn set_storage_low(core: &Core, why: Option<String>) {
    use std::sync::atomic::Ordering::SeqCst;
    let low = why.is_some();
    let was = core.inner.storage_low.swap(low, SeqCst);
    if low && !was {
        let text = "PHONE STORAGE LOW - downloads wait";
        let detail = format!("{text} ({})", why.unwrap_or_default());
        match core.with_ctx(|c, ctx| crate::activity::log(c, ctx.now, None, None, ActivityAction::SyncError, &detail)) {
            Ok(_) => core.emit(Event::ActivityAdded),
            Err(e) => core.emit_error(e.kind(), &format!("could not log the storage guard: {e}")),
        }
        core.emit_error("storage_low", text);
    }
    if low != was {
        core.emit_status();
    }
}

fn retry(e: &NetError, conn: &PeerConn) -> FetchOutcome {
    FetchOutcome::Retry { kind: e.kind().to_string(), message: e.to_string(), connection_lost: !conn.is_open() }
}

/// S9 / S10: thumbnail first (best effort), then the file, then verify type, commit, receipt.
async fn fetch_item(core: &Core, conn: &Arc<PeerConn>, peer: DeviceId, id: ItemId, claimed: BlobHash) -> FetchOutcome {
    let inner = &core.inner;
    let loaded = load_for_fetch(core, id, claimed);
    let (rec, settings) = match loaded {
        Ok(Some(x)) => x,
        Ok(None) => return FetchOutcome::Dropped,
        Err(e) => return FetchOutcome::Retry { kind: e.kind().into(), message: e.to_string(), connection_lost: false },
    };
    let content = rec.content.val.clone();

    // Refused records transfer nothing: not the thumbnail, not the file, no part file. The record
    // itself was accepted (digests stay equal); this device just never holds the file.
    if names::is_refused_record(&rec.name.val, &content.ext) {
        return refuse(core, id, content.hash, "refused", names::REFUSED_TEXT.to_string());
    }
    if content.size > settings.max_blob_bytes {
        // Declared too large by the record: one final refusal, not a retry loop.
        return refuse(core, id, content.hash, "refused", ops::too_large_text(settings.max_blob_bytes));
    }

    // Thumbnails belong to photos and videos only; a File's thumbnail field is never fetched.
    if let Some(t) = content.thumb.as_ref().filter(|_| content.kind != MediaKind::File) {
        let dest = core.thumb_path(t.hash);
        if !dest.exists() {
            if let Err(msg) = fetch_thumb(conn, t, &dest).await {
                // The thumbnail is cosmetic: report it, still get the file.
                core.emit_error("thumbnail", &msg);
            }
        }
    }

    let part = match inner.placement.part_path(&content.hash) {
        Ok(p) => p,
        Err(e) => return FetchOutcome::Retry { kind: e.kind().into(), message: e.to_string(), connection_lost: false },
    };
    let hex = hex::encode(content.hash.0);
    match fetch_blob(conn, &hex, content.size, &part, settings.max_blob_bytes).await {
        Ok(got) => {
            inner.resumed_bytes.fetch_add(got.resumed_from, std::sync::atomic::Ordering::SeqCst);
        }
        Err(NetError::BlobRefused(reason)) if reason == "not found" => {
            // Soft: marked Lost and asked again later (module note on `blob_lost`). Reported
            // once, when the item first becomes Lost; a repeat answer is quiet.
            let r = core.with_ctx(|c, ctx| ops::blob_lost(c, ctx, id, content.hash));
            return match r {
                Ok(true) => FetchOutcome::Final { kind: "file_gone".into(), message: "FILE MISSING on the other device - will ask again".into() },
                Ok(false) => FetchOutcome::Dropped,
                Err(e) => FetchOutcome::Retry { kind: e.kind().into(), message: e.to_string(), connection_lost: false },
            };
        }
        Err(e) => return retry(&e, conn),
    }

    // The file is whole and its BLAKE3 matches. Type check on its first bytes, never trusting
    // the record's extension (a hostile peer could name an exe .jpg).
    let head = match read_head(&part).await {
        Ok(h) => h,
        Err(e) => return FetchOutcome::Retry { kind: e.kind().into(), message: e.to_string(), connection_lost: false },
    };
    // verify_received never re-derives kind or extension from the bytes: a photo must match its
    // bytes, a File must not carry a photo extension, a refused extension never passes.
    if let Err(e) = names::verify_received(content.kind, &content.ext, &head) {
        let _ = tokio::fs::remove_file(&part).await;
        let (kind, text) = match e {
            names::NameError::Refused(_) => ("refused", names::REFUSED_TEXT.to_string()),
            other => ("not_media", format!("REFUSED a file from the other device: {other}")),
        };
        return refuse(core, id, content.hash, kind, text);
    }

    let placed = core.with_ctx(|c, ctx| {
        let Some((now_rec, lfs)) = crate::store::get_item(c, id)? else { return Ok(false) };
        if !matches!(lfs, LocalFileState::Pending | LocalFileState::Lost) || now_rec.content.val.hash != content.hash {
            // Changed while downloading: this file is stale. The next round fetches the new one.
            return Ok(false);
        }
        let target = bin::file_target(c, &now_rec)?;
        let path = ctx.placement.commit(&part, &target)?;
        ops::blob_committed(c, ctx, id, content.hash, &path, peer)?;
        // It may have expired or been deleted while it downloaded: move it to the Bin now.
        bin::sweep_item(c, ctx.placement, ctx.self_id, id, ctx.now)?;
        Ok(true)
    });
    match placed {
        Ok(true) => FetchOutcome::Done,
        Ok(false) => {
            let _ = tokio::fs::remove_file(&part).await;
            FetchOutcome::Dropped
        }
        // The placement refused the name this device would give the file (an always-refused final
        // name, for example `desktop` + `ini`): final and visible, never an endless retry.
        Err(CoreError::Refused(text)) => {
            let _ = tokio::fs::remove_file(&part).await;
            refuse(core, id, content.hash, "refused", text)
        }
        Err(e) => FetchOutcome::Retry { kind: e.kind().into(), message: e.to_string(), connection_lost: false },
    }
}

/// Record the refusal (local state Refused, one activity row, the file never stored) and report it
/// once. `Dropped` when the item changed or was handled elsewhere meanwhile.
fn refuse(core: &Core, id: ItemId, hash: BlobHash, kind: &str, text: String) -> FetchOutcome {
    match core.with_ctx(|c, ctx| ops::blob_refused(c, ctx, id, hash, &text)) {
        Ok(false) => FetchOutcome::Dropped,
        Ok(true) => FetchOutcome::Final { kind: kind.into(), message: text },
        Err(e) => FetchOutcome::Retry { kind: e.kind().into(), message: e.to_string(), connection_lost: false },
    }
}

async fn read_head(path: &std::path::Path) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await.map_err(|e| CoreError::io(path, e))?;
    let mut buf = vec![0u8; names::SNIFF_LEN];
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]).await {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return Err(CoreError::io(path, e)),
        }
    }
    buf.truncate(n);
    Ok(buf)
}

/// S10: a thumbnail is a small JPEG fetched like a blob; no receipt.
async fn fetch_thumb(conn: &Arc<PeerConn>, t: &Thumb, dest: &std::path::Path) -> std::result::Result<(), String> {
    let part = dest.with_extension("jpg.part");
    fetch_blob(conn, &hex::encode(t.hash.0), t.size, &part, MAX_THUMB_BYTES).await.map_err(|e| format!("thumbnail download failed: {e}"))?;
    let head = read_head(&part).await.map_err(|e| e.to_string())?;
    if names::sniff(&head) != Some(names::Family::Jpeg) {
        let _ = tokio::fs::remove_file(&part).await;
        return Err("the other device sent a thumbnail that is not a JPEG".into());
    }
    tokio::fs::rename(&part, dest).await.map_err(|e| format!("could not store the thumbnail: {e}"))
}

// ---------- serving the peer's blob requests ----------

/// S11: blobs we hold (live or Bin files by content hash), and thumbnails by their hash.
struct CoreBlobSource {
    core: Core,
}

impl BlobSource for CoreBlobSource {
    fn open(&self, hash: &str) -> Option<PathBuf> {
        lock(&self.core.inner.blob_requests).push(hash.to_string());
        let raw: [u8; 32] = hex::decode(hash).ok()?.try_into().ok()?;
        let hash = BlobHash(raw);
        let from_items = {
            let db = lock(&self.core.inner.db);
            let ids = crate::store::items_holding(&db.conn, hash).ok()?;
            ids.into_iter().find_map(|id| crate::store::get_file_ref(&db.conn, id).ok().flatten().map(|r| PathBuf::from(r.path)).filter(|p| p.exists()))
        };
        from_items.or_else(|| Some(self.core.thumb_path(hash)).filter(|p| p.exists()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clock::TestClock, facade::CoreConfig, placement::AppStorage, store, store::test_support::*};
    use uuid::Uuid;

    fn open_core(dir: &std::path::Path) -> Core {
        let cfg = CoreConfig { data_dir: dir.join("data"), role: Role::Phone, self_id: f(), device_name: "phone".into(), app_version: "t".into() };
        let storage = AppStorage::new(dir.join("storage")).unwrap();
        Core::open(cfg, Arc::new(TestClock::new(T0)), Arc::new(storage)).unwrap().0
    }

    /// `n` Pending (or Lost) items, ids and hashes from 1..=n, captured oldest first.
    fn add_items(core: &Core, n: u32, lfs: LocalFileState) -> Vec<(ItemId, BlobHash)> {
        core.debug_conn(|c| {
            (1..=n)
                .map(|i| {
                    let mut it = item(1, p(), v(1, p()));
                    it.id = ItemId(Uuid::from_u128(0x10_000 + i as u128));
                    it.captured_at = T0 + i as i64;
                    let mut h = [0u8; 32];
                    h[..4].copy_from_slice(&i.to_le_bytes());
                    it.content.val.hash = BlobHash(h);
                    it.location.val.entered_at = T0;
                    store::insert_item(c, &it, lfs, T0).unwrap();
                    (it.id, it.content.val.hash)
                })
                .collect()
        })
    }

    fn pick(core: &Core, f: &Fetcher, slots: usize) -> Vec<(ItemId, BlobHash, bool)> {
        core.debug_conn(|c| pick_downloads(c, f, &core.inner.fetching, slots, T0, Instant::now(), &mut GuardPass::off(core)).unwrap())
    }

    // Review finding 7: the oldest 500 Pending items all in back-off must not hide item 501.
    #[tokio::test]
    async fn pending_items_behind_a_block_of_backed_off_ones_are_picked() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let items = add_items(&core, 600, LocalFileState::Pending);
        let mut f = Fetcher::default();
        let later = Instant::now() + Duration::from_secs(3600);
        for (id, _) in &items[..500] {
            f.backoff.insert(*id, (1, later));
        }
        let got = pick(&core, &f, 2);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![items[500].0, items[501].0]);
        assert!(got.iter().all(|g| !g.2), "Pending picks are not marked Lost");
        // Every pick claimed its hash core-wide.
        assert_eq!(lock(&core.inner.fetching).len(), 2);
    }

    // Same for Lost items: more than one page of them, the first page asked recently.
    #[tokio::test]
    async fn lost_items_beyond_the_first_page_are_retried() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let items = add_items(&core, 600, LocalFileState::Lost);
        let mut f = Fetcher::default();
        let later = Instant::now() + Duration::from_secs(3600);
        for (id, _) in &items[..500] {
            f.lost_tried.insert(*id, later);
        }
        let got = pick(&core, &f, 2);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![items[500].0, items[501].0]);
        assert!(got.iter().all(|g| g.2), "Lost picks are marked so they are not asked in a loop");
    }

    // Round 2, R13: a Lost item asked for earlier keeps its retry deadline. When it turns Pending
    // (restored from the Bin, new content) the deadline must not hold its download back.
    #[tokio::test]
    async fn a_pending_item_with_a_stale_lost_deadline_is_picked_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let items = add_items(&core, 2, LocalFileState::Pending);
        let mut f = Fetcher::default();
        f.lost_tried.insert(items[0].0, Instant::now() + Duration::from_secs(3600));
        let got = pick(&core, &f, 2);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![items[0].0, items[1].0]);
        assert!(got.iter().all(|g| !g.2), "Pending picks are not marked Lost");
    }

    // Round 2, R14: the task works on the hash the scheduler claimed. The pick hands out the claimed
    // hash; a record whose content changed (or that is no longer Pending/Lost) between the pick and
    // the load is dropped (the claim is freed, the item is picked again under its new hash), never
    // downloaded under the old claim.
    #[tokio::test]
    async fn a_record_changed_after_the_pick_is_not_downloaded_under_the_old_claim() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let items = add_items(&core, 1, LocalFileState::Pending);
        let id = items[0].0;
        let picked = pick(&core, &Fetcher::default(), 1);
        assert_eq!(picked.len(), 1);
        let (pid, claimed, _) = picked[0];
        assert_eq!((pid, claimed), items[0], "the pick hands out the hash it claimed");
        assert!(lock(&core.inner.fetching).contains_key(&claimed));
        assert!(load_for_fetch(&core, id, claimed).unwrap().is_some(), "the claimed hash loads");

        // The content changes after the pick: the old claim no longer matches.
        core.debug_conn(|c| {
            let (mut rec, _) = store::get_item(c, id).unwrap().unwrap();
            rec.content.val.hash.0[31] ^= 0xff;
            store::update_item(c, &rec).unwrap();
        });
        assert!(load_for_fetch(&core, id, claimed).unwrap().is_none(), "the old claim is dropped");
        let fresh = core.debug_conn(|c| store::get_item(c, id).unwrap().unwrap().0.content.val.hash);
        assert!(load_for_fetch(&core, id, fresh).unwrap().is_some(), "the item loads under its current hash");

        // No longer Pending or Lost (already present): nothing to download.
        core.debug_conn(|c| store::set_local_state(c, id, LocalFileState::Present, T0).unwrap());
        assert!(load_for_fetch(&core, id, fresh).unwrap().is_none(), "a present item is dropped");
    }

    // Review finding 11: a hash still held by a download task (of an earlier session) is not
    // picked again; when that task ends the hash is free and the scheduler is woken.
    #[tokio::test]
    async fn a_hash_held_by_a_running_download_is_skipped_until_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let items = add_items(&core, 2, LocalFileState::Pending);
        let (held_id, held_hash) = items[0];
        lock(&core.inner.fetching).insert(held_hash, 100);
        let guard = HashGuard { core: core.clone(), hash: held_hash };
        let f = Fetcher::default();
        let got = pick(&core, &f, 2);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![items[1].0], "the held item is skipped, the other is picked");
        drop(guard);
        assert!(!lock(&core.inner.fetching).contains_key(&held_hash), "freed when the task ends");
        tokio::time::timeout(Duration::from_millis(200), core.inner.kick.notified()).await.expect("the scheduler was woken");
        let got = pick(&core, &f, 2);
        assert!(got.iter().any(|g| g.0 == held_id), "now it is picked");
    }

    // Review finding 11 (verifier): an error in the middle of a pick must not leave the hashes it
    // already claimed held for good, with no task to free them.
    #[tokio::test]
    async fn an_error_mid_pick_frees_the_hashes_it_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let items = add_items(&core, 3, LocalFileState::Pending);
        // Item 3 cannot be read: its stored hash is not 32 bytes.
        core.debug_conn(|c| {
            c.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
            c.execute("UPDATE item SET blob_hash = x'00' WHERE id = ?1", [items[2].0 .0.as_bytes().as_slice()]).unwrap();
            c.execute_batch("PRAGMA ignore_check_constraints = OFF").unwrap();
        });
        let f = Fetcher::default();
        let r = core.debug_conn(|c| pick_downloads(c, &f, &core.inner.fetching, 3, T0, Instant::now(), &mut GuardPass::off(&core)));
        assert!(r.is_err(), "the unreadable item fails the pick");
        assert!(lock(&core.inner.fetching).is_empty(), "nothing stays claimed after the error");
    }

    /// A placement that reports a fixed amount of free space; everything else is the real one.
    struct LowSpace {
        inner: AppStorage,
        free: u64,
    }

    impl crate::placement::Placement for LowSpace {
        fn part_path(&self, hash: &BlobHash) -> Result<std::path::PathBuf> {
            self.inner.part_path(hash)
        }
        fn commit(&self, part: &std::path::Path, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.commit(part, t)
        }
        fn adopt(&self, src: &std::path::Path, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.adopt(src, t)
        }
        fn relocate(&self, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.relocate(t)
        }
        fn to_bin(&self, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.to_bin(t)
        }
        fn from_bin(&self, t: &crate::placement::FileTarget) -> Result<std::path::PathBuf> {
            self.inner.from_bin(t)
        }
        fn purge(&self, t: &crate::placement::FileTarget) -> Result<()> {
            self.inner.purge(t)
        }
        fn available_space(&self) -> Result<u64> {
            Ok(self.free)
        }
    }

    /// A phone-side guard pass that sees `free` bytes (the default guard is 2 GB).
    fn low_space_pass(dir: &std::path::Path, free: u64) -> GuardPass {
        let inner = AppStorage::new(dir.join("storage")).unwrap();
        GuardPass { phone: true, placement: Arc::new(LowSpace { inner, free }), held_back: None }
    }

    fn pick_with(core: &Core, f: &Fetcher, slots: usize, gp: &mut GuardPass) -> Vec<(ItemId, BlobHash, bool)> {
        core.debug_conn(|c| pick_downloads(c, f, &core.inner.fetching, slots, T0, Instant::now(), gp).unwrap())
    }

    // Merge of PR 1 paging and the 0.2.0 guard: items the guard holds back are not walked past for
    // ever. With the phone below its guard every row is held, so the scan of a state stops after one
    // page (0.2.0's window) instead of reading the whole backlog on every scheduling pass. Nothing is
    // picked, nothing is claimed, and the reason is reported (storage_low and the recheck use it).
    #[tokio::test]
    async fn guard_held_items_end_the_scan_after_one_page() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let items = add_items(&core, 600, LocalFileState::Pending);
        // max_blob_bytes = 100, every item is 100 bytes: none is oversize, so the guard holds all.
        core.debug_conn(|c| {
            store::save_settings(c, &crate::model::Settings { max_blob_bytes: 100, ..crate::model::Settings::default() }).unwrap();
        });
        let set_size = |id: ItemId, size: u64| {
            core.debug_conn(|c| {
                let (mut rec, _) = store::get_item(c, id).unwrap().unwrap();
                rec.content.val.size = size;
                store::update_item(c, &rec).unwrap();
            });
        };
        // The last item is over max_blob_bytes: the guard never holds it back, so it would be picked
        // if the scan walked the whole backlog. It sits beyond the window, so it is not reached.
        set_size(items[599].0, 101);
        let mut gp = low_space_pass(dir.path(), 0);
        let got = pick_with(&core, &Fetcher::default(), 2, &mut gp);
        assert!(got.is_empty(), "below the guard nothing in the window is picked");
        assert!(gp.held_back.is_some(), "the hold is reported");
        assert!(lock(&core.inner.fetching).is_empty(), "a held item claims nothing");
        // Control: an oversize item inside the window bypasses the guard (fetch_item refuses it once).
        set_size(items[3].0, 101);
        let mut gp = low_space_pass(dir.path(), 0);
        let got = pick_with(&core, &Fetcher::default(), 2, &mut gp);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![items[3].0]);
    }

    // The claim comes after the guard and the hash check, in that order. A guard-held item leaves no
    // hash in `fetching` (a leaked claim would keep it from downloading until the app restarts); a
    // hash a running task holds is skipped before the guard, so it never turns storage_low on; and
    // a pick spends the spare it needed, so a second item that no longer fits is held back.
    #[tokio::test]
    async fn guard_and_claim_order() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let guard = crate::model::Settings::default().storage_guard_bytes;
        let items = add_items(&core, 2, LocalFileState::Pending);

        // (a) Below the guard: nothing picked, nothing claimed, the hold is reported.
        let mut gp = low_space_pass(dir.path(), 0);
        assert!(pick_with(&core, &Fetcher::default(), 2, &mut gp).is_empty());
        assert!(gp.held_back.is_some());
        assert!(lock(&core.inner.fetching).is_empty(), "a guard-held item claims nothing");

        // (b) Room for exactly one item (100 bytes each) above the guard: one pick, the other is
        // held back, and only the picked hash is claimed.
        let mut gp = low_space_pass(dir.path(), guard + 150);
        let got = pick_with(&core, &Fetcher::default(), 2, &mut gp);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![items[0].0], "the spare is spent by the first pick");
        assert!(gp.held_back.is_some(), "the second item does not fit");
        assert_eq!(lock(&core.inner.fetching).len(), 1);
        lock(&core.inner.fetching).clear();

        // (c) The first item's hash is already held by a running task: it is skipped before the
        // guard, the second fits (the running task's 100 bytes are counted, so room for 250), and the guard
        // reports nothing.
        lock(&core.inner.fetching).insert(items[0].1, 100);
        let mut gp = low_space_pass(dir.path(), guard + 250);
        let got = pick_with(&core, &Fetcher::default(), 2, &mut gp);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![items[1].0]);
        assert!(gp.held_back.is_none(), "a duplicate held by a task never turns storage_low on");
    }

    // A download task outlives its session. Right after a reconnect the new session's `Fetcher` is
    // empty, so the guard must read what the old task still has to write from the core-wide claim,
    // not from the fetcher: else it over-estimates the free space by that task's remaining bytes.
    #[tokio::test]
    async fn guard_counts_the_bytes_an_earlier_sessions_task_still_has_to_write() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_core(dir.path());
        let guard = crate::model::Settings::default().storage_guard_bytes;
        let items = add_items(&core, 2, LocalFileState::Pending);
        // An old task holds item 0's hash and has 100 bytes to go; the new session's fetcher knows
        // nothing about it. Room above the guard: 150, so item 1 (100 bytes) fits only if the old
        // task's 100 are ignored.
        lock(&core.inner.fetching).insert(items[0].1, 100);
        let mut gp = low_space_pass(dir.path(), guard + 150);
        let got = pick_with(&core, &Fetcher::default(), 2, &mut gp);
        assert!(got.is_empty(), "the old task's remaining bytes use the space: {got:?}");
        assert!(gp.held_back.is_some(), "the hold is reported");
        // Once that task has written 60 bytes of its part file, only 40 are still to come.
        let part = core.inner.placement.part_path(&items[0].1).unwrap();
        std::fs::write(&part, [0u8; 60]).unwrap();
        let mut gp = low_space_pass(dir.path(), guard + 150);
        let got = pick_with(&core, &Fetcher::default(), 2, &mut gp);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![items[1].0], "bytes already written no longer count");
    }

    // Review finding 9: the unpair-notice task belongs to the network instance. Stopping the network
    // ends it at once, so a restarted network can start its own (a task that outlived the stop kept
    // the shut-down node, and its running flag kept the new task from ever starting).
    #[tokio::test]
    async fn stopping_the_network_ends_the_notice_task() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = tempfile::tempdir().unwrap();
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).unwrap();
        let cfg = CoreConfig {
            data_dir: dir.path().join("data"),
            role: Role::Phone,
            self_id: crate::facade::device_id_from_secret(&secret),
            device_name: "phone".into(),
            app_version: "t".into(),
        };
        let storage = AppStorage::new(dir.path().join("storage")).unwrap();
        let core = Core::open(cfg, Arc::new(TestClock::new(T0)), Arc::new(storage)).unwrap().0;
        core.start_network(NetworkConfig { bind: "127.0.0.1:0".parse().unwrap(), mdns: false, secret_key: secret }).await.unwrap();
        let mut other = [0u8; 32];
        getrandom::fill(&mut other).unwrap();
        // An old PC that nobody answers for: the task dials (5 s) and backs off.
        let pending = crate::store::PendingUnpair { peer: hex::encode(crate::facade::device_id_from_secret(&other).0), addresses: vec!["127.0.0.1:9".into()] };
        let node = lock(&core.inner.net).as_ref().unwrap().node.clone();
        let before = lock(&core.inner.net).as_ref().unwrap().tasks.len();
        spawn_notice(&core, &node, pending);
        assert!(core.inner.notice_running.load(SeqCst), "the task is running");
        assert_eq!(lock(&core.inner.net).as_ref().unwrap().tasks.len(), before + 1, "the task is registered with this network instance, so stop aborts it");
        tokio::time::sleep(Duration::from_millis(300)).await;
        core.stop_network().await;
        assert!(!core.inner.notice_running.load(SeqCst), "stopping the network ended the task and freed the flag");
    }

    // Review finding 6: only a fully acknowledged frame counts as delivered.
    #[test]
    fn only_an_acknowledged_frame_is_delivered() {
        assert!(confirmed(StreamEnd::Acked).is_ok());
        assert!(confirmed(StreamEnd::Stopped(0)).is_err(), "STOP_SENDING is not delivery");
        assert!(matches!(confirmed(StreamEnd::TimedOut), Err(NetError::Timeout(_))));
        assert!(confirmed(StreamEnd::Failed(NetError::Stream("x".into()))).is_err());
        // The unpair notice also counts as told when the other side closed because it no longer pairs us.
        assert!(unpair_delivered(&Ok(())));
        assert!(unpair_delivered(&Err(NetError::ClosedByPeer { code: close::UNPAIRED, reason: String::new() })));
        assert!(unpair_delivered(&Err(NetError::ClosedByPeer { code: close::NOT_PAIRED, reason: String::new() })));
        assert!(!unpair_delivered(&Err(NetError::ClosedByPeer { code: close::NORMAL, reason: String::new() })));
        assert!(!unpair_delivered(&Err(NetError::Timeout("t".into()))));
        assert!(!unpair_delivered(&Err(NetError::Stream("s".into()))));
    }
}
