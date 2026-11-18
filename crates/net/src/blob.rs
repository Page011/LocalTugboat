//! Resumable, verified blob transfer. The receiver pulls.
//!
//! Why pull: the receiver knows which blobs it needs (from synced item records) and owns its
//! partial file, so it alone knows the resume offset (transfers are chunked and resumable
//! from the last received byte, written to a temp file, hash-checked, then renamed).
//!
//! Stream protocol (one `StreamKind::Blob` stream per blob, opened by the receiver):
//!   receiver -> sender: frame `BlobRequest { hash, size, offset }`, then finish its send side
//!   sender -> receiver: frame `BlobResponse::Ok { size }` then exactly `size - offset` raw
//!                        bytes then FIN; or frame `BlobResponse::Refused { reason }` then FIN.
//!
//! Receiver rules (`fetch_blob`), in order:
//! 1. `size` comes from the synced record; `size > max_size` -> refuse locally (Protocol).
//! 2. The part file is `part_path` (core chooses it: `<hash hex>.part` in the placement's part
//!    dir, same volume as the final place, PC: `<library>\.tugboat\`). If it exists and is
//!    longer than `size`, truncate it to 0. offset = its length.
//! 3. Free space: `fs4::available_space(part dir) >= size - offset`, else `NoSpace` before
//!    any byte is written (fails closed if the space query errors).
//! 4. Hash the existing `offset` bytes of the part file first (streaming, never whole-file in
//!    memory), then append received bytes while hashing.
//! 5. Response `Ok{size}` with a different size -> `SizeMismatch`, part kept as is.
//! 6. Stream ends early -> `Stream` error, part file KEPT (that is the resume point).
//! 7. Any byte after `size` -> `Protocol`, part file deleted.
//! 8. All bytes in: flush + sync_all, compare BLAKE3 with `hash`. Mismatch -> part file
//!    DELETED (corrupt prefix would poison every resume) and `HashMismatch`.
//! 9. Match -> `Ok(Fetched)`. The part file is complete and verified; the caller (core)
//!    renames it into place via its Placement and only then records + sends the receipt.
//!
//! A peer that stops sending mid-body (stream open, no bytes) is cut off after `STALL_TIMEOUT`
//! with `Timeout`, part kept: a stuck transfer is an error, never a silent wait.
//!
//! Sender rules (`serve_blob`):
//! - Read the request frame; `source.open(hash)` -> None -> `Refused{"not found"}`.
//! - The file length must equal `size`, else `Refused{"changed"}` (PC file edited since hashed;
//!   the watcher will produce a new content version).
//! - `offset > size` -> `Refused{"bad offset"}`.
//! - Seek to `offset`, stream in 256 KiB chunks, finish. The sender does not re-hash; the
//!   receiver's hash check is the guard.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use iroh::endpoint::{RecvStream, SendStream, VarInt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::{
    error::{io_err, stream_err},
    frame::{read_frame_limited, write_frame},
    node::{PeerConn, StreamKind},
    NetError,
};

/// Streaming chunk size.
pub const CHUNK: usize = 256 * 1024;

/// No byte for this long inside a body, or at the end-of-stream check, is a stuck peer.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A request is a few dozen bytes; anything bigger is not one.
const MAX_REQUEST_FRAME: u32 = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobRequest {
    /// BLAKE3, 64 lowercase hex chars.
    pub hash: String,
    pub size: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum BlobResponse {
    Ok { size: u64 },
    Refused { reason: String },
}

/// Result of a successful fetch: `part_path` holds exactly `size` bytes whose BLAKE3 is `hash`.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub part_path: PathBuf,
    pub hash: String,
    pub size: u64,
    /// Bytes that were already in the part file (0 = fresh download). Tests assert on it.
    pub resumed_from: u64,
    pub ms: u64,
}

/// Where the sender finds a blob by hash. Implemented by core over its Placement.
pub trait BlobSource: Send + Sync + 'static {
    /// Path of a held file whose content hash is `hash`, or None.
    fn open(&self, hash: &str) -> Option<PathBuf>;
}

