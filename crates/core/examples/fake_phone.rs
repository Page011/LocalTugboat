//! fake_phone: tugboat-core in the phone role, for QA and for trying the PC app before the
//! Android app exists. Owner: sync builder.
//!
//! Usage (binds 127.0.0.1 only unless --bind is given):
//!   cargo run -p tugboat-core --example fake_phone -- pair "<QR payload text>" [--data <dir>]
//!       dials the PC, prints the 6-digit code, waits for the PC user's click, prints the result
//!   cargo run -p tugboat-core --example fake_phone -- send <N> [--data <dir>]
//!       generates N test JPEGs (valid JPEG magic, unique bytes), adds them as captures, syncs,
//!       waits until the PC's receipts arrive, prints one JSON line per item
//!   cargo run -p tugboat-core --example fake_phone -- list [--data <dir>]
//!       syncs once and prints every item it holds (name, folder, state, local)
//! Any command takes `--hold-secs <N>`: stay connected N seconds after it finishes (for screenshots of the
//! "phone connected" state).
//! Default --data: %TEMP%\tugboat-fake-phone. The key lives in <data>\key (32 raw bytes)
//! because this is a test tool; the real phone keeps it in the Android Keystore.
//!
//! Use `--bind 127.0.0.1:0` (the default) with a PC app started with TUGBOAT_BIND=127.0.0.1.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use tugboat_core::{
    events::{Event, PairingEvent, SyncState},
    facade::{device_id_from_secret, NetworkConfig},
    ops::{AddKind, NewLocalFile},
    AppStorage, Clock, Core, CoreConfig, ItemId, ItemQuery, ItemState, Role, SystemClock,
};

/// How long to wait for the first connection and for the PC's receipts.
const SYNC_WAIT: Duration = Duration::from_secs(30);
const RECEIPT_WAIT: Duration = Duration::from_secs(60);

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = rt.block_on(run()) {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}

fn take_flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    if i + 1 >= args.len() {
        return None;
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Some(v)
}

/// The test key: 32 raw bytes, created on first use.
fn load_or_create_key(path: &Path) -> Result<[u8; 32], String> {
    if let Ok(b) = std::fs::read(path) {
        return <[u8; 32]>::try_from(b.as_slice()).map_err(|_| format!("{} is not 32 bytes", path.display()));
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| format!("no random numbers: {e}"))?;
    std::fs::write(path, key).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(key)
}

/// A JPEG-looking file: real JPEG magic and end marker, unique pseudo-random body.
fn fake_jpeg(seed: &str, len: usize) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00];
    let mut body = vec![0u8; len];
    blake3::Hasher::new().update(seed.as_bytes()).finalize_xof().fill(&mut body);
    out.extend_from_slice(&body);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

