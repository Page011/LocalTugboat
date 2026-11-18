//! N3, N4, N7, N8, N22, N23, N24, and the QR address choice.

use std::time::{Duration, Instant};

use iroh::{endpoint::Connection, EndpointAddr, EndpointId, SecretKey};

use super::{bounded, linked, start};
use crate::{
    close,
    node::{mdns_advertise, select_qr_addrs, Role, StreamKind},
    NetError, ALPN_PAIR, ALPN_SYNC,
};

fn ghost() -> [u8; 32] {
    *SecretKey::generate().public().as_bytes()
}

/// N3: a public address is refused at once and nothing is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n3_dial_rejects_non_local_address_without_trying() {
    let n = start(Role::Phone).await;
    for bad in ["8.8.8.8:47300", "[2001:4860:4860::8888]:47300", "[::ffff:8.8.8.8]:47300"] {
        let t0 = Instant::now();
        let err = n.node.dial(ghost(), &[bad.parse().unwrap()], ALPN_SYNC, Duration::from_secs(5)).await.unwrap_err();
        let el = t0.elapsed();
        assert!(matches!(&err, NetError::NotLocal(a) if a == bad), "{err}");
        assert!(el < Duration::from_millis(50), "returned only after {el:?}, so something was attempted");
    }
    // One bad address among good ones still refuses the whole dial.
    let err = n
        .node
        .dial(ghost(), &["192.168.1.5:1".parse().unwrap(), "8.8.8.8:1".parse().unwrap()], ALPN_SYNC, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(err, NetError::NotLocal(_)), "{err}");
}

/// N4: the hook stops a public address even when iroh's own connect is called directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n4_before_connect_hook_rejects_a_public_ip() {
    let n = start(Role::Phone).await;
    let id = EndpointId::from_bytes(&ghost()).unwrap();
    for ea in [
        EndpointAddr::new(id).with_ip_addr("8.8.8.8:47300".parse().unwrap()),
        EndpointAddr::new(id).with_ip_addr("127.0.0.1:9".parse().unwrap()).with_ip_addr("1.1.1.1:9".parse().unwrap()),
        EndpointAddr::new(id).with_relay_url("https://relay.example.com".parse().unwrap()),
    ] {
        let t0 = Instant::now();
        let err = n.node.endpoint.connect(ea, ALPN_SYNC).await.unwrap_err();
        assert!(format!("{err:?}").contains("LocallyRejected") || err.to_string().contains("rejected"), "{err:?}");
        assert!(t0.elapsed() < Duration::from_millis(500));
    }
}

/// N7: sync ALPN is for paired keys only. An unpaired dialer is closed with NOT_PAIRED and
/// the PC reports the refusal; a paired key is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n7_unpaired_key_is_refused_on_the_sync_alpn() {
    let pc = start(Role::Pc).await;
    let stranger = start(Role::Phone).await;
    // The stranger trusts the PC (so its own hook lets the connection through) but the PC does not trust it.
    stranger.gate.allow(pc.node.id());
    let conn = bounded(15, stranger.node.dial(pc.node.id(), &pc.node.bound_addrs(), ALPN_SYNC, Duration::from_secs(10))).await.unwrap();
    let err = bounded(15, conn.closed()).await;
    assert!(matches!(&err, NetError::ClosedByPeer { code, reason } if *code == close::NOT_PAIRED && reason == "not paired"), "{err}");
    // No stream can be opened on it either, and the error says why.
    let err = conn.open_stream(StreamKind::Control).await.unwrap_err();
    assert!(matches!(err, NetError::ClosedByPeer { code, .. } if code == close::NOT_PAIRED), "{err}");
    // The PC's accept sees a refusal, never a usable connection.
    let acc = bounded(15, pc.node.accept()).await.unwrap();
    assert!(matches!(&acc, Err(e) if e.to_string().contains("rejected locally")), "{acc:?}");

    // The same key once paired is accepted and can talk.
    pc.gate.allow(stranger.node.id());
    let conn = bounded(15, stranger.node.dial(pc.node.id(), &pc.node.bound_addrs(), ALPN_SYNC, Duration::from_secs(10))).await.unwrap();
    let acc = bounded(15, pc.node.accept()).await.unwrap().unwrap();
    assert_eq!(acc.peer.peer(), stranger.node.id());
    let (_tx, _rx) = conn.open_stream(StreamKind::Control).await.unwrap();
    let (kind, ..) = bounded(10, acc.peer.accept_stream()).await.unwrap();
    assert_eq!(kind, StreamKind::Control);
}

