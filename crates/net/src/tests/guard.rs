//! N6. A real non-local path cannot be produced on loopback, so the violation path is driven
//! through `enforce_path`, the one function the event loop and the snapshot check both call.

use std::sync::{Arc, Mutex};

use iroh::TransportAddr;

use super::{bounded, linked, make_file, spawn_blob_server, MapSource};
use crate::{
    blob::fetch_blob,
    close,
    guard::{check_paths, enforce_path, first_non_local, OnViolation},
    NetError,
};

fn ip(s: &str) -> TransportAddr {
    TransportAddr::Ip(s.parse().unwrap())
}

#[test]
fn n6_first_non_local_table() {
    let relay = TransportAddr::Relay("https://relay.example.com".parse().unwrap());
    assert_eq!(first_non_local([&ip("127.0.0.1:1"), &ip("192.168.1.2:9")]), None);
    assert_eq!(first_non_local([]), None);
    assert_eq!(first_non_local([&ip("127.0.0.1:1"), &ip("8.8.8.8:1"), &ip("1.1.1.1:1")]), Some(ip("8.8.8.8:1")));
    assert_eq!(first_non_local([&relay, &ip("10.0.0.1:1")]), Some(relay.clone()));
    assert_eq!(first_non_local([&ip("[2001:db8::1]:1")]), Some(ip("[2001:db8::1]:1")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n6_loopback_paths_pass_and_a_guarded_connection_survives_a_transfer() {
    let l = linked().await;
    check_paths(&l.a_conn.conn).expect("loopback path is local");
    check_paths(&l.b_conn.conn).expect("loopback path is local");

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.bin");
    let hash = make_file(&src, 5);
    let _server = spawn_blob_server(l.a_conn.clone(), MapSource::with(&hash, &src));
    let part = dir.path().join("dst.part");
    let got = bounded(60, fetch_blob(&l.b_conn, &hash, 5 * 1024 * 1024, &part, u64::MAX)).await.unwrap();
    assert_eq!(got.hash, hash);
    // Both guards are still running and neither closed anything.
    assert!(l.a_conn.is_open() && l.b_conn.is_open());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n6_a_non_local_path_closes_the_connection_with_path_not_local() {
    let l = linked().await;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let cb: OnViolation = {
        let seen = seen.clone();
        Arc::new(move |a| seen.lock().unwrap().push(a))
    };

    // A local path: nothing happens.
    assert!(!enforce_path(&l.a_conn.conn, &ip("192.168.1.2:9"), &cb));
    assert!(l.a_conn.is_open());
    assert!(seen.lock().unwrap().is_empty());

    // A public path: a closes, calls the callback once, and b sees the code and the reason.
    assert!(enforce_path(&l.a_conn.conn, &ip("8.8.8.8:47300"), &cb));
    assert_eq!(*seen.lock().unwrap(), vec!["8.8.8.8:47300".to_string()]);
    let err = bounded(10, l.b_conn.closed()).await;
    match err {
        NetError::ClosedByPeer { code, reason } => {
            assert_eq!(code, close::PATH_NOT_LOCAL);
            assert!(reason.contains("8.8.8.8:47300"), "{reason}");
        }
        other => panic!("expected ClosedByPeer, got {other}"),
    }
    assert!(!l.a_conn.is_open());
}

// A violation must reach the owner of the node (core turns it into a visible error), for a
// connection made before the handler was installed and for the dialing side too. Before this,
// the handler was a no-op and a closed path left no trace on the enforcing device.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n6_violation_reaches_the_node_owner_on_both_sides() {
    let l = linked().await;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    for node in [&l.a.node, &l.b.node] {
        let seen = seen.clone();
        node.set_violation_handler(Arc::new(move |a| seen.lock().unwrap().push(a)));
    }
    assert!(!l.a_conn.enforce_path(&ip("10.1.2.3:9")), "a local path is fine");
    assert!(seen.lock().unwrap().is_empty());

    assert!(l.a_conn.enforce_path(&ip("8.8.8.8:47300")));
    assert_eq!(*seen.lock().unwrap(), vec!["8.8.8.8:47300".to_string()], "accepting side reports");
    let _ = bounded(10, l.b_conn.closed()).await;

    // The dialed side of a fresh link reports too.
    let l2 = linked().await;
    let seen2: Arc<Mutex<Vec<String>>> = Arc::default();
    {
        let seen2 = seen2.clone();
        l2.b.node.set_violation_handler(Arc::new(move |a| seen2.lock().unwrap().push(a)));
    }
    assert!(l2.b_conn.enforce_path(&ip("[2001:db8::1]:1")));
    assert_eq!(seen2.lock().unwrap().len(), 1, "dialing side reports");
}
