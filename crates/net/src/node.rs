//! The LAN-only iroh endpoint and connections.
//!
//! Endpoint configuration:
//! - `Endpoint::builder(presets::Minimal)`, `.relay_mode(RelayMode::Disabled)`,
//!   `.clear_address_lookup()` (no n0 DNS/pkarr), `.clear_ip_transports()` then
//!   `.bind_addr(cfg.bind)` (one UDP socket; the desktop passes 127.0.0.1 when env
//!   TUGBOAT_BIND=127.0.0.1 is set, tests always bind 127.0.0.1:0).
//! - portmapper is not compiled (iroh default-features = false in the workspace).
//! - `.addr_filter(local::local_addr_filter())`.
//! - mDNS when `cfg.mdns`: `LocalOnly(MdnsAddressLookup::builder().advertise(cfg.role == Role::Pc)
//!   .addr_filter(local_addr_filter()))`. Only the PC advertises; the phone
//!   only resolves. Tests on loopback pass `mdns: false`.
//! - `.hooks(GateHooks { .. })` (see `GateHooks`).
//! - ALPN list: `[ALPN_SYNC]`, plus `ALPN_PAIR` only while `set_pairing_open(true)`
//!   (`Endpoint::set_alpns`, affects new incoming connections only).
//! - `.transport_config(..)`: keep-alive every 10 s and max idle timeout 30 s, so a dead
//!   peer (Wi-Fi gone, phone killed) closes the connection and the UI shows Offline within
//!   30 to 40 s (measured 35 s: the idle timer restarts on the first keep-alive after a
//!   received packet, so the bound is keep-alive + idle) instead of a frozen "Synced".
//!   API (checked in
//!   the 1.4.1 source): `QuicTransportConfig::builder().keep_alive_interval(Duration)
//!   .max_idle_timeout(Some(IdleTimeout))`.

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use iroh::{
    endpoint::{
        presets, AfterHandshakeOutcome, BeforeConnectOutcome, Connection, EndpointHooks, IdleTimeout, Incoming,
        QuicTransportConfig, RecvStream, SendStream, VarInt,
    },
    Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey,
};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info, warn};

use crate::{
    close,
    error::{chain, map_conn_err, stream_err},
    guard::{self, OnViolation},
    local::{is_local_addr, is_local_ip, local_addr_filter, transport_addr_allowed, LocalOnly},
    pairing::MAX_QR_ADDRS,
    NetError, ALPN_PAIR, ALPN_SYNC,
};

/// Dead peers must show Offline within KEEP_ALIVE + MAX_IDLE, so a stopped sync is never shown as synced.
const KEEP_ALIVE: Duration = Duration::from_secs(10);
const MAX_IDLE: Duration = Duration::from_secs(30);
/// An incoming connection must finish its handshake within this, or it is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A stream must announce its kind within this after it opens.
const KIND_TIMEOUT: Duration = Duration::from_secs(10);
/// Finished handshakes waiting for `Node::accept`.
const ACCEPT_QUEUE: usize = 32;

/// Which side of the pair this node is. Decides mDNS advertising and who dials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Accepts connections, advertises over mDNS, opens pairing windows.
    Pc,
    /// Dials the PC (cached addresses first, then mDNS), never advertises.
    Phone,
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// UDP bind address. PC default 0.0.0.0:<udp_port setting>; phone 0.0.0.0:0;
    /// QA and tests 127.0.0.1.
    pub bind: SocketAddr,
    pub role: Role,
    /// Enable mDNS address lookup (needs a real LAN interface).
    pub mdns: bool,
    /// Ed25519 secret key bytes of this device (from Credential Manager / Android Keystore).
    pub secret_key: [u8; 32],
}

/// Decides which remote keys may use the sync ALPN. Implemented by tugboat-core
/// (paired = device row with paired_at set and unpaired_at null). Must not block.
///
/// `Err` means the answer could not be found (a database fault), which is not the same as "not
/// paired": the connection is closed without `NOT_PAIRED`, because the other device answers that
/// code by deleting its pairing.
pub trait AccessGate: Send + Sync + 'static {
    fn is_paired(&self, peer: &[u8; 32]) -> Result<bool, String>;
}