/// 64 hex chars (any case).
fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `needed <= available`, else `NoSpace`. Pure; unit tested.
pub fn check_space(needed: u64, available: u64) -> Result<(), NetError> {
    if needed > available {
        return Err(NetError::NoSpace { needed, available });
    }
    Ok(())
}

/// Download one blob into `part_path` (rules 1-9 above). `max_size` is core's setting
/// (default 16 GiB).
pub async fn fetch_blob(conn: &PeerConn, hash: &str, size: u64, part_path: &Path, max_size: u64) -> Result<Fetched, NetError> {
    let t0 = Instant::now();
    if !valid_hash(hash) {
        return Err(NetError::Protocol(format!("blob hash is not 64 hex characters: {hash:?}")));
    }
    let want = hash.to_ascii_lowercase();
    if size > max_size {
        return Err(NetError::Protocol(format!("blob of {size} bytes is over the {max_size} byte limit")));
    }

    // Rule 2: the part file decides the offset. One longer than the record is garbage.
    let dir = part_path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    tokio::fs::create_dir_all(dir).await.map_err(|e| io_err(dir, e))?;
    let mut offset = match tokio::fs::metadata(part_path).await {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(io_err(part_path, e)),
    };
    if offset > size {
        let f = tokio::fs::OpenOptions::new().write(true).open(part_path).await.map_err(|e| io_err(part_path, e))?;
        f.set_len(0).await.map_err(|e| io_err(part_path, e))?;
        offset = 0;
    }

    // Rule 3: fail closed when the space query itself fails.
    let available = fs4::available_space(dir).map_err(|e| io_err(dir, e))?;
    check_space(size - offset, available)?;

    let (mut tx, mut rx) = conn.open_stream(StreamKind::Blob).await?;
    write_frame(&mut tx, &BlobRequest { hash: want.clone(), size, offset }).await?;
    tx.finish().map_err(|e| stream_err("finish blob request", &e))?;

    let response = tokio::time::timeout(STALL_TIMEOUT, read_frame_limited::<BlobResponse>(&mut rx, MAX_REQUEST_FRAME))
        .await
        .map_err(|_| NetError::Timeout("no response to the blob request".into()))??;
    match response {
        None => return Err(NetError::Stream("peer closed the blob stream without a response".into())),
        Some(BlobResponse::Refused { reason }) => return Err(NetError::BlobRefused(reason)),
        Some(BlobResponse::Ok { size: s }) if s != size => return Err(NetError::SizeMismatch { expected: size, got: s }),
        Some(BlobResponse::Ok { .. }) => {}
    }

    // Rule 4: hash what is already on disk, then continue after it.
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(part_path)
        .await
        .map_err(|e| io_err(part_path, e))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut left = offset;
    while left > 0 {
        let n = file.read(&mut buf[..(left as usize).min(CHUNK)]).await.map_err(|e| io_err(part_path, e))?;
        if n == 0 {
            return Err(NetError::Io { path: part_path.display().to_string(), msg: format!("part file shrank while hashing ({left} bytes missing)") });
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    file.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| io_err(part_path, e))?;

    // Body. Whatever happens, what was received so far is flushed: it is the resume point.
    let body = receive_body(&mut rx, &mut file, &mut hasher, &mut buf, size - offset, part_path).await;
    let flushed = flush_and_sync(&mut file, part_path).await;
    drop(file);
    match body {
        Ok(()) => flushed?,
        Err(e) => {
            // Rule 7: trailing garbage poisons the part; every other error keeps it (rule 6).
            if matches!(e, NetError::Protocol(_)) {
                let _ = tokio::fs::remove_file(part_path).await;
            }
            return Err(e);
        }
    }

    // Rule 8.
    let got = hasher.finalize().to_hex().to_string();
    if got != want {
        let _ = tokio::fs::remove_file(part_path).await;
        return Err(NetError::HashMismatch { expected: want, got });
    }
    Ok(Fetched { part_path: part_path.to_path_buf(), hash: want, size, resumed_from: offset, ms: t0.elapsed().as_millis() as u64 })
}

async fn flush_and_sync(file: &mut tokio::fs::File, path: &Path) -> Result<(), NetError> {
    file.flush().await.map_err(|e| io_err(path, e))?;
    file.sync_all().await.map_err(|e| io_err(path, e))
}

/// Reads exactly `want` body bytes, appends them to `file` and `hasher`, then requires FIN.
async fn receive_body(
    rx: &mut RecvStream,
    file: &mut tokio::fs::File,
    hasher: &mut blake3::Hasher,
    buf: &mut [u8],
    want: u64,
    part_path: &Path,
) -> Result<(), NetError> {
    let mut left = want;
    while left > 0 {
        let take = (left as usize).min(buf.len());
        let read = tokio::time::timeout(STALL_TIMEOUT, rx.read(&mut buf[..take]))
            .await
            .map_err(|_| NetError::Timeout(format!("peer stopped sending ({left} bytes missing)")))?
            .map_err(|e| stream_err("read blob data", &e))?;
        match read {
            Some(n) => {
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n]).await.map_err(|e| io_err(part_path, e))?;
                left -= n as u64;
            }
            None => return Err(NetError::Stream(format!("stream ended early, {left} bytes missing"))),
        }
    }
    // The stream must end here. One extra byte is a protocol violation.
    let mut extra = [0u8; 1];
    let end = tokio::time::timeout(STALL_TIMEOUT, rx.read(&mut extra))
        .await
        .map_err(|_| NetError::Timeout("peer did not end the stream after the last byte".into()))?
        .map_err(|e| stream_err("read end of blob stream", &e))?;
    match end {
        None => Ok(()),
        Some(_) => Err(NetError::Protocol("unexpected bytes after the end of the blob".into())),
    }
}

