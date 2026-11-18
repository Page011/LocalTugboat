//! Sync: the wire messages, the reconcile digest, the protocol state machine, and the network
//! driver around tugboat-net.
//!
//! - `wire`: control-stream message types (sent with tugboat-net `frame`).
//! - `digest`: reconcile digest over (key, versions) of every synced record.
//! - `engine`: `Session::handle`, the protocol as a function of (store, message) -> `Step`,
//!   testable without a network.
//! - `net`: endpoint start/stop, accept and connect loops, the run loop per connection, the
//!   blob download queue and blob serving.
//! - `pair`: the pairing flows (PC window, phone scan) over tugboat-net's primitives.

pub mod digest;
pub mod engine;
pub mod net;
pub mod pair;
pub mod wire;
