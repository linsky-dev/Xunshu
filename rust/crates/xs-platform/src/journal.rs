//! 授权本地卷的只读 NTFS / ReFS 原生变更日志采集。

use crate::local::{io_error, normalize_path};
use crate::settings::same_path;
use crate::{FileRevision, NativeCheckpoint, PlatformError, Result, Settings, SpeedMode};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::{
    ffi::OsStrExt,
    fs::OpenOptionsExt,
    io::{AsRawHandle, FromRawHandle},
};
use std::path::{Component, Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{
    ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED,
    ERROR_JOURNAL_NOT_ACTIVE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    ExtendedFileIdType, FileIdType, FindClose, FindFirstFileNameW, FindNextFileNameW,
    GetFinalPathNameByHandleW, GetVolumeNameForVolumeMountPointW, OpenFileById,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_128, FILE_ID_DESCRIPTOR,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
    MFT_ENUM_DATA_V1, READ_USN_JOURNAL_DATA_V1, USN_REASON_CLOSE, USN_REASON_HARD_LINK_CHANGE,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const PAGE_WORDS: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalStatus {
    pub id: u64,
    pub first: i64,
    pub next: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalReset {
    Replaced,
    CursorLost,
    Unavailable,
}

#[path = "usn_decode.rs"]
mod usn_decode;
use usn_decode::{decode, integer_i64, integer_u64};
pub use usn_decode::{NativePage, UsnRecord};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalRead {
    Changes(NativePage<i64>),
    RescanRequired(JournalReset),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeJournalChanges {
    pub paths: Vec<PathBuf>,
    pub checkpoint: NativeCheckpoint,
    pub requires_rebuild: bool,
}

/// 从已提交游标有界获取变更路径。调用方重新核对路径并原子提交新游标。
/// 缺口、目录拓扑、硬链接拓扑或不完整批次保留原游标并要求重建。
pub fn read_scoped_journal_changes(
    settings: &Settings,
    checkpoint: &NativeCheckpoint,
) -> Result<NativeJournalChanges> {
    let [scope] = settings.roots.as_slice() else {
        return Err(PlatformError::UnsupportedMode(
            "原生日志续读要求一个明确授权的完整卷".into(),
        ));
    };
    let mut volume = NativeVolume::open(settings, Path::new(&scope.path))?;
    volume.scoped_changes(checkpoint)
}

pub struct NativeVolume {
    root: PathBuf,
    device: String,
    file: File,
    scope_token: String,
    filesystem: String,
    volume_id: u64,
    root_object: [u8; 16],
    system_directories: BTreeSet<[u8; 16]>,
    buffer: Box<[u64; PAGE_WORDS]>,
}

impl NativeVolume {
    /// 打开用户明确选定的整个本地固定卷。此接口采集卷级元数据，因而要求
    /// 完整卷根、无类型过滤且关闭智能排除。子目录范围应使用目录监听。
    pub fn open(settings: &Settings, root: &Path) -> Result<Self> {
        let root = authorized_root(settings, root)?;
        let volume = crate::volume::windows_volume(root.to_str().expect("已验证 UTF-8").into());
        if !volume.native_journal_candidate {
            return Err(PlatformError::UnsupportedMode(
                "所选卷不支持原生日志，需使用目录监听".into(),
            ));
        }
        let device = volume_device(&root)?;
        // GUID 锚定到特定卷，避免盘符在校验和打开之间被重新分配。
        let root_identity = FileRevision::at_path(&root)?.identity;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&device)
            .map_err(|error| io_error(&root, error))?;
        let mut volume = Self {
            root,
            device,
            file,
            scope_token: settings.scope_token()?,
            filesystem: volume.filesystem,
            volume_id: root_identity.volume,
            root_object: root_identity.object,
            system_directories: BTreeSet::new(),
            buffer: Box::new([0; PAGE_WORDS]),
        };
        volume.journal_status()?;
        Ok(volume)
    }

    pub fn scope_token(&self) -> &str {
        &self.scope_token
    }

    pub fn volume_id(&self) -> u64 {
        self.volume_id
    }

    pub(crate) fn device_id(&self) -> &str {
        &self.device
    }

    pub fn scoped_changes(
        &mut self,
        checkpoint: &NativeCheckpoint,
    ) -> Result<NativeJournalChanges> {
        const MAX_PAGES: usize = 8;
        const MAX_PATHS: usize = 512;
        let rebuild = || NativeJournalChanges {
            paths: Vec::new(),
            checkpoint: checkpoint.clone(),
            requires_rebuild: true,
        };
        let status = self.journal_status()?;
        if !self.device.eq_ignore_ascii_case(&checkpoint.volume_id)
            || status.id != checkpoint.journal_id
            || checkpoint.cursor < status.first
            || checkpoint.cursor > status.next
        {
            return Ok(rebuild());
        }
        let mut cursor = checkpoint.cursor;
        let mut paths = BTreeSet::new();
        let mut checked_objects = BTreeSet::new();
        let mut record_count = 0;
        for _ in 0..MAX_PAGES {
            if cursor >= status.next {
                break;
            }
            let JournalRead::Changes(page) = self.read_changes(checkpoint.journal_id, cursor)?
            else {
                return Ok(rebuild());
            };
            for record in &page.records {
                record_count += 1;
                if record_count > 2_048 {
                    return Ok(rebuild());
                }
                if record.reason & !USN_REASON_CLOSE == 0 {
                    continue;
                }
                if self.skip_system_record(record)? {
                    continue;
                }
                if record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
                    || record.reason & USN_REASON_HARD_LINK_CHANGE != 0
                {
                    return Ok(rebuild());
                }
                let Ok(path) = self.journal_path(record) else {
                    return Ok(rebuild());
                };
                if internal_ntfs_path(&self.root, &path) {
                    continue;
                }
                paths.insert(path);
                if checked_objects.insert(record.object) {
                    match self.record_paths(record) {
                        Ok(aliases) => paths.extend(aliases),
                        // 已删除对象没有现存别名，日志中的旧路径仍用于删除索引项。
                        Err(PlatformError::Io { source, .. })
                            if source.kind() == io::ErrorKind::NotFound => {}
                        Err(_) => return Ok(rebuild()),
                    }
                }
                if paths.len() > MAX_PATHS {
                    return Ok(rebuild());
                }
            }
            cursor = page.next;
        }
        let verified = self.journal_status()?;
        if cursor < status.next
            || verified.id != status.id
            || cursor < verified.first
            || cursor > verified.next
        {
            return Ok(rebuild());
        }
        Ok(NativeJournalChanges {
            paths: paths.into_iter().collect(),
            checkpoint: NativeCheckpoint {
                volume_id: self.device.clone(),
                journal_id: status.id,
                cursor,
            },
            requires_rebuild: false,
        })
    }

    fn journal_path(&mut self, record: &UsnRecord) -> Result<PathBuf> {
        let parent = if record.parent == self.root_object {
            self.root.clone()
        } else {
            let mut paths = self.record_paths(&UsnRecord {
                object: record.parent,
                parent: [0; 16],
                name: String::new(),
                usn: record.usn,
                attributes: FILE_ATTRIBUTE_DIRECTORY,
                reason: 0,
                version: record.version,
            })?;
            if paths.len() != 1 {
                return Err(invalid_data(&self.root, "USN 父目录无法唯一定位"));
            }
            paths.pop().expect("已验证唯一路径")
        };
        let mut parts = Path::new(&record.name).components();
        if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
            return Err(invalid_data(&self.root, "USN 文件名不是独立路径分量"));
        }
        normalize_path(&parent.join(&record.name))
    }

    /// 从原生记录的固定对象句柄解析真实路径；NTFS 多链接记录返回全部路径。
    pub(crate) fn record_paths(&mut self, record: &UsnRecord) -> Result<Vec<PathBuf>> {
        if record.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || self.skip_system_record(record)?
        {
            return Ok(Vec::new());
        }
        let mut descriptor = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            ..FILE_ID_DESCRIPTOR::default()
        };
        if record.version == 2 {
            let reference = integer_u64(&record.object, 0);
            descriptor.Type = FileIdType;
            descriptor.Anonymous.FileId = reference as i64;
        } else {
            descriptor.Type = ExtendedFileIdType;
            descriptor.Anonymous.ExtendedFileId = FILE_ID_128 {
                Identifier: record.object,
            };
        }
        let handle = unsafe {
            OpenFileById(
                self.file.as_raw_handle(),
                &descriptor,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io_error(&self.root, io::Error::last_os_error()));
        }
        let file = unsafe { File::from_raw_handle(handle) };
        let info = crate::local::imp::file_info(&file, &self.root)?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(Vec::new());
        }
        let revision = crate::revision::imp::from_info(&file, &self.root, &info)?;
        if revision.identity.volume != self.volume_id || revision.identity.object != record.object {
            return Err(invalid_data(&self.root, "原生文件标识在读取时发生变化"));
        }
        let mut name = vec![0; 1_024];
        let mut size = final_name(&file, &mut name);
        if size as usize >= name.len() && size <= 32_768 {
            name.resize(size as usize, 0);
            size = final_name(&file, &mut name);
        }
        if size == 0 {
            return Err(io_error(&self.root, io::Error::last_os_error()));
        }
        if size as usize >= name.len() {
            return Err(invalid_data(&self.root, "原生文件路径超出长度限制"));
        }
        let absolute = String::from_utf16(&name[..size as usize])
            .map_err(|_| invalid_data(&self.root, "原生文件路径编码无效"))?;
        let prefix = absolute.get(..self.device.len());
        if !prefix.is_some_and(|value| value.eq_ignore_ascii_case(&self.device)) {
            return Err(invalid_data(&self.root, "原生文件路径指向未授权卷"));
        }
        let path = relative_to_root(&self.root, &absolute[self.device.len()..])?;
        if record.version == 2 && internal_ntfs_path(&self.root, &path) {
            return Ok(Vec::new());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || info.nNumberOfLinks <= 1 {
            return Ok(vec![path]);
        }
        hard_links(&self.root, &path, info.nNumberOfLinks)
    }

    fn skip_system_record(&mut self, record: &UsnRecord) -> Result<bool> {
        let protected_root_directory = record.parent == self.root_object
            && record.attributes & (FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_DIRECTORY)
                == (FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_DIRECTORY)
            && record
                .name
                .eq_ignore_ascii_case("System Volume Information");
        let skip = record.object == self.root_object
            || internal_ntfs_record(record)
            || protected_root_directory
            || self.system_directories.contains(&record.parent);
        if skip
            && record.object != self.root_object
            && record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
        {
            self.system_directories.insert(record.object);
            if self.system_directories.len() > 1_024 {
                return Err(invalid_data(&self.root, "卷内部目录记录超过安全上限"));
            }
        }
        Ok(skip)
    }

    pub fn journal_status(&mut self) -> Result<JournalStatus> {
        self.check_volume()?;
        let length = self
            .device_control(FSCTL_QUERY_USN_JOURNAL, None::<&u8>)
            .map_err(|error| io_error(&self.root, error))?;
        let bytes = self.bytes(length);
        if bytes.len() < 56 {
            return Err(invalid_data(&self.root, "USN 日志状态不完整"));
        }
        let first = integer_i64(bytes, 8).max(integer_i64(bytes, 24));
        let next = integer_i64(bytes, 16);
        if first < 0 || next < first {
            return Err(invalid_data(&self.root, "USN 日志游标范围无效"));
        }
        Ok(JournalStatus {
            id: integer_u64(bytes, 0),
            first,
            next,
        })
    }

    /// 从 0 开始获取文件表的一页。调用方应在枚举前读取 `journal_status`，
    /// 随后从其 next 游标重放，防止遗漏枚举期间的变化。
    pub fn enumerate_page(&mut self, cursor: u64) -> Result<Option<NativePage<u64>>> {
        self.check_volume()?;
        let outcome = if self.filesystem.eq_ignore_ascii_case("NTFS") {
            self.device_control(
                FSCTL_ENUM_USN_DATA,
                Some(&MFT_ENUM_DATA_V0 {
                    StartFileReferenceNumber: cursor,
                    LowUsn: 0,
                    HighUsn: i64::MAX,
                }),
            )
        } else {
            self.device_control(
                FSCTL_ENUM_USN_DATA,
                Some(&MFT_ENUM_DATA_V1 {
                    StartFileReferenceNumber: cursor,
                    LowUsn: 0,
                    HighUsn: i64::MAX,
                    MinMajorVersion: 2,
                    MaxMajorVersion: 3,
                }),
            )
        };
        let length = match outcome {
            Ok(length) => length,
            Err(error) if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) => return Ok(None),
            Err(error) => return Err(io_error(&self.root, error)),
        };
        let page =
            decode(self.bytes(length)).map_err(|message| invalid_data(&self.root, message))?;
        if page.next <= cursor {
            return Err(invalid_data(&self.root, "文件表枚举游标没有前进"));
        }
        Ok(Some(page))
    }

    /// 日志标识和游标由索引提交层一起保存；卷断开、日志换代或截断应重建。
    pub fn read_changes(&mut self, status_id: u64, cursor: i64) -> Result<JournalRead> {
        let status = match self.journal_status() {
            Err(PlatformError::Io { source, .. }) if journal_missing(&source) => {
                return Ok(JournalRead::RescanRequired(JournalReset::Unavailable));
            }
            other => other?,
        };
        if status.id != status_id {
            return Ok(JournalRead::RescanRequired(JournalReset::Replaced));
        }
        if cursor < status.first || cursor > status.next {
            return Ok(JournalRead::RescanRequired(JournalReset::CursorLost));
        }
        if cursor == status.next {
            return Ok(JournalRead::Changes(NativePage {
                next: cursor,
                records: Vec::new(),
            }));
        }
        let length = match self.device_control(
            FSCTL_READ_USN_JOURNAL,
            Some(&READ_USN_JOURNAL_DATA_V1 {
                StartUsn: cursor,
                ReasonMask: u32::MAX,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: status_id,
                MinMajorVersion: 2,
                MaxMajorVersion: 3,
            }),
        ) {
            Ok(length) => length,
            Err(error) if journal_missing(&error) => {
                return Ok(JournalRead::RescanRequired(JournalReset::Unavailable));
            }
            Err(error) if error.raw_os_error() == Some(ERROR_JOURNAL_ENTRY_DELETED as i32) => {
                return Ok(JournalRead::RescanRequired(JournalReset::CursorLost));
            }
            Err(error) => return Err(io_error(&self.root, error)),
        };
        let page =
            decode(self.bytes(length)).map_err(|message| invalid_data(&self.root, message))?;
        let next = i64::from_le_bytes(page.next.to_le_bytes());
        if next <= cursor
            || page
                .records
                .iter()
                .any(|record| record.usn < cursor || record.usn >= next)
        {
            return Err(invalid_data(&self.root, "USN 变更记录超出当前游标区间"));
        }
        Ok(JournalRead::Changes(NativePage {
            next,
            records: page.records,
        }))
    }

    fn check_volume(&self) -> Result<()> {
        if volume_device(&self.root)? != self.device {
            return Err(PlatformError::InvalidPath("已授权的盘符指向其他卷".into()));
        }
        Ok(())
    }

    fn device_control<T>(&mut self, code: u32, input: Option<&T>) -> io::Result<usize> {
        let mut written = 0;
        let (ptr, size) = input.map_or((null(), 0), |value| {
            ((value as *const T).cast(), size_of::<T>() as u32)
        });
        let success = unsafe {
            DeviceIoControl(
                self.file.as_raw_handle(),
                code,
                ptr,
                size,
                self.buffer.as_mut_ptr().cast(),
                (PAGE_WORDS * 8) as u32,
                &mut written,
                null_mut(),
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize > PAGE_WORDS * 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "USN 返回大小超过缓冲区",
            ));
        }
        Ok(written as usize)
    }

    fn bytes(&self, length: usize) -> &[u8] {
        // DeviceIoControl 同步完成，缓冲区按 u64 对齐，内核返回的字节数已验证。
        unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast(), length) }
    }
}