/// Serve one blob request on an accepted `StreamKind::Blob` stream (sender rules above).
/// Errors are returned for logging; a refusal the peer was told about is `Ok(())`.
pub async fn serve_blob(mut tx: SendStream, mut rx: RecvStream, source: &dyn BlobSource) -> Result<(), NetError> {
    let req: BlobRequest = tokio::time::timeout(STALL_TIMEOUT, read_frame_limited(&mut rx, MAX_REQUEST_FRAME))
        .await
        .map_err(|_| NetError::Timeout("no blob request after the stream opened".into()))??
        .ok_or_else(|| NetError::Protocol("blob stream ended before a request".into()))?;

    let refuse = |reason: &str| BlobResponse::Refused { reason: reason.to_string() };
    let reply = if !valid_hash(&req.hash) {
        Some(refuse("bad request"))
    } else if req.offset > req.size {
        Some(refuse("bad offset"))
    } else {
        None
    };
    let mut file = None;
    let reply = match reply {
        Some(r) => Some(r),
        None => match source.open(&req.hash.to_ascii_lowercase()) {
            None => Some(refuse("not found")),
            Some(path) => match tokio::fs::File::open(&path).await {
                Err(_) => Some(refuse("not found")),
                Ok(f) => {
                    let len = f.metadata().await.map_err(|e| io_err(&path, e))?.len();
                    if len != req.size {
                        Some(refuse("changed"))
                    } else {
                        file = Some((f, path));
                        None
                    }
                }
            },
        },
    };
    if let Some(r) = reply {
        write_frame(&mut tx, &r).await?;
        tx.finish().map_err(|e| stream_err("finish refusal", &e))?;
        return Ok(());
    }
    let (mut f, path) = file.expect("file is set when no refusal was chosen");

    f.seek(std::io::SeekFrom::Start(req.offset)).await.map_err(|e| io_err(&path, e))?;
    write_frame(&mut tx, &BlobResponse::Ok { size: req.size }).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut left = req.size - req.offset;
    while left > 0 {
        let n = f.read(&mut buf[..(left as usize).min(CHUNK)]).await.map_err(|e| io_err(&path, e))?;
        if n == 0 {
            // We promised more bytes than the file has: reset, so the receiver sees a failure
            // and not a clean end it could mistake for a complete body.
            let _ = tx.reset(VarInt::from_u32(1));
            return Err(NetError::Io { path: path.display().to_string(), msg: format!("file shrank while sending ({left} bytes missing)") });
        }
        tx.write_all(&buf[..n]).await.map_err(|e| stream_err("write blob data", &e))?;
        left -= n as u64;
    }
    tx.finish().map_err(|e| stream_err("finish blob", &e))?;
    Ok(())
}
