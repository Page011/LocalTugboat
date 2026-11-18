//! Windows file facts the library needs: NTFS identity, exclusive-open probe, hidden
//! attribute, cloud-sync detection. Everything else in the crate stays platform-neutral std.

use std::{
    fs::OpenOptions,
    io,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
};

use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            GetFileInformationByHandle, GetFileInformationByHandleEx, SetFileAttributesW, BY_HANDLE_FILE_INFORMATION,
            FILE_ATTRIBUTE_HIDDEN, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FileIdInfo,
        },
    },
};

/// NTFS identity of a file or directory: survives rename and move on one volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub volume_serial: u64,
    pub file_id: [u8; 16],
}

impl FileIdentity {
    pub fn id_hex(&self) -> String {
        hex::encode(self.file_id)
    }

    pub fn from_hex(volume_serial: u64, id_hex: &str) -> Option<FileIdentity> {
        let file_id: [u8; 16] = hex::decode(id_hex).ok()?.try_into().ok()?;
        Some(FileIdentity { volume_serial, file_id })
    }
}

const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
/// Cloud-files placeholder flags (OneDrive Files On-Demand and other sync engines).
const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
const FILE_ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Identity of a file or directory. Opens with backup semantics so directories work, and
/// shares everything so a file that is being written is not disturbed.
pub fn identity_of(path: &Path) -> io::Result<FileIdentity> {
    let f = OpenOptions::new().read(true).share_mode(7).custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0).open(path)?;
    let h = HANDLE(f.as_raw_handle());
    let mut info = FILE_ID_INFO::default();
    // FileIdInfo (128-bit ids, Windows 8+) first; older or non-NTFS volumes answer with the
    // 64-bit index from GetFileInformationByHandle.
    let ex = unsafe { GetFileInformationByHandleEx(h, FileIdInfo, &mut info as *mut FILE_ID_INFO as *mut _, std::mem::size_of::<FILE_ID_INFO>() as u32) };
    if ex.is_ok() {
        return Ok(FileIdentity { volume_serial: info.VolumeSerialNumber, file_id: info.FileId.Identifier });
    }
    let mut old = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(h, &mut old) }.map_err(|e| io::Error::other(format!("file identity of {}: {e}", path.display())))?;
    let index = ((old.nFileIndexHigh as u64) << 32) | old.nFileIndexLow as u64;
    let mut file_id = [0u8; 16];
    file_id[..8].copy_from_slice(&index.to_le_bytes());
    Ok(FileIdentity { volume_serial: old.dwVolumeSerialNumber as u64, file_id })
}

/// The "copy finished" probe: a file another process still has open for writing (or that
/// refuses sharing) cannot be opened with share mode 0. Err = not ready.
pub fn open_exclusive(path: &Path) -> io::Result<()> {
    OpenOptions::new().read(true).share_mode(0).open(path).map(|_| ())
}

/// Set the hidden attribute (used on `.tugboat`, so Explorer does not show it).
pub fn hide(path: &Path) -> io::Result<()> {
    let wide: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }
        .map_err(|e| io::Error::other(format!("could not hide {}: {e}", path.display())))
}

/// Hidden or system file (desktop.ini, Thumbs.db, .tmp markers): never listed as "not synced".
pub fn is_hidden_or_system(meta: &std::fs::Metadata) -> bool {
    meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM) != 0
}

/// Attribute bits of a path, 0 when it cannot be read.
pub fn attributes(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).map(|m| m.file_attributes()).unwrap_or(0)
}

/// Why a library folder looks cloud-synced, or None. `roots` are the cloud client folders from
/// the environment (OneDrive, OneDriveConsumer, OneDriveCommercial); `attrs` reads attribute
/// bits (a parameter so the rule is testable without a real OneDrive).
pub fn cloud_reason(path: &Path, roots: &[PathBuf], attrs: &dyn Fn(&Path) -> u32) -> Option<String> {
    let lower = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
    let this = lower(path);
    for root in roots {
        let r = lower(root);
        let r = r.trim_end_matches('\\');
        if !r.is_empty() && (this == r || this.starts_with(&format!("{r}\\"))) {
            return Some(format!("inside the OneDrive folder {}", root.display()));
        }
    }
    // Placeholder attributes on the folder or any parent (a sync root marks its children).
    let cloud_bits = FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_UNPINNED | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    for p in path.ancestors() {
        let a = attrs(p);
        if a & cloud_bits != 0 {
            return Some(format!("cloud-synced folder {}", p.display()));
        }
    }
    // Other clients are not detectable by attribute: match their default folder names.
    for comp in path.components() {
        let c = comp.as_os_str().to_string_lossy().to_lowercase();
        if c == "dropbox" || c == "google drive" || c == "icloud drive" || c == "iclouddrive" || c == "box" || c == "mega" || c.starts_with("onedrive") {
            return Some(format!("inside a cloud-synced folder ({})", comp.as_os_str().to_string_lossy()));
        }
    }
    None
}