/// The dialer's own hook also refuses a PC it does not pair with (unpaired from the phone side).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n7_dialer_refuses_a_pc_it_no_longer_pairs_with() {
    let pc = start(Role::Pc).await;
    let phone = start(Role::Phone).await;
    pc.gate.allow(phone.node.id());
    // phone.gate does not hold the PC key
    let err = phone.node.dial(pc.node.id(), &pc.node.bound_addrs(), ALPN_SYNC, Duration::from_secs(10)).await.unwrap_err();
    assert!(err.to_string().contains("rejected locally"), "{err}");
}

/// N8: ALPN_PAIR works only while the window is open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n8_pairing_alpn_follows_the_window() {
    let pc = start(Role::Pc).await;
    let phone = start(Role::Phone).await;
    let addrs = pc.node.bound_addrs();
    let dial = |t: Duration| phone.node.dial(pc.node.id(), &addrs, ALPN_PAIR, t);

    // closed: refused (the ALPN is not even offered)
    let err = dial(Duration::from_secs(5)).await.unwrap_err();
    println!("closed window: {err}");
    assert!(matches!(err, NetError::Connect(_) | NetError::Timeout(_)), "{err}");

    // the PC also reports the failed handshake, so its log shows why nothing paired
    let first = bounded(15, pc.node.accept()).await.unwrap();
    assert!(first.is_err(), "the refused attempt is reported as an error");

    // open: accepted, and the PC sees the pairing ALPN
    pc.node.set_pairing_open(true);
    let _conn = bounded(15, dial(Duration::from_secs(10))).await.unwrap();
    let acc = bounded(15, pc.node.accept()).await.unwrap().unwrap();
    assert_eq!(acc.alpn, ALPN_PAIR);

    // closed again: refused again
    pc.node.set_pairing_open(false);
    let err = dial(Duration::from_secs(5)).await.unwrap_err();
    assert!(matches!(err, NetError::Connect(_) | NetError::Timeout(_)), "{err}");

    // the sync ALPN is unaffected by the window
    pc.gate.allow(phone.node.id());
    phone.gate.allow(pc.node.id());
    let c = phone.node.dial(pc.node.id(), &pc.node.bound_addrs(), ALPN_SYNC, Duration::from_secs(10)).await.unwrap();
    assert!(c.is_open());
}

/// N8: the hook is the second lock. With the ALPN still offered but the flag closed (a race on
/// close), the connection is closed with PAIRING_CLOSED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n8_hook_refuses_pairing_when_the_flag_is_closed() {
    let pc = start(Role::Pc).await;
    let phone = start(Role::Phone).await;
    pc.node.endpoint.set_alpns(vec![ALPN_SYNC.to_vec(), ALPN_PAIR.to_vec()]); // flag stays false
    let conn = bounded(15, phone.node.dial(pc.node.id(), &pc.node.bound_addrs(), ALPN_PAIR, Duration::from_secs(10))).await.unwrap();
    let err = bounded(15, conn.closed()).await;
    assert!(matches!(err, NetError::ClosedByPeer { code, .. } if code == close::PAIRING_CLOSED), "{err}");
}

/// N22: only the PC advertises.
#[test]
fn n22_only_the_pc_advertises_over_mdns() {
    assert!(mdns_advertise(Role::Pc));
    assert!(!mdns_advertise(Role::Phone));
}

