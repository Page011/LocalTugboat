//! Library location and the folder-to-directory map, kept in `<data dir>\library.json`.
//!
//! Why a file and not the core database: the PC placement needs the library path before
//! `Core::open` runs (open sweeps and may move files), and the database is opened by Core.
//! The file is written atomically (temp + rename) so a crash never leaves half a config, and a
//! file that exists but does not parse is an ERROR, never silently replaced by a fresh library
//! (that would orphan every item's file path).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "library.json";
/// Hidden folder at the library root: identity marker and in-progress downloads only.
pub const HIDDEN_DIR: &str = ".tugboat";
pub const MARKER_FILE: &str = "library-id";

/// One folder's directory: its name on disk and the NTFS identity it had when last seen, so a
/// rename in Explorer is recognised as the same directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A file that was already in the library when 0.2.0 started syncing every file type. It is listed
/// "FROM BEFORE 0.2.0 - not synced" instead of being imported: an import would start its expiry
/// clock and the file would leave the user's folder a day later. Matched by path or NTFS identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyFile {
    /// `placement::norm` of the path.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryConfig {
    /// Library root. None = not chosen yet.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Library id; the marker file under `.tugboat` must hold the same text.
    #[serde(default)]
    pub id: Option<String>,
    /// Folder id (uuid text) -> directory.
    #[serde(default)]
    pub dirs: BTreeMap<String, DirEntry>,
    /// True once the first scan of this library under 0.2.0 has recorded `legacy`. A library made
    /// by 0.2.0 (empty at the start) is born done; a 0.1.x `library.json` has no such key.
    #[serde(default)]
    pub legacy_done: bool,
    /// Non-photo files that were in the library before 0.2.0 (see `LegacyFile`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legacy: Vec<LegacyFile>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("LIBRARY CONFIG UNREADABLE - {path}: {msg}")]
    Unreadable { path: String, msg: String },
}

impl LibraryConfig {
    /// A missing file is a first run (default config); an unreadable or corrupt one is an error.
    pub fn load(data_dir: &Path) -> Result<LibraryConfig, ConfigError> {
        let p = data_dir.join(FILE_NAME);
        let bytes = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LibraryConfig::default()),
            Err(e) => return Err(ConfigError::Unreadable { path: p.display().to_string(), msg: e.to_string() }),
        };
        serde_json::from_slice(&bytes).map_err(|e| ConfigError::Unreadable { path: p.display().to_string(), msg: e.to_string() })
    }

    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let p = data_dir.join(FILE_NAME);
        let tmp = data_dir.join(format!("{FILE_NAME}.tmp"));
        let text = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &p)
    }
}

/// Read the id in a library's marker file, None when the folder or marker is missing.
pub fn read_marker(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join(HIDDEN_DIR).join(MARKER_FILE)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Create `.tugboat` (hidden) with a fresh marker id; returns the id.
pub fn write_new_marker(root: &Path) -> std::io::Result<String> {
    let dir = root.join(HIDDEN_DIR);
    std::fs::create_dir_all(&dir)?;
    // Hiding is cosmetic: a failure must not stop the library from working.
    if let Err(e) = crate::winfs::hide(&dir) {
        tracing::warn!("{e}");
    }
    let id = uuid::Uuid::now_v7().to_string();
    std::fs::write(dir.join(MARKER_FILE), &id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_a_first_run_but_garbage_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(LibraryConfig::load(d.path()).unwrap(), LibraryConfig::default());
        std::fs::write(d.path().join(FILE_NAME), b"{not json").unwrap();
        assert!(matches!(LibraryConfig::load(d.path()), Err(ConfigError::Unreadable { .. })));
    }

    #[test]
    fn save_then_load_roundtrips_and_leaves_no_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let mut c = LibraryConfig { path: Some(PathBuf::from(r"D:\Local Tugboat")), id: Some("abc".into()), dirs: BTreeMap::new(), ..Default::default() };
        c.dirs.insert("f1".into(), DirEntry { name: "Work".into(), volume: Some(7), id: Some("00".repeat(16)) });
        c.save(d.path()).unwrap();
        assert_eq!(LibraryConfig::load(d.path()).unwrap(), c);
        assert!(!d.path().join(format!("{FILE_NAME}.tmp")).exists());
    }

    #[test]
    fn marker_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_marker(d.path()), None);
        let id = write_new_marker(d.path()).unwrap();
        assert_eq!(read_marker(d.path()), Some(id));
    }
}