fn internal_ntfs_record(record: &UsnRecord) -> bool {
    if record.version != 2 {
        return false;
    }
    // MFT 的 0–15 号记录是文件系统内部对象，5 号是卷根。卷根由扫描器单独读取。
    let ordinal = |id: &[u8; 16]| integer_u64(id, 0) & ((1u64 << 48) - 1);
    let parent = ordinal(&record.parent);
    ordinal(&record.object) < 16 || (record.parent != [0; 16] && parent < 16 && parent != 5)
}

fn internal_ntfs_path(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|part| part.as_os_str().to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("$Extend")
                || name.eq_ignore_ascii_case("System Volume Information")
        })
}

fn authorized_root(settings: &Settings, root: &Path) -> Result<PathBuf> {
    let root = normalize_path(root)?;
    let mut components = root.components();
    if !matches!(components.next(), Some(Component::Prefix(_)))
        || !matches!(components.next(), Some(Component::RootDir))
        || components.next().is_some()
    {
        return Err(PlatformError::InvalidPath(
            "原生采集只接受已授权的完整盘符根".into(),
        ));
    }
    let settings = settings.normalized()?;
    if settings.mode == SpeedMode::Standard
        || !settings.roots.iter().any(|scope| {
            same_path(Path::new(&scope.path), &root)
                && !scope.smart_excludes
                && scope.extensions.is_none()
        })
    {
        return Err(PlatformError::UnsupportedMode(
            "需要显式授权读取所选整个卷的元数据".into(),
        ));
    }
    Ok(root)
}

