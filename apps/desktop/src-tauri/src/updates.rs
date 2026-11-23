//! Update check: once a day the PC asks GitHub whether a newer version is published, downloads it in the
//! background, checks its signature and offers it (a banner and a tray item). It sends nothing about the
//! user or the files: one ordinary HTTPS request for `latest.json`, then one for the installer.
//!
//! - The pure part (`decide`, `is_newer`, `overrides`, `needs_pc_update`, `classify`) has no Tauri types and is
//!   unit tested.
//! - `fetch` is generic over the Tauri runtime, so the whole check + download + verify runs in a test against a
//!   local server.
//! - `Updates` is the managed state: the switch, the last check time, the downloaded update and the schedule.
//!
//! TLS: the plugin is built with its `native-tls` feature (SChannel on Windows), so HTTPS trusts the Windows
//! certificate store. That is what makes it work behind a company proxy that re-signs HTTPS with its own root.
//!
//! The key and the endpoint live in `tauri.conf.json` (`plugins.updater`). While the key is still the placeholder
//! the app never checks. Debug builds only: `TUGBOAT_UPDATE_URL` and `TUGBOAT_UPDATE_PUBKEY` replace the
//! endpoint and the key for tests; a release build never reads them.

use std::{
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter as _, Manager, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Notify;
use tugboat_core::events::{Event, SyncState, SyncStatus};
use ts_rs::TS;

use crate::{
    backend::{ev, Backend},
    commands::MAIN,
    error::{CmdError, CmdResult},
    shell,
};

/// What `plugins.updater.pubkey` holds until the owner makes the signing key.
pub const PLACEHOLDER_PUBKEY: &str = "UPDATER_PUBKEY_MISSING";
/// Debug builds only: use this endpoint instead of the one in `tauri.conf.json`.
pub const URL_ENV: &str = "TUGBOAT_UPDATE_URL";
/// Debug builds only: use this public key instead of the one in `tauri.conf.json`.
pub const KEY_ENV: &str = "TUGBOAT_UPDATE_PUBKEY";
/// Whether the two variables above are read at all: debug builds only.
pub const OVERRIDES_ALLOWED: bool = cfg!(debug_assertions);

/// Core setting key (local to this PC, never synced): "false" = checks off. Missing = on.
pub const SETTING_KEY: &str = "check_updates";
/// Core setting key: unix seconds of the last check that went out.
pub const LAST_CHECK_KEY: &str = "update_last_check";
/// Core setting key: the version a check found and downloaded, kept until it is installed or gone. After a restart
/// the update is fetched and verified again soon after start, so a declined prompt is not forgotten for a day.
pub const FOUND_KEY: &str = "update_found";
/// Core setting key: set just before an install starts from the window or the tray. The installer restarts the
/// app with the arguments of the running one (including `--minimized` after a start with Windows), so the next
/// start shows the window regardless.
pub const SHOW_AFTER_KEY: &str = "show_after_update";

/// No check in the first two minutes after start (measured on the monotonic clock, not the wall clock).
pub const WARMUP: Duration = Duration::from_secs(120);
/// A check at most this often (and at least this often while the app runs).
pub const DAILY_SECS: i64 = 24 * 60 * 60;
/// A peer's refusal brings the next check forward, but never within this long of the last one.
pub const REFUSAL_GAP_SECS: i64 = 60 * 60;
/// How often the schedule looks at the clock. No request goes out unless `decide` says so.
pub const TICK: Duration = Duration::from_secs(60);
/// The `latest.json` request.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// The installer download, start to finish.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the webview is told when a verified update is waiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UpdateReady {
    pub version: String,
}

// ---------- pure rules ----------

