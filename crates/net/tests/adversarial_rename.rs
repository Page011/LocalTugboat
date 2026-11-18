//! Adversarial rename check: the Android deep-link intent filter must accept the exact scheme
//! the PC puts in its pairing QR, or a camera app that scans the QR cannot open the phone app.

use std::path::PathBuf;
use tugboat_net::pairing::PairingWindow;

#[test]
fn android_intent_filter_scheme_matches_pairing_payload_scheme() {
    let win = PairingWindow::open([7u8; 32], 1_790_000_000_000).expect("open window");
    let text = win.payload(vec!["192.168.1.5:47300".parse().unwrap()], "PC").encode();
    let scheme = text.split("://").next().unwrap().to_ascii_lowercase();

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/android/app/src/main/AndroidManifest.xml");
    let xml = std::fs::read_to_string(&manifest).expect("read AndroidManifest.xml");
    let want = format!("android:scheme=\"{scheme}\"");
    assert!(xml.contains(&want), "manifest has no {want}; payload is {text}");
}
