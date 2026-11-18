//! fake_pc: tugboat-core in the PC role, for testing the phone app (emulator or real phone)
//! before the desktop app is used. Counterpart of fake_phone. Test tool only: the key is a
//! plain file and `--auto-confirm` skips the person who must compare the two codes.
//!
//! Usage (binds 127.0.0.1 only, so no Windows Firewall dialog):
//!   cargo run -p tugboat-core --example fake_pc -- [options]
//!     --data <dir>          default %TEMP%\tugboat-fake-pc (db, key, storage)
//!     --port <n>            UDP port, default 47300
//!     --advertise <ip:port> put this address in the pairing payload instead of 127.0.0.1:<port>.
//!                           The Android emulator reaches the host's loopback at 10.0.2.2, so
//!                           use --advertise 10.0.2.2:47300
//!     --auto-confirm        accept the pairing code without asking (test only)
//!     --push-dir <dir>      every new file that appears in <dir> is pushed to the phone: photos, videos
//!                           and ANY other type (docx, zip, a file with no extension). An always-refused
//!                           name (x.lnk, desktop.ini) is turned away and printed as push_error.
//!     --send-refused <name> once paired and connected, write a record with that always-refused name and
//!                           no file behind it (the way a hostile or newer peer would), so the phone
//!                           must show it as REFUSED and never ask for its bytes
//!     --proto <n>           speak sync protocol n in the handshake (2 is current; 1 plays a 0.1.x PC:
//!                           both devices must then show "Update Tugboat on the PC")
//!     --new-pairing         allow pairing a new phone although one is paired (replaces it)
//! Without --auto-confirm, type `y` or `n` and Enter after a code is printed.
//!
//! Output: one JSON object per line on stdout, key "event":
//!   started, payload (the QR text), pairing_code, pairing_completed, pairing_cancelled, sync,
//!   received (a phone file arrived and verified: id, name, size, path, blake3, thumb_path),
//!   refused_item (a phone record this side will never hold: id, name), pushed, push_error, error.
//! Run until killed (kill it by its PID).

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tugboat_core::{
    events::{Event, PairingEvent},
    facade::{device_id_from_secret, NetworkConfig},
    ops::{hash_file, AddKind, NewLocalFile},
    AppStorage, Clock, Core, CoreConfig, ItemId, ItemQuery, LocalFileState, Role, SystemClock,
};
use tugboat_net::pairing::PairPayload;
use tokio::sync::mpsc;

struct Opts {
    data: PathBuf,
    port: u16,
    advertise: Option<SocketAddr>,
    auto_confirm: bool,
    push_dir: Option<PathBuf>,
    new_pairing: bool,
    proto: Option<u32>,
    send_refused: Option<String>,
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = rt.block_on(run()) {
        emit(json!({"event": "error", "kind": "fatal", "message": e}));
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}

fn emit(v: Value) {
    println!("{v}");
}

fn take_flag(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(i) = args.iter().position(|a| a == name) else { return Ok(None) };
    if i + 1 >= args.len() {
        return Err(format!("{name} needs a value"));
    }
    let v = args.remove(i + 1);
    args.remove(i);
    Ok(Some(v))
}

fn take_switch(args: &mut Vec<String>, name: &str) -> bool {
    match args.iter().position(|a| a == name) {
        Some(i) => {
            args.remove(i);
            true
        }
        None => false,
    }
}

fn parse_opts() -> Result<Opts, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let data = take_flag(&mut args, "--data")?.map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("tugboat-fake-pc"));
    let port = match take_flag(&mut args, "--port")? {
        Some(p) => p.parse().map_err(|_| format!("bad --port {p:?}"))?,
        None => 47300,
    };
    let advertise = match take_flag(&mut args, "--advertise")? {
        Some(a) => Some(a.parse().map_err(|e| format!("bad --advertise {a:?}: {e}"))?),
        None => None,
    };
    let push_dir = take_flag(&mut args, "--push-dir")?.map(PathBuf::from);
    let auto_confirm = take_switch(&mut args, "--auto-confirm");
    let new_pairing = take_switch(&mut args, "--new-pairing");
    let proto = take_flag(&mut args, "--proto")?.map(|p| p.parse().map_err(|_| format!("bad --proto {p:?}"))).transpose()?;
    let send_refused = take_flag(&mut args, "--send-refused")?;
    if let Some(extra) = args.first() {
        return Err(format!("unknown argument {extra:?}; see the header of fake_pc.rs"));
    }
    Ok(Opts { data, port, advertise, auto_confirm, push_dir, new_pairing, proto, send_refused })
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

