//! Events for the UIs.
//!
//! Delivery: `tokio::sync::broadcast` (capacity 1024). A UI that lags gets
//! `RecvError::Lagged` and must re-query everything it shows (never trust a partial stream).
//! Events carry ids, not full rows: the UI re-reads through the facade.
//! Desktop forwards each event to the webview as "core-event" (ts-rs types).

use serde::{Deserialize, Serialize};

use crate::model::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Event {
    Sync(SyncStatus),
    ItemsChanged { ids: Vec<ItemId> },
    FoldersChanged,
    PlacesChanged,
    ActivityAdded,
    SettingsChanged,
    Pairing(PairingEvent),
    /// Something failed. `kind` = CoreError::kind() or NetError::kind() or "reconcile".
    /// The UI shows `message` (one short line) and the Activity screen keeps the record.
    Error { kind: String, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum SyncState {
    /// No paired device.
    Unpaired,
    /// Paired, not connected (PC: waiting for the phone; phone: PC not found).
    Offline,
    Connecting,
    /// Connected, catching up or downloading.
    Syncing,
    /// Connected, digests equal, nothing pending.
    Synced,
    /// Sync paused (PC library missing / bulk missing).
    Paused,
    /// Handshake refused (version mismatch, not paired). `SyncStatus.message` says why.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SyncStatus {
    pub state: SyncState,
    pub peer_name: Option<String>,
    /// Shown as "last synced" on both apps, so a stopped sync is visible.
    pub last_synced_at: Option<i64>,
    /// Items held here (not in the Bin) that the paired device has not confirmed holding yet
    /// ("PC not found, 3 waiting"). 0 while unpaired. Expiry never waits for them (0.2.0).
    pub waiting: u32,
    /// Items that wait on the other device: Pending here (queued downloads).
    pub downloading: u32,
    /// Phone: the storage guard (`Settings.storage_guard_bytes`) is holding downloads back.
    pub storage_low: bool,
    /// One short line for Refused/Paused ("Update Tugboat on the PC").
    pub message: Option<String>,
}

/// Tagged with "kind", not "t": it is nested inside `Event::Pairing`, which is already tagged "t",
/// and two internal tags with the same name overwrite each other in the JSON (the UI would never
/// see "pairing", and the generated TS type would be impossible).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum PairingEvent {
    /// PC: QR to show (as SVG, rendered in Rust by the desktop) until `expires_at`.
    WindowOpen { payload: String, expires_at: i64 },
    /// Both: show "Phone shows 482 913? [Pair] [Cancel]" (PC) / the code (phone).
    Code { code: String, peer_name: String },
    Completed { peer: DeviceId, peer_name: String },
    /// `code` = PairError code from tugboat-net ("secret_reused", "expired", ...),
    /// `message` the one-line text ("PAIRING BLOCKED - someone else used this code").
    Cancelled { code: String, message: String },
    /// The peer was unpaired (here, or by the other device).
    Unpaired { peer: DeviceId },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI gets events as JSON: `Event::Pairing` must keep both its own tag and the inner kind.
    #[test]
    fn pairing_events_survive_json_with_both_tags() {
        let e = Event::Pairing(PairingEvent::Code { code: "482 913".into(), peer_name: "Pixel".into() });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["t"], "pairing");
        assert_eq!(v["kind"], "code");
        assert_eq!(v["code"], "482 913");
        assert_eq!(serde_json::from_value::<Event>(v).unwrap(), e);
        let c = Event::Pairing(PairingEvent::Cancelled { code: "secret_reused".into(), message: "m".into() });
        assert_eq!(serde_json::from_value::<Event>(serde_json::to_value(&c).unwrap()).unwrap(), c);
    }
}
