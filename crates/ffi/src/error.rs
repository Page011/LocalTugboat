//! The one error type Kotlin sees. A flat UniFFI error: every variant is its own Kotlin
//! exception class (so the UI can `when` on it) and its `message` is the readable line
//! from core, never an empty string. Pairing codes and network kinds are split into their own
//! variants so the onboarding screen needs no string matching.

use tugboat_core::CoreError;

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum SnapError {
    /// The app passed a bad argument (an id that is not a UUID, a key that is not 32 bytes).
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Db(String),
    /// File problem: missing source file, full disk, permission.
    #[error("{0}")]
    Io(String),
    /// The database was written by a newer Tugboat: "update Tugboat".
    #[error("{0}")]
    NewerDatabase(String),
    #[error("{0}")]
    Migration(String),
    #[error("{0}")]
    NotFound(String),
    /// A folder or file name core refuses.
    #[error("{0}")]
    InvalidName(String),
    /// A rule was broken (move into a deleted folder, restore of an item that is not in the Bin).
    #[error("{0}")]
    Invalid(String),
    /// A photo/video extension whose content is not a photo or video (a renamed program).
    #[error("{0}")]
    NotMedia(String),
    /// Refused on purpose, never synced: an always-refused name (shortcut and folder-setting
    /// files) or a file over the size limit. The message is the one-line text to show.
    #[error("{0}")]
    Refused(String),
    /// Restore of a file neither device holds any more.
    #[error("{0}")]
    FileGone(String),
    #[error("{0}")]
    NotPaired(String),
    #[error("{0}")]
    AlreadyPaired(String),
    /// The phone already has the most PCs it can link.
    #[error("{0}")]
    TooManyDevices(String),
    #[error("{0}")]
    Paused(String),
    /// No answer from the PC in time: the usual "wrong network" or PC asleep.
    #[error("{0}")]
    NetTimeout(String),
    /// Could not connect (nothing listening, no route, local network permission denied).
    #[error("{0}")]
    NetUnreachable(String),
    /// The address is not on the local network; Tugboat never dials it.
    #[error("{0}")]
    NetNotLocal(String),
    /// Any other transport error.
    #[error("{0}")]
    Net(String),
    #[error("{0}")]
    Protocol(String),
    /// "Update Tugboat on the PC/phone".
    #[error("{0}")]
    VersionMismatch(String),
    /// The QR code showed a code that expired by the PC's clock.
    #[error("{0}")]
    PairExpired(String),
    #[error("{0}")]
    PairBadSecret(String),
    /// Someone else used the one-time code first.
    #[error("{0}")]
    PairSecretReused(String),
    #[error("{0}")]
    PairCancelled(String),
    /// The PC owner did not click Pair in time.
    #[error("{0}")]
    PairConfirmTimeout(String),
    #[error("{0}")]
    PairNotOpen(String),
    #[error("{0}")]
    PairVersionMismatch(String),
    /// The scanned text is not a Tugboat pairing code.
    #[error("{0}")]
    PairBadPayload(String),
    /// Any other pairing failure (the PC never confirmed delivery).
    #[error("{0}")]
    Pairing(String),
}

impl From<CoreError> for SnapError {
    fn from(e: CoreError) -> Self {
        let m = e.to_string();
        match e {
            CoreError::Db(_) => Self::Db(m),
            CoreError::Io { .. } => Self::Io(m),
            CoreError::NewerDatabase { .. } => Self::NewerDatabase(m),
            CoreError::Migration { .. } => Self::Migration(m),
            CoreError::NotFound { .. } => Self::NotFound(m),
            CoreError::Name(_) => Self::InvalidName(m),
            CoreError::Invalid(_) => Self::Invalid(m),
            CoreError::NotMedia(_) => Self::NotMedia(m),
            CoreError::Refused(_) => Self::Refused(m),
            CoreError::FileGone => Self::FileGone(m),
            CoreError::NotPaired => Self::NotPaired(m),
            CoreError::AlreadyPaired => Self::AlreadyPaired(m),
            CoreError::TooManyDevices => Self::TooManyDevices(m),
            CoreError::Paused(_) => Self::Paused(m),
            CoreError::Net { ref kind, .. } => match kind.as_str() {
                "timeout" => Self::NetTimeout(m),
                "connect" | "not_connected" => Self::NetUnreachable(m),
                "not_local" => Self::NetNotLocal(m),
                _ => Self::Net(m),
            },
            CoreError::Protocol(_) => Self::Protocol(m),
            CoreError::VersionMismatch { .. } => Self::VersionMismatch(m),
            // `msg` is the one-line text shared with the PC; `code` is the PairError tag.
            CoreError::Pairing { ref code, ref msg } => {
                let text = msg.clone();
                match code.as_str() {
                    "expired" => Self::PairExpired(text),
                    "bad_secret" => Self::PairBadSecret(text),
                    "secret_reused" => Self::PairSecretReused(text),
                    "cancelled" => Self::PairCancelled(text),
                    "confirm_timeout" => Self::PairConfirmTimeout(text),
                    "not_open" => Self::PairNotOpen(text),
                    "version_mismatch" => Self::PairVersionMismatch(text),
                    "bad_payload" => Self::PairBadPayload(text),
                    _ => Self::Pairing(text),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pairing code core can produce keeps its own variant, and every message stays readable.
    #[test]
    fn pairing_codes_and_net_kinds_map_to_their_own_variants() {
        let p = |code: &str| SnapError::from(CoreError::Pairing { code: code.into(), msg: format!("text for {code}") });
        assert!(matches!(p("expired"), SnapError::PairExpired(m) if m == "text for expired"));
        assert!(matches!(p("bad_secret"), SnapError::PairBadSecret(_)));
        assert!(matches!(p("secret_reused"), SnapError::PairSecretReused(_)));
        assert!(matches!(p("cancelled"), SnapError::PairCancelled(_)));
        assert!(matches!(p("confirm_timeout"), SnapError::PairConfirmTimeout(_)));
        assert!(matches!(p("not_open"), SnapError::PairNotOpen(_)));
        assert!(matches!(p("version_mismatch"), SnapError::PairVersionMismatch(_)));
        assert!(matches!(p("bad_payload"), SnapError::PairBadPayload(_)));
        assert!(matches!(p("something_new"), SnapError::Pairing(_)), "an unknown code is still an error, not a panic");
        let n = |kind: &str| SnapError::from(CoreError::Net { kind: kind.into(), msg: "m".into() });
        assert!(matches!(n("timeout"), SnapError::NetTimeout(_)));
        assert!(matches!(n("connect"), SnapError::NetUnreachable(_)));
        assert!(matches!(n("not_connected"), SnapError::NetUnreachable(_)));
        assert!(matches!(n("not_local"), SnapError::NetNotLocal(_)));
        assert!(matches!(n("hash_mismatch"), SnapError::Net(_)));
        assert!(SnapError::from(CoreError::NotPaired).to_string().contains("paired device"));
        assert!(matches!(SnapError::from(CoreError::NotMedia("x".into())), SnapError::NotMedia(_)));
        assert!(matches!(SnapError::from(CoreError::Refused("REFUSED - x".into())), SnapError::Refused(m) if m == "REFUSED - x"));
    }
}
