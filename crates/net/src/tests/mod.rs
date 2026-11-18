//! Test helpers shared by the net tests. Every endpoint binds 127.0.0.1 only (binding 0.0.0.0
//! pops a Windows Firewall dialog on the user's screen), and mDNS is always off.
//!
//! Numbering follows notes/core-design.md section 16 (N1..N24).

mod blob;
mod frame;
mod guard;
mod local;
mod node;
mod pairing;

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use iroh::SecretKey;
use tokio::task::JoinHandle;

use crate::{
    blob::{serve_blob, BlobSource},
    node::{AccessGate, Accepted, Node, NodeConfig, PeerConn, Role, StreamKind},
    ALPN_SYNC,
};

/// Paired keys, changeable at runtime like core's device table.
pub struct Gate(Mutex<HashSet<[u8; 32]>>);

impl Gate {
    pub fn new() -> Arc<Gate> {
        Arc::new(Gate(Mutex::new(HashSet::new())))
    }
    pub fn allow(&self, key: [u8; 32]) {
        self.0.lock().unwrap().insert(key);
    }
}

impl AccessGate for Gate {
    fn is_paired(&self, peer: &[u8; 32]) -> Result<bool, String> {
        Ok(self.0.lock().unwrap().contains(peer))
    }
}

pub struct TestNode {
    pub node: Node,
    pub gate: Arc<Gate>,
}

pub async fn start(role: Role) -> TestNode {
    let gate = Gate::new();
    let cfg = NodeConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        role,
        mdns: false,
        secret_key: SecretKey::generate().to_bytes(),
    };
    let node = Node::start(cfg, gate.clone()).await.expect("node starts");
    TestNode { node, gate }
}

/// Fails the test instead of hanging it.
pub async fn bounded<T>(secs: u64, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(secs), fut).await.expect("test step timed out")
}

/// PC `a` and phone `b`, paired with each other, `b` dialed `a` on the sync ALPN.
pub struct Linked {
    pub a: TestNode,
    pub b: TestNode,
    /// a's accepted connection from b.
    pub a_conn: Arc<PeerConn>,
    /// b's dialed connection to a.
    pub b_conn: Arc<PeerConn>,
}

pub async fn linked() -> Linked {
    let a = start(Role::Pc).await;
    let b = start(Role::Phone).await;
    a.gate.allow(b.node.id());
    b.gate.allow(a.node.id());
    let b_conn = bounded(15, b.node.dial(a.node.id(), &a.node.bound_addrs(), ALPN_SYNC, Duration::from_secs(10)))
        .await
        .expect("dial");
    let acc: Accepted = bounded(15, a.node.accept()).await.expect("accept open").expect("accept ok");
    assert_eq!(acc.alpn, ALPN_SYNC);
    Linked { a, b, a_conn: Arc::new(acc.peer), b_conn: Arc::new(b_conn) }
}

/// Deterministic pseudo-random file (xorshift) of `bytes` bytes. Returns its BLAKE3 hex.
pub fn make_file_bytes(path: &Path, bytes: usize, seed: u64) -> String {
    let mut f = std::fs::File::create(path).unwrap();
    let mut h = blake3::Hasher::new();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ seed;
    let mut left = bytes;
    let mut buf = vec![0u8; 1024 * 1024];
    while left > 0 {
        let n = left.min(buf.len());
        for c in buf[..n].chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let b = x.to_le_bytes();
            c.copy_from_slice(&b[..c.len()]);
        }
        f.write_all(&buf[..n]).unwrap();
        h.update(&buf[..n]);
        left -= n;
    }
    h.finalize().to_hex().to_string()
}

pub fn make_file(path: &Path, mb: usize) -> String {
    make_file_bytes(path, mb * 1024 * 1024, mb as u64)
}

/// hash -> path, like core's placement lookup.
#[derive(Default)]
pub struct MapSource(Mutex<HashMap<String, PathBuf>>);

impl MapSource {
    pub fn with(hash: &str, path: &Path) -> Arc<MapSource> {
        let s = MapSource::default();
        s.0.lock().unwrap().insert(hash.to_string(), path.to_path_buf());
        Arc::new(s)
    }
}

impl BlobSource for MapSource {
    fn open(&self, hash: &str) -> Option<PathBuf> {
        self.0.lock().unwrap().get(hash).cloned()
    }
}

/// Answers every Blob stream on `conn` with the real `serve_blob`; other kinds are ignored.
pub fn spawn_blob_server(conn: Arc<PeerConn>, source: Arc<dyn BlobSource>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((kind, tx, rx)) = conn.accept_stream().await {
            if kind != StreamKind::Blob {
                continue;
            }
            let source = source.clone();
            tokio::spawn(async move {
                let _ = serve_blob(tx, rx, &*source).await;
            });
        }
    })
}

/// Names of the files in `dir`.
pub fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}