fn volume_device(root: &Path) -> Result<String> {
    let wide: Vec<_> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut name = [0; 64];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(wide.as_ptr(), name.as_mut_ptr(), name.len() as u32)
    } == 0
    {
        return Err(io_error(root, io::Error::last_os_error()));
    }
    let length = name.iter().position(|&ch| ch == 0).unwrap_or(name.len());
    let path =
        String::from_utf16(&name[..length]).map_err(|_| invalid_data(root, "卷 GUID 编码无效"))?;
    let valid = path
        .strip_prefix(r"\\?\Volume{")
        .and_then(|name| name.strip_suffix("}\\"))
        .is_some_and(|guid| {
            guid.len() == 36
                && guid.bytes().enumerate().all(|(offset, ch)| {
                    if matches!(offset, 8 | 13 | 18 | 23) {
                        ch == b'-'
                    } else {
                        ch.is_ascii_hexdigit()
                    }
                })
        });
    if !valid {
        return Err(invalid_data(root, "卷 GUID 格式无效"));
    }
    Ok(path.trim_end_matches('\\').to_owned())
}

fn invalid_data(root: &Path, message: &'static str) -> PlatformError {
    io_error(root, io::Error::new(io::ErrorKind::InvalidData, message))
}

fn journal_missing(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        matches!(
            code as u32,
            ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS
        )
    })
}