/// N23: kinds are routed; an unknown kind byte closes the connection with PROTOCOL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n23_stream_kinds_are_routed_and_unknown_ones_close_the_connection() {
    let l = linked().await;
    for kind in [StreamKind::Control, StreamKind::Blob, StreamKind::Pair] {
        let (_tx, _rx) = l.b_conn.open_stream(kind).await.unwrap();
        let (got, ..) = bounded(10, l.a_conn.accept_stream()).await.unwrap();
        assert_eq!(got, kind);
    }
    // streams opened by the PC arrive at the phone too
    let (_tx, _rx) = l.a_conn.open_stream(StreamKind::Blob).await.unwrap();
    let (got, ..) = bounded(10, l.b_conn.accept_stream()).await.unwrap();
    assert_eq!(got, StreamKind::Blob);

    // a raw stream with kind byte 99
    let raw: &Connection = &l.b_conn.conn;
    let (mut tx, _rx) = raw.open_bi().await.unwrap();
    tx.write_all(&[99]).await.unwrap();
    let err = bounded(10, l.a_conn.accept_stream()).await.unwrap_err();
    assert!(matches!(err, NetError::Protocol(ref m) if m.contains("99")), "{err}");
    let seen = bounded(10, l.b_conn.closed()).await;
    assert!(matches!(seen, NetError::ClosedByPeer { code, .. } if code == close::PROTOCOL), "{seen}");
    assert!(!l.a_conn.is_open());
}

/// N24: an id nobody knows, no address, mDNS off: a clear error, quickly (spike port).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n24_dial_unknown_id_fails_clearly_and_bounded() {
    let n = start(Role::Phone).await;
    let t0 = Instant::now();
    let err = n.node.dial(ghost(), &[], ALPN_SYNC, Duration::from_secs(8)).await.unwrap_err();
    let el = t0.elapsed();
    println!("dial unknown id, no addr: {el:?} -> {err}");
    assert!(el < Duration::from_secs(9), "took {el:?}");
    assert!(matches!(err, NetError::Connect(_) | NetError::Timeout(_)));
    assert!(err.to_string().contains("no address, mDNS off"), "{err}");

    // An address nobody listens on: bounded by the timeout, and the text names the attempt.
    let t0 = Instant::now();
    let err = n.node.dial(ghost(), &["127.0.0.1:9".parse().unwrap()], ALPN_SYNC, Duration::from_secs(3)).await.unwrap_err();
    let el = t0.elapsed();
    println!("dial dead address: {el:?} -> {err}");
    assert!(el < Duration::from_secs(4), "took {el:?}");
    assert!(err.to_string().contains("127.0.0.1:9"), "{err}");
}

/// Our own key is not a peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dialing_yourself_fails_clearly() {
    let n = start(Role::Phone).await;
    let err = n.node.dial(n.node.id(), &n.node.bound_addrs(), ALPN_SYNC, Duration::from_secs(3)).await.unwrap_err();
    assert!(matches!(err, NetError::Connect(_)), "{err}");
}

