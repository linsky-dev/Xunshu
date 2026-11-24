use crate::local::normalize_path;
use crate::settings::{is_smart_excluded, is_within, same_path, scope_for};
use crate::{FileRevision, FileTimestamp, PlatformError, Result, Scope, Settings, SpeedMode};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub modified: i64,
    pub is_dir: bool,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<FileRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<FileTimestamp>,
    /// Windows 原生文件属性。Linux 文件权限不能解释为 DOS 属性。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScanError {
    pub path: String,
    pub kind: String,
    pub message: String,
}
impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.path, self.message, self.kind)
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanProgress {
    pub visited: u64,
    pub indexed: u64,
    pub errors: u64,
    pub skipped_links: u64,
    pub current_path: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanReport {
    pub entries: Vec<FileEntry>,
    pub errors: Vec<ScanError>,
    pub skipped_links: u64,
    pub skipped_non_utf8: u64,
    pub skipped_mounts: u64,
    pub skipped_excluded: u64,
    pub skipped_special: u64,
    pub cancelled: bool,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_checkpoint: Option<NativeCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_warning: Option<String>,
}

/// 与完整快照一起提交的卷身份、日志标识和续读游标。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeCheckpoint {
    pub volume_id: String,
    pub journal_id: u64,
    pub cursor: i64,
}
impl ScanReport {
    fn error(&mut self, path: &Path, error: PlatformError) {
        let kind = match &error {
            PlatformError::Io { source, .. } => format!("{:?}", source.kind()),
            PlatformError::InvalidPath(_) => "local_boundary".into(),
            _ => "scan_error".into(),
        };
        self.errors.push(ScanError {
            path: path.to_string_lossy().into_owned(),
            kind,
            message: error.to_string(),
        });
        self.complete = false;
    }
}
/// Metadata-only traversal of explicitly configured filename scopes.
pub fn scan(
    settings: &Settings,
    cancel: &AtomicBool,
    progress: impl FnMut(ScanProgress),
) -> Result<ScanReport> {
    scan_with_exclusions(settings, cancel, progress, &[])
}
/// Hard exclusions take precedence over every root and smart-exclusion policy.
/// Callers should pass the application data directory to prevent self-indexing.
pub fn scan_with_exclusions(
    settings: &Settings,
    cancel: &AtomicBool,
    progress: impl FnMut(ScanProgress),
    excluded_paths: &[PathBuf],
) -> Result<ScanReport> {
    scan_inner(settings, cancel, progress, excluded_paths, None)
}

/// 用与标准扫描一致的范围、类型和本地边界核对一条通知路径。
/// 根目录不可用、路径存在链接或读取失败时返回错误，由调用方保留旧代次并校准。
pub fn inspect_scoped_entry(
    settings: &Settings,
    path: &Path,
    excluded_paths: &[PathBuf],
) -> Result<Option<FileEntry>> {
    let settings = settings.normalized()?;
    let path = normalize_path(path)?;
    let Some(scope) = scope_for(&settings.roots, &path) else {
        return Ok(None);
    };
    for excluded in excluded_paths {
        if is_within(&path, &normalize_path(excluded)?) {
            return Ok(None);
        }
    }
    if is_smart_excluded(scope, &path) {
        return Ok(None);
    }
    implementation::inspect(scope, &path)
}

/// 有界批量遍历。结果在回调中交付，返回报告只携带覆盖和错误状态。
/// 中断时交付已完成的最后一批，调用方决定能否将部分结果作为初次索引发布。
pub fn scan_with_batches(
    settings: &Settings,
    cancel: &AtomicBool,
    progress: impl FnMut(ScanProgress),
    excluded_paths: &[PathBuf],
    batch_size: usize,
    mut on_batch: impl FnMut(&[FileEntry]) -> Result<()>,
) -> Result<ScanReport> {
    if batch_size == 0 {
        return Err(PlatformError::InvalidSettings(
            "扫描批次大小必须大于零".into(),
        ));
    }
    scan_inner(
        settings,
        cancel,
        progress,
        excluded_paths,
        Some(BatchOutput {
            size: batch_size,
            delivered: 0,
            callback: &mut on_batch,
        }),
    )
}