/// A key the plugin can use: not empty and not the placeholder.
pub fn key_usable(key: &str) -> bool {
    let k = key.trim();
    !k.is_empty() && k != PLACEHOLDER_PUBKEY
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Overrides {
    pub url: Option<String>,
    pub pubkey: Option<String>,
}

/// The test overrides from an environment lookup. `allowed` false (a release build) never even asks the environment.
pub fn overrides(allowed: bool, env: &dyn Fn(&str) -> Option<String>) -> Overrides {
    if !allowed {
        return Overrides::default();
    }
    let get = |k: &str| env(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    Overrides { url: get(URL_ENV), pubkey: get(KEY_ENV) }
}

pub fn overrides_from_env() -> Overrides {
    overrides(OVERRIDES_ALLOWED, &|k| std::env::var(k).ok())
}

/// Strictly newer. Equal, lower and unreadable versions are never installed.
pub fn is_newer(current: &str, offered: &str) -> bool {
    let parse = |v: &str| semver::Version::parse(v.trim().trim_start_matches('v')).ok();
    match (parse(current), parse(offered)) {
        (Some(c), Some(o)) => o > c,
        _ => false,
    }
}

const PC_UPDATE_TEXT: &str = "Update Tugboat on the PC";

fn refuses_old_pc(message: Option<&str>) -> bool {
    message.is_some_and(|m| m.trim().starts_with(PC_UPDATE_TEXT))
}

/// A linked device refused this PC because the PC is the older side. ("Update Tugboat on the phone" means the
/// other side must update: a newer PC would not help.)
pub fn needs_pc_update(s: &SyncStatus) -> bool {
    (s.state == SyncState::Refused && refuses_old_pc(s.message.as_deref())) || s.devices.iter().any(|d| d.state == SyncState::Refused && refuses_old_pc(d.message.as_deref()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    /// A usable public key (see `key_usable`).
    pub key_ok: bool,
    /// TUGBOAT_PROFILE is set: a QA run.
    pub qa_profile: bool,
    /// A debug-build endpoint override is set.
    pub override_url: bool,
    /// The Settings switch.
    pub enabled: bool,
    /// A verified update is already waiting.
    pub update_ready: bool,
    /// Unix seconds.
    pub now: i64,
    /// The warm-up after start is over.
    pub warmup_over: bool,
    pub last_check: Option<i64>,
    /// An update was found before the last restart and has not been fetched again yet.
    pub found_pending: bool,
    /// A device refused this PC as the older side and no check has answered it yet.
    pub refusal_hint: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    NoKey,
    QaProfile,
    Off,
    UpdateReady,
    Warmup,
    NotDue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Daily,
    Refused,
    /// An update was found before the last restart.
    Found,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Skip(Skip),
    Check(Why),
}

/// Should a check go out now? The one place the schedule is decided.
pub fn decide(i: &Inputs) -> Decision {
    if !i.key_ok {
        return Decision::Skip(Skip::NoKey);
    }
    if i.qa_profile && !i.override_url {
        return Decision::Skip(Skip::QaProfile);
    }
    if !i.enabled {
        return Decision::Skip(Skip::Off);
    }
    if i.update_ready {
        return Decision::Skip(Skip::UpdateReady);
    }
    // A time in the future (the clock was set back) counts as long ago: the next check re-stamps it.
    let waited = |secs: i64| i.last_check.is_none_or(|t| t > i.now || i.now - t >= secs);
    if i.refusal_hint && waited(REFUSAL_GAP_SECS) {
        return Decision::Check(Why::Refused);
    }
    if !i.warmup_over {
        return Decision::Skip(Skip::Warmup);
    }
    if i.found_pending {
        return Decision::Check(Why::Found);
    }
    if waited(DAILY_SECS) {
        Decision::Check(Why::Daily)
    } else {
        Decision::Skip(Skip::NotDue)
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Whether a missing-information line (see `Failure::NoRelease`) is new today. `last_day` is the day number of the
/// last line written in this run of failures, -1 when the last check did not fail that way.
pub fn no_release_is_news(last_day: i64, now: i64) -> bool {
    last_day != now.div_euclid(DAILY_SECS)
}

/// Whether the window opens at start. A start with `--minimized` (Windows autostart) stays in the tray, except right
/// after an update: the person who clicked Install expects to see the app come back.
pub fn show_window_at_start(minimized: bool, after_update: bool) -> bool {
    !minimized || after_update
}

/// Run `work`, but give up at once (dropping it, which stops a request or download in flight) when the switch goes
/// off, and drop its result when the switch went off while it ran. `None` = stopped. `off` is notified by
/// `Updates::set_enabled(false)`.
pub async fn until_off<T>(off: &Notify, enabled: impl Fn() -> bool, work: impl Future<Output = T>) -> Option<T> {
    // Registered before the switch is read, so a switch-off between the two cannot be missed.
    let notified = off.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if !enabled() {
        return None;
    }
    tokio::select! {
        out = work => enabled().then_some(out),
        () = &mut notified => None,
    }
}

/// Tray menu text for a waiting update.
pub fn tray_item_text(version: &str) -> String {
    format!("Install update {version}")
}

// ---------- failures ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Check,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The endpoint has no usable answer (404 before the first release carries `latest.json`, or an error status).
    NoRelease,
    Offline,
    Timeout,
    /// The server answered the download with an error status.
    Server,
    /// The signature or the version it was signed for did not match, or the file is not an installer.
    Rejected,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failed {
    pub stage: Stage,
    pub kind: Failure,
}

impl fmt::Display for Failed {
    /// One line for the log: no address, no file name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.stage {
            Stage::Check => "update check",
            Stage::Download => "update download",
        };
        match self.kind {
            Failure::NoRelease => write!(f, "{what}: no update information (not published, or the server refused the request)"),
            Failure::Offline => write!(f, "{what} failed: no connection"),
            Failure::Timeout => write!(f, "{what} failed: timed out"),
            Failure::Server => write!(f, "{what} failed: the server answered with an error"),
            Failure::Rejected => write!(f, "{what} rejected: the signature or the file did not check out"),
            Failure::Other => write!(f, "{what} failed: unexpected error"),
        }
    }
}

pub fn classify(e: &tauri_plugin_updater::Error) -> Failure {
    use tauri_plugin_updater::Error as E;
    match e {
        E::ReleaseNotFound => Failure::NoRelease,
        E::Reqwest(r) if r.is_timeout() => Failure::Timeout,
        E::Reqwest(r) if r.is_connect() => Failure::Offline,
        E::Network(_) => Failure::Server,
        E::Minisign(_) | E::Base64(_) | E::SignatureUtf8(_) | E::SignedVersionMismatch { .. } | E::MissingSignedVersion | E::InvalidUpdaterFormat => Failure::Rejected,
        _ => Failure::Other,
    }
}

/// The installer did not start because the user said no to the Windows administrator prompt (ShellExecute reports
/// that as "access denied").
pub fn is_declined(e: &tauri_plugin_updater::Error) -> bool {
    use tauri_plugin_updater::Error as E;
    matches!(e, E::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied || matches!(io.raw_os_error(), Some(5 | 1223)))
}

// ---------- check, download, verify ----------

/// A downloaded update whose signature checked out.
#[derive(Clone)]
pub struct Pending {
    pub version: String,
    update: Update,
    bytes: Arc<Vec<u8>>,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending").field("version", &self.version).field("bytes", &self.bytes.len()).finish()
    }
}

#[derive(Debug)]
pub enum Checked {
    /// The newest published version is not newer than this one.
    NoUpdate,
    Ready(Box<Pending>),
}

/// Runs just before the installer starts.
pub type BeforeExit = Arc<dyn Fn() + Send + Sync>;

/// Ask the endpoint, and when it offers a strictly newer version, download it and verify the signature. Only a
/// verified download comes back as `Ready`. One attempt: the caller decides when to try again.
pub async fn fetch<R: Runtime>(app: &AppHandle<R>, ov: &Overrides, before_exit: Option<BeforeExit>) -> Result<Checked, Failed> {
    let check = |kind: Failure| Failed { stage: Stage::Check, kind };
    // Always "strictly newer", whatever `allowDowngrades` says in the config.
    let mut builder = app.updater_builder().timeout(CHECK_TIMEOUT).version_comparator(|current, release| release.version > current);
    if let Some(raw) = &ov.url {
        let endpoint = match raw.parse() {
            Ok(u) => u,
            Err(_) => return Err(check(Failure::Other)),
        };
        builder = builder.endpoints(vec![endpoint]).map_err(|e| check(classify(&e)))?;
    }
    if let Some(key) = &ov.pubkey {
        builder = builder.pubkey(key.clone());
    }
    if let Some(hook) = before_exit {
        builder = builder.on_before_exit(move || hook());
    }
    let updater = builder.build().map_err(|e| check(classify(&e)))?;
    let mut update = match updater.check().await {
        Ok(Some(u)) => u,
        Ok(None) => return Ok(Checked::NoUpdate),
        Err(e) => return Err(check(classify(&e))),
    };
    // The plugin applied the same rule; this one stands on its own.
    if !is_newer(&update.current_version, &update.version) {
        return Ok(Checked::NoUpdate);
    }
    update.timeout = Some(DOWNLOAD_TIMEOUT);
    // `download` verifies the signature against the public key and the version it was signed for.
    let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| Failed { stage: Stage::Download, kind: classify(&e) })?;
    Ok(Checked::Ready(Box::new(Pending { version: update.version.clone(), update, bytes: Arc::new(bytes) })))
}

// ---------- state ----------

/// The managed update state.
pub struct Updates {
    backend: Arc<Backend>,
    enabled: AtomicBool,
    /// Unix seconds, 0 = never.
    last_check: AtomicI64,
    /// Monotonic: a clock set back after start must not stretch the warm-up.
    started: Instant,
    ready: Mutex<Option<Pending>>,
    installing: AtomicBool,
    /// The day number of the last "no update information" line, -1 after any other outcome: a run of the same
    /// failure is logged once a day, not once a check.
    no_release_day: AtomicI64,
    /// An update was found before the last restart: fetch it again once the warm-up is over.
    found_pending: AtomicBool,
    /// Notified when the switch goes off, to stop a check or download in flight.
    off: Notify,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Core has no delete: an empty value counts as not set.
fn clear_value(backend: &Backend, key: &str) {
    if let Ok(Some(v)) = backend.core.get_value(key) {
        if !v.trim().is_empty() {
            if let Err(e) = backend.core.set_value(key, "") {
                tracing::warn!("setting {key} not cleared: {e}");
            }
        }
    }
}

/// True once after an install that was started from this app: clears the mark, so only the one start shows the window.
pub fn take_show_after_update(backend: &Backend) -> bool {
    let set = matches!(backend.core.get_value(SHOW_AFTER_KEY), Ok(Some(v)) if !v.trim().is_empty());
    if set {
        clear_value(backend, SHOW_AFTER_KEY);
        tracing::info!("started after an update: showing the window");
    }
    set
}

impl Updates {
    /// Reads the switch, the last check time and the version found before the last restart from Core's local
    /// settings. `current_version` is the running version.
    pub fn new(backend: Arc<Backend>, current_version: &str) -> Updates {
        let enabled = match backend.core.get_value(SETTING_KEY) {
            Ok(v) => v.as_deref().map(str::trim) != Some("false"),
            Err(e) => {
                tracing::warn!("update setting not read, checks stay on: {e}");
                true
            }
        };
        let last = match backend.core.get_value(LAST_CHECK_KEY) {
            Ok(v) => v.and_then(|t| t.trim().parse::<i64>().ok()).unwrap_or(0),
            Err(e) => {
                tracing::warn!("last update check time not read: {e}");
                0
            }
        };
        let found = match backend.core.get_value(FOUND_KEY) {
            Ok(v) => v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            Err(e) => {
                tracing::warn!("found update not read: {e}");
                None
            }
        };
        // An update that is no longer newer (installed by hand since) is forgotten.
        let found_pending = found.as_deref().is_some_and(|v| is_newer(current_version, v));
        if found.is_some() && !found_pending {
            clear_value(&backend, FOUND_KEY);
        }
        Updates {
            backend,
            enabled: AtomicBool::new(enabled),
            last_check: AtomicI64::new(last),
            started: Instant::now(),
            ready: Mutex::new(None),
            installing: AtomicBool::new(false),
            no_release_day: AtomicI64::new(-1),
            found_pending: AtomicBool::new(found_pending),
            off: Notify::new(),
        }
    }

    /// An update found before the last restart is waiting to be fetched again.
    pub fn found_pending(&self) -> bool {
        self.found_pending.load(Ordering::SeqCst)
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, on: bool) -> CmdResult<()> {
        self.backend.core.set_value(SETTING_KEY, if on { "true" } else { "false" })?;
        self.enabled.store(on, Ordering::SeqCst);
        if !on {
            self.off.notify_waiters();
        }
        tracing::info!("update check switched {}", if on { "on" } else { "off" });
        Ok(())
    }

    /// Unix seconds of the last check that went out, if any.
    pub fn last_check(&self) -> Option<i64> {
        let t = self.last_check.load(Ordering::SeqCst);
        (t > 0).then_some(t)
    }

    pub fn ready(&self) -> Option<UpdateReady> {
        lock(&self.ready).as_ref().map(|p| UpdateReady { version: p.version.clone() })
    }

    fn inputs(&self, key_ok: bool, override_url: bool) -> Inputs {
        Inputs {
            key_ok,
            qa_profile: self.backend.paths_profile_active(),
            override_url,
            enabled: self.enabled(),
            update_ready: lock(&self.ready).is_some(),
            now: now_secs(),
            warmup_over: self.started.elapsed() >= WARMUP,
            last_check: self.last_check(),
            found_pending: self.found_pending(),
            refusal_hint: self.backend.update_hint(),
        }
    }

    fn stamp(&self, now: i64) {
        self.last_check.store(now, Ordering::SeqCst);
        if let Err(e) = self.backend.core.set_value(LAST_CHECK_KEY, &now.to_string()) {
            // The next start would check again: one extra request, never a stuck update.
            tracing::warn!("last update check time not saved: {e}");
        }
    }

    /// One scheduled or hinted check, start to finish. Every outcome ends in one log line.
    async fn run_check(&self, app: &AppHandle, ov: &Overrides, why: Why) {
        self.stamp(now_secs());
        self.backend.clear_update_hint();
        self.found_pending.store(false, Ordering::SeqCst);
        match why {
            Why::Daily => tracing::info!("update check: daily check"),
            Why::Refused => tracing::info!("update check: a linked device needs a newer version here"),
            Why::Found => tracing::info!("update check: an update was found before the last restart"),
        }
        // Switching the check off stops the request or the download at once and drops its result.
        let Some(outcome) = until_off(&self.off, || self.enabled(), fetch(app, ov, Some(self.exit_hook(app)))).await else {
            tracing::info!("update check stopped: switched off");
            return;
        };
        match outcome {
            Ok(Checked::NoUpdate) => {
                self.no_release_day.store(-1, Ordering::SeqCst);
                clear_value(&self.backend, FOUND_KEY);
                tracing::info!("update check: this version is the newest");
            }
            Ok(Checked::Ready(p)) => {
                self.no_release_day.store(-1, Ordering::SeqCst);
                tracing::info!("update check: version {} downloaded and verified", p.version);
                let version = p.version.clone();
                if let Err(e) = self.backend.core.set_value(FOUND_KEY, &version) {
                    tracing::warn!("found update not saved: {e}");
                }
                *lock(&self.ready) = Some(*p);
                announce(app, &version);
            }
            Err(f) if f.kind == Failure::NoRelease => {
                clear_value(&self.backend, FOUND_KEY);
                if no_release_is_news(self.no_release_day.swap(now_secs().div_euclid(DAILY_SECS), Ordering::SeqCst), now_secs()) {
                    tracing::info!("{f}");
                }
            }
            Err(f) => {
                self.no_release_day.store(-1, Ordering::SeqCst);
                tracing::warn!("{f}");
            }
        }
    }

    /// Before the installer starts: hide the tray icon (the process exits right after, which would leave a dead icon
    /// until the mouse passes over it) and stop sync. `install` undoes both if the installer does not start.
    fn exit_hook(&self, app: &AppHandle) -> BeforeExit {
        let backend = self.backend.clone();
        let app = app.clone();
        Arc::new(move || {
            shell::set_tray_visible(&app, false);
            tauri::async_runtime::block_on(backend.quiesce());
        })
    }
}

fn announce(app: &AppHandle, version: &str) {
    if let Err(e) = app.emit_to(MAIN, ev::UPDATE_READY, UpdateReady { version: version.to_string() }) {
        tracing::warn!("update banner not delivered: {e}");
    }
    shell::set_update_item(app, Some(version));
}

/// `plugins.updater.pubkey` from `tauri.conf.json`; empty when missing.
fn configured_pubkey<R: Runtime>(app: &AppHandle<R>) -> String {
    app.config().plugins.0.get("updater").and_then(|v| v.get("pubkey")).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// Make the update state and start the schedule. The state exists even when checks cannot run (placeholder key), so
/// the Settings switch and the commands always work.
pub fn start(app: &AppHandle, backend: Arc<Backend>) -> Arc<Updates> {
    let ov = overrides_from_env();
    let key = ov.pubkey.clone().unwrap_or_else(|| configured_pubkey(app));
    let state = Arc::new(Updates::new(backend, &app.package_info().version.to_string()));
    app.manage(state.clone());
    if !key_usable(&key) {
        tracing::info!("update check off: this build has no update key");
        return state;
    }
    let (app, st) = (app.clone(), state.clone());
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            if let Decision::Check(why) = decide(&st.inputs(true, ov.url.is_some())) {
                st.run_check(&app, &ov, why).await;
            }
        }
    });
    state
}

// ---------- install ----------

/// Install the waiting update. Blocking: call off the UI thread.
///
/// On Windows the plugin writes the installer to a temp folder, runs `before_exit` (hide the tray, stop sync), starts
/// the installer in passive mode through ShellExecute (Windows asks for administrator rights: the install is per
/// machine) and, once it started, exits this process with code 0; the installer restarts the app when it is done.
/// So a call that returns means the installer did not start. `Ok(false)`: the user said no to the Windows prompt,
/// nothing changed and the update stays waiting. Any other failure is an error for the UI.
pub fn install(app: &AppHandle, st: &Updates) -> CmdResult<bool> {
    let Some(p) = lock(&st.ready).clone() else {
        return Err(CmdError::new("update", "NO UPDATE READY"));
    };
    if st.installing.swap(true, Ordering::SeqCst) {
        return Ok(true); // the Windows prompt from the first click is still open
    }
    tracing::info!("update install: starting version {}", p.version);
    // The installer restarts the app with this run's arguments (--minimized after a start with Windows): mark that
    // the next start shows the window. A call that returns means no installer started, so the mark goes again.
    if let Err(e) = st.backend.core.set_value(SHOW_AFTER_KEY, "1") {
        tracing::warn!("restart mark not saved: {e}");
    }
    let result = p.update.install(&p.bytes[..]);
    clear_value(&st.backend, SHOW_AFTER_KEY);
    st.installing.store(false, Ordering::SeqCst);
    // Only here when the installer did not start: put back what the hook stopped.
    tauri::async_runtime::block_on(st.backend.resume());
    shell::set_tray_visible(app, true);
    match result {
        Ok(()) => Ok(true),
        Err(e) if is_declined(&e) => {
            tracing::info!("update install: Windows did not allow the installer to start");
            Ok(false)
        }
        Err(e) => {
            tracing::warn!("update install failed: {}", Failed { stage: Stage::Download, kind: classify(&e) });
            Err(CmdError::new("update", "UPDATE NOT INSTALLED - the installer did not start"))
        }
    }
}

/// The tray item. Errors have no window to land in: show it, then tell it.
pub fn install_from_tray(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(st) = app.try_state::<Arc<Updates>>() else { return };
        if let Err(e) = install(&app, &st) {
            shell::show_main(&app);
            match serde_json::to_value(Event::Error { kind: e.kind, message: e.message }) {
                Ok(v) => {
                    let _ = app.emit_to(MAIN, ev::CORE, v);
                }
                Err(err) => tracing::warn!("update error not delivered: {err}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tugboat_core::events::DeviceStatus;

    const T0: i64 = 1_800_000_000;

    fn base() -> Inputs {
        Inputs { key_ok: true, qa_profile: false, override_url: false, enabled: true, update_ready: false, now: T0 + 10_000, warmup_over: true, last_check: None, found_pending: false, refusal_hint: false }
    }

    #[test]
    fn a_never_checked_pc_checks_once_the_warmup_is_over() {
        let mut i = base();
        i.warmup_over = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::Warmup), "never in the first two minutes");
        i.warmup_over = true;
        assert_eq!(decide(&i), Decision::Check(Why::Daily));
    }

    #[test]
    fn the_warmup_is_not_measured_on_the_wall_clock() {
        // A start with the clock hours ahead that is then set back: the wall clock says "negative time since start",
        // the check still runs once the real two minutes are over.
        let mut i = base();
        i.now = T0 - 6 * 3600;
        i.last_check = None;
        assert_eq!(decide(&i), Decision::Check(Why::Daily));
        assert_eq!(WARMUP, Duration::from_secs(120));
    }

    #[test]
    fn an_update_found_before_a_restart_is_fetched_again_once_the_warmup_is_over() {
        let mut i = base();
        i.found_pending = true;
        i.last_check = Some(i.now - 3600); // checked an hour ago: not due by the daily rule
        assert_eq!(decide(&i), Decision::Check(Why::Found));
        i.warmup_over = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::Warmup), "not before the warm-up");
        i.warmup_over = true;
        i.enabled = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::Off), "never while the switch is off");
        i.enabled = true;
        i.key_ok = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::NoKey));
        i.key_ok = true;
        i.found_pending = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::NotDue), "without the mark an hour is not due");
    }

    #[test]
    fn a_missing_information_line_is_written_once_a_day() {
        let now = 1_800_000_000;
        assert!(no_release_is_news(-1, now), "first failure of a run");
        assert!(!no_release_is_news(now.div_euclid(DAILY_SECS), now + 60), "same day: silent");
        assert!(no_release_is_news(now.div_euclid(DAILY_SECS), now + DAILY_SECS), "next day: written again");
    }

    #[test]
    fn a_start_from_autostart_stays_in_the_tray_except_right_after_an_update() {
        assert!(show_window_at_start(false, false), "a normal start shows the window");
        assert!(!show_window_at_start(true, false), "start with Windows stays in the tray");
        assert!(show_window_at_start(true, true), "the installer restarted the app: show it");
        assert!(show_window_at_start(false, true));
    }

    #[tokio::test]
    async fn switching_off_stops_the_work_at_once() {
        let off = Arc::new(Notify::new());
        let on = Arc::new(AtomicBool::new(true));
        let (o, flag) = (off.clone(), on.clone());
        let flip = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.store(false, Ordering::SeqCst);
            o.notify_waiters();
        });
        let started = Instant::now();
        let out = until_off(&off, || on.load(Ordering::SeqCst), std::future::pending::<u8>()).await;
        flip.await.unwrap();
        assert_eq!(out, None);
        assert!(started.elapsed() < Duration::from_secs(5), "did not wait for the work");
    }

    #[tokio::test]
    async fn work_that_finishes_while_the_switch_is_on_keeps_its_result() {
        let off = Notify::new();
        assert_eq!(until_off(&off, || true, async { 7 }).await, Some(7));
    }

    #[tokio::test]
    async fn a_result_is_dropped_when_the_switch_went_off_before_or_during_the_work() {
        let off = Notify::new();
        assert_eq!(until_off(&off, || false, async { 7 }).await, None, "already off: never started");
        // Off by the time the work is done, with no notification seen: still dropped.
        let on = AtomicBool::new(true);
        let out = until_off(&off, || on.load(Ordering::SeqCst), async {
            on.store(false, Ordering::SeqCst);
            7
        })
        .await;
        assert_eq!(out, None);
    }

    #[test]
    fn a_saved_check_time_keeps_restarts_to_one_check_a_day() {
        let mut i = base();
        i.last_check = Some(i.now - DAILY_SECS + 1);
        assert_eq!(decide(&i), Decision::Skip(Skip::NotDue), "a restart one second short of a day ago does not check");
        i.last_check = Some(i.now - DAILY_SECS);
        assert_eq!(decide(&i), Decision::Check(Why::Daily));
        i.last_check = Some(i.now - 3 * DAILY_SECS);
        assert_eq!(decide(&i), Decision::Check(Why::Daily));
    }

    #[test]
    fn a_check_time_in_the_future_does_not_stall_the_schedule() {
        let mut i = base();
        i.last_check = Some(i.now + 5 * DAILY_SECS);
        assert_eq!(decide(&i), Decision::Check(Why::Daily));
    }

    #[test]
    fn the_switch_off_the_placeholder_key_and_a_qa_profile_never_check() {
        let mut i = base();
        i.enabled = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::Off));
        i.refusal_hint = true;
        assert_eq!(decide(&i), Decision::Skip(Skip::Off), "a refusal does not override the switch");
        let mut i = base();
        i.key_ok = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::NoKey));
        i.enabled = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::NoKey), "no key is reported first");
        let mut i = base();
        i.qa_profile = true;
        assert_eq!(decide(&i), Decision::Skip(Skip::QaProfile));
        i.refusal_hint = true;
        assert_eq!(decide(&i), Decision::Skip(Skip::QaProfile));
        i.override_url = true;
        assert_eq!(decide(&i), Decision::Check(Why::Refused), "a QA run with a test endpoint may check");
        i.refusal_hint = false;
        assert_eq!(decide(&i), Decision::Check(Why::Daily));
    }

    #[test]
    fn a_waiting_update_stops_further_checks() {
        let mut i = base();
        i.update_ready = true;
        assert_eq!(decide(&i), Decision::Skip(Skip::UpdateReady));
    }

    #[test]
    fn a_refusal_brings_the_check_forward_but_at_most_once_an_hour() {
        let mut i = base();
        i.refusal_hint = true;
        i.warmup_over = false; // inside the warmup: a refusal does not wait for it
        i.last_check = None;
        assert_eq!(decide(&i), Decision::Check(Why::Refused));
        i.warmup_over = true;
        i.last_check = Some(i.now - REFUSAL_GAP_SECS + 1);
        assert_eq!(decide(&i), Decision::Skip(Skip::NotDue), "a check an hour less a second ago blocks it");
        i.last_check = Some(i.now - REFUSAL_GAP_SECS);
        assert_eq!(decide(&i), Decision::Check(Why::Refused));
        // Without a refusal the same last check is not due.
        i.refusal_hint = false;
        assert_eq!(decide(&i), Decision::Skip(Skip::NotDue));
    }

    #[test]
    fn only_a_strictly_newer_version_is_an_update() {
        assert!(is_newer("0.3.0", "0.3.1"));
        assert!(is_newer("0.3.0", "0.10.0"), "compared as numbers, not text");
        assert!(is_newer("0.3.0-rc.1", "0.3.0"));
        assert!(is_newer("0.3.0", "v0.4.0"));
        assert!(!is_newer("0.3.0", "0.3.0"), "equal is not an update");
        assert!(!is_newer("0.3.1", "0.3.0"), "a lower version is never installed");
        assert!(!is_newer("0.3.0", "0.3.0-rc.1"));
        assert!(!is_newer("0.3.0", "latest"));
        assert!(!is_newer("0.3.0", ""));
        assert!(!is_newer("junk", "0.4.0"));
    }

    #[test]
    fn the_placeholder_and_an_empty_key_are_not_usable() {
        assert!(!key_usable(PLACEHOLDER_PUBKEY));
        assert!(!key_usable(&format!("  {PLACEHOLDER_PUBKEY}\n")));
        assert!(!key_usable(""));
        assert!(!key_usable("   "));
        assert!(key_usable("dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDEyMzQ="));
    }

    #[test]
    fn a_release_build_never_asks_the_environment_for_the_test_overrides() {
        let panics = |k: &str| -> Option<String> { panic!("the environment was read for {k}") };
        assert_eq!(overrides(false, &panics), Overrides::default());
        // The real switch is the build profile: a release build compiles it to false.
        assert_eq!(OVERRIDES_ALLOWED, cfg!(debug_assertions));
        #[cfg(not(debug_assertions))]
        assert_eq!(overrides_from_env(), Overrides::default());
    }

    #[test]
    fn a_debug_build_reads_both_overrides_and_trims_them() {
        let env = |k: &str| match k {
            URL_ENV => Some(" http://127.0.0.1:9/latest.json ".to_string()),
            KEY_ENV => Some("  ".to_string()),
            _ => None,
        };
        let o = overrides(true, &env);
        assert_eq!(o.url.as_deref(), Some("http://127.0.0.1:9/latest.json"));
        assert_eq!(o.pubkey, None, "a blank value is no override");
    }

    fn device(state: SyncState, message: Option<&str>) -> DeviceStatus {
        DeviceStatus {
            id: "a".repeat(64),
            name: "Pixel".into(),
            state,
            message: message.map(Into::into),
            paired_at: None,
            last_seen_at: None,
            last_synced_at: None,
            waiting: 0,
            downloading: 0,
            unlinking: false,
        }
    }

    fn status(state: SyncState, message: Option<&str>, devices: Vec<DeviceStatus>) -> SyncStatus {
        SyncStatus { state, peer_name: None, last_synced_at: None, waiting: 0, downloading: 0, storage_low: false, message: message.map(Into::into), devices }
    }

    #[test]
    fn only_a_refusal_that_says_this_pc_is_old_asks_for_a_check() {
        assert!(needs_pc_update(&status(SyncState::Refused, Some("Update Tugboat on the PC"), vec![])));
        assert!(!needs_pc_update(&status(SyncState::Refused, Some("Update Tugboat on the phone"), vec![])), "the phone must update, not this PC");
        assert!(!needs_pc_update(&status(SyncState::Refused, Some("NOT PAIRED - pair the devices again"), vec![])));
        assert!(!needs_pc_update(&status(SyncState::Synced, Some("Update Tugboat on the PC"), vec![])), "only while refused");
        assert!(!needs_pc_update(&status(SyncState::Synced, None, vec![])));
        // With several linked devices the refusal can sit on one device row only.
        let devices = vec![device(SyncState::Synced, None), device(SyncState::Refused, Some("Update Tugboat on the PC"))];
        assert!(needs_pc_update(&status(SyncState::Synced, None, devices)));
    }

    #[test]
    fn failure_lines_name_no_address_and_no_file() {
        use tauri_plugin_updater::Error as E;
        for (e, want) in [
            (E::ReleaseNotFound, Failure::NoRelease),
            (E::Network("Download request failed with status: 404 Not Found".into()), Failure::Server),
            (E::MissingSignedVersion, Failure::Rejected),
            (E::SignedVersionMismatch { signed: "1.0.0".into(), announced: "2.0.0".into() }, Failure::Rejected),
            (E::InvalidUpdaterFormat, Failure::Rejected),
            (E::EmptyEndpoints, Failure::Other),
        ] {
            assert_eq!(classify(&e), want, "{e}");
        }
        let none = Failed { stage: Stage::Check, kind: Failure::NoRelease }.to_string();
        assert!(none.contains("server refused"), "an error status looks like missing information: say both ({none})");
        for kind in [Failure::NoRelease, Failure::Offline, Failure::Timeout, Failure::Server, Failure::Rejected, Failure::Other] {
            for stage in [Stage::Check, Stage::Download] {
                let line = Failed { stage, kind }.to_string();
                assert!(!line.contains('\n') && !line.contains("http") && !line.contains(".json") && !line.contains(".exe"), "{line}");
            }
        }
    }

    #[test]
    fn a_declined_windows_prompt_is_told_apart_from_a_real_failure() {
        use tauri_plugin_updater::Error as E;
        assert!(is_declined(&E::Io(std::io::Error::from_raw_os_error(5))), "ShellExecute: access denied");
        assert!(is_declined(&E::Io(std::io::Error::from_raw_os_error(1223))), "operation cancelled by the user");
        assert!(is_declined(&E::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x"))));
        assert!(!is_declined(&E::Io(std::io::Error::from_raw_os_error(2))), "file not found is a failure");
        assert!(!is_declined(&E::InvalidUpdaterFormat));
    }

    #[test]
    fn the_tray_item_names_the_version() {
        assert_eq!(tray_item_text("0.3.1"), "Install update 0.3.1");
    }

    #[test]
    fn the_updater_is_configured_for_this_app() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let up = &conf["plugins"]["updater"];
        assert_eq!(up["endpoints"], serde_json::json!(["https://github.com/Page011/LocalTugboat/releases/latest/download/latest.json"]));
        assert_eq!(up["windows"]["installMode"], "passive");
        assert_eq!(up["requireSignedVersion"], true, "a manifest cannot pair a new version number with an old signed file");
        assert_eq!(conf["bundle"]["createUpdaterArtifacts"], true);
        assert!(up["pubkey"].is_string());
    }

    /// The window can never call the plugin's own commands: only this module's wrappers, which check the caller.
    #[test]
    fn the_webview_has_no_permission_for_the_updater_plugin() {
        let caps = include_str!("../capabilities/main.json");
        assert!(!caps.contains("updater"), "an updater permission in the main window's capability would let the page install anything the plugin accepts");
    }
}