fn final_name(file: &File, name: &mut [u16]) -> u32 {
    unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            name.as_mut_ptr(),
            name.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
        )
    }
}

fn relative_to_root(root: &Path, volume_relative: &str) -> Result<PathBuf> {
    let relative = volume_relative
        .strip_prefix('\\')
        .ok_or_else(|| invalid_data(root, "原生文件路径不是卷内绝对路径"))?;
    if relative.starts_with(['\\', '/']) || relative.contains(':') {
        return Err(invalid_data(root, "原生文件路径含非法前缀"));
    }
    normalize_path(&root.join(relative))
}

struct LinkSearch(HANDLE);

impl Drop for LinkSearch {
    fn drop(&mut self) {
        unsafe { FindClose(self.0) };
    }
}

fn hard_links(root: &Path, path: &Path, expected_links: u32) -> Result<Vec<PathBuf>> {
    const MAX_LINKS: u32 = 1_024;
    if expected_links > MAX_LINKS {
        return Err(invalid_data(path, "硬链接数量超过安全上限"));
    }
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut name = vec![0u16; 32_768];
    let mut length = name.len() as u32;
    let handle = unsafe { FindFirstFileNameW(wide.as_ptr(), 0, &mut length, name.as_mut_ptr()) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io_error(path, io::Error::last_os_error()));
    }
    let search = LinkSearch(handle);
    let mut links = BTreeSet::new();
    loop {
        let name_end = name
            .iter()
            .position(|&ch| ch == 0)
            .ok_or_else(|| invalid_data(path, "原生硬链接路径缺少终止符"))?;
        let relative = String::from_utf16(&name[..name_end])
            .map_err(|_| invalid_data(path, "原生硬链接路径编码无效"))?;
        links.insert(relative_to_root(root, &relative)?);
        if links.len() > expected_links as usize {
            return Err(invalid_data(path, "原生硬链接集合在读取时发生变化"));
        }
        name.fill(0);
        length = name.len() as u32;
        if unsafe { FindNextFileNameW(search.0, &mut length, name.as_mut_ptr()) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                break;
            }
            return Err(io_error(path, error));
        }
    }
    if links.len() != expected_links as usize {
        return Err(invalid_data(path, "原生硬链接集合读取不完整"));
    }
    Ok(links.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_authorization_rejects_partial_scopes_and_implicit_volume_reads() {
        let settings = Settings::default();
        assert!(authorized_root(&settings, Path::new(r"D:\")).is_err());
        assert!(authorized_root(&settings, Path::new(r"D:\fixture")).is_err());
        let mut scope = crate::Scope::new(r"D:\");
        scope.smart_excludes = false;
        let settings = Settings {
            mode: SpeedMode::OnDemandElevated,
            roots: vec![scope],
            ..settings
        };
        assert!(authorized_root(&settings, Path::new(r"d:\")).is_ok());
    }
}
