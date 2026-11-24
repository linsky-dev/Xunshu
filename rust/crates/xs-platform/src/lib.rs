//! Explicit local scopes, strict settings, and cancellable metadata-only scanning.
//!
//! Public contract: `Settings::{load,save}`, `Scope::new`, `scan`, `volumes`,
//! `suggested_roots`. `ScanReport.complete` is the only permission to replace an
//! existing index; cancellation or ANY I/O error makes the report incomplete.
//! `OnDemandElevated` 在 Windows 上支持显式授权的整个本地卷。

#[cfg(windows)]
mod journal;
mod revision;
mod scanner;
mod settings;
mod volume;
mod watcher;

#[cfg(windows)]
pub use journal::{
    read_scoped_journal_changes, JournalRead, JournalReset, JournalStatus, NativeJournalChanges,
    NativePage, NativeVolume, UsnRecord,
};
pub use revision::{FileIdentity, FileRevision, FileTimestamp};
pub use scanner::{
    inspect_scoped_entry, scan, scan_with_batches, scan_with_exclusions, FileEntry,
    NativeCheckpoint, ScanError, ScanProgress, ScanReport,
};
pub use settings::{Scope, Settings, SetupState, SpeedMode, SETTINGS_VERSION};
pub use volume::{suggested_roots, volumes, VolumeCapability};
pub use watcher::{ScopeWatcher, WatchEvent};

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Invalid settings: {0}")]
    InvalidSettings(String),
    #[error("Invalid local path: {0}")]
    InvalidPath(String),
    #[error("JSON settings error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("当前条件下无法使用此扫描方式：{0}")]
    UnsupportedMode(String),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

pub mod apps;
mod local;
/// Validates an absolute, explicit local path without following user-supplied links.
/// `allow_missing` accepts only a missing suffix after all existing ancestors pass.
pub use local::{
    atomic_write_local, create_local_dir_all, create_local_dir_new, create_local_new,
    open_local_read, open_or_create_local_lock, publish_local_dir_new, remove_local_file,
    validate_local_path, write_local_new, write_local_replace, LocalWriteOutcome,
};

/// 仅比较规范化的本地路径组成部分；不会访问文件系统或跟随链接。
pub fn local_path_within(path: &std::path::Path, root: &std::path::Path) -> Result<bool> {
    Ok(settings::is_within(
        &local::normalize_path(path)?,
        &local::normalize_path(root)?,
    ))
}
