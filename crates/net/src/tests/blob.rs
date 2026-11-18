//! N10 to N16, plus the rejected-body cases (oversize, truncated, trailing bytes).
//! Custom senders in these tests speak the blob stream by hand, so a misbehaving peer can be
//! simulated byte for byte.

use std::{path::PathBuf, sync::Arc, time::Duration};

use tokio::io::AsyncReadExt;

use super::{bounded, linked, listing, make_file_bytes, spawn_blob_server, MapSource};
use crate::{
    blob::{check_space, fetch_blob, serve_blob, BlobRequest, BlobResponse, BlobSource},
    frame::{read_frame, write_frame},
    node::{PeerConn, StreamKind},
    NetError, ALPN_SYNC,
};

const MB: u64 = 1024 * 1024;

/// Answers one blob request by hand: reads the request, replies `Ok{announce}`, then writes
/// the first `send` bytes of the real file plus `extra`, finishes (or not).
async fn scripted_sender(conn: Arc<PeerConn>, file: PathBuf, announce: u64, send: usize, extra: &[u8], finish: bool) -> BlobRequest {
    let (kind, mut tx, mut rx) = bounded(15, conn.accept_stream()).await.unwrap();
    assert_eq!(kind, StreamKind::Blob);
    let req: BlobRequest = read_frame(&mut rx).await.unwrap().unwrap();
    write_frame(&mut tx, &BlobResponse::Ok { size: announce }).await.unwrap();
    let mut f = tokio::fs::File::open(&file).await.unwrap();
    tokio::io::AsyncSeekExt::seek(&mut f, std::io::SeekFrom::Start(req.offset)).await.unwrap();
    let mut body = vec![0u8; send];
    f.read_exact(&mut body).await.unwrap();
    tx.write_all(&body).await.unwrap();
    tx.write_all(extra).await.unwrap();
    if finish {
        tx.finish().unwrap();
        // Keep the stream alive until the receiver has read it all.
        let _ = tokio::time::timeout(Duration::from_secs(5), tx.stopped()).await;
    } else {
        // Dropping `tx` would finish the stream and look like a short body, not a cut connection:
        // hold it open until the connection is closed.
        let _ = tokio::time::timeout(Duration::from_secs(10), conn.closed()).await;
    }
    drop(tx);
    req
}

/// N10: 5 MB both directions, hash ok, fresh download.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n10_five_mb_both_directions() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let (a_file, b_file) = (dir.path().join("a.bin"), dir.path().join("b.bin"));
    let ha = make_file_bytes(&a_file, 5 * MB as usize, 1);
    let hb = make_file_bytes(&b_file, 5 * MB as usize, 2);
    assert_ne!(ha, hb);
    let _sa = spawn_blob_server(l.a_conn.clone(), MapSource::with(&ha, &a_file));
    let _sb = spawn_blob_server(l.b_conn.clone(), MapSource::with(&hb, &b_file));

    // phone fetches from PC
    let part1 = dir.path().join("from_a.part");
    let f1 = bounded(60, fetch_blob(&l.b_conn, &ha, 5 * MB, &part1, u64::MAX)).await.unwrap();
    assert_eq!((f1.hash.as_str(), f1.size, f1.resumed_from), (ha.as_str(), 5 * MB, 0));
    assert_eq!(std::fs::metadata(&part1).unwrap().len(), 5 * MB);
    assert_eq!(std::fs::read(&part1).unwrap(), std::fs::read(&a_file).unwrap());

    // PC fetches from phone
    let part2 = dir.path().join("from_b.part");
    let f2 = bounded(60, fetch_blob(&l.a_conn, &hb, 5 * MB, &part2, u64::MAX)).await.unwrap();
    assert_eq!((f2.hash.as_str(), f2.size, f2.resumed_from), (hb.as_str(), 5 * MB, 0));
    println!("5 MB fetch: {} ms and {} ms", f1.ms, f2.ms);
}