/// Endpoint hooks that enforce "an unknown key gets nothing else".
/// - before_connect: `Reject` if the EndpointAddr has any relay URL or any IP address that
///   `local::is_local_addr` rejects (second line behind `Node::dial`'s own check).
/// - after_handshake, ALPN_SYNC: `Reject{close::NOT_PAIRED, "not paired"}` unless
///   `gate.is_paired(remote_id)` is `Ok(true)`; a gate `Err` rejects with `close::NORMAL`. Applies to both sides (the phone also refuses a PC it
///   no longer pairs with).
/// - after_handshake, ALPN_PAIR on an incoming (server side) connection:
///   `Reject{close::PAIRING_CLOSED}` unless `pairing_open` is true (belt and braces with the
///   ALPN list). Outgoing pairing dials (phone) are accepted: iroh already verified the PC key
///   from the QR.
/// - any other ALPN: `Reject{close::PROTOCOL}`.
#[derive(Debug)]
pub struct GateHooks {
    pub gate: GateRef,
    pub pairing_open: Arc<AtomicBool>,
}

/// Debug-printable handle to the gate (EndpointHooks requires Debug).
#[derive(Clone)]
pub struct GateRef(pub Arc<dyn AccessGate>);

impl std::fmt::Debug for GateRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GateRef")
    }
}

impl EndpointHooks for GateHooks {
    async fn before_connect<'a>(&'a self, remote_addr: &'a EndpointAddr, alpn: &'a [u8]) -> BeforeConnectOutcome {
        match remote_addr.addrs.iter().find(|a| !transport_addr_allowed(a)) {
            None => BeforeConnectOutcome::Accept,
            Some(bad) => {
                warn!(addr = %bad, alpn = %String::from_utf8_lossy(alpn), "refusing to connect to a non-local address");
                BeforeConnectOutcome::Reject
            }
        }
    }

    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let reject = |code: u32, reason: &str| AfterHandshakeOutcome::Reject {
            error_code: VarInt::from_u32(code),
            reason: reason.as_bytes().to_vec(),
        };
        let alpn = conn.alpn();
        if alpn == ALPN_SYNC {
            match self.gate.0.is_paired(conn.remote_id().as_bytes()) {
                Ok(true) => AfterHandshakeOutcome::Accept,
                Ok(false) => {
                    debug!(peer = %conn.remote_id().fmt_short(), "sync refused: key not paired");
                    reject(close::NOT_PAIRED, "not paired")
                }
                Err(e) => {
                    warn!(peer = %conn.remote_id().fmt_short(), "sync refused: could not check pairing: {e}");
                    reject(close::NORMAL, "could not check pairing")
                }
            }
        } else if alpn == ALPN_PAIR {
            if conn.side().is_client() || self.pairing_open.load(Ordering::SeqCst) {
                AfterHandshakeOutcome::Accept
            } else {
                reject(close::PAIRING_CLOSED, "pairing is not open")
            }
        } else {
            reject(close::PROTOCOL, "unknown protocol")
        }
    }
}

/// Kind byte written first on every stream either side opens, so the acceptor can route it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamKind {
    /// The one long-lived control stream per sync connection (opened by the dialer).
    Control = 1,
    /// One blob download per stream (opened by the receiver, see `blob`).
    Blob = 2,
    /// The pairing exchange (opened by the phone on an ALPN_PAIR connection).
    Pair = 3,
}

impl StreamKind {
    pub fn from_byte(b: u8) -> Option<StreamKind> {
        match b {
            1 => Some(StreamKind::Control),
            2 => Some(StreamKind::Blob),
            3 => Some(StreamKind::Pair),
            _ => None,
        }
    }
}

/// Only the PC advertises over mDNS; the phone resolves only.
pub fn mdns_advertise(role: Role) -> bool {
    role == Role::Pc
}

