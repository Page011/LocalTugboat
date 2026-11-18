//! Rust log file: <data_dir>/logs/tugboat.log, rotated at 5 MB, three files kept
//! (tugboat.log, tugboat.log.1, tugboat.log.2).
//! The phone cannot show logcat on the user's device, so this file is what "Export log" shares.
//! A failure to open or write the log is reported once through the callback, never swallowed.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Once},
};

use tracing_subscriber::fmt::MakeWriter;

pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const KEEP: usize = 3;

pub struct RotatingLog {
    path: PathBuf,
    // None only while rotating: Windows cannot rename a file that is still open.
    file: Option<File>,
    size: u64,
    max: u64,
}

fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

impl RotatingLog {
    pub fn open(path: PathBuf, max: u64) -> io::Result<RotatingLog> {
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut log = RotatingLog { path, file: Some(file), size, max };
        if log.size >= log.max {
            log.rotate()?;
        }
        Ok(log)
    }

    /// log -> log.1 -> log.2; the old log.2 is deleted.
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        for n in (1..KEEP).rev() {
            let from = if n == 1 { self.path.clone() } else { numbered(&self.path, n - 1) };
            if from.exists() {
                let to = numbered(&self.path, n);
                if to.exists() {
                    fs::remove_file(&to)?;
                }
                fs::rename(&from, &to)?;
            }
        }
        self.file = Some(OpenOptions::new().create(true).append(true).open(&self.path)?);
        self.size = 0;
        Ok(())
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size + buf.len() as u64 > self.max && self.size > 0 {
            self.rotate()?;
        }
        let f = self.file.as_mut().ok_or_else(|| io::Error::other("log file is closed"))?;
        f.write_all(buf)?;
        self.size += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

type Report = Arc<dyn Fn(String) + Send + Sync>;

struct Sink {
    log: Option<RotatingLog>,
    report: Report,
    /// A write failure was already reported for this sink. Per sink, not global: `init` makes a
    /// fresh sink, so it starts false and no other sink's state can change it.
    failed: bool,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static INSTALLED: Once = Once::new();

/// Write one line to the sink in `slot`. The first failure is reported through the sink's callback.
fn write_to(slot: &Mutex<Option<Sink>>, buf: &[u8]) {
    // The failure text and, for the first failure only, the callback to report it with.
    let mut failure: Option<(String, Report)> = None;
    {
        let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sink) = g.as_mut() {
            if let Some(log) = sink.log.as_mut() {
                if let Err(e) = log.write_all(buf) {
                    // Only the first failure is reported, so a full disk is not an error storm.
                    if !sink.failed {
                        sink.failed = true;
                        failure = Some((format!("log write failed: {e}"), sink.report.clone()));
                    }
                }
            }
        }
    }
    // Report with the lock released: the callback may log, and that log write takes the lock.
    if let Some((text, report)) = failure {
        report(text);
    }
}

#[derive(Clone, Copy)]
struct SinkWriter;

impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_to(&SINK, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SinkWriter {
    type Writer = SinkWriter;
    fn make_writer(&'a self) -> SinkWriter {
        *self
    }
}

/// Point the global log at `<data_dir>/logs/tugboat.log` and install the subscriber once.
/// A second call (a second core in tests) re-points the file. `report` gets the first write
/// failure. Returns the log path, or the open error (the caller reports it; the app still runs).
pub fn init(data_dir: &Path, report: Report) -> Result<PathBuf, (PathBuf, io::Error)> {
    let dir = data_dir.join("logs");
    let path = dir.join("tugboat.log");
    let opened = fs::create_dir_all(&dir).and_then(|_| RotatingLog::open(path.clone(), MAX_LOG_BYTES));
    let (log, err) = match opened {
        Ok(l) => (Some(l), None),
        Err(e) => (None, Some(e)),
    };
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink { log, report, failed: false });
    INSTALLED.call_once(|| {
        let filter = "warn,tugboat_ffi=info,tugboat_core=info,tugboat_net=info";
        // try_init: another subscriber (a test harness) already installed is not an error here.
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(SinkWriter).try_init();
    });
    match err {
        None => Ok(path),
        Some(e) => Err((path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 MB x 3 rule at a small size: the newest text is in the live file, the oldest file is
    /// dropped after three fills, and the live file never grows past the limit.
    #[test]
    fn rotates_at_the_limit_and_keeps_three_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tugboat.log");
        let mut log = RotatingLog::open(path.clone(), 100).unwrap();
        for i in 0..40 {
            // One write per line, like tracing's formatter: a line is never split across files.
            log.write_all(format!("line {i:02} xxxxxxxxxxxxxxxxxxxx
").as_bytes()).unwrap();
        }
        let size = |p: &Path| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        assert!(size(&path) <= 100 && size(&path) > 0);
        assert!(size(&numbered(&path, 1)) > 0 && size(&numbered(&path, 1)) <= 100);
        assert!(size(&numbered(&path, 2)) > 0);
        assert!(!numbered(&path, 3).exists(), "only three files are kept");
        let live = fs::read_to_string(&path).unwrap();
        assert!(live.contains("line 39"), "the newest line is in the live file");
        let all = [fs::read_to_string(numbered(&path, 2)).unwrap(), fs::read_to_string(numbered(&path, 1)).unwrap(), live].concat();
        assert!(!all.contains("line 00"), "the oldest lines were dropped with the oldest file");
        // Whole lines only.
        assert!(all.lines().all(|l| l.starts_with("line ") && l.ends_with('x')));
    }

    /// The failure callback may log (the app's listener logs when it throws). That nested write must
    /// not wait for a lock the first write still holds: it used to deadlock. The test owns its sink
    /// (`SLOT`), so other tests that call `init` (every core open) cannot swap it mid-test.
    #[test]
    fn a_log_failure_report_that_logs_again_does_not_deadlock() {
        static SLOT: Mutex<Option<Sink>> = Mutex::new(None);
        let dir = tempfile::tempdir().unwrap();
        let broken = RotatingLog { path: dir.path().join("tugboat.log"), file: None, size: 0, max: 100 };
        let reports = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = reports.clone();
        *SLOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink {
            log: Some(broken),
            report: Arc::new(move |text| {
                seen.lock().unwrap().push(text);
                // Like `deliver` logging "the event listener threw": a log write inside the report.
                write_to(&SLOT, b"nested line\n");
            }),
            failed: false,
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            write_to(&SLOT, b"first line\n");
            let _ = tx.send(());
        });
        let finished = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert!(finished.is_ok(), "the write finished instead of deadlocking");
        assert_eq!(reports.lock().unwrap().len(), 1, "reported once, the nested failure is not reported again");
    }

    #[test]
    fn an_old_oversized_log_rotates_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tugboat.log");
        fs::write(&path, vec![b'a'; 200]).unwrap();
        let mut log = RotatingLog::open(path.clone(), 100).unwrap();
        log.write_all(b"new
").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(fs::metadata(numbered(&path, 1)).unwrap().len(), 200);
    }
}