type BatchCallback<'a> = dyn FnMut(&[FileEntry]) -> Result<()> + 'a;

struct BatchOutput<'a> {
    size: usize,
    delivered: u64,
    callback: &'a mut BatchCallback<'a>,
}

impl BatchOutput<'_> {
    fn flush(&mut self, entries: &mut Vec<FileEntry>) -> Result<()> {
        if !entries.is_empty() {
            (self.callback)(entries)?;
            self.delivered += entries.len() as u64;
            entries.clear();
        }
        Ok(())
    }
}

fn scan_inner(
    settings: &Settings,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ScanProgress),
    excluded_paths: &[PathBuf],
    mut output: Option<BatchOutput<'_>>,
) -> Result<ScanReport> {
    let settings = settings.normalized()?;
    if settings.mode == SpeedMode::FastService
        || (!cfg!(windows) && settings.mode == SpeedMode::OnDemandElevated)
    {
        return Err(PlatformError::UnsupportedMode(settings.mode.to_string()));
    }
    let exclusions: Vec<_> = excluded_paths
        .iter()
        .map(|p| normalize_path(p))
        .collect::<Result<_>>()?;
    let mut report = ScanReport {
        complete: true,
        ..ScanReport::default()
    };
    let mut state = ScanProgress::default();
    if cancel.load(Ordering::Acquire) {
        report.cancelled = true;
        report.complete = false;
        progress(state);
        return Ok(report);
    }
    match settings.mode {
        SpeedMode::Standard => implementation::walk(
            &settings.roots,
            &exclusions,
            cancel,
            &mut report,
            &mut state,
            &mut progress,
            &mut output,
        )?,
        #[cfg(windows)]
        SpeedMode::OnDemandElevated => implementation::walk_native(
            &settings,
            &exclusions,
            cancel,
            &mut report,
            &mut state,
            &mut progress,
            &mut output,
        )?,
        _ => return Err(PlatformError::UnsupportedMode(settings.mode.to_string())),
    }
    if let Some(batch) = &mut output {
        batch.flush(&mut report.entries)?;
    }
    if cancel.load(Ordering::Acquire) {
        report.cancelled = true;
    }
    report.complete &= !report.cancelled && report.errors.is_empty();
    if !report.complete {
        report.native_checkpoint = None;
    }
    report.entries.sort_by(|a, b| a.path.cmp(&b.path));
    state.indexed = indexed_count(&report, &output);
    state.errors = report.errors.len() as u64;
    state.skipped_links = report.skipped_links;
    state.current_path = None;
    progress(state);
    Ok(report)
}
fn excluded(path: &Path, hard: &[PathBuf]) -> bool {
    hard.iter().any(|p| is_within(path, p))
}
fn nested_selected(path: &Path, current_root: &Path, scopes: &[Scope]) -> bool {
    scopes.iter().any(|s| {
        !same_path(Path::new(&s.path), current_root) && same_path(Path::new(&s.path), path)
    })
}
fn notify(
    path: &Path,
    state: &mut ScanProgress,
    report: &mut ScanReport,
    progress: &mut impl FnMut(ScanProgress),
    output: &mut Option<BatchOutput<'_>>,
) -> Result<()> {
    if let Some(batch) = output
        .as_mut()
        .filter(|batch| report.entries.len() >= batch.size)
    {
        batch.flush(&mut report.entries)?;
    }
    state.visited += 1;
    state.indexed = indexed_count(report, output);
    state.errors = report.errors.len() as u64;
    state.skipped_links = report.skipped_links;
    state.current_path = path.to_str().map(str::to_owned);
    progress(state.clone());
    Ok(())
}