/// OneDrive client folders named by the environment (set by the OneDrive client).
pub fn onedrive_roots() -> Vec<PathBuf> {
    ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect()
}

/// `cloud_reason` with the real environment and file attributes.
pub fn cloud_warning(path: &Path) -> Option<String> {
    cloud_reason(path, &onedrive_roots(), &attributes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_rename_and_differs_between_files() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, b"1").unwrap();
        std::fs::write(&b, b"1").unwrap();
        let ia = identity_of(&a).unwrap();
        assert_ne!(ia, identity_of(&b).unwrap());
        let moved = d.path().join("c.txt");
        std::fs::rename(&a, &moved).unwrap();
        assert_eq!(identity_of(&moved).unwrap(), ia);
        // A directory has an identity too (folder rename matching).
        let dir = d.path().join("sub");
        std::fs::create_dir(&dir).unwrap();
        let id = identity_of(&dir).unwrap();
        let renamed = d.path().join("sub2");
        std::fs::rename(&dir, &renamed).unwrap();
        assert_eq!(identity_of(&renamed).unwrap(), id);
        assert!(identity_of(&d.path().join("missing")).is_err());
    }

    #[test]
    fn hex_roundtrip_and_bad_input() {
        let i = FileIdentity { volume_serial: 7, file_id: [9; 16] };
        assert_eq!(FileIdentity::from_hex(7, &i.id_hex()), Some(i));
        assert_eq!(FileIdentity::from_hex(7, "abcd"), None);
        assert_eq!(FileIdentity::from_hex(7, "zz"), None);
    }

    #[test]
    fn exclusive_open_fails_while_another_handle_writes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big.mp4");
        // Same sharing as a copy in progress: others may read, nobody may take it exclusively.
        let writer = OpenOptions::new().write(true).create(true).truncate(true).share_mode(1).open(&p).unwrap();
        assert!(open_exclusive(&p).is_err());
        drop(writer);
        assert!(open_exclusive(&p).is_ok());
    }

    #[test]
    fn hidden_attribute_is_set() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(".tugboat");
        std::fs::create_dir(&p).unwrap();
        hide(&p).unwrap();
        assert!(is_hidden_or_system(&std::fs::metadata(&p).unwrap()));
    }

    #[test]
    fn cloud_rule_flags_onedrive_paths_and_placeholder_attributes() {
        let no_attrs = |_: &Path| 0u32;
        let roots = vec![PathBuf::from(r"C:\Users\Sam\OneDrive")];
        assert!(cloud_reason(Path::new(r"C:\Users\Sam\OneDrive\Pictures\Local Tugboat"), &roots, &no_attrs).is_some());
        assert!(cloud_reason(Path::new(r"c:\users\sam\onedrive"), &roots, &no_attrs).is_some(), "case-insensitive, the root itself");
        assert!(cloud_reason(Path::new(r"C:\Users\Sam\Local Tugboat"), &roots, &no_attrs).is_none());
        // Placeholder bits on a parent folder flag it even without the environment variable.
        let placeholder = |p: &Path| if p == Path::new(r"D:\Sync") { FILE_ATTRIBUTE_UNPINNED } else { 0 };
        assert!(cloud_reason(Path::new(r"D:\Sync\Local Tugboat"), &[], &placeholder).is_some());
        assert!(cloud_reason(Path::new(r"D:\Plain\Local Tugboat"), &[], &placeholder).is_none());
        // Default folder names of other clients.
        assert!(cloud_reason(Path::new(r"D:\Dropbox\Local Tugboat"), &[], &no_attrs).is_some());
        assert!(cloud_reason(Path::new(r"D:\Photos\Local Tugboat"), &[], &no_attrs).is_none());
    }
}
