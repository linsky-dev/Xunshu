use crate::{PlatformError, Result};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// Both variants mean the destination has been committed. Only failures before
/// the commit point are returned as Err. A post-commit warning never authorizes
/// deleting the destination or claiming that the old file is still current.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalWriteOutcome {
    Durable,
    Committed { warning: String },
}
impl LocalWriteOutcome {
    pub fn is_durable(&self) -> bool {
        matches!(self, Self::Durable)
    }
    pub fn warning(&self) -> Option<&str> {
        match self {
            Self::Durable => None,
            Self::Committed { warning } => Some(warning),
        }
    }
}

pub(crate) fn io_error(path: &Path, source: std::io::Error) -> PlatformError {
    PlatformError::Io {
        path: path.to_string_lossy().into_owned(),
        source,
    }
}

/// Lexical validation never probes the filesystem or follows an automount.
pub(crate) fn normalize_path(path: &Path) -> Result<PathBuf> {
    let s = path
        .to_str()
        .ok_or_else(|| PlatformError::InvalidPath("Path is not valid UTF-8".into()))?;
    if s.is_empty()
        || s.len() > 32768
        || s.contains('\0')
        || s.contains("://")
        || s.starts_with("//")
        || s.starts_with("\\\\")
    {
        return Err(PlatformError::InvalidPath(
            "An explicit absolute local path is required; URLs, UNC and device paths are forbidden"
                .into(),
        ));
    }
    if !path.is_absolute() {
        return Err(PlatformError::InvalidPath(format!(
            "Absolute path required: {s}"
        )));
    }
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                return Err(PlatformError::InvalidPath(
                    "Parent components (..) are forbidden".into(),
                ))
            }
            Component::CurDir => {}
            #[cfg(windows)]
            Component::Prefix(p) => match p.kind() {
                std::path::Prefix::Disk(_) => clean.push(p.as_os_str()),
                _ => {
                    return Err(PlatformError::InvalidPath(
                        "Only local drive-letter paths are supported".into(),
                    ))
                }
            },
            _ => clean.push(part.as_os_str()),
        }
    }
    #[cfg(windows)]
    {
        // NT device names and alternate data streams can change path semantics.
        for part in clean.components() {
            if let Component::Normal(name) = part {
                let value = name
                    .to_str()
                    .ok_or_else(|| PlatformError::InvalidPath("Invalid UTF-16 path".into()))?;
                let stem = value.split('.').next().unwrap_or("").to_ascii_uppercase();
                if value.contains(':')
                    || value.ends_with('.')
                    || value.ends_with(' ')
                    || matches!(
                        stem.as_str(),
                        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                    )
                    || (stem.len() == 4
                        && (stem.starts_with("COM") || stem.starts_with("LPT"))
                        && stem.as_bytes()[3].is_ascii_digit())
                {
                    return Err(PlatformError::InvalidPath("Windows device names, alternate streams and ambiguous trailing characters are forbidden".into()));
                }
            }
        }
    }
    Ok(clean)
}

/// A point-in-time validation for consumers. Scanner and settings I/O additionally
/// retain directory descriptors/handles while operating, to avoid link-swap races.
pub fn validate_local_path(path: &Path, allow_missing: bool) -> Result<()> {
    imp::validate(path, allow_missing)
}

pub fn create_local_dir_all(path: &Path) -> Result<()> {
    imp::create_dirs(path)
}

/// 在固定的本地父目录中创建一个全新目录，拒绝已有名称。
pub fn create_local_dir_new(path: &Path) -> Result<()> {
    imp::create_dir_new(path)
}

/// 发布调用方在受控父目录中准备完成的暂存目录，要求目标名称不存在。
pub fn publish_local_dir_new(staged: &Path, destination: &Path) -> Result<LocalWriteOutcome> {
    let staged = normalize_path(staged)?;
    let destination = normalize_path(destination)?;
    let same_parent = staged
        .parent()
        .zip(destination.parent())
        .is_some_and(|(a, b)| crate::settings::same_path(a, b));
    if !same_parent || crate::settings::same_path(&staged, &destination) {
        return Err(PlatformError::InvalidPath(
            "暂存目录和目标目录必须是同一父目录下的不同名称".into(),
        ));
    }
    imp::publish_dir_new(&staged, &destination)
}

/// Opens a regular local file through a verified, pinned inode/handle.
pub fn open_local_read(path: &Path) -> Result<std::fs::File> {
    imp::open_read(path)
}

/// Creates a new read/write regular file under a verified, pinned local parent.
/// Existing objects (including links) are refused. The parent must exist.
/// Callers needing rollback-safe streaming should use write_local_new instead.
pub fn create_local_new(path: &Path) -> Result<std::fs::File> {
    imp::create_new(path)
}

/// 清理应用自有目录中的一个普通文件。不会递归或跟随链接。
pub fn remove_local_file(path: &Path) -> Result<()> {
    imp::remove_file(path)
}