/// Where the owner of the node (core) installs its reaction to a live path violation. Shared by
/// every connection of the node; empty until `Node::set_violation_handler`.
type ViolationSlot = Arc<std::sync::Mutex<Option<OnViolation>>>;

/// Lets a closure field sit in a `#[derive(Debug)]` struct.
#[derive(Clone)]
struct Quiet<T>(T);

impl<T> std::fmt::Debug for Quiet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("..")
    }
}

/// The handler every `PeerConn` of a node holds: forwards to whatever the slot holds now.
fn dispatch_violation(slot: &ViolationSlot) -> OnViolation {
    let slot = slot.clone();
    Arc::new(move |addr: String| {
        let handler = slot.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match handler {
            Some(h) => h(addr),
            // Nobody listens yet: the connection is closed regardless, say so in the log.
            None => warn!(addr, "path violation with no handler installed"),
        }
    })
}

type AcceptRx = tokio::sync::Mutex<mpsc::Receiver<Result<Accepted, NetError>>>;

/// Owns the accept task; dropping the last `Node` clone stops it (the task holds an endpoint
/// clone, so without this the endpoint would outlive every handle).
#[derive(Debug)]
struct Shared {
    incoming: AcceptRx,
    acceptor: JoinHandle<()>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.acceptor.abort();
    }
}

/// A running endpoint. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Node {
    pub endpoint: Endpoint,
    pub role: Role,
    pairing_open: Arc<AtomicBool>,
    mdns: bool,
    shared: Arc<Shared>,
    violation: Quiet<ViolationSlot>,
}

/// An authenticated, path-guarded connection to a peer.
#[derive(Debug)]
pub struct PeerConn {
    pub conn: Connection,
    guard: JoinHandle<()>,
    on_violation: Quiet<OnViolation>,
}

/// An accepted incoming connection, already through the hooks and `guard::check_paths`.
#[derive(Debug)]
pub struct Accepted {
    pub peer: PeerConn,
    /// ALPN_SYNC or ALPN_PAIR.
    pub alpn: Vec<u8>,
}

/// Finish an incoming handshake (hooks run inside) and guard its paths.
async fn complete_incoming(incoming: Incoming, on_violation: OnViolation) -> Result<Accepted, NetError> {
    let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming)
        .await
        .map_err(|_| NetError::Timeout("incoming handshake did not finish".into()))?
        .map_err(|e| NetError::Connect(format!("incoming connection not accepted: {}", chain(&e))))?;
    let alpn = conn.alpn().to_vec();
    let peer = PeerConn::wrap(conn, on_violation)?;
    Ok(Accepted { peer, alpn })
}

impl Node {
    /// Builds and binds the endpoint as described in the module docs.
    /// Errors: `NetError::Endpoint` with the bind address and the iroh error chain.
    pub async fn start(cfg: NodeConfig, gate: Arc<dyn AccessGate>) -> Result<Node, NetError> {
        let pairing_open = Arc::new(AtomicBool::new(false));
        let idle = IdleTimeout::try_from(MAX_IDLE).map_err(|e| NetError::Endpoint(format!("idle timeout: {e}")))?;
        let transport = QuicTransportConfig::builder().keep_alive_interval(KEEP_ALIVE).max_idle_timeout(Some(idle)).build();
        let mut b = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from(cfg.secret_key))
            // No relay servers: nothing may be reached on the internet.
            .relay_mode(RelayMode::Disabled)
            // No n0 DNS / pkarr address lookup (Minimal adds none; cleared explicitly anyway).
            .clear_address_lookup()
            // Only the one configured UDP socket.
            .clear_ip_transports()
            .bind_addr(cfg.bind)
            .map_err(|e| NetError::Endpoint(format!("bad bind address {}: {e}", cfg.bind)))?
            .alpns(vec![ALPN_SYNC.to_vec()])
            .addr_filter(local_addr_filter())
            .transport_config(transport)
            .hooks(GateHooks { gate: GateRef(gate), pairing_open: pairing_open.clone() });
        if cfg.mdns {
            // Wrapped so a spoofed mDNS answer cannot point a dial at a non-local address.
            b = b.address_lookup(LocalOnly(
                MdnsAddressLookup::builder().advertise(mdns_advertise(cfg.role)).addr_filter(local_addr_filter()),
            ));
        }
        let endpoint = b.bind().await.map_err(|e| NetError::Endpoint(format!("bind {}: {}", cfg.bind, chain(&e))))?;
        info!(id = %endpoint.id().fmt_short(), sockets = ?endpoint.bound_sockets(), mdns = cfg.mdns, role = ?cfg.role,
            "endpoint up (relay off, n0 lookup off)");

