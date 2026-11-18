//! One version everywhere: the crate, `tauri.conf.json` and `package.json`. The GitHub release
//! workflow compares them with the tag and fails the build when they differ; this fails earlier.

use std::path::Path;

fn json(rel: &str) -> serde_json::Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

#[test]
fn the_crate_tauri_config_and_package_json_all_say_the_same_version() {
    let krate = env!("CARGO_PKG_VERSION");
    let tauri = json("tauri.conf.json")["version"].as_str().unwrap().to_string();
    let package = json("../package.json")["version"].as_str().unwrap().to_string();
    assert_eq!(tauri, krate, "tauri.conf.json vs the crate");
    assert_eq!(package, krate, "package.json vs the crate");
}
