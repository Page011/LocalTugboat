//! Zip a folder that was dragged onto the window, so it travels as one file.
//!
//! The archive is written by hand (std only), with the "store" method: photos and videos do not
//! compress, so the zip is a plain container and costs no CPU. What goes in:
//! - every file under the folder, in sorted order, with the folder's own name as the top directory
//!   ("Trip/IMG_1.jpg"), forward slashes, UTF-8 names (general purpose bit 11);
//! - a directory entry for each folder that would otherwise vanish because nothing is in it;
//! - the modified time of each file as a DOS date and time (local time, 2 s steps).
//!
//! What is left out: links (junctions and symlinks are never followed, they could lead out of the
//! folder or around in a circle) and the `desktop.ini` / `Thumbs.db` files Windows keeps in folders.
//! Hidden files are included.
//!
//! Each local header is written first, the file is streamed through the CRC, then the CRC is patched
//! into the header (the output is a seekable temp file). Zip64 records are written whenever a size or an
//! offset reaches 0xFFFFFFFF or there are 0xFFFF entries or more. A file that changes size while it is
//! read fails the zip: a half-matching archive is worse than none.

use std::{
    fs::File,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
};

use windows::Win32::{
    Foundation::{FILETIME, SYSTEMTIME},
    System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
};

use crate::{library::config::HIDDEN_DIR, winfs};

/// Files Windows keeps in a folder for its own use; they are not part of the user's data.
const SKIPPED_NAMES: [&str; 2] = ["desktop.ini", "thumbs.db"];

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_END: u32 = 0x0605_4b50;
const SIG_END64: u32 = 0x0606_4b50;
const SIG_LOCATOR64: u32 = 0x0706_4b50;
const EXTRA_ZIP64: u16 = 0x0001;
const FLAG_UTF8: u16 = 0x0800;
const MAX32: u64 = 0xFFFF_FFFF;
const MAX16: usize = 0xFFFF;
const ATTR_DIRECTORY: u32 = 0x10;
/// 1980-01-01 00:00, the earliest DOS date (also used when a time cannot be read).
const DOS_EPOCH: (u16, u16) = (0, 0x0021);
const CHUNK: usize = 256 * 1024;

/// Why a zip could not be made: the file inside the folder that failed (None = the archive itself,
/// for example a full disk) and the reason in plain words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipError {
    pub file: Option<String>,
    pub reason: String,
}

impl ZipError {
    fn on_file(file: &str, e: &io::Error) -> ZipError {
        ZipError { file: Some(file.to_string()), reason: io_text(e) }
    }

    fn archive(e: &io::Error) -> ZipError {
        ZipError { file: None, reason: io_text(e) }
    }

    /// The one-line text for the drop report: "ZIP FAILED - Trip: day 1/IMG_1.jpg <reason>". The file is
    /// named from inside the folder (the folder's own name is already in the line).
    pub fn text(&self, folder: &str) -> String {
        match &self.file {
            // The folder itself could not be read: the reason alone says it.
            Some(f) if f == folder => format!("ZIP FAILED - {folder}: {}", self.reason),
            Some(f) => {
                let inside = f.strip_prefix(&format!("{folder}/")).unwrap_or(f);
                format!("ZIP FAILED - {folder}: {inside} {}", self.reason)
            }
            None => format!("ZIP FAILED - {folder}: {}", self.reason),
        }
    }
}

fn io_text(e: &io::Error) -> String {
    let t = e.to_string();
    t.split(" (os error").next().unwrap_or(&t).to_string()
}

/// One entry of the archive. A directory has `source: None` and a name ending in '/'.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub name: String,
    pub source: Option<PathBuf>,
    pub size: u64,
    /// DOS (time, date).
    pub dos: (u16, u16),
}

/// What a folder holds, found before anything is written: the entries in archive order and the totals.
#[derive(Debug, Clone)]
pub struct Plan {
    pub entries: Vec<ZipEntry>,
    pub files: u32,
    pub bytes: u64,
    /// An estimate of what the zip's own records add to `bytes` (for the free-space check).
    pub overhead: u64,
}

