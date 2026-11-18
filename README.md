# Local Tugboat

Local Tugboat ("Tugboat") syncs photos and files between your Android phone and Windows PC over your own Wi-Fi only. Files are temporary by default.

## Download

- Windows: installer from https://github.com/Page011/LocalTugboat/releases/latest
  (the installer is not code-signed yet; Windows may warn: More info > Run anyway)
- Android: Google Play (coming soon; link added when published)

## Make a release

1. Set the same version in `apps/desktop/src-tauri/tauri.conf.json`, `apps/desktop/package.json` and `[workspace.package]` in the root `Cargo.toml` (the desktop crate inherits it). Run `cargo check -p tugboat-desktop` so `Cargo.lock` updates, and commit all four files.
2. Commit to `main` and push `main` first. Then tag `vX.Y.Z` on that commit and push the tag (a tag pushed before its commit is on `main` on GitHub fails the run after the full build; push `main`, then re-run the workflow).
3. GitHub Actions (`.github/workflows/release.yml`) runs the checks, builds the installer and creates a draft release with the installer and its `.sha256` file. The run fails if the tag and the three versions differ, if the tag is not on `main`, if the tag was moved during the run, or if `Cargo.lock` is out of date.
4. Check the draft release and its installer, then publish it.

The Android version (`versionName` and `versionCode` in `apps/android/app/build.gradle.kts`) is set separately; the workflow does not check it.

To test the build on GitHub without a tag, run the workflow manually (Actions, Release, Run workflow); the installer is kept as a workflow artifact.

## Build from source

Needs Bun and Rust (see `RUST_VERSION` and `BUN_VERSION` in the release workflow for the pinned versions).

- Windows: also needs the Visual Studio C++ Build Tools (MSVC Rust target). In `apps/desktop` run `bun install`, then `bun run tauri build`. The installer lands in `target/release/bundle/nsis`.
- Android: needs the Android SDK with platform 37 (Gradle finds it through `ANDROID_HOME` or `sdk.dir` in `apps/android/local.properties`; Android Studio's SDK Manager installs it), the Rust targets `aarch64-linux-android` and `x86_64-linux-android`, `cargo-ndk`, NDK 30.0.16248370 (`ANDROID_NDK_HOME`, the default used by `build-rust.cmd`) and JDK 17. Run `apps/android/build-rust.cmd`, then `gradlew.bat bundleRelease` in `apps/android`. The bundle is unsigned: sign it before uploading to Google Play.

## Licence

Apache-2.0, see `LICENSE`.
