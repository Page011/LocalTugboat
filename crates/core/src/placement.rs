//! Where held files live on this device. Owner: logic builder.
//!
//! Two real implementations:
//!
//! `AppStorage` (here): phone app-private storage and every core test.
//!
//! ```text
//! <root>/items/<item id>.<ext>   held files (name independent of record name/folder)
//! <root>/bin/<item id>.<ext>     files in the Bin
//! <root>/parts/<hash hex>.part   in-progress downloads (same volume as items/)
//! ```
//!
//! `PcLibrary` (apps/desktop/src-tauri, M2): the real library folder. Disk names come from
//! names::disk_file_name / clean_folder_name + names::disambiguate.
//!
//! ```text
//! <library>\<file>                        Inbox items
//! <library>\<folder disk name>\<file>     other folders (Keep, Unsorted, user folders)
//! <library>\.tugboat\<hash>.part        in-progress downloads (hidden; same drive, so
//!                                         commit is an instant rename)
//! %LOCALAPPDATA%\Local Tugboat\Bin\<item id>.<ext>  Bin
//! ```
//!
//! Contract every implementation keeps:
//! C1 Core calls these only with files that passed BLAKE3 and `names::check_media`.
//! C2 `commit` and `adopt` never overwrite a different item's file (disambiguate); for the
//!    SAME item (new content) they replace its old file.
//! C3 Every method returns the resulting path so core keeps `file_ref` accurate.
//! C4 Errors are returned, never swallowed; a failed call leaves the file where it was.
//! C5 No method touches the database (core owns it) - the PC impl tells its watcher which
//!    paths it is about to change so self-made changes are not re-imported.
//! C6 `relocate`, `to_bin`, `from_bin` and `purge` are idempotent: a file already at its
//!    target is Ok (core retries after a crash, and the PC watcher may have moved it first).

use std::path::{Path, PathBuf};

use crate::{model::*, names, CoreError, Result};

/// Everything a placement needs to know about an item to place its file.
#[derive(Debug, Clone)]
pub struct FileTarget {
    pub item: ItemId,
    pub hash: BlobHash,
    /// Record name (display name; the PC turns it into a safe disk name).
    pub name: String,
    pub ext: String,
    pub captured_at: i64,
    /// Effective folder (folders::effective_folder).
    pub folder: FolderId,
    pub folder_kind: FolderKind,
    pub folder_name: String,
    /// Current path from file_ref, if this device holds the file.
    pub current: Option<PathBuf>,
}

pub trait Placement: Send + Sync + 'static {
    /// Path for the in-progress download of `hash` (same volume as the final location).
    fn part_path(&self, hash: &BlobHash) -> Result<PathBuf>;

    /// Move a complete, verified part file into place for `t` (C1, C2). Returns the final path.
    fn commit(&self, part: &Path, t: &FileTarget) -> Result<PathBuf>;

    /// Take a local file into Tugboat (camera shot, share import, PC drop).
    /// AppStorage: streamed copy of `src` into items/ (the source is left for the caller).
    /// PcLibrary: the file is already in the library; returns `src` unchanged.
    fn adopt(&self, src: &Path, t: &FileTarget) -> Result<PathBuf>;

    /// The folder or name of `t` changed: move/rename the held file to match.
    /// AppStorage: no-op returning `t.current`. PcLibrary: rename on disk.
    fn relocate(&self, t: &FileTarget) -> Result<PathBuf>;

    /// Move the held file into the Bin area. Returns the Bin path.
    fn to_bin(&self, t: &FileTarget) -> Result<PathBuf>;

    /// Move a Bin file back to its folder location. Returns the new path.
    #[allow(clippy::wrong_self_convention)] // "from the Bin", not a conversion
    fn from_bin(&self, t: &FileTarget) -> Result<PathBuf>;

    /// Delete the held file (library or Bin). A file already gone is Ok (idempotent).
    fn purge(&self, t: &FileTarget) -> Result<()>;

    /// Free bytes on the volume that holds the part files (download free-space check).
    fn available_space(&self) -> Result<u64>;
}

/// Phone / test implementation (layout above).
#[derive(Debug, Clone)]
pub struct AppStorage {
    pub root: PathBuf,
}

impl AppStorage {
    /// Creates items/, bin/, parts/ under `root` if missing.
    pub fn new(root: PathBuf) -> Result<AppStorage> {
        for d in ["items", "bin", "parts"] {
            let p = root.join(d);
            std::fs::create_dir_all(&p).map_err(|e| CoreError::io(&p, e))?;
        }
        Ok(AppStorage { root })
    }

