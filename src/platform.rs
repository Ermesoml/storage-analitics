use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub(crate) use linux::discover_drives;
#[cfg(target_os = "windows")]
pub(crate) use windows::discover_drives;

#[derive(Clone, Debug)]
pub(crate) struct DriveInfo {
    pub root: PathBuf,
    pub name: String,
    pub label: Option<String>,
    pub drive_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl DriveInfo {
    pub(crate) fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }
}

// One mount snapshot per scan keeps recursion bounded and cache results consistent.
#[derive(Clone)]
pub(crate) struct ScanPolicy {
    mounts: BTreeMap<PathBuf, bool>,
    pub fingerprint: [u8; 32],
}

impl ScanPolicy {
    pub(crate) fn new(path: &Path) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let mounts = linux::mount_boundaries()?;
        #[cfg(target_os = "windows")]
        let mounts = BTreeMap::new();
        let policy = Self::from_mounts(mounts);
        if policy
            .mounts
            .iter()
            .filter(|(mount, _)| path.starts_with(mount))
            .max_by_key(|(mount, _)| mount.components().count())
            .is_some_and(|(_, virtual_fs)| *virtual_fs)
        {
            return Err(io::Error::other("Virtual filesystems are not scanned."));
        }
        Ok(policy)
    }

    pub(crate) fn from_mounts(mounts: BTreeMap<PathBuf, bool>) -> Self {
        let mut hash = blake3::Hasher::new();
        for (path, virtual_fs) in &mounts {
            let bytes = path.as_os_str().as_encoded_bytes();
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
            hash.update(&[u8::from(*virtual_fs)]);
        }
        Self {
            mounts,
            fingerprint: *hash.finalize().as_bytes(),
        }
    }

    pub(crate) fn mount_kind(&self, path: &Path) -> Option<bool> {
        self.mounts.get(path).copied()
    }

    pub(crate) fn check_deletion(&self, path: &Path) -> io::Result<()> {
        if self.mounts.keys().any(|mount| mount.starts_with(path)) {
            return Err(io::Error::other(
                "Mounted filesystems and directories containing them cannot be deleted.",
            ));
        }
        Ok(())
    }
}

pub(crate) fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::is_link(metadata)
    }
    #[cfg(target_os = "linux")]
    {
        metadata.file_type().is_symlink()
    }
}

pub(crate) fn metadata_fingerprint(metadata: &fs::Metadata) -> [u64; 8] {
    #[cfg(target_os = "windows")]
    {
        windows::metadata_fingerprint(metadata)
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        [
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
            u64::from(metadata.mode()),
            metadata.ino(),
            metadata.dev(),
            metadata.len(),
        ]
    }
}

pub(crate) fn normalize_path_key(path: &Path) -> String {
    #[cfg(target_os = "windows")]
    {
        windows::normalize_path_key(path)
    }
    #[cfg(target_os = "linux")]
    {
        format!(
            "linux:{}",
            blake3::hash(path.as_os_str().as_encoded_bytes())
        )
    }
}

// Linux permits control characters in names; do not emit terminal escape sequences.
pub(crate) fn display_text(text: &str) -> String {
    let mut escaped = String::new();
    for character in text.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

pub(crate) fn display_path(path: &Path) -> String {
    display_text(&path.to_string_lossy())
}
