//! 从已固定的文件对象读取身份与修改版本。

use crate::Result;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::Path;

/// 同一卷内硬链接共享对象标识；路径和索引来源仍由调用方区分。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub volume: u64,
    pub object: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileTimestamp {
    pub seconds: i64,
    pub nanoseconds: u32,
}

/// 通过对象身份、大小、修改时间和元数据变更时间判断缓存是否仍可用。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRevision {
    pub identity: FileIdentity,
    pub size: u64,
    pub modified: FileTimestamp,
    pub changed: FileTimestamp,
}

impl FileRevision {
    /// 对经过本地边界校验的已打开文件取证。此调用不会重新解析路径。
    pub fn from_file(file: &File) -> Result<Self> {
        imp::from_file(file, Path::new("<文件句柄>"))
    }

    /// 从受路径边界保护的对象句柄读取元数据，无须读取正文。
    pub fn at_path(path: &Path) -> Result<Self> {
        imp::at_path(path)
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod imp {
    use super::*;
    use crate::local::imp::resolve;
    use crate::local::io_error;
    use crate::PlatformError;
    use std::os::unix::fs::MetadataExt;

    fn identity(volume: u64, inode: u64) -> FileIdentity {
        FileIdentity {
            volume,
            object: u128::from(inode).to_le_bytes(),
        }
    }

    pub(crate) fn from_stat(stat: &libc::stat) -> FileRevision {
        FileRevision {
            identity: identity(stat.st_dev, stat.st_ino),
            size: if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                0
            } else {
                stat.st_size.max(0) as u64
            },
            modified: FileTimestamp {
                seconds: stat.st_mtime,
                nanoseconds: stat.st_mtime_nsec as u32,
            },
            changed: FileTimestamp {
                seconds: stat.st_ctime,
                nanoseconds: stat.st_ctime_nsec as u32,
            },
        }
    }

    pub(super) fn from_file(file: &File, path: &Path) -> Result<FileRevision> {
        let meta = file.metadata().map_err(|e| io_error(path, e))?;
        if !meta.is_file() && !meta.is_dir() {
            return Err(PlatformError::InvalidPath("需要普通文件或目录".into()));
        }
        Ok(FileRevision {
            identity: identity(meta.dev(), meta.ino()),
            size: if meta.is_dir() { 0 } else { meta.size() },
            modified: FileTimestamp {
                seconds: meta.mtime(),
                nanoseconds: meta.mtime_nsec() as u32,
            },
            changed: FileTimestamp {
                seconds: meta.ctime(),
                nanoseconds: meta.ctime_nsec() as u32,
            },
        })
    }

    pub(super) fn at_path(path: &Path) -> Result<FileRevision> {
        let node = resolve(path, false)?.expect("缺失路径必须返回错误");
        if !node.is_file() && !node.is_dir() {
            return Err(PlatformError::InvalidPath("需要普通文件或目录".into()));
        }
        Ok(from_stat(&node.stat))
    }
}

#[cfg(windows)]
pub(crate) mod imp {
    use super::*;
    use crate::local::imp::{file_info, guard};
    use crate::local::io_error;
    use crate::PlatformError;
    use std::mem::{size_of, MaybeUninit};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_ID_INFO,
    };

    pub(crate) fn time(ticks: i64) -> FileTimestamp {
        FileTimestamp {
            seconds: ticks.div_euclid(10_000_000) - 11_644_473_600,
            nanoseconds: ticks.rem_euclid(10_000_000) as u32 * 100,
        }
    }

    pub(crate) fn from_info(
        file: &File,
        path: &Path,
        info: &BY_HANDLE_FILE_INFORMATION,
    ) -> Result<FileRevision> {
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(PlatformError::InvalidPath("不允许使用重解析点".into()));
        }
        let mut basic = MaybeUninit::<FILE_BASIC_INFO>::uninit();
        let mut id = MaybeUninit::<FILE_ID_INFO>::uninit();
        // ReFS 使用 128 位文件标识。查询失败时不得用可能截断的旧字段替代。
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                id.as_mut_ptr().cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
            || unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    basic.as_mut_ptr().cast(),
                    size_of::<FILE_BASIC_INFO>() as u32,
                )
            } == 0
        {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        let (id, basic) = unsafe { (id.assume_init(), basic.assume_init()) };
        Ok(FileRevision {
            identity: FileIdentity {
                volume: id.VolumeSerialNumber,
                object: id.FileId.Identifier,
            },
            size: if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                0
            } else {
                (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow)
            },
            modified: time(basic.LastWriteTime),
            changed: time(basic.ChangeTime),
        })
    }

    pub(super) fn from_file(file: &File, path: &Path) -> Result<FileRevision> {
        from_info(file, path, &file_info(file, path)?)
    }

    pub(super) fn at_path(path: &Path) -> Result<FileRevision> {
        let held = guard(path, false)?;
        let file = held
            .handles
            .last()
            .ok_or_else(|| PlatformError::InvalidPath("无法获取文件对象句柄".into()))?;
        from_file(file, path)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::*;

    pub(super) fn from_file(_: &File, path: &Path) -> Result<FileRevision> {
        at_path(path)
    }

    pub(super) fn at_path(_: &Path) -> Result<FileRevision> {
        Err(crate::PlatformError::InvalidPath(
            "当前平台不提供本地文件对象身份".into(),
        ))
    }
}
