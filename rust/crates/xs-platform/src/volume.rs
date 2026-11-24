use crate::{PlatformError, Result, Scope};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VolumeCapability {
    pub root: String,
    pub label: String,
    pub filesystem: String,
    pub local: bool,
    pub fixed: bool,
    pub standard_supported: bool,
    pub mft_supported: bool,
    /// 文件系统具备候选资格。实际权限与日志可用性仅在明确授权后探测。
    #[serde(default)]
    pub native_journal_candidate: bool,
    pub reason: String,
}

/// Suggestions only. Neither discovery nor these suggestions initiate a scan.
pub fn suggested_roots() -> Vec<Scope> {
    let base = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let Some(base) = base else { return Vec::new() };
    ["Desktop", "Documents", "Downloads"]
        .into_iter()
        .filter_map(|name| {
            let p = PathBuf::from(&base).join(name);
            // Do not probe these directories: displaying a suggestion must not follow a link.
            p.to_str().map(|s| Scope::new(s.to_owned()))
        })
        .collect()
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub(crate) struct MountInfo {
    pub id: u64,
    pub mount_point: PathBuf,
    pub filesystem: String,
    pub source: String,
}

#[cfg(target_os = "linux")]
pub(crate) fn local_filesystem(kind: &str) -> bool {
    // Unknown/FUSE/network/autofs/overlay filesystems fail closed. Overlay layers
    // can reside on remote storage, so their contents are never treated as local.
    matches!(
        kind,
        "ext2"
            | "ext3"
            | "ext4"
            | "btrfs"
            | "xfs"
            | "tmpfs"
            | "ramfs"
            | "zfs"
            | "vfat"
            | "msdos"
            | "exfat"
            | "ntfs"
            | "ntfs3"
            | "f2fs"
            | "jfs"
            | "reiserfs"
            | "hfs"
            | "hfsplus"
            | "squashfs"
            | "iso9660"
            | "udf"
            | "ubifs"
            | "bcachefs"
    )
}

#[cfg(target_os = "linux")]
fn unescape_mount(raw: &str) -> Result<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if i + 3 >= bytes.len()
                || !bytes[i + 1..=i + 3]
                    .iter()
                    .all(|b| (b'0'..=b'7').contains(b))
            {
                return Err(PlatformError::InvalidPath(
                    "Malformed mountinfo escape".into(),
                ));
            }
            let value = ((bytes[i + 1] - b'0') as u16) * 64
                + ((bytes[i + 2] - b'0') as u16) * 8
                + (bytes[i + 3] - b'0') as u16;
            if value > 255 {
                return Err(PlatformError::InvalidPath("Invalid mountinfo byte".into()));
            }
            out.push(value as u8);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| PlatformError::InvalidPath("Non-UTF-8 mount point is unsupported".into()))
}

#[cfg(target_os = "linux")]
pub(crate) fn parse_mountinfo(text: &str) -> Result<Vec<MountInfo>> {
    let mut out = Vec::new();
    for line in text.lines() {
        let (head, tail) = line
            .split_once(" - ")
            .ok_or_else(|| PlatformError::InvalidPath("Malformed mountinfo".into()))?;
        let a: Vec<_> = head.split_whitespace().collect();
        let b: Vec<_> = tail.split_whitespace().collect();
        if a.len() < 6 || b.len() < 3 {
            return Err(PlatformError::InvalidPath("Incomplete mountinfo".into()));
        }
        let id = a[0]
            .parse()
            .map_err(|_| PlatformError::InvalidPath("Invalid mount id".into()))?;
        out.push(MountInfo {
            id,
            mount_point: PathBuf::from(unescape_mount(a[4])?),
            filesystem: b[0].into(),
            source: unescape_mount(b[1])?,
        });
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
pub(crate) fn mount_info() -> Result<Vec<MountInfo>> {
    let text =
        std::fs::read_to_string("/proc/self/mountinfo").map_err(|source| PlatformError::Io {
            path: "/proc/self/mountinfo".into(),
            source,
        })?;
    parse_mountinfo(&text)
}

#[cfg(target_os = "linux")]
pub fn volumes() -> Result<Vec<VolumeCapability>> {
    let mut result: Vec<_> = mount_info()?
        .into_iter()
        .map(|m| {
            let local = local_filesystem(&m.filesystem);
            VolumeCapability {
                root: m.mount_point.to_string_lossy().into_owned(),
                label: m.source.clone(),
                filesystem: m.filesystem,
                local,
                fixed: local && m.source.starts_with("/dev/"),
                standard_supported: local,
                mft_supported: false,
                native_journal_candidate: false,
                reason: if local {
                    "选择范围后可使用本地目录扫描和监听。"
                } else {
                    "网络、虚拟、自动挂载或未知文件系统不支持遍历。"
                }
                .into(),
            }
        })
        .collect();
    result.sort_by(|a, b| a.root.cmp(&b.root));
    Ok(result)
}

#[cfg(windows)]
pub fn volumes() -> Result<Vec<VolumeCapability>> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Err(PlatformError::Io {
            path: "logical volumes".into(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok((0..26)
        .filter(|n| mask & (1 << n) != 0)
        .map(|n| windows_volume(format!("{}:\\", (b'A' + n) as char)))
        .collect())
}

#[cfg(windows)]
pub(crate) fn windows_volume(root: String) -> VolumeCapability {
    use std::ptr::null_mut;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};

    let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
    let kind = unsafe { GetDriveTypeW(wide.as_ptr()) };
    let local = matches!(kind, 2 | 3 | 5 | 6);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    // 只查询本地卷，展示能力时不连接网络映射驱动器。
    let available = local
        && unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                null_mut(),
                null_mut(),
                null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        } != 0;
    let decode = |v: &[u16]| {
        String::from_utf16(&v[..v.iter().position(|c| *c == 0).unwrap_or(v.len())])
            .unwrap_or_else(|_| "<无效 UTF-16>".into())
    };
    let filesystem = decode(&fs);
    let native_journal_candidate = available
        && kind == 3
        && (filesystem.eq_ignore_ascii_case("NTFS") || filesystem.eq_ignore_ascii_case("ReFS"));
    let mft_supported = native_journal_candidate && filesystem.eq_ignore_ascii_case("NTFS");
    VolumeCapability {
        root,
        label: decode(&label),
        filesystem,
        local,
        fixed: kind == 3,
        standard_supported: available,
        mft_supported,
        native_journal_candidate,
        reason: if !local {
            "远程或未知驱动器不支持遍历。"
        } else if !available {
            "本地卷不可用或访问被拒绝。"
        } else if native_journal_candidate {
            "此固定卷可在明确授权后探测原生日志。"
        } else {
            "此卷支持本地目录扫描。"
        }
        .into(),
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn volumes() -> Result<Vec<VolumeCapability>> {
    Err(PlatformError::InvalidPath(
        "Local filesystem capabilities are implemented only for Linux and Windows".into(),
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn mount_parser_decodes_paths_and_rejects_network() {
        let v = parse_mountinfo("11 1 8:1 / /work\\040space rw - ext4 /dev/sda1 rw\n12 1 0:5 / /network rw - nfs server:/share rw\n").unwrap();
        assert_eq!(v[0].mount_point, PathBuf::from("/work space"));
        assert!(local_filesystem(&v[0].filesystem));
        assert!(!local_filesystem(&v[1].filesystem));
        for fs in [
            "cifs",
            "nfs4",
            "fuse.sshfs",
            "fuse",
            "autofs",
            "9p",
            "proc",
            "sysfs",
            "unknown",
        ] {
            assert!(!local_filesystem(fs));
        }
    }
}
