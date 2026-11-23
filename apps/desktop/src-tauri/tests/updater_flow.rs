//! The update check, download and signature check against a local server on 127.0.0.1 (nothing leaves the PC),
//! through the real updater plugin on Tauri's mock runtime. The installer itself is never started.
//!
//! The key pairs are throwaway ones made for these tests (the private halves were discarded). `SIGNATURE` signs
//! `PAYLOAD` for version 0.4.0 under `PUBKEY`; it was made by `tauri signer sign --app-version 0.4.0`, the same
//! signer a release build uses.
//!
//! One test, `the_real_endpoint_answers`, goes to the internet and is ignored by default:
//! `cargo test -p tugboat-desktop --test updater_flow -- --ignored`.

#![cfg(all(windows, target_arch = "x86_64"))]

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

use serde_json::json;
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};
use tugboat_desktop_lib::updates::{fetch, Checked, Failed, Failure, Overrides, Stage};

const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEQyM0ExMDlDM0RENjYyMjYKUldRbVl0WTluQkE2MHBHSWZiK0lwY3gzRVpkQVVPNzkyU3ZKMXBvV0dUY0QwSDFoQUVOV1V2S1oK";
const OTHER_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDk1RjZBNUFCRERCNjAxRDIKUldUU0FiYmRxNlgybFNnS3VldWU0NXhhRmxZQ1Rodjk3K1JQVytBdm1HcEVkN0ZRcW5CMXd4K2gK";
const PAYLOAD: &[u8] = b"Not a real installer: bytes for the updater signature test.";
const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRbVl0WTluQkE2MHVvZGtISU5WQXQrS2QwbUZiSWwwRElGc0dWTVE4OTF6RHVUdm9JNTlRV3hnallFY2ZRWUVhT1BuVjlZUElpdVdFT29zWncxN2RZOEZyTXJWMGpHbmdVPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk1NDcwMDcwCWZpbGU6cGF5bG9hZC5iaW4JdmVyc2lvbjowLjQuMApDWUVjbVFHRHhtV3cvbUoxMVNNc1ArRmZic0JWcCs2dDQxcE1ZRVR3RWR1UEtYSFY0eE1UOEVoQ0hxcHRwc3A2YkVFU1NyMXhSaTFoVnArQTRsTnJEZz09Cg==";
/// The installer is published under a different name than the one the signature's comment records.
const INSTALLER_PATH: &str = "/LocalTugboat_0.4.0_x64-setup.exe";

type Routes = HashMap<String, (u16, Vec<u8>)>;

struct Server {
    port: u16,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

fn answer(mut s: TcpStream, routes: &Routes, hits: &Mutex<Vec<String>>) {
    let mut buf = [0u8; 4096];
    let mut seen = Vec::new();
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
    let line = String::from_utf8_lossy(&seen).lines().next().unwrap_or("").to_string();
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    hits.lock().unwrap().push(path.clone());
    let (status, body) = routes.get(&path).cloned().unwrap_or((404, b"not found".to_vec()));
    let head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n", body.len());
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(&body);
}

/// Bind first (the manifest needs the port), then serve `routes(port)` until the test process ends.
fn serve(routes: impl FnOnce(u16) -> Routes) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let routes = Arc::new(routes(port));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let h = hits.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (routes, hits) = (routes.clone(), h.clone());
            thread::spawn(move || answer(stream, &routes, &hits));
        }
    });
    Server { port, hits }
}

fn manifest(version: &str, port: u16) -> Vec<u8> {
    json!({
        "version": version,
        "notes": "test",
        "pub_date": "2026-11-24T10:00:00Z",
        "platforms": { "windows-x86_64": { "signature": SIGNATURE, "url": format!("http://127.0.0.1:{port}{INSTALLER_PATH}") } }
    })
    .to_string()
    .into_bytes()
}

/// A normal release: `latest.json` announcing `version`, and the installer.
fn release(version: &'static str, installer: Vec<u8>) -> Server {
    serve(move |port| {
        let mut r = Routes::new();
        r.insert("/latest.json".into(), (200, manifest(version, port)));
        r.insert(INSTALLER_PATH.into(), (200, installer));
        r
    })
}

/// A mock app running this version with the updater plugin configured like `tauri.conf.json`.
fn app(current: &str, endpoint: Option<&str>) -> tauri::App<MockRuntime> {
    let mut ctx = mock_context(noop_assets());
    ctx.package_info_mut().version = current.parse().expect("test version");
    ctx.config_mut().plugins.0.insert(
        "updater".into(),
        json!({
            "pubkey": PUBKEY,
            "endpoints": endpoint.map(|e| vec![e.to_string()]).unwrap_or_default(),
            "requireSignedVersion": true,
            "dangerousInsecureTransportProtocol": true,
            "windows": { "installMode": "passive" }
        }),
    );
    mock_builder().plugin(tauri_plugin_updater::Builder::new().build()).build(ctx).expect("mock app")
}

fn ov(s: &Server) -> Overrides {
    Overrides { url: Some(s.url("/latest.json")), pubkey: None }
}

fn run(app: &tauri::App<MockRuntime>, o: &Overrides) -> Result<Checked, Failed> {
    tauri::async_runtime::block_on(fetch(app.handle(), o, None))
}

