//! The one error type of tugboat-core. The sync layer uses the Net/Protocol/Version/Pairing
//! variants.
//! Errors are surfaced (returned, and turned into `Event::Error` by the facade), never swallowed.

use crate::names::NameError;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("file error on {path}: {msg}")]
    Io { path: String, msg: String },
    /// An older app opening a newer database refuses with a clear message.
    #[error("this database was written by a newer Tugboat (schema {found}, this app knows up to {supported}); update Tugboat")]
    NewerDatabase { found: u32, supported: u32 },
    #[error("database upgrade {from} -> {to} failed: {msg}")]
    Migration { from: u32, to: u32, msg: String },
    #[error("{what} not found: {id}")]
    NotFound { what: &'static str, id: String },
    #[error("invalid name: {0}")]
    Name(#[from] NameError),
    /// Input that violates a rule (empty folder name, retention 0, move into a deleted folder).
    #[error("invalid: {0}")]
    Invalid(String),
    /// A photo/video extension whose content is not a photo or video (a renamed program).
    #[error("not a supported photo or video: {0}")]
    NotMedia(String),
    /// Refused on purpose, never synced: an always-refused name ("REFUSED - shortcut and
    /// folder-setting files never sync") or a file over the size limit ("TOO LARGE - over 16 GB").
    /// The text is the one-line message for the user.
    #[error("{0}")]
    Refused(String),
    /// Restore or fetch of a file neither device holds any more.
    #[error("file no longer on either device")]
    FileGone,
    #[error("no paired device")]
    NotPaired,
    /// Pairing a new phone while one is paired, without `replace_existing`.
    #[error("a phone is already paired; unpair it first")]
    AlreadyPaired,
    /// The phone already has `MAX_LINKED` PCs linked.
    #[error("LIMIT {} DEVICES - unlink one first", crate::model::MAX_LINKED)]
    TooManyDevices,
    /// Sync paused (PC library missing or bulk-missing check).
    #[error("sync paused: {0}")]
    Paused(String),
    /// Transport error from tugboat-net (kind = NetError::kind()).
    #[error("network ({kind}): {msg}")]
    Net { kind: String, msg: String },
    /// Peer sent something the sync protocol does not allow.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Handshake refusal. `update` says which device must be updated ("pc" / "phone").
    #[error("Update Tugboat on the {update} (protocol {ours} here, {theirs} there)")]
    VersionMismatch { ours: u32, theirs: u32, update: String },
    /// Pairing failure; `code` is the snake_case PairError code from tugboat-net.
    #[error("pairing: {msg}")]
    Pairing { code: String, msg: String },
}

impl CoreError {
    /// io::Error with path and kind kept.
    pub fn io(path: &std::path::Path, e: std::io::Error) -> CoreError {
        CoreError::Io { path: path.display().to_string(), msg: format!("{e} [{:?}]", e.kind()) }
    }

    /// Stable short name for `Event::Error.kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            CoreError::Db(_) => "db",
            CoreError::Io { .. } => "io",
            CoreError::NewerDatabase { .. } => "newer_database",
            CoreError::Migration { .. } => "migration",
            CoreError::NotFound { .. } => "not_found",
            CoreError::Name(_) => "name",
            CoreError::Invalid(_) => "invalid",
            CoreError::NotMedia(_) => "not_media",
            CoreError::Refused(_) => "refused",
            CoreError::FileGone => "file_gone",
            CoreError::NotPaired => "not_paired",
            CoreError::AlreadyPaired => "already_paired",
            CoreError::TooManyDevices => "too_many_devices",
            CoreError::Paused(_) => "paused",
            CoreError::Net { .. } => "net",
            CoreError::Protocol(_) => "protocol",
            CoreError::VersionMismatch { .. } => "version_mismatch",
            CoreError::Pairing { .. } => "pairing",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_stable_snake_case_and_messages_are_one_line() {
        let all = vec![
            CoreError::Db(rusqlite::Error::QueryReturnedNoRows),
            CoreError::Io { path: "p".into(), msg: "m".into() },
            CoreError::NewerDatabase { found: 2, supported: 1 },
            CoreError::Migration { from: 1, to: 2, msg: "m".into() },
            CoreError::NotFound { what: "item", id: "x".into() },
            CoreError::Name(NameError::Empty),
            CoreError::Invalid("x".into()),
            CoreError::NotMedia("x".into()),
            CoreError::Refused("x".into()),
            CoreError::FileGone,
            CoreError::NotPaired,
            CoreError::AlreadyPaired,
            CoreError::TooManyDevices,
            CoreError::Paused("x".into()),
            CoreError::Net { kind: "timeout".into(), msg: "m".into() },
            CoreError::Protocol("x".into()),
            CoreError::VersionMismatch { ours: 1, theirs: 2, update: "pc".into() },
            CoreError::Pairing { code: "expired".into(), msg: "m".into() },
        ];
        let mut kinds = std::collections::HashSet::new();
        for e in &all {
            let k = e.kind();
            assert!(k.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{k}");
            assert!(kinds.insert(k), "kind {k} is used twice");
            assert!(!e.to_string().contains('\n'));
        }
        let io = CoreError::io(std::path::Path::new("dir/x.jpg"), std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(io.to_string().contains("x.jpg") && io.to_string().contains("NotFound"));
        assert_eq!(CoreError::VersionMismatch { ours: 1, theirs: 2, update: "pc".into() }.to_string(), "Update Tugboat on the pc (protocol 1 here, 2 there)");
        assert_eq!(CoreError::TooManyDevices.to_string(), "LIMIT 5 DEVICES - unlink one first");
    }
}