/// N11: the connection dies after >= 1 MB of 20 MB. The part is kept, a new connection
/// resumes from where it stopped, and the final hash is right.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n11_resume_after_a_cut_connection() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    let size = 20 * MB;
    let hash = make_file_bytes(&src, size as usize, 11);
    let parts = dir.path().join("parts");
    let part = parts.join(format!("{hash}.part"));

    // First attempt: the sender delivers 2 MB, the test waits for them to arrive, then the PC's
    // connection is closed under the transfer (a Wi-Fi drop looks the same to the receiver).
    let sender = {
        let (conn, src, part) = (l.a_conn.clone(), src.clone(), part.clone());
        tokio::spawn(async move {
            let held = tokio::spawn(scripted_sender(conn.clone(), src, 20 * MB, 2 * MB as usize, &[], false));
            // Cut only once every byte the sender writes is in the part file (which also means the
            // sender's write_all has returned): a readiness wait, not a guess at how long that takes.
            bounded(30, async {
                while std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0) < 2 * MB {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            conn.close(0, "test cut");
            held.await.unwrap()
        })
    };
    let err = bounded(30, fetch_blob(&l.b_conn, &hash, size, &part, u64::MAX)).await.unwrap_err();
    let req = sender.await.unwrap();
    assert_eq!(req.offset, 0);
    assert!(matches!(err, NetError::ClosedByPeer { code: 0, .. }), "a cut connection, not a short body: {err}");
    let kept = std::fs::metadata(&part).expect("part file is kept").len();
    assert!(kept >= MB && kept < size, "kept {kept}");

    // Second attempt on a fresh connection: resumes at `kept`.
    let b2 = bounded(15, l.b.node.dial(l.a.node.id(), &l.a.node.bound_addrs(), ALPN_SYNC, Duration::from_secs(10))).await.unwrap();
    let acc = bounded(15, l.a.node.accept()).await.unwrap().unwrap();
    let _server = spawn_blob_server(Arc::new(acc.peer), MapSource::with(&hash, &src));
    let got = bounded(60, fetch_blob(&b2, &hash, size, &part, u64::MAX)).await.unwrap();
    assert_eq!(got.resumed_from, kept);
    assert_eq!(got.hash, hash);
    assert_eq!(std::fs::metadata(&part).unwrap().len(), size);

    // What the caller (core) does next: rename into place. No .part is left.
    let final_path = dir.path().join("final.bin");
    std::fs::rename(&part, &final_path).unwrap();
    assert!(listing(&parts).is_empty());
    assert_eq!(blake3::hash(&std::fs::read(&final_path).unwrap()).to_hex().to_string(), hash);
}

/// N12: same size, wrong bytes -> HashMismatch, part deleted, nothing to resume from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n12_wrong_bytes_are_rejected_and_the_part_deleted() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let (real, fake) = (dir.path().join("real.bin"), dir.path().join("fake.bin"));
    let hash = make_file_bytes(&real, 300_000, 1);
    make_file_bytes(&fake, 300_000, 2);
    // The PC's index says "hash" but the file on disk has other bytes of the same length.
    let _server = spawn_blob_server(l.a_conn.clone(), MapSource::with(&hash, &fake));
    let part = dir.path().join("x.part");
    let err = bounded(30, fetch_blob(&l.b_conn, &hash, 300_000, &part, u64::MAX)).await.unwrap_err();
    assert!(matches!(err, NetError::HashMismatch { ref expected, .. } if *expected == hash), "{err}");
    assert!(!part.exists(), "a corrupt part must not survive: it would poison every resume");
}

/// N12b: a corrupt prefix already on disk is caught by the whole-file hash and removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n12_corrupt_resume_prefix_is_caught() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.bin");
    let hash = make_file_bytes(&src, 600_000, 3);
    let _server = spawn_blob_server(l.a_conn.clone(), MapSource::with(&hash, &src));
    let part = dir.path().join("x.part");
    let mut prefix = std::fs::read(&src).unwrap()[..100_000].to_vec();
    prefix[500] ^= 0xFF;
    std::fs::write(&part, &prefix).unwrap();
    let err = bounded(30, fetch_blob(&l.b_conn, &hash, 600_000, &part, u64::MAX)).await.unwrap_err();
    assert!(matches!(err, NetError::HashMismatch { .. }), "{err}");
    assert!(!part.exists());
    // The next try starts from zero and succeeds.
    let ok = bounded(30, fetch_blob(&l.b_conn, &hash, 600_000, &part, u64::MAX)).await.unwrap();
    assert_eq!(ok.resumed_from, 0);
}