    /// <dir>/<item id>.<ext>. The extension comes from a synced record, so it is checked
    /// against the allowlist before it becomes part of a path.
    fn file_in(&self, dir: &str, t: &FileTarget) -> Result<PathBuf> {
        if names::ext_family(&t.ext).is_none() {
            return Err(CoreError::Invalid(format!("extension '{}' is not allowed", t.ext)));
        }
        Ok(self.root.join(dir).join(format!("{}.{}", t.item.0.hyphenated(), t.ext.to_ascii_lowercase())))
    }

    fn items_path(&self, t: &FileTarget) -> Result<PathBuf> {
        self.file_in("items", t)
    }

    fn bin_path(&self, t: &FileTarget) -> Result<PathBuf> {
        self.file_in("bin", t)
    }
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    // std::fs::rename replaces an existing destination on Windows and Unix.
    std::fs::rename(from, to).map_err(|e| CoreError::io(from, e))
}

/// Remove a file; already gone is fine (idempotent).
fn remove_if_exists(p: &Path) -> Result<()> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CoreError::io(p, e)),
    }
}

impl Placement for AppStorage {
    fn part_path(&self, hash: &BlobHash) -> Result<PathBuf> {
        Ok(self.root.join("parts").join(format!("{}.part", hex::encode(hash.0))))
    }

    fn commit(&self, part: &Path, t: &FileTarget) -> Result<PathBuf> {
        let dest = self.items_path(t)?;
        // Same item, new content: the old file goes (it may sit in the Bin area).
        let bin = self.bin_path(t)?;
        move_file(part, &dest)?;
        remove_if_exists(&bin)?;
        Ok(dest)
    }