/// Open a pairing window and print its QR text, with the advertised address swapped in.
fn open_window(core: &Core, opts: &Opts) -> Result<(), String> {
    let text = core.begin_pairing(opts.new_pairing).map_err(|e| e.to_string())?;
    let text = match opts.advertise {
        None => text,
        Some(addr) => {
            let mut p = PairPayload::parse(&text).map_err(|e| format!("own payload does not parse: {e}"))?;
            p.addrs = vec![addr];
            p.encode()
        }
    };
    emit(json!({"event": "payload", "payload": text, "note": "valid for 2 minutes; the phone scans or pastes this text"}));
    Ok(())
}

async fn run() -> Result<(), String> {
    let opts = parse_opts()?;
    std::fs::create_dir_all(&opts.data).map_err(|e| format!("cannot create {}: {e}", opts.data.display()))?;
    let key = load_or_create_key(&opts.data.join("key"))?;
    let id = device_id_from_secret(&key);
    let cfg = CoreConfig { data_dir: opts.data.clone(), role: Role::Pc, self_id: id, device_name: "Fake PC".into(), app_version: "fake-pc".into() };
    let storage = AppStorage::new(opts.data.join("storage")).map_err(|e| e.to_string())?;
    let (core, mut rx) = Core::open(cfg, Arc::new(SystemClock), Arc::new(storage)).map_err(|e| e.to_string())?;
    if let Some(n) = opts.proto {
        core.debug_set_proto(n);
    }
    let bind: SocketAddr = format!("127.0.0.1:{}", opts.port).parse().map_err(|e| format!("bad port: {e}"))?;
    core.start_network(NetworkConfig { bind, mdns: false, secret_key: key }).await.map_err(|e| e.to_string())?;
    emit(json!({"event": "started", "id": id.to_string(), "bind": bind.to_string(), "data": opts.data}));

    let paired = core.paired_device().map_err(|e| e.to_string())?;
    match &paired {
        Some(p) if !opts.new_pairing => emit(json!({"event": "already_paired", "peer": p.name, "id": p.id.to_string()})),
        _ => open_window(&core, &opts)?,
    }

    // Typed lines from stdin ("y" / "n") answer the pairing question when --auto-confirm is off.
    let (line_tx, mut lines) = mpsc::unbounded_channel::<String>();
    if !opts.auto_confirm {
        std::thread::spawn(move || {
            let mut s = String::new();
            while std::io::stdin().read_line(&mut s).is_ok_and(|n| n > 0) {
                if line_tx.send(s.trim().to_string()).is_err() {
                    return;
                }
                s.clear();
            }
        });
    }

    let mut reported: HashSet<ItemId> = HashSet::new();
    let mut refused_sent = opts.send_refused.is_none();
    let mut seen_files: HashMap<PathBuf, (u64, u32)> = HashMap::new();
    let mut handled_files: HashSet<PathBuf> = HashSet::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    let mut last_second = Instant::now();

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(e) => on_event(&core, &opts, e).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => emit(json!({"event": "error", "kind": "lagged", "message": format!("{n} events dropped")})),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("the core closed its event stream".into()),
            },
            Some(line) = lines.recv() => match line.as_str() {
                "y" | "Y" => report("pairing confirm", core.confirm_pairing(true).await),
                "n" | "N" => report("pairing cancel", core.confirm_pairing(false).await),
                other => emit(json!({"event": "error", "kind": "input", "message": format!("type y or n, not {other:?}")})),
            },
            _ = ticker.tick() => {
                if last_second.elapsed() >= Duration::from_secs(1) {
                    last_second = Instant::now();
                    // Expires the pairing window and sweeps the Bin.
                    report("tick", core.tick().map(|_| ()));
                }
                report_received(&core, id, &mut reported);
                if !refused_sent && matches!(core.status().state, tugboat_core::events::SyncState::Synced) {
                    refused_sent = true;
                    let name = opts.send_refused.clone().unwrap_or_default();
                    match core.debug_add_record(&name, &tugboat_core::names::ext_of(&name), tugboat_core::MediaKind::File, None, LocalFileState::Refused) {
                        Ok(rid) => emit(json!({"event": "refused_record_sent", "id": rid.0.to_string(), "name": name})),
                        Err(e) => emit(json!({"event": "error", "kind": e.kind(), "message": format!("send-refused: {e}")})),
                    }
                }
                if let Some(dir) = &opts.push_dir {
                    push_new_files(&core, dir, &mut seen_files, &mut handled_files);
                }
            }
        }
    }
}

fn report<E: std::fmt::Display>(what: &str, r: Result<(), E>) {
    if let Err(e) = r {
        emit(json!({"event": "error", "kind": "failed", "message": format!("{what}: {e}")}));
    }
}

