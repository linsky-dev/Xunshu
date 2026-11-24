//! 本地文件名范围的变更提示。通知不替代带边界校验的扫描。

use crate::local::normalize_path;
use crate::settings::{is_smart_excluded, is_within, same_path, scope_for};
use crate::{FileIdentity, FileRevision, Result, Scope, Settings};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchEvent {
    Idle,
    Changed,
    /// 通知丢失或目录拓扑变化。重新订阅后执行完整校准。
    RescanRequired,
}

pub struct ScopeWatcher {
    roots: Vec<(PathBuf, FileIdentity)>,
    watch_scope: WatchScope,
    backend: imp::Backend,
    last_identity_check: Instant,
    invalid: bool,
    last_paths: Option<Vec<PathBuf>>,
}

impl ScopeWatcher {
    /// 仅订阅保存的文件名范围。硬排除优先于任何显式根目录。
    pub fn new(settings: &Settings, excluded_paths: &[PathBuf]) -> Result<Self> {
        let watch_scope = WatchScope {
            roots: settings.normalized()?.roots,
            excluded: excluded_paths
                .iter()
                .map(|path| normalize_path(path))
                .collect::<Result<_>>()?,
            single_file: None,
        };
        let roots = watch_scope
            .roots
            .iter()
            .map(|scope| PathBuf::from(&scope.path))
            .filter(|path| watch_scope.accepts(path))
            .map(|path| {
                let identity = FileRevision::at_path(&path)?.identity;
                Ok((path, identity))
            })
            .collect::<Result<Vec<_>>>()?;
        Self::open(watch_scope, roots)
    }

    /// 仅监听指定文件在父目录中的变化，包括原子替换。不会遍历父目录。
    pub fn for_file(path: &Path) -> Result<Self> {
        let path = normalize_path(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| crate::PlatformError::InvalidPath("监听文件必须有父目录".into()))?;
        let roots = vec![(parent.to_owned(), FileRevision::at_path(parent)?.identity)];
        Self::open(
            WatchScope {
                roots: Vec::new(),
                excluded: Vec::new(),
                single_file: Some(path),
            },
            roots,
        )
    }

    fn open(watch_scope: WatchScope, roots: Vec<(PathBuf, FileIdentity)>) -> Result<Self> {
        let backend = imp::Backend::new(&watch_scope, &roots)?;
        Ok(Self {
            roots,
            watch_scope,
            backend,
            last_identity_check: Instant::now(),
            invalid: false,
            last_paths: Some(Vec::new()),
        })
    }

    /// 监听仅产生失效提示。调用方在扫描前再次校验当前设置的授权代次。
    pub fn poll(&mut self, timeout: Duration) -> Result<WatchEvent> {
        self.last_paths = None;
        if self.invalid {
            return Ok(WatchEvent::RescanRequired);
        }
        // 根目录的祖先被替换时，原生通知未必报告该变化。
        let timeout = timeout.min(Duration::from_secs(1));
        let batch = self.backend.poll(&self.watch_scope, timeout)?;
        if self.last_identity_check.elapsed() >= Duration::from_secs(1) {
            self.last_identity_check = Instant::now();
            if self.roots.iter().any(|(path, identity)| {
                FileRevision::at_path(path).map_or(true, |current| current.identity != *identity)
            }) {
                self.invalid = true;
            }
        }
        match batch {
            ChangeBatch::Paths(paths) if !self.invalid => {
                let event = if paths.is_empty() {
                    WatchEvent::Idle
                } else {
                    WatchEvent::Changed
                };
                self.last_paths = Some(paths.into_iter().collect());
                Ok(event)
            }
            _ => {
                self.invalid = true;
                Ok(WatchEvent::RescanRequired)
            }
        }
    }

    /// 取出上次轮询的有界路径集合。路径只是提示，操作前必须重新校验。
    /// `None` 表示需要校准、上次轮询出错，或本批路径已取走。
    pub fn take_changed_paths(&mut self) -> Option<Vec<PathBuf>> {
        self.last_paths.take()
    }
}