/// Opens or creates a read/write regular lock file without truncation.
/// Both parent traversal and the final object reject links/reparse points.
pub fn open_or_create_local_lock(path: &Path) -> Result<std::fs::File> {
    imp::open_lock(path)
}

/// Streams to a private sibling, syncs the data and commits create-only.
/// Failure before commit cleans only the temporary name under the pinned parent.
/// The callback must not retain the supplied file or independently publish it.
pub fn write_local_new(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<LocalWriteOutcome> {
    imp::write_stream(path, false, write)
}

/// 将流式写入结果原子替换到已验证的本地路径。
pub fn write_local_replace(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<LocalWriteOutcome> {
    imp::write_stream(path, true, write)
}

/// Atomically writes under a verified local parent. The parent must already exist.
/// overwrite=false is create-only. Inspect the returned durability outcome.
pub fn atomic_write_local(path: &Path, bytes: &[u8], overwrite: bool) -> Result<LocalWriteOutcome> {
    imp::write_stream(path, overwrite, |file| file.write_all(bytes))
}

pub(crate) fn read_local_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
    imp::read_file(path, limit)
}
pub(crate) fn atomic_write(
    path: &Path,
    bytes: &[u8],
    overwrite: bool,
) -> Result<LocalWriteOutcome> {
    atomic_write_local(path, bytes, overwrite)
}

#[cfg(target_os = "linux")]
pub(crate) mod imp {
    use super::*;
    use crate::volume::{local_filesystem, mount_info, MountInfo};
    use std::ffi::{CStr, CString, OsString};
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) struct LocalNode {
        pub fd: OwnedFd,
        pub mount_id: u64,
        pub stat: libc::stat,
    }
    impl LocalNode {
        pub fn is_dir(&self) -> bool {
            self.stat.st_mode & libc::S_IFMT == libc::S_IFDIR
        }
        pub fn is_link(&self) -> bool {
            self.stat.st_mode & libc::S_IFMT == libc::S_IFLNK
        }
        pub fn is_file(&self) -> bool {
            self.stat.st_mode & libc::S_IFMT == libc::S_IFREG
        }
    }
    pub(crate) enum Child {
        Node(LocalNode),
        OtherMount,
    }

    fn mount_id(fd: &OwnedFd) -> Result<u64> {
        let path = PathBuf::from(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()));
        let text = std::fs::read_to_string(&path).map_err(|e| io_error(&path, e))?;
        text.lines().find_map(|line| line.strip_prefix("mnt_id:").and_then(|v| v.trim().parse::<u64>().ok()))
            .ok_or_else(|| PlatformError::InvalidPath("Kernel did not supply a mount identity; local-only access cannot be guaranteed".into()))
    }
    fn inspect(
        fd: OwnedFd,
        path: &Path,
        required_mount: Option<u64>,
        mounts: &[MountInfo],
    ) -> Result<Child> {
        // O_PATH does not read contents/get attributes or trigger a final-component
        // automount. Read kernel mount identity BEFORE any filesystem metadata call.
        let id = mount_id(&fd)?;
        if required_mount.is_some_and(|expected| expected != id) {
            return Ok(Child::OtherMount);
        }
        let known = mounts
            .iter()
            .find(|m| m.id == id)
            .map(|m| local_filesystem(&m.filesystem));
        let local = match known {
            Some(v) => v,
            None => mount_info()?
                .iter()
                .find(|m| m.id == id)
                .is_some_and(|m| local_filesystem(&m.filesystem)),
        };
        if !local {
            return Err(PlatformError::InvalidPath(format!(
                "Network, automount, virtual or unknown filesystem refused: {}",
                path.display()
            )));
        }
        let mut stat = std::mem::MaybeUninit::uninit();
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        Ok(Child::Node(LocalNode {
            fd,
            mount_id: id,
            stat: unsafe { stat.assume_init() },
        }))
    }
    pub(crate) fn open_child(
        parent: &LocalNode,
        name: &CStr,
        path: &Path,
        required_mount: Option<u64>,
        mounts: &[MountInfo],
    ) -> Result<Child> {
        let fd = unsafe {
            libc::openat(
                parent.fd.as_raw_fd(),
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        inspect(
            unsafe { OwnedFd::from_raw_fd(fd) },
            path,
            required_mount,
            mounts,
        )
    }
    fn root_descriptor() -> Result<OwnedFd> {
        let fd = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io_error(Path::new("/"), std::io::Error::last_os_error()));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    fn check_mount(fd: &OwnedFd, path: &Path, mounts: &[MountInfo], ancestor: bool) -> Result<()> {
        let id = mount_id(fd)?;
        let kind = match mounts.iter().find(|m| m.id == id) {
            Some(m) => m.filesystem.clone(),
            None => mount_info()?
                .into_iter()
                .find(|m| m.id == id)
                .map(|m| m.filesystem)
                .ok_or_else(|| {
                    PlatformError::InvalidPath("Mount identity is no longer known".into())
                })?,
        };
        // Overlay is permitted only as an existing O_PATH directory ancestor.
        // No fstat, readdir, data open, or mutation is performed on it. This lets
        // a mounted local /tmp remain usable inside an overlay-root container.
        if local_filesystem(&kind) || (ancestor && kind == "overlay") {
            Ok(())
        } else {
            Err(PlatformError::InvalidPath(format!(
                "Network, automount, overlay, virtual or unknown data filesystem refused: {}",
                path.display()
            )))
        }
    }
    fn path_child(parent: &OwnedFd, name: &CStr, path: &Path, directory: bool) -> Result<OwnedFd> {
        let flags = libc::O_PATH
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if directory { libc::O_DIRECTORY } else { 0 };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            Err(io_error(path, std::io::Error::last_os_error()))
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }
    pub(crate) fn resolve(path: &Path, allow_missing: bool) -> Result<Option<LocalNode>> {
        let path = normalize_path(path)?;
        let mounts = mount_info()?;
        let mut fd = root_descriptor()?;
        let mut current = PathBuf::from("/");
        let parts: Vec<_> = path
            .components()
            .filter_map(|c| {
                if let Component::Normal(n) = c {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        for (index, name) in parts.iter().enumerate() {
            check_mount(&fd, &current, &mounts, true)?;
            let parent_path = current.clone();
            current.push(name);
            let c = CString::new(name.as_bytes())
                .map_err(|_| PlatformError::InvalidPath("NUL in path".into()))?;
            fd = match path_child(&fd, &c, &current, index + 1 < parts.len()) {
                Err(PlatformError::Io { source, .. })
                    if allow_missing && source.kind() == std::io::ErrorKind::NotFound =>
                {
                    // A missing descendant can only be created under a local
                    // parent; an overlay ancestor does not authorize writes.
                    check_mount(&fd, &parent_path, &mounts, false)?;
                    return Ok(None);
                }
                other => other?,
            };
        }
        let node = match inspect(fd, &path, None, &mounts)? {
            Child::Node(n) => n,
            Child::OtherMount => unreachable!(),
        };
        if node.is_link() {
            return Err(PlatformError::InvalidPath(format!(
                "Symbolic link refused: {}",
                path.display()
            )));
        }
        Ok(Some(node))
    }
    pub fn validate(path: &Path, allow_missing: bool) -> Result<()> {
        resolve(path, allow_missing).map(|_| ())
    }
    pub fn create_dirs(path: &Path) -> Result<()> {
        let path = normalize_path(path)?;
        let mounts = mount_info()?;
        let mut fd = root_descriptor()?;
        let mut current = PathBuf::from("/");
        let parts: Vec<_> = path
            .components()
            .filter_map(|c| {
                if let Component::Normal(n) = c {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        for (index, name) in parts.iter().enumerate() {
            check_mount(&fd, &current, &mounts, true)?;
            let parent_path = current.clone();
            current.push(name);
            let c = CString::new(name.as_bytes())
                .map_err(|_| PlatformError::InvalidPath("NUL in path".into()))?;
            let next = match path_child(&fd, &c, &current, true) {
                Err(PlatformError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    check_mount(&fd, &parent_path, &mounts, false)?;
                    if unsafe { libc::mkdirat(fd.as_raw_fd(), c.as_ptr(), 0o700) } != 0 {
                        let e = std::io::Error::last_os_error();
                        if e.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(io_error(&current, e));
                        }
                    }
                    path_child(&fd, &c, &current, true)?
                }
                other => other?,
            };
            check_mount(&next, &current, &mounts, index + 1 < parts.len())?;
            fd = next;
        }
        check_mount(&fd, &path, &mounts, false)
    }
    pub(crate) struct DirStream {
        ptr: *mut libc::DIR,
    }
    impl DirStream {
        pub fn new(node: &LocalNode, path: &Path) -> Result<Self> {
            let fd = unsafe {
                libc::openat(
                    node.fd.as_raw_fd(),
                    c".".as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io_error(path, std::io::Error::last_os_error()));
            }
            let ptr = unsafe { libc::fdopendir(fd) };
            if ptr.is_null() {
                let e = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(io_error(path, e));
            }
            Ok(Self { ptr })
        }
        pub(crate) fn next_entry(&mut self) -> Option<std::io::Result<(OsString, u8)>> {
            loop {
                unsafe { *libc::__errno_location() = 0 };
                let ptr = unsafe { libc::readdir(self.ptr) };
                if ptr.is_null() {
                    let errno = unsafe { *libc::__errno_location() };
                    return if errno == 0 {
                        None
                    } else {
                        Some(Err(std::io::Error::from_raw_os_error(errno)))
                    };
                }
                let bytes = unsafe { CStr::from_ptr((*ptr).d_name.as_ptr()) }.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                return Some(Ok((OsString::from_vec(bytes.to_vec()), unsafe {
                    (*ptr).d_type
                })));
            }
        }
    }
    impl Iterator for DirStream {
        type Item = std::io::Result<OsString>;
        fn next(&mut self) -> Option<Self::Item> {
            self.next_entry().map(|entry| entry.map(|(name, _)| name))
        }
    }
    impl Drop for DirStream {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.ptr) };
        }
    }
    pub fn open_read(path: &Path) -> Result<File> {
        let node = resolve(path, false)?
            .ok_or_else(|| io_error(path, std::io::Error::from(std::io::ErrorKind::NotFound)))?;
        if !node.is_file() {
            return Err(PlatformError::InvalidPath(
                "A regular local file is required".into(),
            ));
        }
        // Reopen the verified inode itself, not a mutable user path. The proc link
        // is a kernel-owned descriptor reference and cannot redirect to another inode.
        let descriptor = PathBuf::from(format!("/proc/self/fd/{}", node.fd.as_raw_fd()));
        File::open(&descriptor).map_err(|e| io_error(path, e))
    }
    pub fn read_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
        let file = open_read(path)?;
        if file.metadata().map_err(|e| io_error(path, e))?.len() > limit as u64 {
            return Err(PlatformError::InvalidSettings(format!(
                "File exceeds {limit}-byte limit"
            )));
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| io_error(path, e))?;
        if bytes.len() > limit {
            return Err(PlatformError::InvalidSettings(format!(
                "File exceeds {limit}-byte limit"
            )));
        }
        Ok(bytes)
    }
    fn pinned_parent(path: &Path) -> Result<(PathBuf, LocalNode, CString)> {
        let path = normalize_path(path)?;
        let parent_path = path
            .parent()
            .ok_or_else(|| PlatformError::InvalidPath("File has no parent".into()))?;
        let name = path
            .file_name()
            .ok_or_else(|| PlatformError::InvalidPath("File name required".into()))?;
        let parent = resolve(parent_path, false)?.ok_or_else(|| {
            io_error(
                parent_path,
                std::io::Error::from(std::io::ErrorKind::NotFound),
            )
        })?;
        if !parent.is_dir() {
            return Err(PlatformError::InvalidPath(
                "Parent must be a local directory".into(),
            ));
        }
        let c_name = CString::new(name.as_bytes())
            .map_err(|_| PlatformError::InvalidPath("NUL in file name".into()))?;
        Ok((path, parent, c_name))
    }
    fn create_new_at(parent: &LocalNode, name: &CStr, path: &Path) -> Result<File> {
        let fd = unsafe {
            libc::openat(
                parent.fd.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    pub fn create_new(path: &Path) -> Result<File> {
        let (path, parent, name) = pinned_parent(path)?;
        create_new_at(&parent, &name, &path)
    }
    pub fn create_dir_new(path: &Path) -> Result<()> {
        let (path, parent, name) = pinned_parent(path)?;
        if unsafe { libc::mkdirat(parent.fd.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io_error(&path, std::io::Error::last_os_error()));
        }
        Ok(())
    }
    pub fn publish_dir_new(staged: &Path, destination: &Path) -> Result<LocalWriteOutcome> {
        let (staged, parent, name) = pinned_parent(staged)?;
        let destination_name = destination.file_name().expect("已验证目标父目录");
        let destination_name = CString::new(destination_name.as_bytes())
            .map_err(|_| PlatformError::InvalidPath("目录名含 NUL".into()))?;
        match open_child(
            &parent,
            &name,
            &staged,
            Some(parent.mount_id),
            &mount_info()?,
        )? {
            Child::Node(node) if node.is_dir() => {}
            _ => {
                return Err(PlatformError::InvalidPath(
                    "暂存对象不是本地普通目录".into(),
                ))
            }
        }
        // 调用方持有应用自己的写锁，两个名称只在固定父目录内解析。
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent.fd.as_raw_fd(),
                name.as_ptr(),
                parent.fd.as_raw_fd(),
                destination_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(io_error(destination, std::io::Error::last_os_error()));
        }
        Ok(match sync_parent(&parent) {
            Ok(()) => LocalWriteOutcome::Durable,
            Err(error) => LocalWriteOutcome::Committed {
                warning: format!("目录已提交，父目录同步失败：{error}"),
            },
        })
    }
    pub fn remove_file(path: &Path) -> Result<()> {
        let (path, parent, name) = pinned_parent(path)?;
        match open_child(&parent, &name, &path, Some(parent.mount_id), &mount_info()?)? {
            Child::Node(node) if node.is_file() => {}
            _ => return Err(PlatformError::InvalidPath("只能清理本地普通文件".into())),
        }
        // 文件名从固定的父目录移除。Linux 无法按已打开的 inode 直接 unlink；
        // 调用方只能在自己持有写锁、由应用管理的目录内执行代次回收。
        if unsafe { libc::unlinkat(parent.fd.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io_error(&path, std::io::Error::last_os_error()));
        }
        Ok(())
    }
    fn open_lock_at(parent: &LocalNode, name: &CStr, path: &Path) -> Result<File> {
        let mounts = mount_info()?;
        for _ in 0..8 {
            match open_child(parent, name, path, Some(parent.mount_id), &mounts) {
                Ok(Child::Node(node)) if node.is_file() => {
                    // Inspect before RW open: never block on a FIFO or contact a
                    // foreign mount. Reopen the verified inode, not the user path.
                    let descriptor =
                        PathBuf::from(format!("/proc/self/fd/{}", node.fd.as_raw_fd()));
                    return std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(descriptor)
                        .map_err(|e| io_error(path, e));
                }
                Ok(_) => {
                    return Err(PlatformError::InvalidPath(
                        "Lock must be a regular file on the parent's local mount".into(),
                    ))
                }
                Err(PlatformError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    match create_new_at(parent, name, path) {
                        Err(PlatformError::Io { source, .. })
                            if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                        other => return other,
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(PlatformError::InvalidPath(
            "Lock path changed repeatedly while opening; retry".into(),
        ))
    }
    pub fn open_lock(path: &Path) -> Result<File> {
        let (path, parent, name) = pinned_parent(path)?;
        open_lock_at(&parent, &name, &path)
    }
    struct Temporary<'a> {
        parent: &'a LocalNode,
        name: CString,
        present: bool,
    }
    impl Temporary<'_> {
        fn remove(&mut self) -> std::io::Result<()> {
            if self.present {
                if unsafe { libc::unlinkat(self.parent.fd.as_raw_fd(), self.name.as_ptr(), 0) } != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                self.present = false;
            }
            Ok(())
        }
    }
    impl Drop for Temporary<'_> {
        fn drop(&mut self) {
            // A renamed ancestor cannot redirect this cleanup. The guard never
            // stores or deletes the committed destination name.
            let _ = self.remove();
        }
    }
    fn sync_parent(parent: &LocalNode) -> std::io::Result<()> {
        let fd = unsafe {
            libc::openat(
                parent.fd.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::fsync(fd.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    pub fn write_stream(
        path: &Path,
        overwrite: bool,
        write: impl FnOnce(&mut File) -> std::io::Result<()>,
    ) -> Result<LocalWriteOutcome> {
        write_stream_with_sync(path, overwrite, write, sync_parent)
    }
    fn write_stream_with_sync(
        path: &Path,
        overwrite: bool,
        write: impl FnOnce(&mut File) -> std::io::Result<()>,
        sync: impl FnOnce(&LocalNode) -> std::io::Result<()>,
    ) -> Result<LocalWriteOutcome> {
        let (path, parent, name) = pinned_parent(path)?;
        let mounts = mount_info()?;
        match open_child(&parent, &name, &path, Some(parent.mount_id), &mounts) {
            Ok(Child::Node(n)) if n.is_link() || !n.is_file() => {
                return Err(PlatformError::InvalidPath(
                    "Refusing to replace a link or non-regular file".into(),
                ))
            }
            Ok(Child::OtherMount) => {
                return Err(PlatformError::InvalidPath(
                    "Refusing to replace a file on a different mount".into(),
                ))
            }
            Ok(_) if !overwrite => {
                return Err(io_error(
                    &path,
                    std::io::Error::from(std::io::ErrorKind::AlreadyExists),
                ))
            }
            Ok(_) => {}
            Err(PlatformError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_name = CString::new(format!(
            ".xs-write-{}-{}-{}.tmp",
            std::process::id(),
            stamp,
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
        .unwrap();
        let mut file = create_new_at(&parent, &temp_name, &path)?;
        let mut temp = Temporary {
            parent: &parent,
            name: temp_name,
            present: true,
        };
        write(&mut file).map_err(|e| io_error(&path, e))?;
        file.sync_all().map_err(|e| io_error(&path, e))?;
        let status = if overwrite {
            unsafe {
                libc::renameat(
                    parent.fd.as_raw_fd(),
                    temp.name.as_ptr(),
                    parent.fd.as_raw_fd(),
                    name.as_ptr(),
                )
            }
        } else {
            unsafe {
                libc::linkat(
                    parent.fd.as_raw_fd(),
                    temp.name.as_ptr(),
                    parent.fd.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            }
        };
        if status != 0 {
            return Err(io_error(&path, std::io::Error::last_os_error()));
        }
        // COMMIT POINT. No error below may be represented as an uncommitted Err.
        let mut warnings = Vec::new();
        if overwrite {
            temp.present = false; // rename consumed only the temporary name
        } else if let Err(e) = temp.remove() {
            warnings.push(format!(
                "committed file; temporary-name cleanup failed: {e}"
            ));
        }
        if let Err(e) = sync(&parent) {
            warnings.push(format!(
                "file already committed at {}; parent directory sync failed, durability is unconfirmed: {e}",
                path.display()
            ));
        }
        Ok(if warnings.is_empty() {
            LocalWriteOutcome::Durable
        } else {
            LocalWriteOutcome::Committed {
                warning: warnings.join("; "),
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;
        use std::os::unix::fs::symlink;

        struct Fixture(PathBuf);
        impl Fixture {
            fn new() -> Self {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "xs-local-write-{}-{stamp}-{}",
                    std::process::id(),
                    NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
                ));
                create_dirs(&path).unwrap();
                Self(path)
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }

        #[test]
        fn new_and_lock_creation_remain_on_the_pinned_parent_at_the_race_boundary() {
            let f = Fixture::new();
            let chosen = f.0.join("chosen");
            let moved = f.0.join("original-parent");
            let outside = f.0.join("outside");
            fs::create_dir(&chosen).unwrap();
            fs::create_dir(&outside).unwrap();
            fs::write(chosen.join("existing.lock"), b"existing").unwrap();
            for name in ["new.bin", "new.lock", "existing.lock"] {
                fs::write(outside.join(name), b"outside decoy").unwrap();
            }
            // Pause at precisely the production boundary after parent traversal,
            // before either creating a child or opening an existing lock.
            let (path, parent, name) = pinned_parent(&chosen.join("new.bin")).unwrap();
            fs::rename(&chosen, &moved).unwrap();
            symlink(&outside, &chosen).unwrap();
            create_new_at(&parent, &name, &path)
                .unwrap()
                .write_all(b"new local file")
                .unwrap();
            open_lock_at(&parent, c"new.lock", &chosen.join("new.lock"))
                .unwrap()
                .write_all(b"new local lock")
                .unwrap();
            let mut lock =
                open_lock_at(&parent, c"existing.lock", &chosen.join("existing.lock")).unwrap();
            let mut contents = String::new();
            lock.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "existing");
            lock.write_all(b"-appended").unwrap();
            assert_eq!(fs::read(moved.join("new.bin")).unwrap(), b"new local file");
            assert_eq!(fs::read(moved.join("new.lock")).unwrap(), b"new local lock");
            assert_eq!(
                fs::read(moved.join("existing.lock")).unwrap(),
                b"existing-appended"
            );
            for name in ["new.bin", "new.lock", "existing.lock"] {
                assert_eq!(fs::read(outside.join(name)).unwrap(), b"outside decoy");
            }
        }

        #[test]
        fn directory_sync_failure_after_replace_is_reported_as_committed() {
            let f = Fixture::new();
            let path = f.0.join("CURRENT.json");
            fs::write(&path, b"old generation").unwrap();
            let outcome = write_stream_with_sync(
                &path,
                true,
                |file| file.write_all(b"new generation"),
                |_| Err(std::io::Error::from_raw_os_error(libc::EIO)),
            )
            .unwrap();
            assert!(!outcome.is_durable());
            assert!(outcome.warning().unwrap().contains("already committed"));
            assert_eq!(
                serde_json::to_value(&outcome).unwrap()["state"],
                "committed"
            );
            assert_eq!(fs::read(&path).unwrap(), b"new generation");
            assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
        }

        #[test]
        fn directory_sync_failure_after_create_only_never_rolls_back_the_file() {
            let f = Fixture::new();
            let path = f.0.join("generation.xsi");
            let outcome = write_stream_with_sync(
                &path,
                false,
                |file| file.write_all(b"complete immutable generation"),
                |_| Err(std::io::Error::from_raw_os_error(libc::EIO)),
            )
            .unwrap();
            assert!(matches!(outcome, LocalWriteOutcome::Committed { .. }));
            assert_eq!(fs::read(&path).unwrap(), b"complete immutable generation");
            assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
            assert!(create_new(&path).is_err());
        }

        #[test]
        fn failed_stream_before_commit_preserves_the_old_destination() {
            let f = Fixture::new();
            let path = f.0.join("CURRENT.json");
            fs::write(&path, b"old generation").unwrap();
            let outcome = write_stream_with_sync(
                &path,
                true,
                |file| {
                    file.write_all(b"partial replacement")?;
                    Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
                },
                |_| panic!("directory sync cannot run before a successful commit"),
            );
            assert!(outcome.is_err());
            assert_eq!(fs::read(&path).unwrap(), b"old generation");
            assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
        }
    }
}

#[cfg(windows)]
pub(crate) mod imp {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::io::Read;
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, FileRenameInfo, GetDriveTypeW, GetFileInformationByHandle,
        MoveFileExW, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, DELETE,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    pub(crate) struct Guard {
        pub handles: Vec<File>,
    }
    fn wide(path: &Path) -> Result<Vec<u16>> {
        use std::os::windows::ffi::OsStrExt;
        Ok(path.as_os_str().encode_wide().chain(Some(0)).collect())
    }
    pub(crate) fn file_info(file: &File, path: &Path) -> Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = std::mem::MaybeUninit::uninit();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return Err(io_error(path, std::io::Error::last_os_error()));
        }
        Ok(unsafe { info.assume_init() })
    }
    pub(crate) fn guard(path: &Path, allow_missing: bool) -> Result<Guard> {
        let path = normalize_path(path)?;
        let prefix = path
            .components()
            .next()
            .ok_or_else(|| PlatformError::InvalidPath("Drive required".into()))?;
        let mut drive = PathBuf::from(prefix.as_os_str());
        drive.push("\\");
        let kind = unsafe { GetDriveTypeW(wide(&drive)?.as_ptr()) };
        if !matches!(kind, 2 | 3 | 5 | 6) {
            return Err(PlatformError::InvalidPath(
                "Remote or unknown drive refused".into(),
            ));
        }
        let mut handles = Vec::new();
        let mut current = drive.clone();
        let mut paths = vec![drive];
        for part in path.components() {
            if let Component::Normal(n) = part {
                current.push(n);
                paths.push(current.clone());
            }
        }
        for part in paths {
            let opened = OpenOptions::new()
                .read(true)
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&part);
            let file = match opened {
                Ok(f) => f,
                Err(e) if allow_missing && e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(io_error(&part, e)),
            };
            if file_info(&file, &part)?.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(PlatformError::InvalidPath(format!(
                    "Reparse point refused: {}",
                    part.display()
                )));
            }
            handles.push(file);
        }
        Ok(Guard { handles })
    }
    pub fn validate(path: &Path, allow_missing: bool) -> Result<()> {
        guard(path, allow_missing).map(|_| ())
    }
    pub fn create_dirs(path: &Path) -> Result<()> {
        let path = normalize_path(path)?;
        let mut current = PathBuf::new();
        let mut guards = Vec::new();
        let mut depth = 0;
        for part in path.components() {
            current.push(part.as_os_str());
            if matches!(part, Component::Prefix(_)) {
                continue;
            }
            depth += 1;
            let held = guard(&current, depth > 1)?;
            // 已存在的驱动器根和祖先只验证，不调用 CreateDirectoryW。
            // 某些 NTFS 根的创建请求返回 ACCESS_DENIED，而不是 ALREADY_EXISTS。
            if held.handles.len() == depth {
                ensure_dir(&held, &current)?;
                guards.push(held);
                continue;
            }
            match std::fs::create_dir(&current) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_error(&current, e)),
            }
            guards.push(held);
            let created = guard(&current, false)?;
            ensure_dir(&created, &current)?;
            guards.push(created);
        }
        Ok(())
    }
    pub fn open_read(path: &Path) -> Result<File> {
        // Ancestors remain locked against mutation until the content handle opens.
        // The returned file itself denies sharing for writes/deletes and is pinned.
        let _guard = guard(path, false)?;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|e| io_error(path, e))?;
        let info = file_info(&file, path)?;
        if info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | 0x10) != 0 {
            return Err(PlatformError::InvalidPath("Regular file required".into()));
        }
        Ok(file)
    }
    pub fn read_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
        let file = open_read(path)?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| io_error(path, e))?;
        if bytes.len() > limit {
            return Err(PlatformError::InvalidSettings(
                "Settings exceeds size limit".into(),
            ));
        }
        Ok(bytes)
    }
    fn pinned_parent(path: &Path) -> Result<(PathBuf, Guard)> {
        let path = normalize_path(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| PlatformError::InvalidPath("File parent required".into()))?;
        if path.file_name().is_none() {
            return Err(PlatformError::InvalidPath("File name required".into()));
        }
        let held = guard(parent, false)?;
        ensure_dir(&held, parent)?;
        Ok((path, held))
    }
    fn ensure_dir(held: &Guard, path: &Path) -> Result<()> {
        let last = held
            .handles
            .last()
            .ok_or_else(|| PlatformError::InvalidPath("无法获取目录句柄".into()))?;
        if file_info(last, path)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(PlatformError::InvalidPath(format!(
                "需要目录：{}",
                path.display()
            )));
        }
        Ok(())
    }
    fn regular(file: File, path: &Path) -> Result<File> {
        if file_info(&file, path)?.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | 0x10) != 0 {
            return Err(PlatformError::InvalidPath(
                "Regular file required; reparse points are refused".into(),
            ));
        }
        Ok(file)
    }
    pub fn create_new(path: &Path) -> Result<File> {
        let (path, _parent) = pinned_parent(path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        regular(file, &path)
    }
    pub fn create_dir_new(path: &Path) -> Result<()> {
        let (path, _parent) = pinned_parent(path)?;
        std::fs::create_dir(&path).map_err(|error| io_error(&path, error))?;
        ensure_dir(&guard(&path, false)?, &path)
    }
    pub fn publish_dir_new(staged: &Path, destination: &Path) -> Result<LocalWriteOutcome> {
        let (staged, _parent) = pinned_parent(staged)?;
        let file = OpenOptions::new()
            .access_mode(DELETE | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&staged)
            .map_err(|error| io_error(&staged, error))?;
        let attrs = file_info(&file, &staged)?.dwFileAttributes;
        if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 || attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(PlatformError::InvalidPath(
                "暂存对象不是本地普通目录".into(),
            ));
        }
        let name = wide(destination)?;
        let filename_offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let bytes = filename_offset + name.len() * std::mem::size_of::<u16>();
        let mut buffer = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // 父目录链保持固定，按句柄改名源对象，ReplaceIfExists 保持为 false。
        let committed = unsafe {
            (*info).FileNameLength = ((name.len() - 1) * std::mem::size_of::<u16>()) as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                buffer
                    .as_mut_ptr()
                    .cast::<u8>()
                    .add(filename_offset)
                    .cast::<u16>(),
                name.len(),
            );
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileRenameInfo,
                info.cast(),
                bytes as u32,
            )
        };
        if committed == 0 {
            return Err(io_error(destination, std::io::Error::last_os_error()));
        }
        Ok(LocalWriteOutcome::Committed {
            warning: "目录已提交，Windows 未提供父目录持久性确认".into(),
        })
    }
    pub fn remove_file(path: &Path) -> Result<()> {
        let (path, _held) = pinned_parent(path)?;
        let file = regular(
            OpenOptions::new()
                .access_mode(DELETE | FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)
                .map_err(|e| io_error(&path, e))?,
            &path,
        )?;
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } == 0
        {
            return Err(io_error(&path, std::io::Error::last_os_error()));
        }
        Ok(())
    }
    pub fn open_lock(path: &Path) -> Result<File> {
        let (path, _parent) = pinned_parent(path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
            .map_err(|e| io_error(&path, e))?;
        regular(file, &path)
    }
    struct Temporary {
        file: Option<File>,
        path: PathBuf,
        present: bool,
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            drop(self.file.take());
            // The caller retains every ancestor handle without DELETE sharing.
            // Only our unpublished sibling name is ever eligible for cleanup.
            if self.present {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub fn write_stream(
        path: &Path,
        overwrite: bool,
        write: impl FnOnce(&mut File) -> std::io::Result<()>,
    ) -> Result<LocalWriteOutcome> {
        let (path, _parent) = pinned_parent(path)?;
        let parent = path.parent().expect("pinned parent");
        match OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
        {
            Ok(file) => {
                regular(file, &path)?;
                if !overwrite {
                    return Err(io_error(&path, std::io::ErrorKind::AlreadyExists.into()));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(&path, e)),
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_path = parent.join(format!(
            ".xs-write-{}-{stamp}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&temp_path)
            .map_err(|e| io_error(&temp_path, e))?;
        let mut temp = Temporary {
            file: Some(file),
            path: temp_path,
            present: true,
        };
        let file = temp.file.as_mut().expect("temporary file");
        write(file).map_err(|e| io_error(&path, e))?;
        file.sync_all().map_err(|e| io_error(&path, e))?;
        drop(temp.file.take());
        let flags = MOVEFILE_WRITE_THROUGH
            | if overwrite {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        if unsafe { MoveFileExW(wide(&temp.path)?.as_ptr(), wide(&path)?.as_ptr(), flags) } == 0 {
            return Err(io_error(&path, std::io::Error::last_os_error()));
        }
        // MoveFileExW with WRITE_THROUGH reported success. No post-commit
        // rollback is attempted, and the published name is never cleaned up.
        temp.present = false;
        Ok(LocalWriteOutcome::Durable)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;
    fn unsupported<T>() -> Result<T> {
        Err(PlatformError::InvalidPath(
            "Local-only path access is implemented only on Linux and Windows".into(),
        ))
    }
    pub fn validate(_: &Path, _: bool) -> Result<()> {
        unsupported()
    }
    pub fn create_dirs(_: &Path) -> Result<()> {
        unsupported()
    }
    pub fn open_read(_: &Path) -> Result<std::fs::File> {
        unsupported()
    }
    pub fn read_file(_: &Path, _: usize) -> Result<Vec<u8>> {
        unsupported()
    }
    pub fn create_new(_: &Path) -> Result<std::fs::File> {
        unsupported()
    }
    pub fn create_dir_new(_: &Path) -> Result<()> {
        unsupported()
    }
    pub fn publish_dir_new(_: &Path, _: &Path) -> Result<LocalWriteOutcome> {
        unsupported()
    }
    pub fn remove_file(_: &Path) -> Result<()> {
        unsupported()
    }
    pub fn open_lock(_: &Path) -> Result<std::fs::File> {
        unsupported()
    }
    pub fn write_stream(
        _: &Path,
        _: bool,
        _: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    ) -> Result<LocalWriteOutcome> {
        unsupported()
    }
}
