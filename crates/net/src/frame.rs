//! Framed control messages: `u32 BE length | JSON body`. Used for the sync control stream
//! (message types defined in tugboat-core `sync::wire`) and the pairing exchange.
//!
//! Rules:
//! - A declared length over `MAX_FRAME` is refused with `FrameTooLarge` before reading
//!   the body (a paired peer is still untrusted input).
//! - A length of 0 is a protocol error (every message is a JSON object).
//! - Clean end of stream before a length prefix -> `Ok(None)`; end of stream inside a
//!   frame -> `Stream` error.
//! - Once a frame has started it must finish within `FRAME_BODY_TIMEOUT` (`Timeout`).
//! - Bodies that are not valid JSON for `T` -> `Protocol` with the serde error text.

use std::time::Duration;

use iroh::endpoint::{RecvStream, SendStream};
use serde::{de::DeserializeOwned, Serialize};

use crate::{error::stream_err, NetError};

/// Largest accepted frame body. Sync pages are capped well below this (core `wire::PAGE`).
pub const MAX_FRAME: u32 = 8 * 1024 * 1024;

/// Once a frame has started, its remaining bytes must keep coming. A peer that stalls inside
/// a frame is a fault, not an idle control stream (idle = no frame started, which has no limit).
pub const FRAME_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Serialize `msg` to JSON and write one frame. Refuses (Protocol) to send a body over
/// `MAX_FRAME`, so the bug shows on the sender, not as a refusal on the peer.
pub async fn write_frame<T: Serialize>(tx: &mut SendStream, msg: &T) -> Result<(), NetError> {
    let buf = encode(msg)?;
    tx.write_all(&buf).await.map_err(|e| stream_err("write frame", &e))
}

/// Read one frame and decode it. `Ok(None)` on clean end of stream before a frame starts.
pub async fn read_frame<T: DeserializeOwned>(rx: &mut RecvStream) -> Result<Option<T>, NetError> {
    read_frame_limited(rx, MAX_FRAME).await
}

/// `read_frame` with a smaller cap, for streams read before the peer is trusted (pairing).
pub async fn read_frame_limited<T: DeserializeOwned>(rx: &mut RecvStream, max: u32) -> Result<Option<T>, NetError> {
    let mut first = [0u8; 1];
    if rx.read(&mut first).await.map_err(|e| stream_err("read frame length", &e))?.is_none() {
        return Ok(None);
    }
    let rest = async {
        let mut len_rest = [0u8; 3];
        rx.read_exact(&mut len_rest).await.map_err(|e| stream_err("frame truncated in its length", &e))?;
        let len = u32::from_be_bytes([first[0], len_rest[0], len_rest[1], len_rest[2]]);
        if len == 0 {
            return Err(NetError::Protocol("empty frame".into()));
        }
        // Checked before the body is read or allocated.
        if len > max {
            return Err(NetError::FrameTooLarge(len as u64));
        }
        let mut body = vec![0u8; len as usize];
        rx.read_exact(&mut body).await.map_err(|e| stream_err("frame truncated in its body", &e))?;
        Ok(body)
    };
    let body = tokio::time::timeout(FRAME_BODY_TIMEOUT, rest)
        .await
        .map_err(|_| NetError::Timeout("peer stalled inside a frame".into()))??;
    serde_json::from_slice(&body).map(Some).map_err(|e| NetError::Protocol(format!("bad frame ({} bytes): {e}", body.len())))
}

/// Pure frame encoder, shared by `write_frame` and the tests.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, NetError> {
    let body = serde_json::to_vec(msg).map_err(|e| NetError::Protocol(format!("cannot encode frame: {e}")))?;
    if body.len() > MAX_FRAME as usize {
        return Err(NetError::Protocol(format!("frame body of {} bytes is over the {MAX_FRAME} byte limit", body.len())));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}