async fn on_event(core: &Core, opts: &Opts, e: Event) -> Result<(), String> {
    match e {
        Event::Pairing(PairingEvent::Code { code, peer_name }) => {
            emit(json!({"event": "pairing_code", "code": code, "peer_name": peer_name, "auto_confirm": opts.auto_confirm}));
            if opts.auto_confirm {
                report("pairing confirm", core.confirm_pairing(true).await);
            }
        }
        Event::Pairing(PairingEvent::Completed { peer_name, peer }) => emit(json!({"event": "pairing_completed", "peer_name": peer_name, "id": peer.to_string()})),
        Event::Pairing(PairingEvent::Cancelled { code, message }) => {
            emit(json!({"event": "pairing_cancelled", "code": code, "message": message}));
            // A cancelled or expired window is replaced so a test can retry without a restart,
            // unless a phone is already paired and the caller did not ask for a new pairing.
            let paired = core.paired_device().map_err(|e| e.to_string())?.is_some();
            if !paired || opts.new_pairing {
                open_window(core, opts)?;
            }
        }
        Event::Pairing(PairingEvent::Unpaired { peer }) => emit(json!({"event": "unpaired", "id": peer.to_string()})),
        Event::Sync(s) => emit(json!({"event": "sync", "status": s})),
        Event::Error { kind, message } => emit(json!({"event": "error", "kind": kind, "message": message})),
        _ => {}
    }
    Ok(())
}

/// Print every item the phone made that this side now holds and has verified. Items are
/// polled, not taken from events, because a file is "received" only when its download is done.
fn report_received(core: &Core, me: tugboat_core::DeviceId, reported: &mut HashSet<ItemId>) {
    let page = match core.items(&ItemQuery { folder: None, bin: false, after: None, limit: 500 }) {
        Ok(p) => p,
        Err(e) => {
            emit(json!({"event": "error", "kind": e.kind(), "message": e.to_string()}));
            return;
        }
    };
    for v in page.rows {
        if v.origin != me && v.local == LocalFileState::Refused && reported.insert(v.id) {
            emit(json!({"event": "refused_item", "id": v.id.0.to_string(), "name": v.name}));
            continue;
        }
        if v.origin == me || v.local != LocalFileState::Present || reported.contains(&v.id) {
            continue;
        }
        let blake3 = v.path.as_deref().map(|p| hash_file(Path::new(p)).map(|(h, _)| h.to_string()));
        reported.insert(v.id);
        emit(json!({
            "event": "received", "id": v.id.0.to_string(), "name": v.name, "size": v.size, "path": v.path,
            "thumb_path": v.thumb_path, "blake3": match blake3 { Some(Ok(h)) => Value::String(h), Some(Err(e)) => Value::String(format!("hash failed: {e}")), None => Value::Null },
            "folder": v.folder.0.to_string(),
        }));
    }
}

/// A file counts as finished when its size is the same on two polls in a row (copy done).
/// Each file is pushed once; a failure is printed once, not every half second.
fn push_new_files(core: &Core, dir: &Path, seen: &mut HashMap<PathBuf, (u64, u32)>, handled: &mut HashSet<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            if handled.insert(dir.to_path_buf()) {
                emit(json!({"event": "push_error", "name": dir, "error": format!("cannot read the push dir: {e}")}));
            }
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || handled.contains(&path) || !path.is_file() {
            continue;
        }
        let Ok(len) = entry.metadata().map(|m| m.len()) else { continue };
        let stable = match seen.get(&path) {
            Some((l, n)) if *l == len => *n + 1,
            _ => 0,
        };
        seen.insert(path.clone(), (len, stable));
        if stable < 1 {
            continue;
        }
        handled.insert(path.clone());
        let t0 = Instant::now();
        // The desktop makes the thumbnail of a dropped file; this tool has no image decoder, so
        // a ready JPEG at <push-dir>/.thumbs/<name> is used when present.
        let thumb = dir.join(".thumbs").join(&name);
        let thumb = if thumb.is_file() { Some(thumb) } else { None };
        let r = core.add_local_file(NewLocalFile {
            src: path.clone(),
            name: name.clone(),
            folder: None,
            captured_at: SystemClock.now_ms(),
            geo: None,
            thumb,
            how: AddKind::Added,
            ntfs: None,
        });
        match r {
            Ok(id) => emit(json!({"event": "pushed", "id": id.0.to_string(), "name": name, "size": len, "ms": t0.elapsed().as_millis() as u64})),
            Err(e) => emit(json!({"event": "push_error", "name": name, "error": e.to_string()})),
        }
    }
}