async fn wait_synced(core: &Core) -> Result<(), String> {
    let t0 = Instant::now();
    loop {
        let s = core.status();
        match s.state {
            SyncState::Synced => return Ok(()),
            SyncState::Refused => return Err(s.message.unwrap_or_else(|| "refused".into())),
            SyncState::Unpaired => return Err("not paired: run `pair` first".into()),
            _ => {}
        }
        if t0.elapsed() > SYNC_WAIT {
            return Err(format!("no sync within {}s (state {:?}); is the PC app running and bound to 127.0.0.1?", SYNC_WAIT.as_secs(), s.state));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let data = take_flag(&mut args, "--data").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("tugboat-fake-phone"));
    let bind: SocketAddr = take_flag(&mut args, "--bind").unwrap_or_else(|| "127.0.0.1:0".into()).parse().map_err(|e| format!("bad --bind: {e}"))?;
    let hold: u64 = take_flag(&mut args, "--hold-secs").map(|s| s.parse().map_err(|_| "bad --hold-secs".to_string())).transpose()?.unwrap_or(0);
    let cmd = args.first().cloned().ok_or("usage: fake_phone pair <payload> | send <N> | list  [--data <dir>] [--bind <addr>]")?;
    std::fs::create_dir_all(&data).map_err(|e| format!("cannot create {}: {e}", data.display()))?;
    let key = load_or_create_key(&data.join("key"))?;

    let cfg = CoreConfig {
        data_dir: data.clone(),
        role: Role::Phone,
        self_id: device_id_from_secret(&key),
        device_name: "Fake phone".into(),
        app_version: "fake-phone".into(),
    };
    let storage = AppStorage::new(data.join("storage")).map_err(|e| e.to_string())?;
    let (core, _rx) = Core::open(cfg, Arc::new(SystemClock), Arc::new(storage)).map_err(|e| e.to_string())?;
    core.start_network(NetworkConfig { bind, mdns: false, secret_key: key }).await.map_err(|e| e.to_string())?;

    let result = match cmd.as_str() {
        "pair" => pair(&core, args.get(1).ok_or("pair needs the QR payload text")?).await,
        "send" => send(&core, &data, args.get(1).ok_or("send needs a number")?.parse().map_err(|_| "N must be a number")?).await,
        "list" => list(&core).await,
        other => Err(format!("unknown command {other:?}")),
    };
    if hold > 0 && result.is_ok() {
        tokio::time::sleep(Duration::from_secs(hold)).await;
    }
    core.stop_network().await;
    result
}

async fn pair(core: &Core, payload: &str) -> Result<(), String> {
    let mut rx = core.subscribe();
    let printer = tokio::spawn(async move {
        while let Ok(e) = rx.recv().await {
            match e {
                Event::Pairing(PairingEvent::Code { code, peer_name }) => println!("CODE {code}   (the PC {peer_name} must show the same; click [Pair] there)"),
                Event::Pairing(PairingEvent::Cancelled { message, .. }) => println!("CANCELLED {message}"),
                _ => {}
            }
        }
    });
    let row = core.pair_with(payload).await.map_err(|e| e.to_string())?;
    printer.abort();
    println!("PAIRED with {} ({})", row.name, row.id);
    Ok(())
}

async fn send(core: &Core, data: &Path, n: u32) -> Result<(), String> {
    wait_synced(core).await?;
    let outbox = data.join("outbox");
    std::fs::create_dir_all(&outbox).map_err(|e| e.to_string())?;
    let now = SystemClock.now_ms();
    let mut ids: Vec<ItemId> = Vec::new();
    for i in 0..n {
        let name = format!("fake_{now}_{i}.jpg");
        let src = outbox.join(&name);
        std::fs::write(&src, fake_jpeg(&format!("{now}-{i}"), 256 * 1024)).map_err(|e| e.to_string())?;
        let id = core
            .add_local_file(NewLocalFile { src, name, folder: None, captured_at: now, geo: None, thumb: None, how: AddKind::Captured, ntfs: None })
            .map_err(|e| e.to_string())?;
        ids.push(id);
    }
    // An item shows Waiting until the PC has confirmed holding the file (its receipt).
    let t0 = Instant::now();
    loop {
        // Confirmed means the item is there and no longer Waiting. A failed query or a missing
        // item is not a confirmation: stop and say which one.
        let mut waiting = 0;
        for id in &ids {
            match core.item(*id) {
                Ok(Some(v)) if v.state == ItemState::Waiting => waiting += 1,
                Ok(Some(_)) => {}
                Ok(None) => return Err(format!("item {} is missing, it cannot be confirmed", id.0)),
                Err(e) => return Err(format!("could not read item {}: {e}", id.0)),
            }
        }
        if waiting == 0 {
            break;
        }
        if t0.elapsed() > RECEIPT_WAIT {
            print_items(core, &ids);
            return Err(format!("{waiting} of {n} items are still waiting for the PC after {}s", RECEIPT_WAIT.as_secs()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    println!("SENT {n} items in {} ms, all confirmed by the PC", t0.elapsed().as_millis());
    print_items(core, &ids);
    Ok(())
}

fn print_items(core: &Core, ids: &[ItemId]) {
    for id in ids {
        match core.item(*id) {
            Ok(Some(v)) => println!("{}", serde_json::to_string(&v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))),
            Ok(None) => println!("{{\"id\":\"{}\",\"error\":\"missing\"}}", id.0),
            Err(e) => println!("{{\"id\":\"{}\",\"error\":\"{e}\"}}", id.0),
        }
    }
}

async fn list(core: &Core) -> Result<(), String> {
    wait_synced(core).await?;
    // Let queued downloads finish: Synced is only reported when nothing is left to fetch.
    // Every page, then the true count first, so the total is never a silent first-page cut.
    let mut rows = Vec::new();
    let mut after = None;
    loop {
        let page = core.items(&ItemQuery { folder: None, bin: false, after, limit: 500 }).map_err(|e| e.to_string())?;
        let (more, last) = (page.next.is_some(), page.rows.last().map(|v| v.id));
        rows.extend(page.rows);
        match (more, last) {
            (true, Some(id)) => after = Some(id),
            _ => break,
        }
    }
    println!("{} items", rows.len());
    for v in rows {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    }
    Ok(())
}
