//! The one error type of tugboat-net. Every variant carries enough text to show the
//! user or write to the log; `kind()` gives a stable short name for events.

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("not connected")]
    NotConnected,
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("connect failed: {0}")]
    Connect(String),
    /// Address (dial target, QR address, mDNS result or live path) is not on the local network.
    #[error("address not on local network: {0}")]
    NotLocal(String),
    /// The connection was closed by the remote with one of `crate::close` codes.
    #[error("closed by peer (code {code}): {reason}")]
    ClosedByPeer { code: u32, reason: String },
    #[error("file error on {path}: {msg}")]
    Io { path: String, msg: String },
    #[error("stream error: {0}")]
    Stream(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Frame longer than `frame::MAX_FRAME`; the stream is not read further.
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u64),
    #[error("hash mismatch: expected {expected} got {got}")]
    HashMismatch { expected: String, got: String },
    /// Sender's declared size differs from the size the receiver asked for.
    #[error("size mismatch: expected {expected} got {got}")]
    SizeMismatch { expected: u64, got: u64 },
    #[error("not enough free space: {needed} bytes needed, {available} available")]
    NoSpace { needed: u64, available: u64 },
    /// The sender refused a blob request (not found, changed on disk, too large).
    #[error("peer refused the blob: {0}")]
    BlobRefused(String),
    #[error("endpoint error: {0}")]
    Endpoint(String),
    #[error("pairing: {0}")]
    Pairing(#[from] crate::pairing::PairError),
}

impl NetError {
    /// Short stable name of the variant, for error events and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            NetError::NotConnected => "not_connected",
            NetError::Timeout(_) => "timeout",
            NetError::Connect(_) => "connect",
            NetError::NotLocal(_) => "not_local",
            NetError::ClosedByPeer { .. } => "closed_by_peer",
            NetError::Io { .. } => "io",
            NetError::Stream(_) => "stream",
            NetError::Protocol(_) => "protocol",
            NetError::FrameTooLarge(_) => "frame_too_large",
            NetError::HashMismatch { .. } => "hash_mismatch",
            NetError::SizeMismatch { .. } => "size_mismatch",
            NetError::NoSpace { .. } => "no_space",
            NetError::BlobRefused(_) => "blob_refused",
            NetError::Endpoint(_) => "endpoint",
            NetError::Pairing(_) => "pairing",
        }
    }
}

/// Error plus its source chain (Display of many iroh errors hides the cause).
pub fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

/// Maps a QUIC connection error to the matching `NetError`. An application close from the
/// peer keeps its code and reason, so a refusal is shown as such and never as "stream broke".
pub fn map_conn_err(e: &iroh::endpoint::ConnectionError) -> NetError {
    use iroh::endpoint::ConnectionError as C;
    match e {
        C::ApplicationClosed(ac) => NetError::ClosedByPeer {
            code: u32::try_from(ac.error_code.into_inner()).unwrap_or(u32::MAX),
            reason: String::from_utf8_lossy(&ac.reason).into_owned(),
        },
        C::TimedOut => NetError::Timeout("peer silent (connection idle timeout)".into()),
        other => NetError::Stream(chain(other)),
    }
}

/// Stream read/write error -> `NetError`. Walks the source chain: if a connection error is in
/// there (stream calls wrap it in `ConnectionLost`), it is mapped as `map_conn_err` does.
pub fn stream_err(what: &str, e: &(dyn std::error::Error + 'static)) -> NetError {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cur {
        if let Some(ce) = c.downcast_ref::<iroh::endpoint::ConnectionError>() {
            return match map_conn_err(ce) {
                NetError::Stream(m) => NetError::Stream(format!("{what}: {m}")),
                other => other,
            };
        }
        cur = c.source();
    }
    NetError::Stream(format!("{what}: {}", chain(e)))
}

/// io::Error with the path and the error kind kept (e.g. "Permission denied ... [PermissionDenied]").
pub fn io_err(path: &std::path::Path, e: std::io::Error) -> NetError {
    NetError::Io { path: path.display().to_string(), msg: format!("{e} [{:?}]", e.kind()) }
}
