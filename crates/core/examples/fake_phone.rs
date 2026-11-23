//! fake_phone: tugboat-core in the phone role, for QA and for trying the PC app before the
//! Android app exists.
//!
//! Usage (binds 127.0.0.1 only unless --bind is given):
//!   cargo run -p tugboat-core --example fake_phone -- pair "<QR payload text>" [--data <dir>]
//!       dials the PC, prints the 6-digit code, waits for the PC user's click, prints the result
//!   cargo run -p tugboat-core --example fake_phone -- send <N> [--data <dir>]
//!       generates N test JPEGs (valid JPEG magic, unique bytes), adds them as captures, syncs,
//!       waits until the PC's receipts arrive, prints one JSON line per item
//!   cargo run -p tugboat-core --example fake_phone -- send-file <path> [--name <display name>] [--data <dir>]
//!       adds ANY file (pdf, docx, zip, no extension ...) as it is, syncs, waits for the PC's receipt
//!   cargo run -p tugboat-core --example fake_phone -- send-refused <name> [--ext <ext>] [--data <dir>]
//!       writes an item RECORD with an always-refused name (x.lnk, desktop.ini ...) the way a hostile
//!       or newer peer would, so the PC's Not synced list can be checked. No file is behind it.
//!   cargo run -p tugboat-core --example fake_phone -- list [--data <dir>]
//!       syncs once and prints every item it holds (name, folder, state, local)
//! Any command takes `--hold-secs <N>`: stay connected N seconds after it finishes (for screenshots of the
//! "phone connected" state), and `--proto <N>`: speak sync protocol N in the handshake (3 is current;
//! 1 plays a 0.1.x phone, 2 a 0.3.0 one: both devices must then show "Update Tugboat on the phone").
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
    store, AppStorage, Clock, Core, CoreConfig, ItemId, ItemQuery, LocalFileState, MediaKind, Role, SystemClock,
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
    let proto: Option<u32> = take_flag(&mut args, "--proto").map(|s| s.parse().map_err(|_| "bad --proto".to_string())).transpose()?;
    let name_flag = take_flag(&mut args, "--name");
    let ext_flag = take_flag(&mut args, "--ext");
    let cmd = args.first().cloned().ok_or("usage: fake_phone pair <payload> | send <N> | send-file <path> | send-refused <name> | list  [--data <dir>] [--bind <addr>] [--proto <N>]")?;
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
    if let Some(n) = proto {
        core.debug_set_proto(n);
    }
    core.start_network(NetworkConfig { bind, mdns: false, secret_key: key }).await.map_err(|e| e.to_string())?;

    let result = match cmd.as_str() {
        "pair" => pair(&core, args.get(1).ok_or("pair needs the QR payload text")?).await,
        "send" => send(&core, &data, args.get(1).ok_or("send needs a number")?.parse().map_err(|_| "N must be a number")?).await,
        "send-file" => send_file(&core, Path::new(args.get(1).ok_or("send-file needs a path")?), name_flag).await,
        "send-refused" => send_refused(&core, args.get(1).ok_or("send-refused needs a name")?, ext_flag).await,
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
    wait_confirmed(core, &ids).await?;
    println!("SENT {n} items, all confirmed by the PC");
    print_items(core, &ids);
    Ok(())
}

/// Wait until the paired PC has confirmed holding every item (its receipt for the item's hash).
/// Items no longer wait for it to expire (0.2.0); the receipt is what proves the PC got the file.
async fn wait_confirmed(core: &Core, ids: &[ItemId]) -> Result<(), String> {
    let pc = core.paired_device().map_err(|e| e.to_string())?.ok_or("not paired")?.id;
    let t0 = Instant::now();
    loop {
        // Confirmed means the item is there and the PC holds a receipt for its hash. A failed
        // query or a missing item is not a confirmation: stop and say which one.
        let mut waiting = 0;
        for id in ids {
            let held = core.debug_conn(|c| match store::get_item(c, *id) {
                Ok(Some((rec, _))) => store::peer_has_receipt(c, rec.content.val.hash, pc).map_err(|e| format!("could not read item {}: {e}", id.0)),
                Ok(None) => Err(format!("item {} is missing, it cannot be confirmed", id.0)),
                Err(e) => Err(format!("could not read item {}: {e}", id.0)),
            })?;
            if !held {
                waiting += 1;
            }
        }
        if waiting == 0 {
            return Ok(());
        }
        if t0.elapsed() > RECEIPT_WAIT {
            print_items(core, ids);
            return Err(format!("{waiting} of {} items are not confirmed by the PC after {}s", ids.len(), RECEIPT_WAIT.as_secs()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Any file as it is: the name defaults to the file's own name; a file with no extension stays one.
async fn send_file(core: &Core, path: &Path, name: Option<String>) -> Result<(), String> {
    wait_synced(core).await?;
    let name = match name {
        Some(n) => n,
        None => path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("the path has no file name")?,
    };
    let id = core
        .add_local_file(NewLocalFile { src: path.to_path_buf(), name, folder: None, captured_at: SystemClock.now_ms(), geo: None, thumb: None, how: AddKind::Added, ntfs: None })
        .map_err(|e| format!("{}: {e}", e.kind()))?;
    wait_confirmed(core, &[id]).await?;
    println!("SENT 1 file, confirmed by the PC");
    print_items(core, &[id]);
    Ok(())
}

/// A record with an always-refused name and no file behind it. The PC must list it as REFUSED and
/// never ask for its bytes. (A real phone cannot make one: `add_local_file` refuses the name.)
async fn send_refused(core: &Core, name: &str, ext: Option<String>) -> Result<(), String> {
    wait_synced(core).await?;
    let ext = ext.unwrap_or_else(|| tugboat_core::names::ext_of(name));
    let id = core.debug_add_record(name, &ext, MediaKind::File, None, LocalFileState::Refused).map_err(|e| e.to_string())?;
    // Give the live session a moment to send the change; the PC answers with a pull.
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("SENT refused record {name:?} (ext {ext:?})");
    print_items(core, &[id]);
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
