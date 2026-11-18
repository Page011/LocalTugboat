//! The thread behind the library: owns the file watcher, the debounce and the timers, and
//! calls `Inner::pass`. The watcher only says "look again". Passes are full scans (with
//! cached file ids), so a dropped or coalesced event can never lose a change, and the
//! start-up scan and the 10-minute rescan are the same code as an event-driven one.

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread::JoinHandle,
    time::Instant,
};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::{
    config::HIDDEN_DIR,
    placement::{is_inside, same_path},
    Inner, Library,
};

enum Msg {
    /// Something changed under the library.
    Dirty,
    /// Scan now; true = full (re-read every file id).
    Scan(bool),
    Stop,
}

/// Keeps the library thread (which owns the watcher) alive; dropping it stops both.
pub struct Worker {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Something Core changed that may need the disk to follow (a move or rename made on the
    /// other device): scan soon, debounced like a watcher event. Core's own changes reach the
    /// disk through the placement, but a change that arrives from the phone only updates the
    /// record, so the disk is brought in line by the next scan.
    pub fn poke(&self) {
        let _ = self.tx.send(Msg::Dirty);
    }

    /// Ask for a scan on the worker thread (UI "Rescan", after Locate / Choose).
    pub fn request_scan(&self, full: bool) {
        let _ = self.tx.send(Msg::Scan(full));
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Library {
    /// Start watching the library root and scanning. The first pass is a full scan.
    /// A watcher that cannot start is reported through `last_error`; the 10-minute rescan and
    /// the user's Rescan still work without it.
    pub fn start(&self) -> Worker {
        let (tx, rx) = mpsc::channel::<Msg>();
        let inner = self.inner.clone();
        let tx_thread = tx.clone();
        let thread = std::thread::Builder::new()
            .name("tugboat-library".into())
            .spawn(move || run(inner, rx, tx_thread))
            .map_err(|e| {
                tracing::error!("library thread failed to start: {e}");
                super::lock(&self.inner.st).last_error = Some(format!("LIBRARY THREAD NOT RUNNING - {e}"));
            })
            .ok();
        Worker { tx, thread }
    }
}

fn start_watcher(root: &Path, tx: Sender<Msg>, inner: &Inner) -> Option<RecommendedWatcher> {
    let hidden = root.join(HIDDEN_DIR);
    let handler = move |res: notify::Result<notify::Event>| {
        let msg = match res {
            // Reading a file is not a change; downloads write `.tugboat\*.part` all the time.
            Ok(ev) if matches!(ev.kind, EventKind::Access(_)) => return,
            Ok(ev) if !ev.paths.is_empty() && ev.paths.iter().all(|p| is_inside(&hidden, p)) => return,
            Ok(_) => Msg::Dirty,
            // A watcher error (buffer overflow) means events were lost: do a full scan.
            Err(_) => Msg::Scan(true),
        };
        let _ = tx.send(msg);
    };
    let started = notify::recommended_watcher(handler).and_then(|mut w| w.watch(root, RecursiveMode::Recursive).map(|()| w));
    match started {
        Ok(w) => {
            super::lock(&inner.st).watcher_error = None;
            Some(w)
        }
        Err(e) => {
            tracing::error!("file watcher did not start: {e}");
            // Kept in State: every pass rewrites last_error from it, so it stays on screen.
            // A missing folder is reported once, as LIBRARY FOLDER NOT FOUND, not twice.
            let mut st = super::lock(&inner.st);
            st.watcher_error = root.exists().then(|| format!("FILE WATCHER NOT RUNNING - rescans every 10 minutes only: {e}"));
            None
        }
    }
}

fn run(inner: std::sync::Arc<Inner>, rx: mpsc::Receiver<Msg>, tx: Sender<Msg>) {
    let o = inner.opts.clone();
    let root = super::lock(&inner.st).cfg.path.clone();
    let mut watched: Option<PathBuf> = root.clone();
    let mut watcher = root.as_deref().and_then(|r| start_watcher(r, tx.clone(), &inner));
    let mut last_full = Instant::now();
    let mut dirty_first: Option<Instant> = None;
    let mut dirty_last: Option<Instant> = None;
    let mut retry_at: Option<Instant> = None;

    let mut scan = |full: bool, last_full: &mut Instant, retry_at: &mut Option<Instant>| {
        let now = Instant::now();
        // A bug in one pass must not end the library thread silently: report it and go on.
        let rep = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.pass(full, now))).unwrap_or_else(|_| {
            tracing::error!("library scan panicked");
            let mut st = super::lock(&inner.st);
            st.last_error = Some("LIBRARY SCAN CRASHED - will try again".into());
            inner.publish(&mut st);
            super::PassReport { retry_after: Some(std::time::Duration::from_secs(30)), ..Default::default() }
        });
        // Watch the library as it is NOW. The folder was missing and is back (the old watch
        // handle may be dead), or Choose / Locate pointed the library elsewhere, or it was
        // chosen after start: make a new watcher on the current root.
        let (current, rewatch) = {
            let mut st = super::lock(&inner.st);
            (st.cfg.path.clone(), std::mem::take(&mut st.rewatch))
        };
        let moved = match (&current, &watched) {
            (Some(a), Some(b)) => !same_path(a, b),
            (None, None) => false,
            _ => true,
        };
        if rewatch || moved {
            let before = super::lock(&inner.st).watcher_error.clone();
            watcher = None;
            watcher = current.as_deref().and_then(|r| start_watcher(r, tx.clone(), &inner));
            watched = current;
            // The pass above showed the watcher error as it was before this new watcher: if it
            // came or went, look again so the screen does not keep a stale line.
            if super::lock(&inner.st).watcher_error != before {
                let _ = tx.send(Msg::Scan(false));
            }
        }
        if full {
            *last_full = now;
        }
        *retry_at = rep.retry_after.map(|d| now + d);
    };
    scan(true, &mut last_full, &mut retry_at);

    loop {
        let now = Instant::now();
        let mut deadline = last_full + o.rescan_every;
        if let (Some(first), Some(last)) = (dirty_first, dirty_last) {
            deadline = deadline.min((last + o.debounce).min(first + o.max_debounce));
        }
        if let Some(r) = retry_at {
            deadline = deadline.min(r);
        }
        match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(Msg::Dirty) => {
                let t = Instant::now();
                dirty_first.get_or_insert(t);
                dirty_last = Some(t);
            }
            Ok(Msg::Scan(full)) => {
                dirty_first = None;
                dirty_last = None;
                scan(full, &mut last_full, &mut retry_at);
                continue;
            }
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        let full_due = now >= last_full + o.rescan_every;
        let dirty_due = matches!((dirty_first, dirty_last), (Some(f), Some(l)) if now >= l + o.debounce || now >= f + o.max_debounce);
        let retry_due = retry_at.is_some_and(|r| now >= r);
        if full_due || dirty_due || retry_due {
            dirty_first = None;
            dirty_last = None;
            scan(full_due, &mut last_full, &mut retry_at);
        }
    }
    drop(watcher);
}