/// N13: a size the record does not have, and bytes after the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n13_size_mismatch_and_trailing_bytes() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.bin");
    let hash = make_file_bytes(&src, 200_000, 4);
    let part = dir.path().join("x.part");

    // The sender announces a different size than the record: refused, nothing written.
    let s = tokio::spawn(scripted_sender(l.a_conn.clone(), src.clone(), 200_001, 0, &[], true));
    let err = bounded(30, fetch_blob(&l.b_conn, &hash, 200_000, &part, u64::MAX)).await.unwrap_err();
    s.await.unwrap();
    assert!(matches!(err, NetError::SizeMismatch { expected: 200_000, got: 200_001 }), "{err}");
    assert!(!part.exists() || std::fs::metadata(&part).unwrap().len() == 0);

    // The right body plus one extra byte: Protocol error, part deleted, no file left behind.
    let s = tokio::spawn(scripted_sender(l.a_conn.clone(), src.clone(), 200_000, 200_000, b"X", true));
    let err = bounded(30, fetch_blob(&l.b_conn, &hash, 200_000, &part, u64::MAX)).await.unwrap_err();
    s.await.unwrap();
    assert!(matches!(err, NetError::Protocol(ref m) if m.contains("unexpected bytes")), "{err}");
    assert!(!part.exists());
    assert!(listing(dir.path()).iter().all(|n| !n.ends_with(".part")));
}

/// A body that ends early (sender sends FIN too soon): error, no verified file, the received
/// prefix stays as the resume point.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n13_truncated_body_is_an_error_and_keeps_only_the_prefix() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.bin");
    let hash = make_file_bytes(&src, 200_000, 5);
    let part = dir.path().join("x.part");
    let s = tokio::spawn(scripted_sender(l.a_conn.clone(), src.clone(), 200_000, 150_000, &[], true));
    let err = bounded(30, fetch_blob(&l.b_conn, &hash, 200_000, &part, u64::MAX)).await.unwrap_err();
    s.await.unwrap();
    assert!(matches!(err, NetError::Stream(ref m) if m.contains("50000 bytes missing")), "{err}");
    let kept = std::fs::read(&part).unwrap();
    assert_eq!(kept, std::fs::read(&src).unwrap()[..150_000]);
}

/// N14: free-space arithmetic, the size cap, and a size no disk has. None writes anything.
#[test]
fn n14_check_space() {
    assert!(check_space(100, 100).is_ok());
    assert!(check_space(0, 0).is_ok());
    let e = check_space(101, 100).unwrap_err();
    assert!(matches!(e, NetError::NoSpace { needed: 101, available: 100 }), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n14_size_over_the_cap_and_over_the_disk_write_nothing() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let hash = "a".repeat(64);
    let part = dir.path().join("x.part");

    let err = fetch_blob(&l.b_conn, &hash, 1001, &part, 1000).await.unwrap_err();
    assert!(matches!(err, NetError::Protocol(ref m) if m.contains("over the")), "{err}");
    assert!(!part.exists());

    let err = fetch_blob(&l.b_conn, &hash, 1 << 62, &part, u64::MAX).await.unwrap_err();
    assert!(matches!(err, NetError::NoSpace { .. }), "{err}");
    assert!(!part.exists());

    // A bad hash from a record is refused before anything else.
    let err = fetch_blob(&l.b_conn, "not-a-hash", 10, &part, u64::MAX).await.unwrap_err();
    assert!(matches!(err, NetError::Protocol(_)), "{err}");
}

/// N15: the sender's refusals reach the receiver with their reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n15_sender_refusals() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.bin");
    let hash = make_file_bytes(&src, 100_000, 6);
    let _server = spawn_blob_server(l.a_conn.clone(), MapSource::with(&hash, &src));
    let part = dir.path().join("x.part");

    // unknown hash
    let other = "b".repeat(64);
    let err = bounded(15, fetch_blob(&l.b_conn, &other, 100_000, &part, u64::MAX)).await.unwrap_err();
    assert!(matches!(err, NetError::BlobRefused(ref r) if r == "not found"), "{err}");

    // the record's size is not the file's size (the file changed since it was hashed)
    let err = bounded(15, fetch_blob(&l.b_conn, &hash, 100_001, &part, u64::MAX)).await.unwrap_err();
    assert!(matches!(err, NetError::BlobRefused(ref r) if r == "changed"), "{err}");
    assert!(!part.exists() || std::fs::metadata(&part).unwrap().len() == 0);

    // offset past the end: only a hand-written request can ask for it
    let (mut tx, mut rx) = l.b_conn.open_stream(StreamKind::Blob).await.unwrap();
    write_frame(&mut tx, &BlobRequest { hash: hash.clone(), size: 100_000, offset: 100_001 }).await.unwrap();
    let resp: BlobResponse = bounded(15, read_frame(&mut rx)).await.unwrap().unwrap();
    assert_eq!(resp, BlobResponse::Refused { reason: "bad offset".into() });

    // a request that is not a hash at all
    let (mut tx, mut rx) = l.b_conn.open_stream(StreamKind::Blob).await.unwrap();
    write_frame(&mut tx, &BlobRequest { hash: "../../etc/passwd".into(), size: 1, offset: 0 }).await.unwrap();
    let resp: BlobResponse = bounded(15, read_frame(&mut rx)).await.unwrap().unwrap();
    assert_eq!(resp, BlobResponse::Refused { reason: "bad request".into() });
}