// ---------- CRC-32 ----------

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (the zip / PNG / zlib one, polynomial 0xEDB88320).
#[derive(Debug, Clone, Copy)]
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Crc32 {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(self) -> u32 {
        !self.0
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

// ---------- planning ----------

/// DOS (time, date) of a file's modified time as local wall-clock time, to the 2 s step the format has.
/// Every reader shows a DOS time as the wall clock of that date, so the UTC time is converted with the
/// daylight saving rule in force on the file's own date (not today's). Before 1980 or after 2107
/// (what the format can say) it clamps.
fn dos_time(meta: &std::fs::Metadata) -> (u16, u16) {
    let t = meta.last_write_time();
    let utc = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
    let mut utc_parts = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: plain value conversions between stack variables.
    let converted = unsafe { FileTimeToSystemTime(&utc, &mut utc_parts) }.is_ok()
        && unsafe { SystemTimeToTzSpecificLocalTime(None, &utc_parts, &mut local) }.is_ok();
    if !converted {
        return DOS_EPOCH;
    }
    dos_pack(i64::from(local.wYear), local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond)
}

/// DOS (time, date) of a FILETIME (100 ns steps since 1601-01-01), read as it stands (no zone change).
#[cfg(test)]
fn dos_from_filetime(ft: u64) -> (u16, u16) {
    const UNIX_OFFSET_SECS: u64 = 11_644_473_600;
    let secs = ft / 10_000_000;
    if secs < UNIX_OFFSET_SECS {
        return DOS_EPOCH;
    }
    let unix = secs - UNIX_OFFSET_SECS;
    let (days, rest) = ((unix / 86_400) as i64, (unix % 86_400) as u32);
    // Days since 1970-01-01 to a calendar date (the civil-from-days algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u16;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u16;
    let year = yoe + era * 400 + i64::from(month <= 2);
    dos_pack(year, month, day, (rest / 3600) as u16, ((rest % 3600) / 60) as u16, (rest % 60) as u16)
}

/// Pack a calendar date and time into DOS (time, date), clamped to 1980-01-01 .. 2107-12-31.
fn dos_pack(year: i64, month: u16, day: u16, hour: u16, minute: u16, second: u16) -> (u16, u16) {
    if year < 1980 {
        return DOS_EPOCH;
    }
    if year > 2107 {
        return ((23 << 11) | (59 << 5) | 29, (127 << 9) | (12 << 5) | 31);
    }
    ((hour << 11) | (minute << 5) | (second / 2), (((year - 1980) as u16) << 9) | (month << 5) | day)
}

fn is_skipped_name(name: &str) -> bool {
    SKIPPED_NAMES.iter().any(|s| name.eq_ignore_ascii_case(s))
}

/// Room the zip's own records take on top of the file bytes, for the free-space check: a local header,
/// a central header (the name is in both) and, to be safe, the zip64 extra fields.
fn record_overhead(name: &str) -> u64 {
    30 + 46 + 2 * name.len() as u64 + 20 + 28
}

/// Is `path` the library's hidden folder (`skip`)? Only looked at for folders named like it.
fn is_skipped_dir(name: &str, path: &Path, skip: Option<&Path>) -> bool {
    match skip {
        Some(s) if name.eq_ignore_ascii_case(HIDDEN_DIR) => std::fs::canonicalize(path).is_ok_and(|p| p == s),
        _ => false,
    }
}

/// List `root` and everything under it, ready to zip. Sorted, links skipped, housekeeping files skipped.
/// `skip` is the library's own hidden folder (canonical path): it holds the library's marker and the
/// copies in progress, so it never goes in a zip. An empty plan (`files == 0`) is for the caller to refuse.
pub fn plan_folder(root: &Path, skip: Option<&Path>) -> Result<Plan, ZipError> {
    /// A folder being listed: its children still to visit, in order.
    struct Frame {
        rel: String,
        kids: std::vec::IntoIter<(String, PathBuf, std::fs::Metadata)>,
        /// Entries in the plan when this folder was opened; unchanged at the end = nothing came out of it.
        before: usize,
        meta: Option<std::fs::Metadata>,
    }

    fn open(dir: &Path, rel: String, meta: Option<std::fs::Metadata>, before: usize) -> Result<Frame, ZipError> {
        let fail = |e: &io::Error| ZipError::on_file(&rel, e);
        let mut kids = Vec::new();
        for e in std::fs::read_dir(dir).map_err(|e| fail(&e))? {
            let e = e.map_err(|e| fail(&e))?;
            // Does not follow a link and costs no extra call on Windows.
            let m = e.metadata().map_err(|err| ZipError::on_file(&format!("{rel}/{}", e.file_name().to_string_lossy()), &err))?;
            kids.push((e.file_name().to_string_lossy().into_owned(), e.path(), m));
        }
        kids.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
        Ok(Frame { rel, kids: kids.into_iter(), before, meta })
    }

    let top = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut plan = Plan { entries: Vec::new(), files: 0, bytes: 0, overhead: 0 };
    // An explicit stack, not recursion: a very deep tree must not overflow this thread's stack.
    let mut stack = vec![open(root, top, None, 0)?];
    while let Some(frame) = stack.last_mut() {
        let Some((name, path, meta)) = frame.kids.next() else {
            let done = stack.pop().expect("a frame was just looked at");
            if plan.entries.len() == done.before {
                if let Some(m) = &done.meta {
                    // Nothing came out of it (empty, or only skipped files): keep the folder itself.
                    plan.entries.push(ZipEntry { name: format!("{}/", done.rel), source: None, size: 0, dos: dos_time(m) });
                }
            }
            continue;
        };
        if winfs::is_link(&path, &meta) {
            continue;
        }
        let child = format!("{}/{name}", frame.rel);
        if meta.is_dir() {
            if is_skipped_dir(&name, &path, skip) {
                continue;
            }
            let next = open(&path, child, Some(meta), plan.entries.len())?;
            stack.push(next);
        } else if !is_skipped_name(&name) {
            plan.entries.push(ZipEntry { name: child, source: Some(path), size: meta.len(), dos: dos_time(&meta) });
            plan.files += 1;
            plan.bytes += meta.len();
        }
    }
    plan.overhead = plan.entries.iter().map(|e| record_overhead(&e.name)).sum();
    Ok(plan)
}

// ---------- writing ----------

/// A writer that knows where it is, so offsets need no extra seek.
struct Out<W: Write + Seek> {
    w: W,
    pos: u64,
}

impl<W: Write + Seek> Out<W> {
    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    fn u16(&mut self, v: u16) -> io::Result<()> {
        self.put(&v.to_le_bytes())
    }

    fn u32(&mut self, v: u32) -> io::Result<()> {
        self.put(&v.to_le_bytes())
    }

    fn u64(&mut self, v: u64) -> io::Result<()> {
        self.put(&v.to_le_bytes())
    }

    /// Overwrite 4 bytes at `at` and come back to the end.
    fn patch_u32(&mut self, at: u64, v: u32) -> io::Result<()> {
        self.w.seek(SeekFrom::Start(at))?;
        self.w.write_all(&v.to_le_bytes())?;
        self.w.seek(SeekFrom::Start(self.pos))?;
        Ok(())
    }
}

/// What the central directory needs to remember about a written entry.
struct Written {
    crc: u32,
    offset: u64,
    /// The local header carries zip64 sizes.
    big: bool,
}

fn min32(v: u64, big: bool) -> u32 {
    if big { 0xFFFF_FFFF } else { v as u32 }
}

/// Write the archive to `out` (made new: an existing file is an error). On any error the partial file is removed.
/// `progress` gets the number of source bytes read so far.
pub fn write_zip(out: &Path, entries: &[ZipEntry], progress: &mut dyn FnMut(u64)) -> Result<(), ZipError> {
    write_zip_with(out, entries, false, progress)
}

/// `force_zip64` writes every zip64 record even for a small archive (the tests use it to check the readers).
fn write_zip_with(out: &Path, entries: &[ZipEntry], force_zip64: bool, progress: &mut dyn FnMut(u64)) -> Result<(), ZipError> {
    let file = File::options().write(true).create_new(true).open(out).map_err(|e| ZipError::archive(&e))?;
    let result = write_to(BufWriter::with_capacity(CHUNK, file), entries, force_zip64, progress);
    if result.is_err() {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn write_to<W: Write + Seek>(w: W, entries: &[ZipEntry], force_zip64: bool, progress: &mut dyn FnMut(u64)) -> Result<(), ZipError> {
    let mut out = Out { w, pos: 0 };
    let mut written: Vec<Written> = Vec::with_capacity(entries.len());
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    let archive = |e: io::Error| ZipError::archive(&e);

    for e in entries {
        if e.name.len() > MAX16 {
            return Err(ZipError { file: Some(e.name.clone()), reason: "the path is too long for a zip".into() });
        }
        let big = force_zip64 || e.size >= MAX32;
        let offset = out.pos;
        out.u32(SIG_LOCAL).map_err(archive)?;
        out.u16(if big { 45 } else { 20 }).map_err(archive)?;
        out.u16(FLAG_UTF8).map_err(archive)?;
        out.u16(0).map_err(archive)?; // stored
        out.u16(e.dos.0).map_err(archive)?;
        out.u16(e.dos.1).map_err(archive)?;
        out.u32(0).map_err(archive)?; // the CRC, patched below
        out.u32(min32(e.size, big)).map_err(archive)?;
        out.u32(min32(e.size, big)).map_err(archive)?;
        out.u16(e.name.len() as u16).map_err(archive)?;
        out.u16(if big { 20 } else { 0 }).map_err(archive)?;
        out.put(e.name.as_bytes()).map_err(archive)?;
        if big {
            out.u16(EXTRA_ZIP64).map_err(archive)?;
            out.u16(16).map_err(archive)?;
            out.u64(e.size).map_err(archive)?;
            out.u64(e.size).map_err(archive)?;
        }

        let mut crc = Crc32::new();
        if let Some(src) = &e.source {
            let mut f = File::open(src).map_err(|err| ZipError::on_file(&e.name, &err))?;
            let mut n: u64 = 0;
            loop {
                let k = match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(k) => k,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => return Err(ZipError::on_file(&e.name, &err)),
                };
                n += k as u64;
                if n > e.size {
                    return Err(ZipError { file: Some(e.name.clone()), reason: "changed size while it was being read".into() });
                }
                crc.update(&buf[..k]);
                out.put(&buf[..k]).map_err(archive)?;
                done += k as u64;
                progress(done);
            }
            if n != e.size {
                return Err(ZipError { file: Some(e.name.clone()), reason: "changed size while it was being read".into() });
            }
        }
        let crc = crc.finish();
        out.patch_u32(offset + 14, crc).map_err(archive)?;
        written.push(Written { crc, offset, big });
    }

    // Central directory.
    let cd_offset = out.pos;
    for (e, wr) in entries.iter().zip(&written) {
        let big_size = wr.big;
        let big_offset = force_zip64 || wr.offset >= MAX32;
        let mut extra: Vec<u8> = Vec::new();
        if big_size {
            extra.extend_from_slice(&e.size.to_le_bytes());
            extra.extend_from_slice(&e.size.to_le_bytes());
        }
        if big_offset {
            extra.extend_from_slice(&wr.offset.to_le_bytes());
        }
        let zip64 = !extra.is_empty();
        let version = if zip64 { 45 } else { 20 };
        out.u32(SIG_CENTRAL).map_err(archive)?;
        // Made by: NTFS (host 11), so readers that go by the host byte (Info-ZIP) also treat the names as
        // UTF-8; then the spec version needed.
        out.u16(0x0B00 | version).map_err(archive)?;
        out.u16(version).map_err(archive)?;
        out.u16(FLAG_UTF8).map_err(archive)?;
        out.u16(0).map_err(archive)?;
        out.u16(e.dos.0).map_err(archive)?;
        out.u16(e.dos.1).map_err(archive)?;
        out.u32(wr.crc).map_err(archive)?;
        out.u32(min32(e.size, big_size)).map_err(archive)?;
        out.u32(min32(e.size, big_size)).map_err(archive)?;
        out.u16(e.name.len() as u16).map_err(archive)?;
        out.u16(if zip64 { 4 + extra.len() as u16 } else { 0 }).map_err(archive)?;
        out.u16(0).map_err(archive)?; // comment
        out.u16(0).map_err(archive)?; // disk
        out.u16(0).map_err(archive)?; // internal attributes
        out.u32(if e.source.is_none() { ATTR_DIRECTORY } else { 0 }).map_err(archive)?;
        out.u32(min32(wr.offset, big_offset)).map_err(archive)?;
        out.put(e.name.as_bytes()).map_err(archive)?;
        if zip64 {
            out.u16(EXTRA_ZIP64).map_err(archive)?;
            out.u16(extra.len() as u16).map_err(archive)?;
            out.put(&extra).map_err(archive)?;
        }
    }
    let cd_size = out.pos - cd_offset;

    let count = entries.len();
    let big_end = force_zip64 || count >= MAX16 || cd_offset >= MAX32 || cd_size >= MAX32;
    if big_end {
        let end64 = out.pos;
        out.u32(SIG_END64).map_err(archive)?;
        out.u64(44).map_err(archive)?; // size of the rest of this record
        out.u16(45).map_err(archive)?;
        out.u16(45).map_err(archive)?;
        out.u32(0).map_err(archive)?;
        out.u32(0).map_err(archive)?;
        out.u64(count as u64).map_err(archive)?;
        out.u64(count as u64).map_err(archive)?;
        out.u64(cd_size).map_err(archive)?;
        out.u64(cd_offset).map_err(archive)?;
        out.u32(SIG_LOCATOR64).map_err(archive)?;
        out.u32(0).map_err(archive)?;
        out.u64(end64).map_err(archive)?;
        out.u32(1).map_err(archive)?;
    }
    out.u32(SIG_END).map_err(archive)?;
    out.u16(0).map_err(archive)?;
    out.u16(0).map_err(archive)?;
    let n16 = if big_end { MAX16 as u16 } else { count as u16 };
    out.u16(n16).map_err(archive)?;
    out.u16(n16).map_err(archive)?;
    out.u32(min32(cd_size, big_end)).map_err(archive)?;
    out.u32(min32(cd_offset, big_end)).map_err(archive)?;
    out.u16(0).map_err(archive)?; // comment
    out.w.flush().map_err(archive)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Windows' own zip reader (.NET `ZipFile`, through PowerShell) extracts `zip` into `dest` (made new).
    /// Returns the largest difference in seconds between an extracted file's time and its source's time
    /// under `src_parent` (the folder that holds the zipped folder). Panics with the reader's message when
    /// it cannot open the archive.
    pub(crate) fn extract_with_dotnet(zip: &Path, dest: &Path, src_parent: &Path) -> f64 {
        let script = "$ErrorActionPreference='Stop'; \
            Add-Type -AssemblyName System.IO.Compression.FileSystem; \
            [System.IO.Compression.ZipFile]::ExtractToDirectory($env:ZIPFILE, $env:ZIPDEST); \
            $z=[System.IO.Compression.ZipFile]::OpenRead($env:ZIPFILE); $max=0.0; \
            foreach($e in $z.Entries){ if($e.FullName.EndsWith('/')){continue}; \
              $o=Get-Item -LiteralPath (Join-Path $env:ZIPSRC $e.FullName); \
              $d=[math]::Abs(($e.LastWriteTime.DateTime - $o.LastWriteTime).TotalSeconds); if($d -gt $max){$max=$d} }; \
            $z.Dispose(); Write-Output ('MAXDIFF ' + $max.ToString([cultureinfo]::InvariantCulture))";
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("ZIPFILE", zip)
            .env("ZIPDEST", dest)
            .env("ZIPSRC", src_parent)
            .output()
            .expect("run powershell");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "the .NET zip reader refused the archive: {text} {}", String::from_utf8_lossy(&out.stderr));
        text.lines().find_map(|l| l.strip_prefix("MAXDIFF ")).and_then(|v| v.trim().parse().ok()).unwrap_or_else(|| panic!("no MAXDIFF in {text:?}"))
    }

    /// Windows' tar (libarchive, `System32\tar.exe`) extracts `zip` into `dest` (made here). Unlike .NET's
    /// reader it checks every file's CRC, so it is the one that proves the CRCs in the archive are right.
    pub(crate) fn extract_with_tar(zip: &Path, dest: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dest).unwrap();
        let tar = Path::new(&std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())).join("System32").join("tar.exe");
        let out = std::process::Command::new(tar).arg("-xf").arg(zip).arg("-C").arg(dest).output().expect("run tar.exe");
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    /// Every file and folder under `dir`, relative with '/', sorted; files with their bytes.
    pub(crate) fn tree(dir: &Path) -> Vec<(String, Option<Vec<u8>>)> {
        fn go(base: &Path, dir: &Path, out: &mut Vec<(String, Option<Vec<u8>>)>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
                if p.is_dir() {
                    out.push((rel, None));
                    go(base, &p, out);
                } else {
                    out.push((rel, Some(std::fs::read(&p).unwrap())));
                }
            }
        }
        let mut v = Vec::new();
        go(dir, dir, &mut v);
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn le32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    fn sample(root: &Path) -> PathBuf {
        let dir = root.join("Trip");
        std::fs::create_dir_all(dir.join("sub").join("deeper")).unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("IMG_1.jpg"), (0..70_000u32).map(|i| (i * 7 % 251) as u8).collect::<Vec<u8>>()).unwrap();
        std::fs::write(dir.join("zero.bin"), b"").unwrap();
        std::fs::write(dir.join("sub").join("note.txt"), b"hello").unwrap();
        std::fs::write(dir.join("sub").join("deeper").join("caf\u{e9} \u{65e5}\u{672c}.txt"), b"unicode").unwrap();
        dir
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.finish(), 0xCBF4_3926);
        let mut split = Crc32::new();
        split.update(b"12345");
        split.update(b"6789");
        assert_eq!(split.finish(), 0xCBF4_3926, "feeding it in pieces gives the same answer");
        assert_eq!(Crc32::new().finish(), 0);
    }

    #[test]
    fn dos_dates_come_out_right() {
        let ft = |unix: u64| (unix + 11_644_473_600) * 10_000_000;
        assert_eq!(dos_from_filetime(ft(315_532_800)), DOS_EPOCH, "1980-01-01 00:00:00");
        // 2024-02-29 13:45:31 -> 13:45:30 (2 s steps)
        let (t, d) = dos_from_filetime(ft(1_709_214_331));
        assert_eq!((d >> 9, (d >> 5) & 15, d & 31), (44, 2, 29));
        assert_eq!((t >> 11, (t >> 5) & 63, (t & 31) * 2), (13, 45, 30));
        assert_eq!(dos_from_filetime(ft(0)), DOS_EPOCH, "before 1980 clamps");
        assert_eq!(dos_from_filetime(0), DOS_EPOCH, "before 1970 clamps");
        assert_eq!(dos_from_filetime(ft(5_000_000_000)).1 >> 9, 127, "after 2107 clamps");
    }

    #[test]
    fn the_plan_is_sorted_skips_housekeeping_files_and_keeps_empty_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = sample(tmp.path());
        std::fs::write(dir.join("desktop.ini"), b"x").unwrap();
        std::fs::write(dir.join("sub").join("THUMBS.DB"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("only-ini")).unwrap();
        std::fs::write(dir.join("only-ini").join("Desktop.ini"), b"x").unwrap();
        let plan = plan_folder(&dir, None).unwrap();
        assert!(plan.overhead > 0, "the zip's own records are counted for the space check");
        let names: Vec<&str> = plan.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["Trip/empty/", "Trip/IMG_1.jpg", "Trip/only-ini/", "Trip/sub/deeper/caf\u{e9} \u{65e5}\u{672c}.txt", "Trip/sub/note.txt", "Trip/zero.bin"]
        );
        assert_eq!((plan.files, plan.bytes), (4, 70_000 + 7 + 5));
    }

    #[test]
    fn a_junction_is_skipped_and_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = sample(tmp.path());
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"x").unwrap();
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(dir.join("link")).arg(&outside).output().map(|o| o.status.success()).unwrap_or(false);
        assert!(made, "mklink /J needs no privilege");
        let plan = plan_folder(&dir, None).unwrap();
        assert!(plan.entries.iter().all(|e| !e.name.contains("link") && !e.name.contains("secret")), "{:?}", plan.entries);
        assert_eq!(plan.files, 4);
        std::fs::remove_dir(dir.join("link")).unwrap(); // the junction, not its target
        assert!(outside.join("secret.txt").exists());
    }

    /// Set a file's modified time (a Unix time in seconds).
    fn touch(p: &Path, unix_secs: u64) {
        let f = File::options().write(true).open(p).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs)).unwrap();
    }

    /// `sample` plus a file bigger than one read chunk (so the CRC runs over several reads and is patched
    /// into an entry that is not the first) and files modified in January and in July (the two halves of a
    /// daylight saving year: a time converted with today's offset is an hour out for one of them).
    fn sample_big(root: &Path) -> PathBuf {
        let dir = sample(root);
        std::fs::write(dir.join("big.bin"), (0..3 * CHUNK as u32 / 2 + 11).map(|i| (i * 31 % 253) as u8).collect::<Vec<u8>>()).unwrap();
        touch(&dir.join("sub").join("note.txt"), 1_704_164_800); // 2024-01-02
        touch(&dir.join("IMG_1.jpg"), 1_720_000_000); // 2024-07-03
        dir
    }

    #[test]
    fn windows_own_reader_gets_every_byte_and_the_empty_folders_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = sample_big(tmp.path());
        let plan = plan_folder(&dir, None).unwrap();
        let zip = tmp.path().join("Trip.zip");
        let mut last = 0;
        write_zip(&zip, &plan.entries, &mut |d| last = d).unwrap();
        assert_eq!(last, plan.bytes, "progress ends at the total");
        let dest = tmp.path().join("out");
        let diff = extract_with_dotnet(&zip, &dest, tmp.path());
        assert!(diff <= 2.0, "times are kept to the DOS 2 s step (January and July files alike), off by {diff}");
        assert_eq!(tree(&dest.join("Trip")), tree(&dir));
        assert!(dest.join("Trip").join("empty").is_dir());
        // tar checks the CRC of every file; the .NET reader does not.
        let by_tar = tmp.path().join("out-tar");
        extract_with_tar(&zip, &by_tar).unwrap();
        assert_eq!(tree(&by_tar.join("Trip")), tree(&dir));
    }

    #[test]
    fn a_wrong_crc_is_caught_by_the_tar_check_so_that_check_means_something() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = sample_big(tmp.path());
        let plan = plan_folder(&dir, None).unwrap();
        let zip = tmp.path().join("Trip.zip");
        write_zip(&zip, &plan.entries, &mut |_| {}).unwrap();
        let mut bytes = std::fs::read(&zip).unwrap();
        // The file data of the first entry with data starts after its local header (30 + name).
        let first = plan.entries.iter().position(|e| e.size > 0).unwrap();
        let at: usize = plan.entries[..first].iter().map(|e| 30 + e.name.len() + e.size as usize).sum::<usize>() + 30 + plan.entries[first].name.len();
        bytes[at] ^= 0xFF;
        let bad = tmp.path().join("Bad.zip");
        std::fs::write(&bad, bytes).unwrap();
        assert!(extract_with_tar(&bad, &tmp.path().join("out-bad")).is_err(), "tar refuses a zip whose data does not match its CRC");
    }

    #[test]
    fn zip64_records_are_written_when_forced_and_windows_reader_still_opens_the_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = sample_big(tmp.path());
        let plan = plan_folder(&dir, None).unwrap();
        let zip = tmp.path().join("Trip64.zip");
        write_zip_with(&zip, &plan.entries, true, &mut |_| {}).unwrap();
        let bytes = std::fs::read(&zip).unwrap();
        assert!(contains(&bytes, &SIG_END64.to_le_bytes()) && contains(&bytes, &SIG_LOCATOR64.to_le_bytes()));
        let small = tmp.path().join("Trip32.zip");
        write_zip(&small, &plan.entries, &mut |_| {}).unwrap();
        assert!(!contains(&std::fs::read(&small).unwrap(), &SIG_END64.to_le_bytes()), "a small archive has no zip64 records");
        let dest = tmp.path().join("out64");
        extract_with_dotnet(&zip, &dest, tmp.path());
        assert_eq!(tree(&dest.join("Trip")), tree(&dir));
        let by_tar = tmp.path().join("out64-tar");
        extract_with_tar(&zip, &by_tar).unwrap();
        assert_eq!(tree(&by_tar.join("Trip")), tree(&dir));
    }

    #[test]
    fn the_librarys_hidden_folder_is_left_out_and_a_deep_tree_does_not_need_deep_recursion() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("Library");
        std::fs::create_dir_all(lib.join(HIDDEN_DIR)).unwrap();
        std::fs::write(lib.join(HIDDEN_DIR).join("marker"), b"x").unwrap();
        std::fs::write(lib.join("a.txt"), b"a").unwrap();
        let other = lib.join("Work").join(HIDDEN_DIR); // a folder that only has the same name stays
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("keep.txt"), b"k").unwrap();
        let skip = std::fs::canonicalize(lib.join(HIDDEN_DIR)).unwrap();
        let names: Vec<String> = plan_folder(&lib, Some(&skip)).unwrap().entries.into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Library/a.txt", "Library/Work/.tugboat/keep.txt"]);

        // 3000 levels is far past what recursion on a 2 MiB thread stack survives; the paths stay short
        // enough for Windows because each level is one character.
        let root = PathBuf::from(format!(r"\\?\{}", tmp.path().join("Deep").display()));
        let mut p = root.clone();
        for _ in 0..3000 {
            p.push("d");
        }
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("end.txt"), b"e").unwrap();
        let walk_root = root.clone();
        let plan = std::thread::Builder::new().stack_size(256 * 1024).spawn(move || plan_folder(&walk_root, None)).unwrap().join().unwrap().unwrap();
        assert_eq!((plan.files, plan.entries.len()), (1, 1));
        assert!(plan.entries[0].name.starts_with("Deep/d/d/") && plan.entries[0].name.ends_with("/d/end.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn zip64_end_records_start_at_65535_entries_and_not_before() {
        let dirs = |n: usize| -> Vec<ZipEntry> { (0..n).map(|i| ZipEntry { name: format!("d/{i}/"), source: None, size: 0, dos: DOS_EPOCH }).collect() };
        for (n, want64) in [(MAX16 - 1, false), (MAX16, true)] {
            let mut sink = io::Cursor::new(Vec::new());
            write_to(&mut sink, &dirs(n), false, &mut |_| {}).unwrap();
            let bytes = sink.into_inner();
            assert_eq!(contains(&bytes, &SIG_END64.to_le_bytes()), want64, "{n} entries");
            let end = bytes.len() - 22;
            assert_eq!(le32(&bytes, end), SIG_END);
            assert_eq!(u16::from_le_bytes([bytes[end + 10], bytes[end + 11]]), if want64 { 0xFFFF } else { n as u16 });
        }
    }

    #[test]
    fn a_file_that_changes_size_fails_the_zip_by_name_and_leaves_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("grow.bin");
        std::fs::write(&f, b"0123456789").unwrap();
        let zip = tmp.path().join("x.zip");
        for planned in [4u64, 20] {
            let entries = [ZipEntry { name: "Dir/grow.bin".into(), source: Some(f.clone()), size: planned, dos: DOS_EPOCH }];
            let err = write_zip(&zip, &entries, &mut |_| {}).unwrap_err();
            assert_eq!(err.file.as_deref(), Some("Dir/grow.bin"));
            assert!(err.text("Dir").starts_with("ZIP FAILED - Dir: grow.bin "), "{}", err.text("Dir"));
            assert!(!zip.exists(), "the partial archive is removed");
        }
    }

    #[test]
    fn local_and_central_records_agree_for_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, b"123456789").unwrap();
        let zip = tmp.path().join("a.zip");
        write_zip(&zip, &[ZipEntry { name: "a.txt".into(), source: Some(f), size: 9, dos: (0x1234, 0x5678) }], &mut |_| {}).unwrap();
        let b = std::fs::read(&zip).unwrap();
        assert_eq!(le32(&b, 0), SIG_LOCAL);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), FLAG_UTF8, "UTF-8 names");
        assert_eq!(u16::from_le_bytes([b[8], b[9]]), 0, "stored");
        assert_eq!(le32(&b, 14), 0xCBF4_3926, "the CRC is patched into the local header");
        assert_eq!((le32(&b, 18), le32(&b, 22)), (9, 9));
        let cd = 30 + 5 + 9;
        assert_eq!(le32(&b, cd), SIG_CENTRAL);
        assert_eq!(le32(&b, cd + 16), 0xCBF4_3926);
        assert_eq!(le32(&b, cd + 42), 0, "offset of the one local header");
        assert_eq!(le32(&b, b.len() - 22), SIG_END);
    }
}
