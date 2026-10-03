use super::DriveInfo;
use std::{
    ffi::OsStr,
    fs, io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
    GetVolumeInformationW,
};

const DRIVE_UNKNOWN: u32 = 0;
const DRIVE_NO_ROOT_DIR: u32 = 1;
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;
pub(crate) fn discover_drives() -> io::Result<Vec<DriveInfo>> {
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut drives = Vec::new();
    for index in 0..26 {
        if mask & (1 << index) == 0 {
            continue;
        }

        let letter = char::from(b'A' + index as u8);
        let root_str = format!("{letter}:\\");
        let root_wide = wide_null(&root_str);
        let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
        if matches!(drive_type, DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR) {
            continue;
        }

        let mut free_for_caller = 0u64;
        let mut total_bytes = 0u64;
        let mut total_free = 0u64;

        let has_space = unsafe {
            GetDiskFreeSpaceExW(
                root_wide.as_ptr(),
                &mut free_for_caller,
                &mut total_bytes,
                &mut total_free,
            )
        };
        if has_space == 0 {
            continue;
        }

        drives.push(DriveInfo {
            root: PathBuf::from(&root_str),
            name: format!("{letter}:"),
            label: volume_label(&root_wide),
            drive_type: drive_type_label(drive_type).to_string(),
            total_bytes,
            free_bytes: total_free,
        });
    }

    drives.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(drives)
}

fn volume_label(root: &[u16]) -> Option<String> {
    let mut name_buffer = [0u16; 260];
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            name_buffer.as_mut_ptr(),
            name_buffer.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };

    if ok == 0 {
        return None;
    }

    let length = name_buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(name_buffer.len());

    if length == 0 {
        None
    } else {
        Some(String::from_utf16_lossy(&name_buffer[..length]))
    }
}

fn drive_type_label(drive_type: u32) -> &'static str {
    match drive_type {
        DRIVE_FIXED => "fixed",
        DRIVE_REMOVABLE => "remov.",
        DRIVE_REMOTE => "network",
        DRIVE_CDROM => "optical",
        DRIVE_RAMDISK => "ramdisk",
        _ => "other",
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub(super) fn is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(super) fn metadata_fingerprint(metadata: &fs::Metadata) -> [u64; 8] {
    [
        metadata.last_write_time(),
        metadata.creation_time(),
        u64::from(metadata.file_attributes()),
        0,
        0,
        0,
        0,
        0,
    ]
}

pub(super) fn normalize_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}
