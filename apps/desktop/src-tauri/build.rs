fn main() {
    // The exe icon is compiled into resource.lib by tauri_build; cargo only re-runs this script for files it is told
    // about, so without this line a changed icon.ico left the OLD icon in the exe. Declared before
    // tauri_build::build(), which adds its own rerun lines.
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/icon.png");
    tauri_build::build();
    // Integration-test executables that link tauri code need the common-controls v6 manifest, or
    // they die at load with STATUS_ENTRYPOINT_NOT_FOUND (muda imports comctl32 v6 symbols).
    // Tauri's build step embeds a manifest only into the real binary, so give test targets
    // theirs here: the same manifest as Vera's scripts/cargo-test.ps1, linked in, so plain
    // `cargo test` works with no wrapper script. (Unit tests in src/ reference no window code,
    // the linker drops the muda code, and they load without it.)
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("tests.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}", manifest.display());
}
