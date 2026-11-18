//! Live path guard.
//!
//! Why: iroh 1.4.1 always negotiates QUIC NAT traversal (`max_remote_nat_traversal_addresses`
//! ignores values below 8, spike/RESULTS.md correction 1), so a paired peer can advertise
//! public candidate addresses and the endpoint may open a path to one. PLAN M0 lesson:
//! "M1 must drop or close any path whose remote address is not on the local network".
//!
//! API used (iroh 1.4.1, checked in the registry source):
//! - `Connection::paths()` -> `PathList` snapshot; `Path::remote_addr() -> &TransportAddr`.
//! - `Connection::path_events()` -> `PathEventStream` ('static, may move into a task) yielding
//!   `PathEvent::{Opened{remote_addr,..}, Closed{..}, Selected{remote_addr,..}, Lagged{..}}`.
//! - `Connection::weak_handle()` -> `WeakConnectionHandle::upgrade()`; the guard must hold
//!   only the weak handle so it never keeps a connection alive (iroh docs: a strong clone
//!   disables close-on-drop).
//!
//! There is no public API to veto a single path (PathSelector is behind
//! `unstable-custom-transports`, which we do not enable), so the enforcement is: close the
//! whole connection with `close::PATH_NOT_LOCAL`.
//!
//! Residual (documented, not hidden): QUIC probe packets to a candidate address can leave
//! before the `Opened` event; the guard bounds what is carried (no application data on a
//! non-local path for longer than one event-loop turn), it cannot stop the probe itself.
//! The before-connect hook in `node` and the dial filter stop every non-local address we
//! would dial ourselves. A real non-local path cannot be produced in a test, so the tests
//! drive `enforce_path` directly; a packet capture on the real LAN is the proof (M5).

use std::sync::Arc;

use iroh::{
    endpoint::{Connection, PathEvent, VarInt, WeakConnectionHandle},
    TransportAddr,
};
use n0_future::StreamExt;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::{close, local::transport_addr_allowed, NetError};

/// Called once with the offending address text when the guard closes a connection.
pub type OnViolation = Arc<dyn Fn(String) + Send + Sync>;

/// First address that `local::transport_addr_allowed` rejects, if any. Pure; unit tested.
pub fn first_non_local<'a>(addrs: impl IntoIterator<Item = &'a TransportAddr>) -> Option<TransportAddr> {
    addrs.into_iter().find(|a| !transport_addr_allowed(a)).cloned()
}

/// Text of a path address for messages: the bare socket address for IP paths.
fn addr_text(a: &TransportAddr) -> String {
    match a {
        TransportAddr::Ip(s) => s.to_string(),
        other => other.to_string(),
    }
}

/// Closes `conn` with `PATH_NOT_LOCAL` if `addr` is not local. Returns true when it closed.
/// The one place the close happens: the event loop and the snapshot check both end up here.
pub fn enforce_path(conn: &Connection, addr: &TransportAddr, on_violation: &OnViolation) -> bool {
    if transport_addr_allowed(addr) {
        return false;
    }
    let text = addr_text(addr);
    warn!(addr = %text, peer = %conn.remote_id().fmt_short(), "path is not on the local network, closing the connection");
    conn.close(VarInt::from_u32(close::PATH_NOT_LOCAL), format!("path not local: {text}").as_bytes());
    on_violation(text);
    true
}

/// Snapshot check: `Err(NetError::NotLocal(addr))` and the connection closed with
/// `close::PATH_NOT_LOCAL` if any currently open path is not local. Called right after
/// connect/accept, before any stream is opened.
pub fn check_paths(conn: &Connection) -> Result<(), NetError> {
    let bad = {
        let paths = conn.paths();
        first_non_local(paths.iter().map(|p| p.remote_addr().clone()).collect::<Vec<_>>().iter())
    };
    match bad {
        None => Ok(()),
        Some(addr) => {
            enforce_path(conn, &addr, &(Arc::new(|_| {}) as OnViolation));
            Err(NetError::NotLocal(addr_text(&addr)))
        }
    }
}

/// Spawns a task that watches `conn.path_events()` until the connection closes.
/// On `Opened` or `Selected` with a non-local remote address: close the connection with
/// `close::PATH_NOT_LOCAL` (reason "path not local: <addr>") and call `on_violation` once.
/// On `Lagged`: re-check the full `paths()` snapshot (events were dropped).
/// Holds only a `WeakConnectionHandle`; exits when `upgrade()` returns None or the stream ends.
///
/// The event stream is subscribed before this function returns, so a path that opens between
/// here and the caller's `check_paths` is seen by one of the two.
pub fn spawn_path_guard(conn: &Connection, on_violation: OnViolation) -> JoinHandle<()> {
    let mut events = conn.path_events();
    let weak: WeakConnectionHandle = conn.weak_handle();
    tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            let Some(conn) = weak.upgrade() else { return };
            let violated = match &ev {
                PathEvent::Opened { remote_addr, .. } | PathEvent::Selected { remote_addr, .. } => {
                    enforce_path(&conn, remote_addr, &on_violation)
                }
                PathEvent::Lagged { .. } => {
                    // Events were dropped: look at everything that is open now.
                    let addrs: Vec<TransportAddr> = conn.paths().iter().map(|p| p.remote_addr().clone()).collect();
                    match first_non_local(addrs.iter()) {
                        Some(bad) => enforce_path(&conn, &bad, &on_violation),
                        None => false,
                    }
                }
                // Closed paths and any future event kinds carry no new remote address.
                _ => false,
            };
            if violated {
                return;
            }
        }
    })
}
