use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CString, OsString},
    fs, io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use super::{DriveInfo, display_path, display_text};

#[derive(Debug)]
struct Mount {
    path: PathBuf,
    filesystem: String,
    source: OsString,
}

pub(crate) fn discover_drives() -> io::Result<Vec<DriveInfo>> {
    let mut paths = BTreeSet::new();
    let mut drives = Vec::new();
    for mount in read_mounts()? {
        if (is_virtual_filesystem(&mount.filesystem) && mount.path != Path::new("/"))
            || !mount.path.is_dir()
            || !paths.insert(mount.path.clone())
        {
            continue;
        }
        let Ok((total_bytes, free_bytes)) = disk_space(&mount.path) else {
            continue;
        };
        drives.push(DriveInfo {
            name: display_path(&mount.path),
            label: Some(display_text(&mount.source.to_string_lossy())),
            drive_type: mount.filesystem,
            root: mount.path,
            total_bytes,
            free_bytes,
        });
    }
    drives.sort_by(|left, right| left.root.cmp(&right.root));
    Ok(drives)
}

fn read_mounts() -> io::Result<Vec<Mount>> {
    fs::read("/proc/self/mountinfo")?
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(parse_mount)
        .collect()
}

pub(super) fn mount_boundaries() -> io::Result<BTreeMap<PathBuf, bool>> {
    Ok(read_mounts()?
        .into_iter()
        .map(|mount| {
            let virtual_fs =
                is_virtual_filesystem(&mount.filesystem) && mount.path != Path::new("/");
            (mount.path, virtual_fs)
        })
        .collect())
}

fn parse_mount(line: &[u8]) -> io::Result<Mount> {
    let fields: Vec<&[u8]> = line
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .collect();
    let separator = fields
        .iter()
        .position(|field| *field == b"-")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Missing mountinfo separator"))?;
    if separator < 6 || fields.len() < separator + 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Incomplete mountinfo record",
        ));
    }
    let path = PathBuf::from(OsString::from_vec(unescape(fields[4])?));
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Mount path must be absolute",
        ));
    }
    Ok(Mount {
        path,
        filesystem: String::from_utf8(fields[separator + 1].to_vec())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        source: OsString::from_vec(unescape(fields[separator + 2])?),
    })
}

fn unescape(value: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'\\' {
            let digits = value
                .get(index + 1..index + 4)
                .filter(|digits| digits.iter().all(|byte| (b'0'..=b'7').contains(byte)))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Invalid mountinfo escape")
                })?;
            let number = u16::from(digits[0] - b'0') * 64
                + u16::from(digits[1] - b'0') * 8
                + u16::from(digits[2] - b'0');
            decoded.push(
                u8::try_from(number)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            );
            index += 4;
        } else {
            decoded.push(value[index]);
            index += 1;
        }
    }
    Ok(decoded)
}

fn is_virtual_filesystem(filesystem: &str) -> bool {
    matches!(
        filesystem,
        "proc"
            | "sysfs"
            | "tmpfs"
            | "devtmpfs"
            | "devpts"
            | "cgroup"
            | "cgroup2"
            | "securityfs"
            | "pstore"
            | "debugfs"
            | "tracefs"
            | "configfs"
            | "fusectl"
            | "mqueue"
            | "hugetlbfs"
            | "rpc_pipefs"
            | "nsfs"
            | "autofs"
            | "bpf"
            | "binfmt_misc"
    )
}

#[allow(clippy::unnecessary_cast)] // statvfs field widths differ on 32-bit Linux.
fn disk_space(path: &Path) -> io::Result<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs writes the complete structure on success; the path is NUL-terminated.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    let block_size = stats.f_frsize as u64;
    Ok((
        (stats.f_blocks as u64).saturating_mul(block_size),
        (stats.f_bfree as u64).saturating_mul(block_size),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mounts_with_optional_fields_and_escaped_paths() {
        let mount =
            parse_mount(br"36 25 8:1 / /media/My\040Disk rw shared:1 - ext4 /dev/sda1 rw").unwrap();
        assert_eq!(mount.path, Path::new("/media/My Disk"));
        assert_eq!(mount.filesystem, "ext4");
        assert_eq!(mount.source, OsString::from("/dev/sda1"));
    }

    #[test]
    fn preserves_non_utf8_mount_names() {
        let mount = parse_mount(b"1 0 0:1 / /mnt/\xff rw - ext4 /dev/test rw").unwrap();
        assert_eq!(mount.path.as_os_str().as_bytes(), b"/mnt/\xff");
    }

    #[test]
    fn rejects_malformed_mount_records_and_escapes() {
        for line in [
            b"invalid".as_slice(),
            br"1 0 0:1 / /bad\xyz rw - ext4 /dev/test rw",
            br"1 0 0:1 / /bad\777 rw - ext4 /dev/test rw",
        ] {
            assert!(parse_mount(line).is_err());
        }
    }

    #[test]
    fn discovers_a_real_filesystem_with_usable_capacity() {
        let drives = discover_drives().unwrap();
        let root = drives
            .iter()
            .find(|drive| drive.root == Path::new("/"))
            .unwrap();
        assert!(root.total_bytes > 0);
        assert!(root.free_bytes <= root.total_bytes);
        assert!(!drives.iter().any(|drive| drive.root == Path::new("/proc")));
    }
}
