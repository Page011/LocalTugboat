//! The app log (3 files of 5 MB): every `tracing` event of the desktop app, Core and
//! the network crate goes to `<data dir>\logs\tugboat.log`. Without a subscriber every
//! error and warning in the code is dropped, and a user who says "it stopped syncing" leaves
//! nothing to read.
//!
//! Rotation is by size: at the limit `tugboat.log` becomes `.1`, `.1` becomes `.2`, the old
//! `.2` is deleted. Total on disk stays under `KEEP * MAX_BYTES`.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// The live file plus this many older ones: 3 files in all.
const KEEP: usize = 3;
const FILE: &str = "tugboat.log";

pub struct RotatingFile {
    dir: PathBuf,
    max: u64,
    file: File,
    len: u64,
}

fn numbered(dir: &Path, n: usize) -> PathBuf {
    if n == 0 {
        dir.join(FILE)
    } else {
        dir.join(format!("{FILE}.{n}"))
    }
}

impl RotatingFile {
    pub fn open(dir: &Path, max: u64) -> io::Result<RotatingFile> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new().create(true).append(true).open(numbered(dir, 0))?;
        let len = file.metadata()?.len();
        Ok(RotatingFile { dir: dir.to_path_buf(), max, file, len })
    }

    fn rotate(&mut self) -> io::Result<()> {
        // The oldest goes first; a rename onto an existing file fails on Windows.
        let _ = std::fs::remove_file(numbered(&self.dir, KEEP - 1));
        for n in (0..KEEP - 1).rev() {
            let from = numbered(&self.dir, n);
            if from.exists() {
                std::fs::rename(&from, numbered(&self.dir, n + 1))?;
            }
        }
        self.file = OpenOptions::new().create(true).append(true).open(numbered(&self.dir, 0))?;
        self.len = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.len >= self.max {
            // A failed rotation must not stop logging: keep appending to the live file.
            if let Err(e) = self.rotate() {
                eprintln!("log rotation failed: {e}");
                self.len = 0;
            }
        }
        let n = self.file.write(buf)?;
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Start logging to `dir`. Returns the error when the folder cannot be written; the app still
/// runs (the caller shows it), it just has no log.
pub fn init(dir: &Path) -> io::Result<()> {
    let writer = Mutex::new(RotatingFile::open(dir, MAX_BYTES)?);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .try_init()
        .map_err(io::Error::other)?;
    // A panic on any thread lands in the log too, not only on a console nobody sees.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        previous(info);
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(dir: &Path, n: usize) -> String {
        std::fs::read_to_string(numbered(dir, n)).unwrap_or_default()
    }

    #[test]
    fn rotates_at_the_limit_and_keeps_three_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = RotatingFile::open(tmp.path(), 10).unwrap();
        for line in ["aaaaaaaaaaaa\n", "bbbbbbbbbbbb\n", "cccccccccccc\n", "dddddddddddd\n"] {
            w.write_all(line.as_bytes()).unwrap();
        }
        // Each line is over the limit, so every write after the first rotates.
        assert_eq!(read(tmp.path(), 0), "dddddddddddd\n");
        assert_eq!(read(tmp.path(), 1), "cccccccccccc\n");
        assert_eq!(read(tmp.path(), 2), "bbbbbbbbbbbb\n");
        assert!(!numbered(tmp.path(), 3).exists(), "the oldest file was dropped");
    }

    #[test]
    fn reopening_appends_to_the_live_file() {
        let tmp = tempfile::tempdir().unwrap();
        RotatingFile::open(tmp.path(), 1000).unwrap().write_all(b"one\n").unwrap();
        RotatingFile::open(tmp.path(), 1000).unwrap().write_all(b"two\n").unwrap();
        assert_eq!(read(tmp.path(), 0), "one\ntwo\n");
    }

    #[test]
    fn an_unwritable_folder_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        assert!(RotatingFile::open(&file.join("logs"), 10).is_err());
    }
}