fn indexed_count(report: &ScanReport, output: &Option<BatchOutput<'_>>) -> u64 {
    report.entries.len() as u64 + output.as_ref().map_or(0, |batch| batch.delivered)
}

#[cfg(target_os = "linux")]
mod implementation {
    use super::*;
    use crate::local::imp::{self, Child, DirStream, LocalNode};
    use crate::volume::mount_info;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    struct Frame {
        node: LocalNode,
        path: PathBuf,
        stream: DirStream,
    }

    pub(super) fn inspect(scope: &Scope, path: &Path) -> Result<Option<FileEntry>> {
        let root = Path::new(&scope.path);
        let mut node = imp::resolve(root, false)?.expect("缺失路径必须返回错误");
        if !node.is_dir() {
            return Err(PlatformError::InvalidPath("收录根必须是目录".into()));
        }
        let root_mount = node.mount_id;
        let root_identity = crate::revision::imp::from_stat(&node.stat).identity;
        let relative = path.strip_prefix(root).expect("已检查范围包含关系");
        let mounts = mount_info()?;
        let mut current = root.to_owned();
        let verify_root = || {
            if FileRevision::at_path(root)?.identity != root_identity {
                return Err(PlatformError::InvalidPath(
                    "收录根目录在核对时发生变化".into(),
                ));
            }
            Ok(())
        };
        for part in relative {
            current.push(part);
            let name = CString::new(part.as_bytes())
                .map_err(|_| PlatformError::InvalidPath("文件名含 NUL".into()))?;
            node = match imp::open_child(&node, &name, &current, Some(root_mount), &mounts) {
                Ok(Child::OtherMount) => return Ok(None),
                Ok(Child::Node(child)) => child,
                Err(PlatformError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    verify_root()?;
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            if node.is_link() {
                return Err(PlatformError::InvalidPath(format!(
                    "不允许使用符号链接：{}",
                    current.display()
                )));
            }
        }
        verify_root()?;
        Ok(
            ((node.is_file() || node.is_dir()) && scope.accepts(path, node.is_dir()))
                .then(|| entry(path, &node, &scope.path)),
        )
    }
    pub fn walk(
        scopes: &[Scope],
        hard: &[PathBuf],
        cancel: &AtomicBool,
        report: &mut ScanReport,
        state: &mut ScanProgress,
        progress: &mut impl FnMut(ScanProgress),
        output: &mut Option<BatchOutput<'_>>,
    ) -> Result<()> {
        if scopes.is_empty() {
            return Ok(());
        }
        let mounts = mount_info()?;
        for scope in scopes {
            if cancel.load(Ordering::Acquire) {
                report.cancelled = true;
                break;
            }
            let root = PathBuf::from(&scope.path);
            if excluded(&root, hard) {
                report.skipped_excluded += 1;
                continue;
            }
            let node = match imp::resolve(&root, false) {
                Ok(Some(n)) => n,
                Ok(None) => unreachable!(),
                Err(e) => {
                    report.error(&root, e);
                    continue;
                }
            };
            if !node.is_dir() {
                report.error(
                    &root,
                    PlatformError::InvalidPath("Selected root is not a directory".into()),
                );
                continue;
            }
            let root_mount = node.mount_id;
            report.entries.push(entry(&root, &node, &scope.path));
            notify(&root, state, report, progress, output)?;
            if cancel.load(Ordering::Acquire) {
                report.cancelled = true;
                break;
            }
            let stream = match DirStream::new(&node, &root) {
                Ok(s) => s,
                Err(e) => {
                    report.error(&root, e);
                    continue;
                }
            };
            let mut stack = vec![Frame {
                node,
                path: root.clone(),
                stream,
            }];
            while !stack.is_empty() {
                if cancel.load(Ordering::Acquire) {
                    report.cancelled = true;
                    break;
                }
                let frame = stack.last_mut().unwrap();
                let name = match frame.stream.next() {
                    None => {
                        stack.pop();
                        continue;
                    }
                    Some(Err(e)) => {
                        report.error(&frame.path, crate::local::io_error(&frame.path, e));
                        stack.pop();
                        continue;
                    }
                    Some(Ok(name)) => name,
                };
                let Some(text) = name.to_str() else {
                    report.skipped_non_utf8 += 1;
                    report.complete = false;
                    let hex: String = name.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
                    report.errors.push(ScanError {
                        path: format!("{}/<non-utf8:{hex}>", frame.path.display()),
                        kind: "non_utf8".into(),
                        message: "Filename is not valid UTF-8; skipped without lossy conversion"
                            .into(),
                    });
                    continue;
                };
                let path = frame.path.join(text);
                if excluded(&path, hard) {
                    report.skipped_excluded += 1;
                    continue;
                }
                if nested_selected(&path, &root, scopes) {
                    continue;
                }
                let policy = scope_for(scopes, &path).unwrap_or(scope);
                if is_smart_excluded(policy, &path) {
                    report.skipped_excluded += 1;
                    continue;
                }
                let name = CString::new(name.as_bytes())
                    .map_err(|_| PlatformError::InvalidPath("NUL in filename".into()))?;
                let child =
                    match imp::open_child(&frame.node, &name, &path, Some(root_mount), &mounts) {
                        Ok(Child::OtherMount) => {
                            report.skipped_mounts += 1;
                            continue;
                        }
                        Ok(Child::Node(n)) => n,
                        Err(e) => {
                            report.error(&path, e);
                            notify(&path, state, report, progress, output)?;
                            continue;
                        }
                    };
                if child.is_link() {
                    report.skipped_links += 1;
                    notify(&path, state, report, progress, output)?;
                    continue;
                }
                if !child.is_dir() && !child.is_file() {
                    report.skipped_special += 1;
                    notify(&path, state, report, progress, output)?;
                    continue;
                }
                if policy.accepts(&path, child.is_dir()) {
                    report.entries.push(entry(&path, &child, &policy.path));
                }
                notify(&path, state, report, progress, output)?;
                if child.is_dir() {
                    if cancel.load(Ordering::Acquire) {
                        report.cancelled = true;
                        break;
                    }
                    match DirStream::new(&child, &path) {
                        Ok(stream) => stack.push(Frame {
                            node: child,
                            path,
                            stream,
                        }),
                        Err(e) => report.error(&path, e),
                    }
                }
            }
            if report.cancelled {
                break;
            }
        }
        Ok(())
    }
    fn entry(path: &Path, node: &LocalNode, _source: &str) -> FileEntry {
        FileEntry {
            path: path.to_str().expect("validated UTF-8").into(),
            size: if node.is_file() {
                node.stat.st_size.max(0) as u64
            } else {
                0
            },
            modified: node.stat.st_mtime,
            is_dir: node.is_dir(),
            source: "local".into(),
            revision: Some(crate::revision::imp::from_stat(&node.stat)),
            created: None,
            attributes: None,
        }
    }
}

#[cfg(windows)]
mod implementation {
    use super::*;
    use crate::local::imp::{file_info, guard, Guard};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    struct Frame {
        _guard: Guard,
        path: PathBuf,
        stream: std::fs::ReadDir,
    }

    pub(super) fn inspect(scope: &Scope, path: &Path) -> Result<Option<FileEntry>> {
        let root = Path::new(&scope.path);
        let root_guard = guard(root, false)?;
        let root_info = file_info(root_guard.handles.last().expect("已验证根目录"), root)?;
        if root_info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(PlatformError::InvalidPath("收录根必须是目录".into()));
        }
        let held = match guard(path, false) {
            Ok(guard) => guard,
            Err(PlatformError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        let info = file_info(held.handles.last().expect("已验证文件"), path)?;
        if info.dwVolumeSerialNumber != root_info.dwVolumeSerialNumber {
            return Ok(None);
        }
        Ok(scope
            .accepts(path, info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0)
            .then(|| entry(path, &info, &held)))
    }

    pub(super) fn walk_native(
        settings: &Settings,
        hard: &[PathBuf],
        cancel: &AtomicBool,
        report: &mut ScanReport,
        state: &mut ScanProgress,
        progress: &mut impl FnMut(ScanProgress),
        output: &mut Option<BatchOutput<'_>>,
    ) -> Result<()> {
        let [scope] = settings.roots.as_slice() else {
            return Err(PlatformError::UnsupportedMode(
                "极速模式每次只接受一个显式授权的完整卷".into(),
            ));
        };
        let root = Path::new(&scope.path);
        if root.components().count() != 2 {
            return Err(PlatformError::UnsupportedMode(
                "极速模式要求完整盘符根，目录范围请使用标准模式".into(),
            ));
        }
        if hard
            .iter()
            .any(|path| is_within(path, root) || is_within(root, path))
        {
            return Err(PlatformError::UnsupportedMode(
                "极速模式的数据目录与硬排除目录必须在所选卷以外".into(),
            ));
        }
        let opened = crate::NativeVolume::open(settings, root).and_then(|mut volume| {
            // 不同盘符也可能指向同一个文件系统对象。输出目录必须在实际卷外。
            for excluded in hard {
                let drive: PathBuf = excluded.components().take(2).collect();
                if FileRevision::at_path(&drive)?.identity.volume == volume.volume_id() {
                    return Err(PlatformError::UnsupportedMode(
                        "极速模式的数据目录必须在另一个本地卷上".into(),
                    ));
                }
            }
            let start = volume.journal_status()?;
            let page = volume.enumerate_page(0)?;
            Ok((volume, start, page))
        });
        let (mut volume, start, mut page) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                // 目录扫描的授权范围与设置一致。只在 ReFS 设备明确报告无原生支持时回落。
                if matches!(error, PlatformError::Io { ref source, .. } if native_unavailable(source))
                    && crate::volume::windows_volume(scope.path.clone())
                        .filesystem
                        .eq_ignore_ascii_case("ReFS")
                {
                    report.backend_warning =
                        Some(format!("ReFS 原生日志不可用，已使用目录扫描：{error}"));
                    let mut fallback_exclusions = hard.to_vec();
                    fallback_exclusions.push(root.join("System Volume Information"));
                    return walk(
                        &settings.roots,
                        &fallback_exclusions,
                        cancel,
                        report,
                        state,
                        progress,
                        output,
                    );
                }
                return Err(error);
            }
        };
        if let Some(entry) = inspect(scope, root)? {
            report.entries.push(entry);
            notify(root, state, report, progress, output)?;
        }
        while let Some(batch) = page {
            for record in batch.records {
                if cancel.load(Ordering::Acquire) {
                    report.cancelled = true;
                    return Ok(());
                }
                if record.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    report.skipped_links += 1;
                    continue;
                }
                let paths = match volume.record_paths(&record) {
                    Ok(paths) => paths,
                    Err(error) => {
                        report.error(root, error);
                        return Ok(());
                    }
                };
                if paths.is_empty() {
                    report.skipped_excluded += 1;
                }
                for path in paths {
                    if same_path(&path, root) {
                        continue;
                    }
                    match inspect(scope, &path) {
                        Ok(Some(entry))
                            if entry.revision.is_some_and(|revision| {
                                revision.identity.object == record.object
                            }) =>
                        {
                            report.entries.push(entry);
                        }
                        Err(PlatformError::InvalidPath(message))
                            if message.starts_with("Reparse point refused") =>
                        {
                            report.skipped_links += 1;
                        }
                        Ok(_) => {
                            report.error(
                                &path,
                                PlatformError::InvalidPath(
                                    "原生目录记录与当前文件对象不一致".into(),
                                ),
                            );
                            return Ok(());
                        }
                        Err(error) => {
                            report.error(&path, error);
                            return Ok(());
                        }
                    }
                    notify(&path, state, report, progress, output)?;
                }
            }
            page = match volume.enumerate_page(batch.next) {
                Ok(page) => page,
                Err(error) => {
                    report.error(root, error);
                    return Ok(());
                }
            };
        }
        let finish = match volume.journal_status() {
            Ok(status) => status,
            Err(error) => {
                report.error(root, error);
                return Ok(());
            }
        };
        if start != finish {
            report.error(
                root,
                PlatformError::InvalidPath("原生日志在扫描期间发生变化，需要重新校准".into()),
            );
            return Ok(());
        }
        report.native_checkpoint = Some(NativeCheckpoint {
            volume_id: volume.device_id().to_owned(),
            journal_id: finish.id,
            cursor: finish.next,
        });
        Ok(())
    }

