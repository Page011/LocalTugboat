//! tugboat-core: everything in Rust except the network transport, platform-neutral.
//!
//! Contract: notes/core-design.md (not committed). Module owners:
//! - logic builder: error, clock, model, db (+ schema/), store, merge, expiry, bin, activity,
//!   folders, places, names, placement, ops
//! - sync builder: sync/ (wire, engine, digest, net, pair), facade, events, tests/, examples/
//!
//! Layering (PLAN question 9 "Core modules do not reach into each other's tables"):
//! only `store` and `activity` run SQL; every other module calls them. `merge`, `expiry`,
//! `places` and `names` are pure (no DB, no clock, no IO) so their rules are table-tested.

pub mod activity;
pub mod bin;
pub mod clock;
pub mod db;
pub mod error;
pub mod events;
pub mod expiry;
pub mod facade;
pub mod folders;
pub mod merge;
pub mod model;
pub mod names;
pub mod ops;
pub mod placement;
pub mod places;
pub mod store;
pub mod sync;

pub use clock::{Clock, SystemClock, TestClock};
pub use error::{CoreError, Result};
pub use events::Event;
pub use facade::{device_id_from_secret, Core, CoreConfig, NetworkConfig};
pub use model::*;
pub use placement::{AppStorage, FileTarget, Placement};