/// A source whose file vanished between `open` and the read: refused as not found, no hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n15_file_gone_after_lookup_is_not_found() {
    struct Gone;
    impl BlobSource for Gone {
        fn open(&self, _h: &str) -> Option<std::path::PathBuf> {
            Some("Z:\\definitely\\not\\here.bin".into())
        }
    }
    let l = linked().await;
    let a = l.a_conn.clone();
    tokio::spawn(async move {
        let (_k, tx, rx) = a.accept_stream().await.unwrap();
        let _ = serve_blob(tx, rx, &Gone).await;
    });
    let dir = tempfile::tempdir().unwrap();
    let err = bounded(15, fetch_blob(&l.b_conn, &"c".repeat(64), 10, &dir.path().join("x.part"), u64::MAX)).await.unwrap_err();
    assert!(matches!(err, NetError::BlobRefused(ref r) if r == "not found"), "{err}");
}

/// N16: a part longer than the record is garbage: truncated, fresh fetch succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n16_part_longer_than_size_is_restarted() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.bin");
    let hash = make_file_bytes(&src, 400_000, 7);
    let _server = spawn_blob_server(l.a_conn.clone(), MapSource::with(&hash, &src));
    let part = dir.path().join("x.part");
    std::fs::write(&part, vec![9u8; 500_000]).unwrap();
    let got = bounded(30, fetch_blob(&l.b_conn, &hash, 400_000, &part, u64::MAX)).await.unwrap();
    assert_eq!(got.resumed_from, 0);
    assert_eq!(std::fs::read(&part).unwrap(), std::fs::read(&src).unwrap());
}

/// A part that is already complete (crash between the last byte and the rename) is re-verified
/// and handed back without asking the sender for any bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_part_is_verified_and_returned() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.bin");
    let hash = make_file_bytes(&src, 250_000, 8);
    let _server = spawn_blob_server(l.a_conn.clone(), MapSource::with(&hash, &src));
    let part = dir.path().join("x.part");
    std::fs::copy(&src, &part).unwrap();
    let got = bounded(30, fetch_blob(&l.b_conn, &hash, 250_000, &part, u64::MAX)).await.unwrap();
    assert_eq!(got.resumed_from, 250_000);
    assert_eq!(got.hash, hash);
}

/// An empty file transfers (size 0, no body bytes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_byte_blob_transfers() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("empty.bin");
    let hash = make_file_bytes(&src, 0, 0);
    let _server = spawn_blob_server(l.a_conn.clone(), MapSource::with(&hash, &src));
    let got = bounded(30, fetch_blob(&l.b_conn, &hash, 0, &dir.path().join("e.part"), u64::MAX)).await.unwrap();
    assert_eq!(got.size, 0);
}

/// Throughput on loopback, for the record. Run: cargo test -p tugboat-net --release -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn throughput_200_mb() {
    let l = linked().await;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    let size = 200 * MB;
    let hash = make_file_bytes(&src, size as usize, 200);
    let _server = spawn_blob_server(l.a_conn.clone(), MapSource::with(&hash, &src));
    let got = bounded(300, fetch_blob(&l.b_conn, &hash, size, &dir.path().join("x.part"), u64::MAX)).await.unwrap();
    println!("200 MB loopback: {} ms = {:.0} MB/s", got.ms, 200.0 / (got.ms as f64 / 1000.0));
}