    fn native_unavailable(error: &std::io::Error) -> bool {
        use windows_sys::Win32::Foundation::{
            ERROR_INVALID_FUNCTION, ERROR_JOURNAL_NOT_ACTIVE, ERROR_NOT_SUPPORTED,
        };
        error.raw_os_error().is_some_and(|code| {
            matches!(
                code as u32,
                ERROR_INVALID_FUNCTION | ERROR_JOURNAL_NOT_ACTIVE | ERROR_NOT_SUPPORTED
            )
        })
    }
    pub fn walk(
        scopes: &[Scope],
        hard: &[PathBuf],
        cancel: &AtomicBool,
        report: &mut ScanReport,
        state: &mut ScanProgress,
        progress: &mut impl FnMut(ScanProgress),
        output: &mut Option<BatchOutput<'_>>,
    ) -> Result<()> {
        for scope in scopes {
            if cancel.load(Ordering::Acquire) {
                report.cancelled = true;
                break;
            }
            let root = PathBuf::from(&scope.path);
            if excluded(&root, hard) {
                report.skipped_excluded += 1;
                continue;
            }
            let held = match guard(&root, false) {
                Ok(g) => g,
                Err(e) => {
                    report.error(&root, e);
                    continue;
                }
            };
            let info = match held.handles.last().map(|f| file_info(f, &root)) {
                Some(Ok(i)) => i,
                Some(Err(e)) => {
                    report.error(&root, e);
                    continue;
                }
                None => {
                    report.error(
                        &root,
                        PlatformError::InvalidPath("Root handle unavailable".into()),
                    );
                    continue;
                }
            };
            if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
                report.error(
                    &root,
                    PlatformError::InvalidPath("Selected root must be a directory".into()),
                );
                continue;
            }
            let serial = info.dwVolumeSerialNumber;
            report.entries.push(entry(&root, &info, &held));
            notify(&root, state, report, progress, output)?;
            if cancel.load(Ordering::Acquire) {
                report.cancelled = true;
                break;
            }
            let stream = match std::fs::read_dir(&root) {
                Ok(s) => s,
                Err(e) => {
                    report.error(&root, crate::local::io_error(&root, e));
                    continue;
                }
            };
            let mut stack = vec![Frame {
                _guard: held,
                path: root.clone(),
                stream,
            }];
            while !stack.is_empty() {
                if cancel.load(Ordering::Acquire) {
                    report.cancelled = true;
                    break;
                }
                let frame = stack.last_mut().unwrap();
                let item = match frame.stream.next() {
                    None => {
                        stack.pop();
                        continue;
                    }
                    Some(Err(e)) => {
                        report.error(&frame.path, crate::local::io_error(&frame.path, e));
                        stack.pop();
                        continue;
                    }
                    Some(Ok(v)) => v,
                };
                let name = item.file_name();
                if name.to_str().is_none() {
                    report.skipped_non_utf8 += 1;
                    report.complete = false;
                    report.errors.push(ScanError {
                        path: format!("{} / {:?}", frame.path.display(), name),
                        kind: "non_utf8".into(),
                        message: "Invalid UTF-16 filename skipped explicitly".into(),
                    });
                    continue;
                }
                let path = frame.path.join(name);
                if excluded(&path, hard) {
                    report.skipped_excluded += 1;
                    continue;
                }
                if nested_selected(&path, &root, scopes) {
                    continue;
                }
                let policy = scope_for(scopes, &path).unwrap_or(scope);
                if is_smart_excluded(policy, &path) {
                    report.skipped_excluded += 1;
                    continue;
                }
                // Parent chain is held without FILE_SHARE_WRITE/DELETE, so it
                // cannot be exchanged or converted to a reparse point during I/O.
                let child = match guard(&path, false) {
                    Ok(g) => g,
                    Err(PlatformError::InvalidPath(message))
                        if message.starts_with("Reparse point refused") =>
                    {
                        report.skipped_links += 1;
                        continue;
                    }
                    Err(e) => {
                        report.error(&path, e);
                        continue;
                    }
                };
                let info = match child.handles.last().map(|f| file_info(f, &path)) {
                    Some(Ok(v)) => v,
                    Some(Err(e)) => {
                        report.error(&path, e);
                        continue;
                    }
                    None => {
                        report.error(
                            &path,
                            PlatformError::InvalidPath("Child handle unavailable".into()),
                        );
                        continue;
                    }
                };
                if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    report.skipped_links += 1;
                    continue;
                }
                if info.dwVolumeSerialNumber != serial {
                    report.skipped_mounts += 1;
                    continue;
                }
                let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                if policy.accepts(&path, is_dir) {
                    report.entries.push(entry(&path, &info, &child));
                }
                notify(&path, state, report, progress, output)?;
                if is_dir {
                    if cancel.load(Ordering::Acquire) {
                        report.cancelled = true;
                        break;
                    }
                    match std::fs::read_dir(&path) {
                        Ok(stream) => stack.push(Frame {
                            _guard: child,
                            path,
                            stream,
                        }),
                        Err(e) => report.error(&path, crate::local::io_error(&path, e)),
                    }
                }
            }
            if report.cancelled {
                break;
            }
        }
        Ok(())
    }
    fn entry(
        path: &Path,
        info: &windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
        held: &Guard,
    ) -> FileEntry {
        let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let ticks = ((info.ftLastWriteTime.dwHighDateTime as u64) << 32)
            | info.ftLastWriteTime.dwLowDateTime as u64;
        let created = (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(info.ftCreationTime.dwLowDateTime);
        FileEntry {
            path: path.to_str().expect("validated UTF-8").into(),
            size: if is_dir {
                0
            } else {
                ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64
            },
            modified: (ticks / 10_000_000) as i64 - 11_644_473_600,
            is_dir,
            source: "local".into(),
            revision: held
                .handles
                .last()
                .and_then(|file| crate::revision::imp::from_info(file, path, info).ok()),
            created: (created != 0).then(|| crate::revision::imp::time(created as i64)),
            attributes: Some(info.dwFileAttributes),
        }
    }
}
#[cfg(not(any(target_os = "linux", windows)))]
mod implementation {
    use super::*;
    pub(super) fn inspect(_: &Scope, _: &Path) -> Result<Option<FileEntry>> {
        Err(PlatformError::InvalidPath(
            "当前平台不支持本地文件元数据核对".into(),
        ))
    }
    pub fn walk(
        _: &[Scope],
        _: &[PathBuf],
        _: &AtomicBool,
        _: &mut ScanReport,
        _: &mut ScanProgress,
        _: &mut impl FnMut(ScanProgress),
        _: &mut Option<BatchOutput<'_>>,
    ) -> Result<()> {
        Err(PlatformError::InvalidPath(
            "Safe local traversal is implemented only on Linux and Windows".into(),
        ))
    }
}