#[test]
fn a_newer_signed_release_is_downloaded_and_verified() {
    let s = release("0.4.0", PAYLOAD.to_vec());
    match run(&app("0.3.0", None), &ov(&s)) {
        Ok(Checked::Ready(p)) => assert_eq!(p.version, "0.4.0"),
        other => panic!("expected a verified update, got {other:?}"),
    }
    assert_eq!(s.hits(), ["/latest.json", INSTALLER_PATH]);
}

/// Minisign signs the file's bytes. The name in the signature's trusted comment (`payload.bin`) is only text, so
/// the installer can be renamed after signing (the release workflow renames it) without breaking the signature.
#[test]
fn renaming_the_installer_after_signing_does_not_break_the_signature() {
    let sig = String::from_utf8(base64_decode(SIGNATURE)).unwrap();
    assert!(sig.contains("file:payload.bin"), "the signed comment names another file than the one published");
    assert!(!INSTALLER_PATH.contains("payload.bin"));
    let s = release("0.4.0", PAYLOAD.to_vec());
    assert!(matches!(run(&app("0.3.0", None), &ov(&s)), Ok(Checked::Ready(_))));
}

fn base64_decode(s: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| *c != b'=') {
        acc = (acc << 6) | ALPHABET.iter().position(|a| *a == c).expect("base64") as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

#[test]
fn the_same_or_an_older_version_is_never_downloaded() {
    for offered in ["0.3.0", "0.2.9", "0.1.0"] {
        let s = release(offered, PAYLOAD.to_vec());
        let r = run(&app("0.3.0", None), &ov(&s));
        assert!(matches!(r, Ok(Checked::NoUpdate)), "offered {offered}: {r:?}");
        assert_eq!(s.hits(), ["/latest.json"], "offered {offered}: the installer was requested");
    }
}

#[test]
fn a_changed_download_is_rejected() {
    let mut bad = PAYLOAD.to_vec();
    bad[0] ^= 1;
    let s = release("0.4.0", bad);
    let e = run(&app("0.3.0", None), &ov(&s)).unwrap_err();
    assert_eq!(e, Failed { stage: Stage::Download, kind: Failure::Rejected });
}

#[test]
fn a_signature_from_another_key_is_rejected() {
    let s = release("0.4.0", PAYLOAD.to_vec());
    let o = Overrides { pubkey: Some(OTHER_PUBKEY.into()), ..ov(&s) };
    let e = run(&app("0.3.0", None), &o).unwrap_err();
    assert_eq!(e, Failed { stage: Stage::Download, kind: Failure::Rejected });
}

/// The manifest is not signed: it must not be able to pair a newer version number with an older signed file.
#[test]
fn a_manifest_that_announces_another_version_than_the_signed_one_is_rejected() {
    let s = release("0.5.0", PAYLOAD.to_vec());
    let e = run(&app("0.3.0", None), &ov(&s)).unwrap_err();
    assert_eq!(e, Failed { stage: Stage::Download, kind: Failure::Rejected });
}

#[test]
fn a_404_for_the_manifest_means_nothing_is_published_yet() {
    let s = serve(|_| Routes::new());
    let e = run(&app("0.3.0", None), &ov(&s)).unwrap_err();
    assert_eq!(e, Failed { stage: Stage::Check, kind: Failure::NoRelease });
    assert_eq!(s.hits(), ["/latest.json"], "one request, no retry loop");
}

#[test]
fn a_download_that_fails_is_a_server_failure() {
    let s = serve(|port| {
        let mut r = Routes::new();
        r.insert("/latest.json".into(), (200, manifest("0.4.0", port)));
        r.insert(INSTALLER_PATH.into(), (500, b"oops".to_vec()));
        r
    });
    let e = run(&app("0.3.0", None), &ov(&s)).unwrap_err();
    assert_eq!(e, Failed { stage: Stage::Download, kind: Failure::Server });
}

#[test]
fn nothing_listening_is_offline() {
    // A port that was just free and has no listener: the connection is refused.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let o = Overrides { url: Some(format!("http://127.0.0.1:{port}/latest.json")), pubkey: None };
    let e = run(&app("0.3.0", None), &o).unwrap_err();
    assert_eq!(e, Failed { stage: Stage::Check, kind: Failure::Offline });
}

#[test]
fn a_malformed_manifest_is_a_failure_not_an_update() {
    let s = serve(|_| {
        let mut r = Routes::new();
        r.insert("/latest.json".into(), (200, b"{\"version\": \"not a version\"}".to_vec()));
        r
    });
    assert!(run(&app("0.3.0", None), &ov(&s)).is_err());
}

/// Real HTTPS to GitHub, endpoint from the plugin config (no override). Until a release carries `latest.json` the
/// answer is a 404, which is "no update information", not a failure. Run on a normal PC and behind a company proxy
/// that re-signs HTTPS: the Windows certificate store decides, not a bundled list of roots.
#[test]
#[ignore = "needs the internet"]
fn the_real_endpoint_answers() {
    let endpoint = "https://github.com/Page011/LocalTugboat/releases/latest/download/latest.json";
    let r = run(&app("0.0.1", Some(endpoint)), &Overrides::default());
    println!("real endpoint: {r:?}");
    match r {
        Err(Failed { kind: Failure::NoRelease, .. }) | Ok(_) => {}
        Err(e) => panic!("the real endpoint did not answer cleanly: {e}"),
    }
}
