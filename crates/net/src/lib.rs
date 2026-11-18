//! tugboat-net: the only crate that uses iroh.
//!
//! What lives here (contract in notes/core-design.md, sections "Net" and "Wire"):
//! - `node`: the LAN-only iroh endpoint (relays off, address lookup cleared, portmapper not
//!   compiled, mDNS from `iroh-mdns-address-lookup`, fixed UDP port, configurable bind),
//!   ALPN gating through endpoint hooks, dialing (cached addresses first, then mDNS).
//! - `local`: the local-address filter applied to every dial, to mDNS results and to live paths.
//! - `guard`: closes a connection the moment any of its paths has a non-local remote address.
//!   iroh 1.4.1 always negotiates QUIC NAT traversal and its public API cannot turn it off.
//! - `frame`: length-prefixed JSON control messages on a QUIC stream.
//! - `blob`: resumable, BLAKE3-verified blob download (receiver pulls from an offset).
//! - `pairing`: QR payload, 6-digit code derivation, the PC pairing window, the pairing exchange.
//!
//! This crate knows nothing about items, folders or SQLite. The sync engine in
//! tugboat-core defines its own message types and sends them through `frame`.

pub mod blob;
pub mod error;
pub mod frame;
pub mod guard;
pub mod local;
pub mod node;
pub mod pairing;

#[cfg(test)]
mod tests;

pub use error::NetError;
pub use iroh;

/// Sync protocol. Accepted only from paired keys (checked in the after-handshake hook).
pub const ALPN_SYNC: &[u8] = b"localtugboat/1";
/// Pairing protocol. In the endpoint's ALPN list only while the PC's pairing window is open.
pub const ALPN_PAIR: &[u8] = b"localtugboat-pair/1";

/// Default fixed UDP port of the PC (a setting; the firewall rule is per program, not per port).
pub const DEFAULT_PC_PORT: u16 = 47300;

/// QUIC application close codes. Both sides map a received code to a user-visible state,
/// so a refusal is never a silent no-sync.
pub mod close {
    /// Normal shutdown (app exit, network change).
    pub const NORMAL: u32 = 0;
    /// The remote key is not paired (sync ALPN) or was unpaired.
    pub const NOT_PAIRED: u32 = 1;
    /// Pairing ALPN used while no pairing window is open.
    pub const PAIRING_CLOSED: u32 = 2;
    /// A path with a non-local remote address appeared on the connection.
    pub const PATH_NOT_LOCAL: u32 = 3;
    /// The handshake found incompatible protocol versions (sent by core after its `Refuse`).
    pub const VERSION_MISMATCH: u32 = 4;
    /// The peer sent the `Unpair` message; the connection is closed after it.
    pub const UNPAIRED: u32 = 5;
    /// A protocol violation (bad frame, oversized frame, unknown stream kind).
    pub const PROTOCOL: u32 = 6;
}