    fn adopt(&self, src: &Path, t: &FileTarget) -> Result<PathBuf> {
        let dest = self.items_path(t)?;
        // Copy next to the final place, then rename: a crash leaves a stray .tmp, never a
        // half-written file under the final name.
        let tmp = self.root.join("parts").join(format!("{}.adopt", t.item.0.hyphenated()));
        std::fs::copy(src, &tmp).map_err(|e| CoreError::io(src, e))?;
        if let Err(e) = move_file(&tmp, &dest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(dest)
    }

    fn relocate(&self, t: &FileTarget) -> Result<PathBuf> {
        // Names are independent of record name and folder here: nothing to move.
        t.current.clone().ok_or_else(|| CoreError::Invalid("relocate: the file is not held".into()))
    }

    fn to_bin(&self, t: &FileTarget) -> Result<PathBuf> {
        let (items, bin) = (self.items_path(t)?, self.bin_path(t)?);
        if items.exists() {
            move_file(&items, &bin)?;
            return Ok(bin);
        }
        if bin.exists() {
            return Ok(bin); // already there (C6)
        }
        Err(CoreError::Io { path: items.display().to_string(), msg: "file not found [NotFound]".into() })
    }

    fn from_bin(&self, t: &FileTarget) -> Result<PathBuf> {
        let (items, bin) = (self.items_path(t)?, self.bin_path(t)?);
        if bin.exists() {
            move_file(&bin, &items)?;
            return Ok(items);
        }
        if items.exists() {
            return Ok(items); // already back (C6)
        }
        Err(CoreError::Io { path: bin.display().to_string(), msg: "file not found [NotFound]".into() })
    }

    fn purge(&self, t: &FileTarget) -> Result<()> {
        remove_if_exists(&self.items_path(t)?)?;
        remove_if_exists(&self.bin_path(t)?)
    }

    fn available_space(&self) -> Result<u64> {
        let dir = self.root.join("parts");
        fs4::available_space(&dir).map_err(|e| CoreError::io(&dir, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::*;
    use uuid::Uuid;

    fn target(n: u128, ext: &str) -> FileTarget {
        FileTarget {
            item: ItemId(Uuid::from_u128(n)),
            hash: hash(n as u8),
            name: "x".into(),
            ext: ext.into(),
            captured_at: T0,
            folder: INBOX_ID,
            folder_kind: FolderKind::Inbox,
            folder_name: "Inbox".into(),
            current: None,
        }
    }

    fn rig() -> (tempfile::TempDir, AppStorage) {
        let dir = tempfile::tempdir().unwrap();
        let s = AppStorage::new(dir.path().join("store")).unwrap();
        (dir, s)
    }

    // L27: commit / adopt / to_bin / from_bin / purge, purge idempotent, adopt leaves source,
    // part in the same dir tree.
    #[test]
    fn full_lifecycle() {
        let (dir, s) = rig();
        let t = target(1, "jpg");
        // part path is under the same root (same volume as items/).
        let part = s.part_path(&t.hash).unwrap();
        assert!(part.starts_with(&s.root));
        assert_eq!(part.parent().unwrap(), s.root.join("parts"));
        std::fs::write(&part, b"downloaded").unwrap();
        let placed = s.commit(&part, &t).unwrap();
        assert_eq!(std::fs::read(&placed).unwrap(), b"downloaded");
        assert!(!part.exists(), "part consumed by commit");
        assert_eq!(placed, s.root.join("items").join("00000000-0000-0000-0000-000000000001.jpg"));
        // Bin round trip.
        let binned = s.to_bin(&t).unwrap();
        assert!(binned.starts_with(s.root.join("bin")) && binned.exists() && !placed.exists());
        assert_eq!(s.to_bin(&t).unwrap(), binned, "to_bin is idempotent");
        let back = s.from_bin(&t).unwrap();
        assert_eq!(back, placed);
        assert!(back.exists() && !binned.exists());
        assert_eq!(s.from_bin(&t).unwrap(), back, "from_bin is idempotent");
        // Purge removes the held file and is idempotent.
        s.purge(&t).unwrap();
        assert!(!back.exists());
        s.purge(&t).unwrap();
        // Purge also clears a Bin copy.
        std::fs::write(&part, b"again").unwrap();
        s.commit(&part, &t).unwrap();
        let b2 = s.to_bin(&t).unwrap();
        s.purge(&t).unwrap();
        assert!(!b2.exists());
        // adopt: copy, source untouched.
        let src = dir.path().join("camera.jpg");
        std::fs::write(&src, b"shot").unwrap();
        let t2 = target(2, "jpg");
        let adopted = s.adopt(&src, &t2).unwrap();
        assert_eq!(std::fs::read(&adopted).unwrap(), b"shot");
        assert!(src.exists(), "adopt leaves the source for the caller");
        assert!(std::fs::read_dir(s.root.join("parts")).unwrap().next().is_none(), "no temp left behind");
        // relocate is a no-op returning the current path.
        let mut t3 = t2.clone();
        assert!(s.relocate(&t3).is_err());
        t3.current = Some(adopted.clone());
        assert_eq!(s.relocate(&t3).unwrap(), adopted);
        // Free space is a real number.
        assert!(s.available_space().unwrap() > 0);
    }

    #[test]
    fn missing_files_are_errors_not_silent() {
        let (dir, s) = rig();
        let t = target(3, "png");
        assert!(matches!(s.to_bin(&t), Err(CoreError::Io { .. })));
        assert!(matches!(s.from_bin(&t), Err(CoreError::Io { .. })));
        assert!(matches!(s.adopt(&dir.path().join("nope.jpg"), &t), Err(CoreError::Io { .. })));
        let part = s.part_path(&t.hash).unwrap();
        assert!(matches!(s.commit(&part, &t), Err(CoreError::Io { .. })));
        // A failed adopt leaves nothing under items/.
        assert!(std::fs::read_dir(s.root.join("items")).unwrap().next().is_none());
    }

    // C2: a new content for the SAME item replaces the old file (also from the Bin).
    #[test]
    fn commit_replaces_the_same_items_old_file() {
        let (_dir, s) = rig();
        let t = target(4, "jpg");
        let part = s.part_path(&t.hash).unwrap();
        std::fs::write(&part, b"v1").unwrap();
        s.commit(&part, &t).unwrap();
        let old_bin = s.to_bin(&t).unwrap();
        std::fs::write(&part, b"v2").unwrap();
        let p2 = s.commit(&part, &t).unwrap();
        assert_eq!(std::fs::read(&p2).unwrap(), b"v2");
        assert!(!old_bin.exists(), "stale Bin copy removed");
    }

    // C2: files of different items never collide, even with the same record name.
    #[test]
    fn different_items_never_share_a_file() {
        let (dir, s) = rig();
        let src = dir.path().join("a.jpg");
        std::fs::write(&src, b"1").unwrap();
        let a = s.adopt(&src, &target(5, "jpg")).unwrap();
        let b = s.adopt(&src, &target(6, "jpg")).unwrap();
        assert_ne!(a, b);
    }

    // Trust boundary: the extension is from a synced record and becomes part of a path.
    #[test]
    fn hostile_extension_is_rejected() {
        let (dir, s) = rig();
        let src = dir.path().join("a.jpg");
        std::fs::write(&src, b"1").unwrap();
        for ext in ["exe", "jpg/../../evil", "..", "", "jpg\\x", "jpg:stream"] {
            let t = target(7, ext);
            assert!(matches!(s.adopt(&src, &t), Err(CoreError::Invalid(_))), "{ext:?}");
            assert!(matches!(s.to_bin(&t), Err(CoreError::Invalid(_))));
            assert!(matches!(s.purge(&t), Err(CoreError::Invalid(_))));
        }
        // Nothing escaped the storage root.
        assert!(!dir.path().join("evil").exists());
    }
}