const CHANGED_PATH_LIMIT: usize = 512;

#[derive(Debug, Eq, PartialEq)]
enum ChangeBatch {
    Paths(BTreeSet<PathBuf>),
    Lost,
}

impl ChangeBatch {
    fn idle() -> Self {
        Self::Paths(BTreeSet::new())
    }
}

struct WatchScope {
    roots: Vec<Scope>,
    excluded: Vec<PathBuf>,
    single_file: Option<PathBuf>,
}

impl WatchScope {
    fn accepts(&self, path: &Path) -> bool {
        if let Some(file) = &self.single_file {
            return same_path(file, path);
        }
        !self.excluded.iter().any(|root| is_within(path, root))
            && scope_for(&self.roots, path).is_some_and(|scope| !is_smart_excluded(scope, path))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use crate::local::imp::{open_child, resolve, Child, DirStream, LocalNode};
    use crate::local::io_error;
    use crate::volume::mount_info;
    use crate::PlatformError;
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    const WATCH_LIMIT: usize = 32_768;
    const TOPOLOGY: u32 =
        libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO;
    const LOST: u32 = libc::IN_MOVE_SELF
        | libc::IN_DELETE_SELF
        | libc::IN_UNMOUNT
        | libc::IN_IGNORED
        | libc::IN_Q_OVERFLOW;

    struct Frame {
        node: LocalNode,
        path: PathBuf,
        stream: DirStream,
    }

    pub(super) struct Backend {
        fd: OwnedFd,
        directories: HashMap<i32, PathBuf>,
    }

    impl Backend {
        pub(super) fn new(scope: &WatchScope, roots: &[(PathBuf, FileIdentity)]) -> Result<Self> {
            let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
            if fd < 0 {
                return Err(io_error(Path::new("inotify"), io::Error::last_os_error()));
            }
            let mut backend = Self {
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                directories: HashMap::new(),
            };
            let mounts = mount_info()?;
            for (root, expected) in roots {
                let node = resolve(root, false)?.expect("缺失路径必须返回错误");
                if !node.is_dir()
                    || crate::revision::imp::from_stat(&node.stat).identity != *expected
                {
                    return Err(PlatformError::InvalidPath(format!(
                        "订阅时目录发生变化：{}",
                        root.display()
                    )));
                }
                if scope.single_file.is_some() {
                    backend.add(&node, root, libc::IN_MODIFY)?;
                    continue;
                }
                let root_mount = node.mount_id;
                let mut stack = vec![backend.register(node, root.clone())?];
                while let Some(frame) = stack.last_mut() {
                    let Some(entry) = frame.stream.next_entry() else {
                        stack.pop();
                        continue;
                    };
                    let (name, kind) = entry.map_err(|error| io_error(&frame.path, error))?;
                    if !matches!(kind, libc::DT_DIR | libc::DT_UNKNOWN) || name.to_str().is_none() {
                        continue;
                    }
                    let path = frame.path.join(&name);
                    if !scope.accepts(&path) || roots.iter().any(|(other, _)| *other == path) {
                        continue;
                    }
                    let name = CString::new(name.as_bytes())
                        .map_err(|_| PlatformError::InvalidPath("目录名含 NUL".into()))?;
                    if let Child::Node(child) =
                        open_child(&frame.node, &name, &path, Some(root_mount), &mounts)?
                    {
                        if child.is_dir() {
                            stack.push(backend.register(child, path)?);
                        }
                    }
                }
            }
            Ok(backend)
        }

        fn register(&mut self, node: LocalNode, path: PathBuf) -> Result<Frame> {
            self.add(&node, &path, 0)?;
            Ok(Frame {
                stream: DirStream::new(&node, &path)?,
                node,
                path,
            })
        }

        fn add(&mut self, node: &LocalNode, path: &Path, extra_mask: u32) -> Result<()> {
            if self.directories.len() >= WATCH_LIMIT {
                return Err(PlatformError::InvalidSettings(
                    "已达目录监听上限，需要定期校准".into(),
                ));
            }
            let pinned = CString::new(format!("/proc/self/fd/{}", node.fd.as_raw_fd()))
                .expect("文件描述符不含 NUL");
            let mask = TOPOLOGY
                | libc::IN_CLOSE_WRITE
                | libc::IN_ATTRIB
                | libc::IN_MOVE_SELF
                | libc::IN_DELETE_SELF
                | libc::IN_ONLYDIR
                | extra_mask;
            // 内核通过已验证的描述符定位目录，避免重新解析用户路径。
            let watch =
                unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), pinned.as_ptr(), mask) };
            if watch < 0 {
                return Err(io_error(path, io::Error::last_os_error()));
            }
            self.directories.insert(watch, path.to_owned());
            Ok(())
        }

        pub(super) fn poll(
            &mut self,
            scope: &WatchScope,
            timeout: Duration,
        ) -> Result<ChangeBatch> {
            let mut descriptor = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout.as_millis() as i32) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::Interrupted {
                    Ok(ChangeBatch::idle())
                } else {
                    Err(io_error(Path::new("inotify"), error))
                };
            }
            if ready == 0 {
                return Ok(ChangeBatch::idle());
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Ok(ChangeBatch::Lost);
            }
            let mut buffer = [0u8; 32_768];
            let bytes = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if bytes < 0 {
                let error = io::Error::last_os_error();
                return if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    Ok(ChangeBatch::idle())
                } else {
                    Err(io_error(Path::new("inotify"), error))
                };
            }
            Ok(self.decode(scope, &buffer[..bytes as usize]))
        }

        fn decode(&self, scope: &WatchScope, mut buffer: &[u8]) -> ChangeBatch {
            let mut changed = BTreeSet::new();
            while !buffer.is_empty() {
                let Some(header) = buffer.get(..16) else {
                    return ChangeBatch::Lost;
                };
                let watch = i32::from_ne_bytes(header[0..4].try_into().unwrap());
                let mask = u32::from_ne_bytes(header[4..8].try_into().unwrap());
                let length = u32::from_ne_bytes(header[12..16].try_into().unwrap()) as usize;
                if mask & LOST != 0 || length > buffer.len() - 16 {
                    return ChangeBatch::Lost;
                }
                let Some(parent) = self.directories.get(&watch) else {
                    return ChangeBatch::Lost;
                };
                let name = &buffer[16..16 + length];
                let name = OsStr::from_bytes(name.split(|&b| b == 0).next().unwrap_or_default());
                buffer = &buffer[16 + length..];
                let path = parent.join(name);
                if !scope.accepts(&path) {
                    continue;
                }
                if mask & libc::IN_ISDIR != 0 && mask & TOPOLOGY != 0 {
                    return ChangeBatch::Lost;
                }
                if path.to_str().is_none() {
                    return ChangeBatch::Lost;
                }
                changed.insert(path);
                if changed.len() > CHANGED_PATH_LIMIT {
                    return ChangeBatch::Lost;
                }
            }
            ChangeBatch::Paths(changed)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn queue_overflow_and_truncated_records_require_reconciliation() {
            let backend = Backend::new(
                &WatchScope {
                    roots: vec![],
                    excluded: vec![],
                    single_file: None,
                },
                &[],
            )
            .unwrap();
            let scope = WatchScope {
                roots: vec![],
                excluded: vec![],
                single_file: None,
            };
            let mut event = [0; 16];
            event[4..8].copy_from_slice(&libc::IN_Q_OVERFLOW.to_ne_bytes());
            assert_eq!(backend.decode(&scope, &event), ChangeBatch::Lost);
            assert_eq!(backend.decode(&scope, &event[..8]), ChangeBatch::Lost);
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::local::imp::{file_info, guard};
    use crate::local::io_error;
    use crate::PlatformError;
    use std::cell::UnsafeCell;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{
        ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        ReadDirectoryChangesW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY,
        FILE_NOTIFY_CHANGE_SIZE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};
    use windows_sys::Win32::System::IO::{
        CancelIoEx, CreateIoCompletionPort, GetOverlappedResult, GetQueuedCompletionStatus,
        OVERLAPPED,
    };

    const BUFFER_WORDS: usize = 4_096;

    struct DirectoryWatch {
        root: PathBuf,
        file: File,
        completed: OwnedHandle,
        overlapped: Box<UnsafeCell<OVERLAPPED>>,
        buffer: Box<UnsafeCell<[u32; BUFFER_WORDS]>>,
        recursive: bool,
        pending: bool,
    }

    // 句柄与固定地址缓冲区随所有权移交，完成前仅由内核访问该缓冲区。
    unsafe impl Send for DirectoryWatch {}

    impl DirectoryWatch {
        fn arm(&mut self) -> Result<()> {
            debug_assert!(!self.pending);
            if unsafe { ResetEvent(self.completed.as_raw_handle()) } == 0 {
                return Err(io_error(&self.root, io::Error::last_os_error()));
            }
            unsafe {
                self.overlapped.get().write(OVERLAPPED {
                    hEvent: self.completed.as_raw_handle(),
                    ..OVERLAPPED::default()
                });
            }
            let mask = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_ATTRIBUTES
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_CREATION
                | FILE_NOTIFY_CHANGE_SECURITY;
            let started = unsafe {
                ReadDirectoryChangesW(
                    self.file.as_raw_handle(),
                    self.buffer.get().cast(),
                    (BUFFER_WORDS * 4) as u32,
                    i32::from(self.recursive),
                    mask,
                    null_mut(),
                    self.overlapped.get(),
                    None,
                )
            };
            if started == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return Err(io_error(&self.root, error));
                }
            }
            self.pending = true;
            Ok(())
        }
    }

    impl Drop for DirectoryWatch {
        fn drop(&mut self) {
            if self.pending {
                let mut transferred = 0;
                // CancelIoEx 仅请求取消。内核完成前不得释放重叠 I/O 结构。
                unsafe {
                    CancelIoEx(self.file.as_raw_handle(), self.overlapped.get());
                    GetOverlappedResult(
                        self.file.as_raw_handle(),
                        self.overlapped.get(),
                        &mut transferred,
                        1,
                    );
                }
            }
        }
    }

    pub(super) struct Backend {
        watches: Vec<DirectoryWatch>,
        port: OwnedHandle,
    }

    impl Backend {
        pub(super) fn new(scope: &WatchScope, roots: &[(PathBuf, FileIdentity)]) -> Result<Self> {
            let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, 1) };
            if port.is_null() {
                return Err(io_error(Path::new("IOCP"), io::Error::last_os_error()));
            }
            let mut backend = Self {
                watches: Vec::with_capacity(roots.len()),
                port: unsafe { OwnedHandle::from_raw_handle(port) },
            };
            for (index, (root, identity)) in roots.iter().enumerate() {
                let _guard = guard(root, false)?;
                let file = OpenOptions::new()
                    .read(true)
                    .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                    .custom_flags(
                        FILE_FLAG_BACKUP_SEMANTICS
                            | FILE_FLAG_OPEN_REPARSE_POINT
                            | FILE_FLAG_OVERLAPPED,
                    )
                    .open(root)
                    .map_err(|error| io_error(root, error))?;
                let info = file_info(&file, root)?;
                let observed = crate::revision::imp::from_info(&file, root, &info)?.identity;
                if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 || observed != *identity {
                    return Err(PlatformError::InvalidPath(format!(
                        "订阅时目录发生变化：{}",
                        root.display()
                    )));
                }
                let associated = unsafe {
                    CreateIoCompletionPort(
                        file.as_raw_handle(),
                        backend.port.as_raw_handle(),
                        index + 1,
                        0,
                    )
                };
                if associated.is_null() {
                    return Err(io_error(root, io::Error::last_os_error()));
                }
                let completed = unsafe { CreateEventW(null(), 1, 0, null()) };
                if completed.is_null() {
                    return Err(io_error(root, io::Error::last_os_error()));
                }
                let mut watch = DirectoryWatch {
                    root: root.clone(),
                    file,
                    completed: unsafe { OwnedHandle::from_raw_handle(completed) },
                    overlapped: Box::new(UnsafeCell::new(OVERLAPPED::default())),
                    buffer: Box::new(UnsafeCell::new([0; BUFFER_WORDS])),
                    recursive: scope.single_file.is_none(),
                    pending: false,
                };
                watch.arm()?;
                backend.watches.push(watch);
            }
            Ok(backend)
        }

        pub(super) fn poll(
            &mut self,
            scope: &WatchScope,
            timeout: Duration,
        ) -> Result<ChangeBatch> {
            let mut transferred = 0;
            let mut key = 0;
            let mut overlapped = null_mut();
            let success = unsafe {
                GetQueuedCompletionStatus(
                    self.port.as_raw_handle(),
                    &mut transferred,
                    &mut key,
                    &mut overlapped,
                    timeout.as_millis() as u32,
                )
            } != 0;
            let error = (!success).then(io::Error::last_os_error);
            if overlapped.is_null() {
                return match error {
                    Some(error) if error.raw_os_error() == Some(WAIT_TIMEOUT as i32) => {
                        Ok(ChangeBatch::idle())
                    }
                    Some(error) => Err(io_error(Path::new("IOCP"), error)),
                    None => Ok(ChangeBatch::Lost),
                };
            }
            let Some(watch) = key
                .checked_sub(1)
                .and_then(|index| self.watches.get_mut(index))
            else {
                return Ok(ChangeBatch::Lost);
            };
            if watch.overlapped.get() != overlapped {
                return Ok(ChangeBatch::Lost);
            }
            watch.pending = false;
            if let Some(error) = error {
                if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                    return Ok(ChangeBatch::Lost);
                }
                return Err(io_error(&watch.root, error));
            }
            if transferred == 0 || transferred as usize > BUFFER_WORDS * 4 {
                return Ok(ChangeBatch::Lost);
            }
            // 完成包到达后，内核已交还缓冲区。
            let bytes = unsafe {
                std::slice::from_raw_parts(watch.buffer.get().cast::<u8>(), transferred as usize)
            };
            let event = decode(scope, &watch.root, bytes);
            watch.arm()?;
            Ok(event)
        }
    }

    fn decode(scope: &WatchScope, root: &Path, mut bytes: &[u8]) -> ChangeBatch {
        let mut changed = BTreeSet::new();
        loop {
            let Some(header) = bytes.get(..12) else {
                return ChangeBatch::Lost;
            };
            let next = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
            if length == 0 || !length.is_multiple_of(2) || length > bytes.len() - 12 {
                return ChangeBatch::Lost;
            }
            let utf16: Vec<_> = bytes[12..12 + length]
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(u16::from_le_bytes)
                .collect();
            let Ok(name) = String::from_utf16(&utf16) else {
                return ChangeBatch::Lost;
            };
            let Ok(path) = normalize_path(&root.join(name)) else {
                return ChangeBatch::Lost;
            };
            if !is_within(&path, root) {
                return ChangeBatch::Lost;
            }
            if scope.accepts(&path) {
                changed.insert(path);
                if changed.len() > CHANGED_PATH_LIMIT {
                    return ChangeBatch::Lost;
                }
            }
            if next == 0 {
                return ChangeBatch::Paths(changed);
            }
            if next < 12 + length || next > bytes.len() || !next.is_multiple_of(4) {
                return ChangeBatch::Lost;
            }
            bytes = &bytes[next..];
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;
    pub(super) struct Backend;
    impl Backend {
        pub(super) fn new(_: &WatchScope, _: &[(PathBuf, FileIdentity)]) -> Result<Self> {
            Err(crate::PlatformError::InvalidPath(
                "当前平台不支持目录监听".into(),
            ))
        }
        pub(super) fn poll(&mut self, _: &WatchScope, _: Duration) -> Result<ChangeBatch> {
            Ok(ChangeBatch::Lost)
        }
    }
}
