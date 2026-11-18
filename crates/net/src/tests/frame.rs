//! N9.

use std::time::{Duration, Instant};

use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};

use super::{bounded, linked, Linked};
use crate::{
    frame::{encode, read_frame, write_frame, MAX_FRAME},
    node::StreamKind,
    NetError,
};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Msg {
    t: String,
    n: u64,
}

/// A fresh Control stream from b to a: (b's send half, a's receive half).
async fn stream(l: &Linked) -> (SendStream, RecvStream) {
    let (tx, _rx) = l.b_conn.open_stream(StreamKind::Control).await.unwrap();
    let (kind, _a_tx, a_rx) = bounded(10, l.a_conn.accept_stream()).await.unwrap();
    assert_eq!(kind, StreamKind::Control);
    (tx, a_rx)
}

#[test]
fn n9_encode_is_length_prefixed_json() {
    let bytes = encode(&Msg { t: "x".into(), n: 7 }).unwrap();
    let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(len, bytes.len() - 4);
    assert_eq!(&bytes[4..], br#"{"t":"x","n":7}"#);
}

#[test]
fn n9_sender_refuses_an_oversize_body() {
    let big = Msg { t: "a".repeat(MAX_FRAME as usize), n: 0 };
    let err = encode(&big).unwrap_err();
    assert!(matches!(err, NetError::Protocol(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n9_frames_roundtrip_and_end_cleanly() {
    let l = linked().await;
    let (mut tx, mut rx) = stream(&l).await;
    let a = Msg { t: "first".into(), n: 1 };
    let b = Msg { t: "second \u{00e9}\u{4e2d}".into(), n: u64::MAX };
    write_frame(&mut tx, &a).await.unwrap();
    write_frame(&mut tx, &b).await.unwrap();
    assert_eq!(bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap(), Some(a));
    assert_eq!(bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap(), Some(b));
    // Clean end of stream before a frame starts is Ok(None), not an error.
    tx.finish().unwrap();
    assert_eq!(bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n9_oversize_length_is_refused_before_the_body() {
    let l = linked().await;
    let (mut tx, mut rx) = stream(&l).await;
    // Only the length arrives; the body never does. The refusal must not wait for it.
    tx.write_all(&(MAX_FRAME + 1).to_be_bytes()).await.unwrap();
    let t0 = Instant::now();
    let err = bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap_err();
    assert!(matches!(err, NetError::FrameTooLarge(n) if n == MAX_FRAME as u64 + 1), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n9_zero_truncated_and_bad_json_frames() {
    let l = linked().await;

    let (mut tx, mut rx) = stream(&l).await;
    tx.write_all(&0u32.to_be_bytes()).await.unwrap();
    let err = bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap_err();
    assert!(matches!(err, NetError::Protocol(ref m) if m.contains("empty")), "{err}");

    // Length says 10, only 3 body bytes, then the stream ends.
    let (mut tx, mut rx) = stream(&l).await;
    tx.write_all(&10u32.to_be_bytes()).await.unwrap();
    tx.write_all(b"abc").await.unwrap();
    tx.finish().unwrap();
    let err = bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap_err();
    assert!(matches!(err, NetError::Stream(ref m) if m.contains("truncated")), "{err}");

    // The length prefix itself cut short.
    let (mut tx, mut rx) = stream(&l).await;
    tx.write_all(&[0, 0]).await.unwrap();
    tx.finish().unwrap();
    let err = bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap_err();
    assert!(matches!(err, NetError::Stream(ref m) if m.contains("truncated")), "{err}");

    // Valid length, body is not JSON for Msg.
    let (mut tx, mut rx) = stream(&l).await;
    tx.write_all(&3u32.to_be_bytes()).await.unwrap();
    tx.write_all(b"abc").await.unwrap();
    let err = bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap_err();
    assert!(matches!(err, NetError::Protocol(ref m) if m.contains("bad frame")), "{err}");

    // Valid JSON, wrong shape.
    let (mut tx, mut rx) = stream(&l).await;
    let body = br#"{"nope":1}"#;
    tx.write_all(&(body.len() as u32).to_be_bytes()).await.unwrap();
    tx.write_all(body).await.unwrap();
    let err = bounded(10, read_frame::<Msg>(&mut rx)).await.unwrap_err();
    assert!(matches!(err, NetError::Protocol(_)), "{err}");
}