        let (tx, rx) = mpsc::channel(ACCEPT_QUEUE);
        let ep = endpoint.clone();
        let violation: ViolationSlot = Arc::default();
        let on_violation = dispatch_violation(&violation);
        let accept_violation = on_violation.clone();
        let acceptor = tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let tx = tx.clone();
                let on_violation = accept_violation.clone();
                // One task per handshake: a peer that stalls its handshake blocks nobody else.
                tokio::spawn(async move {
                    match complete_incoming(incoming, on_violation).await {
                        Ok(a) => {
                            let _ = tx.send(Ok(a)).await;
                        }
                        // A refusal nobody is waiting for is dropped: it is already logged here.
                        Err(e) => {
                            debug!("incoming connection ended: {e}");
                            let _ = tx.try_send(Err(e));
                        }
                    }
                });
            }
        });
        Ok(Node {
            endpoint,
            role: cfg.role,
            pairing_open,
            mdns: cfg.mdns,
            shared: Arc::new(Shared { incoming: tokio::sync::Mutex::new(rx), acceptor }),
            violation: Quiet(violation),
        })
    }

    /// Installs what happens when the live path guard closes a connection because a path left
    /// the local network (core turns it into a visible error and an activity row; errors
    /// are surfaced, never swallowed). Takes effect for every connection, old and new. A
    /// violation found while a connection is being set up is returned as `NetError::NotLocal`
    /// by `dial` / `accept` instead.
    pub fn set_violation_handler(&self, handler: OnViolation) {
        *self.violation.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(handler);
    }

    /// This device's public key.
    pub fn id(&self) -> [u8; 32] {
        *self.endpoint.id().as_bytes()
    }

    /// Sockets actually bound (port 0 resolved).
    pub fn bound_addrs(&self) -> Vec<SocketAddr> {
        self.endpoint.bound_sockets()
    }

    /// Addresses to put in the QR payload: the endpoint's direct IP addresses that are local,
    /// excluding loopback and link-local, IPv4 first, 192.168.x.x first among them, at most
    /// `pairing::MAX_QR_ADDRS`, all with the bound port. When bound to 127.0.0.1 (QA), returns
    /// the loopback address so the fake phone on the same machine can dial it.
    /// Empty when the machine has no LAN address: the caller must show that, not a blank QR.
    pub fn qr_addrs(&self) -> Vec<SocketAddr> {
        let direct: Vec<SocketAddr> = self.endpoint.addr().ip_addrs().copied().collect();
        select_qr_addrs(&direct, &self.bound_addrs())
    }

    /// Adds or removes ALPN_PAIR from the accepted ALPN list (`Endpoint::set_alpns`) and sets
    /// the flag the after-handshake hook reads. Called by core's pairing window open/close.
    pub fn set_pairing_open(&self, open: bool) {
        // The flag moves first: the hook is the stricter of the two checks, so a connection
        // racing the ALPN change on close is still refused.
        self.pairing_open.store(open, Ordering::SeqCst);
        let mut alpns = vec![ALPN_SYNC.to_vec()];
        if open {
            alpns.push(ALPN_PAIR.to_vec());
        }
        self.endpoint.set_alpns(alpns);
    }

    /// Dial `peer` on `alpn`. Rules:
    /// 1. Any address in `addrs` that is not local -> `Err(NotLocal)` at once, nothing sent
    ///    (see `n3_dial_rejects_non_local_address_without_trying`).
    /// 2. With addresses: one connect with all of them in the EndpointAddr (iroh races them),
    ///    bounded by `timeout`.
    /// 3. If that fails (or `addrs` is empty) and mDNS is enabled: connect by id alone (mDNS
    ///    lookup through `LocalOnly`), bounded by `timeout`. The returned error names both
    ///    attempts.
    /// 4. On success: the path guard is subscribed, then `guard::check_paths` runs.
    ///
    /// A peer that refuses us after the handshake (not paired, pairing closed) shows up on the
    /// returned connection as `closed()` or the next stream call, not here: QUIC finishes the
    /// client handshake before the server's hook runs.
    pub async fn dial(&self, peer: [u8; 32], addrs: &[SocketAddr], alpn: &[u8], timeout: Duration) -> Result<PeerConn, NetError> {
        if let Some(bad) = addrs.iter().find(|a| !is_local_addr(**a)) {
            return Err(NetError::NotLocal(bad.to_string()));
        }
        let on_violation = dispatch_violation(&self.violation.0);
        let id = EndpointId::from_bytes(&peer).map_err(|e| NetError::Connect(format!("not a valid peer key: {e}")))?;
        let mut tried: Vec<String> = Vec::new();
        if !addrs.is_empty() {
            let ea = addrs.iter().fold(EndpointAddr::new(id), |ea, a| ea.with_ip_addr(*a));
            match self.connect_once(ea, alpn, timeout, id).await {
                Ok(conn) => return PeerConn::wrap(conn, on_violation),
                Err(e) => tried.push(format!("cached addresses {addrs:?}: {e}")),
            }
        }
        // By id alone only helps when mDNS can resolve it; with no addresses and no mDNS it
        // fails at once with iroh's own "no addressing information" text.
        if self.mdns || addrs.is_empty() {
            match self.connect_once(EndpointAddr::new(id), alpn, timeout, id).await {
                Ok(conn) => return PeerConn::wrap(conn, on_violation),
                Err(e) => tried.push(format!("by key ({}): {e}", if self.mdns { "mDNS" } else { "no address, mDNS off" })),
            }
        }
        let text = tried.join("; ");
        Err(if tried.iter().all(|t| t.contains("no connection to")) { NetError::Timeout(text) } else { NetError::Connect(text) })
    }

    async fn connect_once(&self, ea: EndpointAddr, alpn: &[u8], timeout: Duration, id: EndpointId) -> Result<Connection, NetError> {
        match tokio::time::timeout(timeout, self.endpoint.connect(ea, alpn)).await {
            Err(_) => Err(NetError::Timeout(format!("no connection to {} within {}s", id.fmt_short(), timeout.as_secs()))),
            Ok(Err(e)) => Err(NetError::Connect(chain(&e))),
            Ok(Ok(c)) => Ok(c),
        }
    }

    /// Next incoming connection that passed the hooks. `None` when the endpoint is closed.
    /// A connection that fails its handshake or `check_paths` yields `Some(Err(..))` so the
    /// caller can log it; the accept loop continues.
    pub async fn accept(&self) -> Option<Result<Accepted, NetError>> {
        self.shared.incoming.lock().await.recv().await
    }

    /// Closes the endpoint (all connections get `close::NORMAL`).
    pub async fn shutdown(&self) {
        self.endpoint.close().await;
    }
}