/// QR addresses: loopback QA bind returns loopback; LAN bind picks and orders LAN addresses.
#[test]
fn qr_addresses_are_chosen_and_ordered() {
    let sa = |s: &str| -> std::net::SocketAddr { s.parse().unwrap() };
    // bound to loopback only: the loopback address, so the fake phone can dial it
    assert_eq!(select_qr_addrs(&[sa("127.0.0.1:5")], &[sa("127.0.0.1:5")]), vec![sa("127.0.0.1:5")]);

    let bound = [sa("0.0.0.0:47300")];
    let direct = [
        sa("[fd12::1]:47300"),
        sa("10.1.1.9:47300"),
        sa("127.0.0.1:47300"),
        sa("169.254.4.4:47300"),
        sa("[fe80::1]:47300"),
        sa("172.20.0.3:47300"),
        sa("192.168.1.50:47300"),
        sa("192.168.1.50:47300"),
        sa("8.8.8.8:47300"),
        sa("192.168.56.1:47300"),
    ];
    let got = select_qr_addrs(&direct, &bound);
    // 192.168 first, then 10, then 172, then IPv6; loopback, link-local and public never; at most 4
    assert_eq!(got, vec![sa("192.168.1.50:47300"), sa("192.168.56.1:47300"), sa("10.1.1.9:47300"), sa("172.20.0.3:47300")]);

    // the port is the bound port, whatever port the direct addresses carried
    let got = select_qr_addrs(&[sa("192.168.1.2:1111")], &[sa("0.0.0.0:2222")]);
    assert_eq!(got, vec![sa("192.168.1.2:2222")]);
    // nothing usable: empty, so the caller can say "no LAN address"
    assert!(select_qr_addrs(&[sa("127.0.0.1:1"), sa("8.8.8.8:1")], &[sa("0.0.0.0:1")]).is_empty());
    assert!(select_qr_addrs(&[], &[]).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_on_loopback_reports_its_own_address() {
    let n = start(Role::Pc).await;
    assert_eq!(n.node.qr_addrs(), n.node.bound_addrs());
    assert!(n.node.bound_addrs()[0].port() != 0);
    assert_eq!(n.node.id().len(), 32);
}

/// A peer that vanishes WITHOUT closing (laptop lid shut, Wi-Fi gone, phone frozen by the OS)
/// sends no CONNECTION_CLOSE, so only the keep-alive / idle timeout can end the connection.
/// PLAN silent failure "Sync stopped": the survivor must see the connection end instead of a
/// frozen "Synced". Measured: 35 s, not the 30 s the design note claimed. quinn restarts the
/// idle timer when the first keep-alive goes out after a received packet, so the bound is
/// keep-alive (10 s) + idle (30 s) = 40 s worst case, 30 s best case.
///
/// The peer lives on its own single-thread runtime; "vanishing" blocks that one thread for good,
/// which freezes the endpoint exactly like a suspended process: the UDP socket stays open and
/// nothing answers, not even the QUIC acks. A graceful stop would not prove anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n_dead_peer_is_noticed_within_the_idle_timeout() {
    let pc = start(Role::Pc).await;
    let pc_addrs = pc.node.bound_addrs();
    let pc_id = pc.node.id();
    let phone_key = SecretKey::generate().to_bytes();
    let phone_id = *SecretKey::from(phone_key).public().as_bytes();
    pc.gate.allow(phone_id);

    let (linked_tx, linked_rx) = std::sync::mpsc::channel::<()>();
    let (freeze_tx, freeze_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let gate = super::Gate::new();
            gate.allow(pc_id);
            let cfg = crate::node::NodeConfig { bind: "127.0.0.1:0".parse().unwrap(), role: Role::Phone, mdns: false, secret_key: phone_key };
            let node = crate::node::Node::start(cfg, gate).await.expect("phone node");
            let _conn = node.dial(pc_id, &pc_addrs, ALPN_SYNC, Duration::from_secs(10)).await.expect("dial");
            linked_tx.send(()).unwrap();
            // Wait (without blocking the runtime thread) until told to vanish.
            let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
            std::thread::spawn(move || {
                let _ = freeze_rx.recv();
                let _ = go_tx.send(());
            });
            let _ = go_rx.await;
            // Freeze: this is the only thread of the runtime, so the endpoint stops answering.
            std::thread::sleep(Duration::from_secs(120));
        });
    });

    bounded(15, async { while linked_rx.try_recv().is_err() { tokio::time::sleep(Duration::from_millis(20)).await } }).await;
    let acc = bounded(15, pc.node.accept()).await.expect("accept open").expect("accept ok");
    assert!(acc.peer.is_open());
    freeze_tx.send(()).unwrap();
    let t0 = Instant::now();
    let err = bounded(55, acc.peer.closed()).await;
    let el = t0.elapsed();
    assert!(el >= Duration::from_secs(25), "noticed after only {el:?}: the idle timeout is not what the test thinks");
    assert!(el < Duration::from_secs(42), "dead peer noticed only after {el:?}");
    assert!(matches!(err, NetError::Timeout(_) | NetError::Stream(_)), "a silent peer ends in a timeout, got: {err}");
    assert!(!matches!(err, NetError::ClosedByPeer { .. }), "the peer never said goodbye: {err}");
    assert!(!acc.peer.is_open());
}