/// Picks the QR addresses from the endpoint's direct addresses (pure, so it is unit tested).
/// Bound only to loopback (QA, tests): the bound loopback addresses. Otherwise local,
/// non-loopback, non-link-local addresses; IPv4 before IPv6, 192.168/16, then 10/8, then
/// 172.16/12; all with the first bound port; deduplicated; at most `MAX_QR_ADDRS`.
pub fn select_qr_addrs(direct: &[SocketAddr], bound: &[SocketAddr]) -> Vec<SocketAddr> {
    if !bound.is_empty() && bound.iter().all(|a| a.ip().is_loopback()) {
        return bound.to_vec();
    }
    let Some(port) = bound.first().map(|a| a.port()) else { return Vec::new() };
    let mut ips: Vec<IpAddr> = Vec::new();
    for ip in direct.iter().map(|a| a.ip()) {
        if is_local_ip(ip) && !ip.is_loopback() && !is_link_local(&ip) && !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    ips.sort_by_key(|ip| match ip {
        IpAddr::V4(v) if v.octets()[0] == 192 => (0, 0),
        IpAddr::V4(v) if v.octets()[0] == 10 => (0, 1),
        IpAddr::V4(_) => (0, 2),
        IpAddr::V6(_) => (1, 0),
    });
    ips.into_iter().take(MAX_QR_ADDRS).map(|ip| SocketAddr::new(ip, port)).collect()
}

fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_link_local(),
        IpAddr::V6(v) => v.is_unicast_link_local(),
    }
}

impl PeerConn {
    /// Subscribes the path guard, then checks the paths that already exist. On a violation the
    /// connection is already closed with `PATH_NOT_LOCAL` and the error says which address.
    pub fn wrap(conn: Connection, on_violation: OnViolation) -> Result<PeerConn, NetError> {
        let guard = guard::spawn_path_guard(&conn, on_violation.clone());
        let peer = PeerConn { conn, guard, on_violation: Quiet(on_violation) };
        guard::check_paths(&peer.conn)?;
        Ok(peer)
    }

    /// Runs the guard's rule for `addr` as the live path watcher would: a non-local address
    /// closes the connection (`PATH_NOT_LOCAL`) and calls the node's violation handler.
    /// Returns true when it closed. Lets tests (a real non-local path cannot be produced on
    /// loopback) drive the whole chain, including what the owner of the node does about it.
    pub fn enforce_path(&self, addr: &iroh::TransportAddr) -> bool {
        guard::enforce_path(&self.conn, addr, &self.on_violation.0)
    }

    /// Remote public key.
    pub fn peer(&self) -> [u8; 32] {
        *self.conn.remote_id().as_bytes()
    }

    /// True until a close reason exists.
    pub fn is_open(&self) -> bool {
        self.conn.close_reason().is_none()
    }

    /// Opens a bi stream and writes the kind byte.
    pub async fn open_stream(&self, kind: StreamKind) -> Result<(SendStream, RecvStream), NetError> {
        let (mut tx, rx) = self.conn.open_bi().await.map_err(|e| map_conn_err(&e))?;
        tx.write_all(&[kind as u8]).await.map_err(|e| stream_err("write stream kind", &e))?;
        Ok((tx, rx))
    }

    /// Accepts the next bi stream and reads its kind byte. Unknown kind -> close the
    /// connection with `close::PROTOCOL` and return `NetError::Protocol`.
    pub async fn accept_stream(&self) -> Result<(StreamKind, SendStream, RecvStream), NetError> {
        let (tx, mut rx) = self.conn.accept_bi().await.map_err(|e| map_conn_err(&e))?;
        let mut b = [0u8; 1];
        match tokio::time::timeout(KIND_TIMEOUT, rx.read_exact(&mut b)).await {
            Err(_) => {
                self.close(close::PROTOCOL, "stream kind not sent");
                return Err(NetError::Protocol("peer opened a stream and never sent its kind".into()));
            }
            Ok(Err(e)) => return Err(stream_err("read stream kind", &e)),
            Ok(Ok(())) => {}
        }
        match StreamKind::from_byte(b[0]) {
            Some(k) => Ok((k, tx, rx)),
            None => {
                self.close(close::PROTOCOL, "unknown stream kind");
                Err(NetError::Protocol(format!("unknown stream kind {}", b[0])))
            }
        }
    }

    /// Close with an application code from `crate::close` and a short reason.
    pub fn close(&self, code: u32, reason: &str) {
        self.conn.close(VarInt::from_u32(code), reason.as_bytes());
    }

    /// Waits for the close and maps it: application close from the peer ->
    /// `ClosedByPeer{code, reason}`; local close, timeout or reset -> `Stream`/`Timeout`.
    pub async fn closed(&self) -> NetError {
        map_conn_err(&self.conn.closed().await)
    }
}

impl Drop for PeerConn {
    fn drop(&mut self) {
        self.guard.abort();
    }
}
