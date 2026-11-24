//! Application operations shared by CLI and native UI.
use crate::persistence::{PublicationOutcome, Store};
use crate::{FileProperties, FileRecord, Index, IndexBuilder, JournalCheckpoint};
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use std::{
    borrow::Cow,
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use xs_platform::{FileEntry, FileRevision, LocalWriteOutcome, ScanProgress, ScanReport, Settings};

pub fn default_data_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("XunshuRustV2"))
            .context("LOCALAPPDATA is unavailable; specify --data-dir")
    }
    #[cfg(not(windows))]
    {
        if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
            let p = PathBuf::from(path);
            if p.is_absolute() {
                return Ok(p.join("xunshu-rust-v2"));
            }
        }
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .map(|p| p.join(".local/share/xunshu-rust-v2"))
            .context("HOME is unavailable; specify --data-dir")
    }
}
pub fn settings_path(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}
pub fn load_settings(dir: &Path) -> Result<Settings> {
    match Settings::load(settings_path(dir)) {
        Ok(s) => Ok(s),
        Err(xs_platform::PlatformError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(Settings::default())
        }
        Err(e) => Err(e.into()),
    }
}
pub fn save_settings(dir: &Path, settings: &Settings) -> Result<LocalWriteOutcome> {
    Store::reject_m1_directory(&dir.join("index"))?;
    xs_platform::create_local_dir_all(dir)?;
    Ok(settings.save(settings_path(dir))?)
}

/// Shared by resident and one-shot readers before and after a query. Empty
/// tokens are reserved for imported metadata and can never authorize local rows.
pub fn validate_index_scope(index: &Index, settings: &Settings) -> Result<()> {
    if index.scope_token().is_empty() {
        ensure!(
            !index.has_local_records(),
            "空范围标记的索引包含本地来源，索引无效。请重新扫描，或重新导入离线目录。"
        );
    } else {
        ensure!(
            index.scope_token() == settings.scope_token()?,
            "搜索范围已变更，旧索引暂不可用。请重新扫描，使结果与当前范围一致。"
        );
    }
    Ok(())
}
pub fn load_index(dir: &Path) -> Result<Index> {
    let settings = load_settings(dir)?;
    let index = Store::open(dir.join("index"))?.load()?;
    validate_index_scope(&index, &settings)?;
    Ok(index)
}

#[derive(Debug, Serialize)]
pub struct MigrationOutcome {
    pub destination: PathBuf,
    pub generation: u64,
    pub indexed: usize,
    pub offline_entries: usize,
    pub durable: bool,
    pub durability_warnings: Vec<String>,
}

/// 只读复制 M1 设置和已发布索引。全量验证后以不可覆盖的同父目录改名发布。
pub fn migrate_m1(source: &Path, destination: &Path) -> Result<MigrationOutcome> {
    xs_platform::validate_local_path(source, false)?;
    match xs_platform::validate_local_path(destination, false) {
        Ok(()) => bail!("迁移目标已存在，请使用全新目录"),
        Err(xs_platform::PlatformError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = destination.parent().context("迁移目标须有已存在的父目录")?;
    xs_platform::validate_local_path(parent, false)?;
    ensure!(
        !migration_path_key(destination).starts_with(migration_path_key(source)),
        "迁移目标须位于 M1 源目录之外"
    );

    let (original, index) = Store::read_m1(&source.join("index"))?;
    let settings = load_settings(source)?;
    validate_index_scope(&index, &settings)?;
    let token = settings.scope_token()?;
    let mut builder = IndexBuilder::new(index.scope_token().to_owned());
    let mut offline_entries = 0;
    for id in 0..index.len() as u32 {
        offline_entries += usize::from(index.record(id)?.source != "local");
        builder.add_from_index(&index, id)?;
    }
    let migrated = builder.finish(index.generation())?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staged = parent.join(format!(".xunshu-migrate-{}-{nonce}", std::process::id()));
    xs_platform::create_local_dir_new(&staged)?;
    let finalize = || -> Result<MigrationOutcome> {
        let settings_write = save_settings(&staged, &settings)?;
        let staged_store = Store::open(staged.join("index"))?;
        let index_write = staged_store.publish(&migrated)?;
        validate_index_scope(&staged_store.load()?, &load_settings(&staged)?)?;
        ensure!(
            original.current_generation()? == Some(index.generation())
                && load_settings(source)?.scope_token()? == token,
            "迁移期间 M1 的索引或授权范围发生变化，请重试"
        );
        let rename = xs_platform::publish_local_dir_new(&staged, destination)?;
        let durable = settings_write.is_durable() && index_write.durable && rename.is_durable();
        let durability_warnings = settings_write
            .warning()
            .map(ToOwned::to_owned)
            .into_iter()
            .chain(index_write.warning)
            .chain(rename.warning().map(ToOwned::to_owned))
            .collect();
        Ok(MigrationOutcome {
            destination: destination.to_path_buf(),
            generation: migrated.generation(),
            indexed: migrated.len(),
            offline_entries,
            durable,
            durability_warnings,
        })
    };
    finalize().with_context(|| format!("迁移未完成，暂存目录为 {}", staged.display()))
}

fn migration_path_key(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().replace('/', "\\").to_lowercase())
    } else {
        path.to_path_buf()
    }
}

#[derive(Serialize)]
pub struct RebuildOutcome {
    pub published: bool,
    pub generation: Option<u64>,
    pub durable: bool,
    pub durability_warning: Option<String>,
    pub indexed: usize,
    pub elapsed_ms: f64,
    pub report: ScanReport,
}
fn unpublished(start: Instant, indexed: usize, report: ScanReport) -> RebuildOutcome {
    RebuildOutcome {
        published: false,
        generation: None,
        durable: false,
        durability_warning: None,
        indexed,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        report,
    }
}

pub fn rebuild(
    dir: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(ScanProgress),
) -> Result<RebuildOutcome> {
    rebuild_inner(dir, None, cancel, progress)
}

/// 监听器只校准已经订阅的范围。打开索引前检查设置，发布前再次校验。
pub fn rebuild_if_scope(
    dir: &Path,
    expected_scope: &str,
    cancel: &AtomicBool,
    progress: impl FnMut(ScanProgress),
) -> Result<RebuildOutcome> {
    rebuild_inner(dir, Some(expected_scope), cancel, progress)
}

fn rebuild_inner(
    dir: &Path,
    expected_scope: Option<&str>,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ScanProgress),
) -> Result<RebuildOutcome> {
    let start = Instant::now();
    let settings = load_settings(dir)?;
    let token = settings.scope_token()?;
    ensure!(
        expected_scope.is_none_or(|expected| expected == token),
        "监听范围已变更，请重新订阅后校准。"
    );
    let store = Store::open(dir.join("index"))?;
    let previous = if store.has_snapshot()? {
        Some(store.load()?)
    } else {
        None
    };
    if previous.is_none() {
        return rebuild_initial::<SCAN_BATCH, INITIAL_PUBLISH>(
            dir, cancel, progress, &settings, &token, &store, start,
        );
    }
    let generation = previous.as_ref().map_or(Ok(1), |i| {
        i.generation()
            .checked_add(1)
            .context("generation exhausted")
    })?;
    let mut builder = IndexBuilder::new(token.clone());
    let mut build_error = None;
    let scanned = xs_platform::scan_with_batches(
        &settings,
        cancel,
        &mut progress,
        &[dir.to_path_buf()],
        SCAN_BATCH,
        |entries| capture_batch_error(add_entries(&mut builder, entries), &mut build_error),
    );
    if let Some(error) = build_error {
        return Err(error);
    }
    let mut report = scanned?;
    let indexed = builder.len();
    if !report.complete || report.cancelled || cancel.load(Ordering::Relaxed) {
        return Ok(unpublished(start, indexed, report));
    }
    if let Some(previous) = previous {
        for id in 0..previous.len() as u32 {
            if id % 256 == 0 && cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                report.complete = false;
                return Ok(unpublished(start, indexed, report));
            }
            if previous.record(id)?.source != "local" {
                builder.add_from_index(&previous, id)?;
            }
        }
    }
    set_native_checkpoint(&mut builder, &report);
    let index = builder.finish(generation)?;
    ensure!(
        load_settings(dir)?.scope_token()? == token,
        "扫描期间范围已更改，本次结果未发布，请重新扫描。"
    );
    if cancel.load(Ordering::Relaxed) {
        report.cancelled = true;
        report.complete = false;
        return Ok(unpublished(start, indexed, report));
    }
    let publication = store.publish(&index)?;
    Ok(RebuildOutcome {
        published: true,
        generation: Some(generation),
        durable: publication.durable,
        durability_warning: publication.warning,
        indexed,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        report,
    })
}

const SCAN_BATCH: usize = 4096;
const INITIAL_PUBLISH: usize = 16_384;

fn set_native_checkpoint(builder: &mut IndexBuilder, report: &ScanReport) {
    builder.set_journal_checkpoint(report.native_checkpoint.as_ref().map(|checkpoint| {
        JournalCheckpoint {
            volume_id: checkpoint.volume_id.clone(),
            journal_id: checkpoint.journal_id,
            cursor: checkpoint.cursor,
        }
    }));
}

#[derive(Serialize)]
#[serde(tag = "result", content = "publication", rename_all = "snake_case")]
pub enum ReconcileOutcome {
    NoChange,
    Published(PublicationOutcome),
    FullScanRequired,
}

/// 监听路径仅为提示。以当前授权和重新打开的文件身份决定增删，
/// 无法确定目录覆盖、代次或权限时保留原索引并要求校准。
pub fn reconcile_changed_paths(
    dir: &Path,
    expected_scope_token: &str,
    paths: &[PathBuf],
    cancel: &AtomicBool,
) -> Result<ReconcileOutcome> {
    reconcile(
        dir,
        expected_scope_token,
        ReconcileSource::Watcher(paths),
        cancel,
    )
}

#[cfg(windows)]
/// 从当前已发布游标增量读取 Windows 卷日志。不能安全解释的事件要求完整校准。
pub fn reconcile_native_journal(
    dir: &Path,
    expected_scope_token: &str,
    cancel: &AtomicBool,
) -> Result<ReconcileOutcome> {
    reconcile(
        dir,
        expected_scope_token,
        ReconcileSource::NativeJournal,
        cancel,
    )
}

enum ReconcileSource<'a> {
    Watcher(&'a [PathBuf]),
    #[cfg(windows)]
    NativeJournal,
}

fn reconcile(
    dir: &Path,
    expected_scope_token: &str,
    source: ReconcileSource<'_>,
    cancel: &AtomicBool,
) -> Result<ReconcileOutcome> {
    if cancel.load(Ordering::Acquire)
        || matches!(source, ReconcileSource::Watcher(paths) if paths.is_empty())
    {
        return Ok(ReconcileOutcome::NoChange);
    }
    let settings = load_settings(dir)?;
    let token = settings.scope_token()?;
    if token != expected_scope_token {
        return Ok(ReconcileOutcome::FullScanRequired);
    }
    let store = Store::open(dir.join("index"))?;
    if !store.has_snapshot()? {
        return Ok(ReconcileOutcome::FullScanRequired);
    }
    let current = store.load()?;
    if validate_index_scope(&current, &settings).is_err() || !current.coverage_complete() {
        return Ok(ReconcileOutcome::FullScanRequired);
    }
    let (paths, checkpoint): (Cow<'_, [PathBuf]>, Option<JournalCheckpoint>) = match source {
        ReconcileSource::Watcher(paths) => {
            if current.journal_checkpoint().is_some() {
                return Ok(ReconcileOutcome::FullScanRequired);
            }
            (Cow::Borrowed(paths), None)
        }
        #[cfg(windows)]
        ReconcileSource::NativeJournal => {
            let Some(previous) = current.journal_checkpoint() else {
                return Ok(ReconcileOutcome::FullScanRequired);
            };
            let previous = xs_platform::NativeCheckpoint {
                volume_id: previous.volume_id.clone(),
                journal_id: previous.journal_id,
                cursor: previous.cursor,
            };
            let changes = xs_platform::read_scoped_journal_changes(&settings, &previous)?;
            if changes.requires_rebuild {
                return Ok(ReconcileOutcome::FullScanRequired);
            }
            let checkpoint = JournalCheckpoint {
                volume_id: changes.checkpoint.volume_id,
                journal_id: changes.checkpoint.journal_id,
                cursor: changes.checkpoint.cursor,
            };
            (Cow::Owned(changes.paths), Some(checkpoint))
        }
    };
    if paths.len() > 4096 {
        return Ok(ReconcileOutcome::FullScanRequired);
    }
    let start = Instant::now();
    let mut deleted = BTreeSet::new();
    let mut seen_paths = BTreeSet::new();
    let mut builder = IndexBuilder::new(token.clone());
    for path in paths.iter() {
        if cancel.load(Ordering::Acquire) {
            return Ok(ReconcileOutcome::NoChange);
        }
        // 大小写改名可能同时给出旧名、新名。同一 Windows 键的多个提示
        // 需要校准，防止按旧拼写生成重复路径。
        let key = if cfg!(windows) {
            path.to_string_lossy().replace('\\', "/").to_lowercase()
        } else {
            path.to_string_lossy().into_owned()
        };
        if !seen_paths.insert(key) {
            return Ok(ReconcileOutcome::FullScanRequired);
        }
        let Some(prior_ids) = current.find_local_path(path, start, cancel)? else {
            return Ok(ReconcileOutcome::FullScanRequired);
        };
        if prior_ids.len() > 1 {
            return Ok(ReconcileOutcome::FullScanRequired);
        }
        let entry = xs_platform::inspect_scoped_entry(&settings, path, &[dir.to_path_buf()])?;
        if entry.as_ref().is_some_and(|entry| entry.is_dir) {
            return Ok(ReconcileOutcome::FullScanRequired);
        }
        if let Some(&id) = prior_ids.first() {
            let record = current.record(id)?;
            if record.is_dir
                || (cfg!(windows)
                    && record.path.replace('\\', "/") != path.to_string_lossy().replace('\\', "/"))
            {
                return Ok(ReconcileOutcome::FullScanRequired);
            }
            if let Some(entry) = &entry {
                let properties = FileProperties {
                    created: entry.created,
                    attributes: entry.attributes,
                };
                if record.path == entry.path
                    && record.size == entry.size
                    && record.modified == entry.modified
                    && current.record_revision(id)? == entry.revision
                    && current.record_properties(id)? == properties
                {
                    continue;
                }
            }
            deleted.insert(id);
        }
        if let Some(entry) = entry {
            add_entries(&mut builder, std::slice::from_ref(&entry))?;
        }
    }
    if deleted.is_empty() && builder.is_empty() {
        // 不为自己的索引文件等范围外写入重复发布空代次，避免制造日志反馈循环。
        // 未移动的持久游标保守地覆盖下次仍需复核的事件。
        return Ok(ReconcileOutcome::NoChange);
    }
    if load_settings(dir)?.scope_token()? != token {
        return Ok(ReconcileOutcome::FullScanRequired);
    }
    if cancel.load(Ordering::Acquire) {
        return Ok(ReconcileOutcome::NoChange);
    }
    let generation = current.generation();
    builder.set_journal_checkpoint(checkpoint);
    let additions = builder.finish(generation.checked_add(1).context("generation exhausted")?)?;
    let publication = store.publish_delta(
        generation,
        &deleted.into_iter().collect::<Vec<_>>(),
        &additions,
    )?;
    Ok(ReconcileOutcome::Published(publication))
}

fn add_entries(builder: &mut IndexBuilder, entries: &[FileEntry]) -> Result<()> {
    for entry in entries {
        builder.add_record_with_properties(
            FileRecord {
                path: entry.path.clone(),
                size: entry.size,
                modified: entry.modified,
                is_dir: entry.is_dir,
                source: entry.source.clone(),
            },
            None,
            entry.revision,
            FileProperties {
                created: entry.created,
                attributes: entry.attributes,
            },
        )?;
    }
    Ok(())
}

fn capture_batch_error(
    result: Result<()>,
    slot: &mut Option<anyhow::Error>,
) -> xs_platform::Result<()> {
    if let Err(error) = result {
        *slot = Some(error);
        return Err(xs_platform::PlatformError::InvalidSettings(
            "索引批次未能写入".into(),
        ));
    }
    Ok(())
}

fn rebuild_initial<const BATCH: usize, const FIRST: usize>(
    dir: &Path,
    cancel: &AtomicBool,
    progress: impl FnMut(ScanProgress),
    settings: &Settings,
    token: &str,
    store: &Store,
    start: Instant,
) -> Result<RebuildOutcome> {
    let mut builder = IndexBuilder::new(token.to_owned());
    let mut latest = None;
    let mut published_entries = 0;
    let mut limit = FIRST;
    let mut build_error = None;
    let scanned = xs_platform::scan_with_batches(
        settings,
        cancel,
        progress,
        &[dir.to_path_buf()],
        BATCH,
        |entries| {
            if cancel.load(Ordering::Acquire) {
                return Ok(());
            }
            let publish = (|| -> Result<()> {
                add_entries(&mut builder, entries)?;
                if builder.len() < limit {
                    return Ok(());
                }
                let mut batch =
                    std::mem::replace(&mut builder, IndexBuilder::new(token.to_owned()));
                batch.set_coverage_complete(false);
                let generation = latest
                    .as_ref()
                    .map_or(0, |p: &crate::persistence::PublicationOutcome| p.generation);
                let index =
                    batch.finish(generation.checked_add(1).context("generation exhausted")?)?;
                ensure!(
                    load_settings(dir)?.scope_token()? == token,
                    "扫描期间范围已更改，本次结果未发布，请重新扫描。"
                );
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                let publication = if generation == 0 {
                    store.publish(&index)?
                } else {
                    store.publish_delta(generation, &[], &index)?
                };
                published_entries += index.len();
                latest = Some(publication);
                // 扩大下一批的边界，避免首扫在大目录上重复合并整个前缀。
                limit = limit.saturating_mul(2).min(1_048_576);
                Ok(())
            })();
            capture_batch_error(publish, &mut build_error)
        },
    );
    if let Some(error) = build_error {
        return Err(error);
    }
    let mut report = scanned?;
    let indexed = published_entries + builder.len();
    if !report.complete || report.cancelled || cancel.load(Ordering::Acquire) {
        if cancel.load(Ordering::Acquire) {
            report.cancelled = true;
            report.complete = false;
        }
        return Ok(match latest {
            Some(publication) => RebuildOutcome {
                published: true,
                generation: Some(publication.generation),
                durable: publication.durable,
                durability_warning: publication.warning,
                indexed: published_entries,
                elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
                report,
            },
            None => unpublished(start, indexed, report),
        });
    }
    ensure!(
        load_settings(dir)?.scope_token()? == token,
        "扫描期间范围已更改，本次结果未发布，请重新扫描。"
    );
    set_native_checkpoint(&mut builder, &report);
    let publication = if let Some(latest) = latest {
        let index = builder.finish(
            latest
                .generation
                .checked_add(1)
                .context("generation exhausted")?,
        )?;
        store.finish_initial_scan(latest.generation, &index)?
    } else {
        store.publish(&builder.finish(1)?)?
    };
    Ok(RebuildOutcome {
        published: true,
        generation: Some(publication.generation),
        durable: publication.durable,
        durability_warning: publication.warning,
        indexed,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        report,
    })
}

/// Scope checks are lexical and happen before any filesystem probe. Local-path
/// and actual-type checks follow immediately before handing a path to the OS.
pub fn authorize_open(settings: &Settings, record: &FileRecord) -> Result<PathBuf> {
    ensure!(
        record.source == "local",
        "离线目录仅提供文件信息；请先将真实本地目录加入范围并重新扫描。"
    );
    let path = PathBuf::from(&record.path);
    ensure!(
        settings.allows_filename(&path, record.is_dir)?,
        "文件已不在当前授权目录、文件类型或排除策略内。"
    );
    xs_platform::validate_local_path(&path, false)?;
    let metadata = std::fs::symlink_metadata(&path)?;
    ensure!(
        (metadata.is_file() || metadata.is_dir()) && metadata.is_dir() == record.is_dir,
        "文件类型已发生变化，请重新扫描。"
    );
    ensure!(
        settings.allows_filename(&path, metadata.is_dir())?,
        "文件已不在当前授权目录、文件类型或排除策略内。"
    );
    Ok(path)
}

/// 对当前搜索代次的结果再次确认授权和原文件对象，防止打开同名替换的文件。
pub fn authorize_indexed_open(dir: &Path, index: &Index, id: u32) -> Result<PathBuf> {
    let settings = load_settings(dir)?;
    validate_index_scope(index, &settings)?;
    let record = index.record(id)?;
    let path = authorize_open(&settings, &record)?;
    if let Some(expected) = index.record_revision(id)? {
        ensure!(
            FileRevision::at_path(&path)? == expected,
            "文件已被替换或修改，请刷新搜索结果。"
        );
    }
    validate_index_scope(index, &load_settings(dir)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_first_scan_keeps_only_committed_batches_marked_partial() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("fixture");
        let data = temporary.path().join("data");
        std::fs::create_dir(&root).unwrap();
        for number in 0..6 {
            std::fs::write(root.join(format!("fixture-{number}.txt")), b"synthetic").unwrap();
        }
        let settings = Settings {
            roots: vec![xs_platform::Scope::new(root.to_str().unwrap())],
            ..Default::default()
        };
        save_settings(&data, &settings).unwrap();
        let store = Store::open(data.join("index")).unwrap();
        let token = settings.scope_token().unwrap();
        let cancel = AtomicBool::new(false);
        let outcome = rebuild_initial::<2, 2>(
            &data,
            &cancel,
            |progress| {
                if progress.indexed >= 2 {
                    let index = store.load().unwrap();
                    assert_eq!(index.len(), 2);
                    assert!(!index.coverage_complete());
                    cancel.store(true, Ordering::Release);
                }
            },
            &settings,
            &token,
            &store,
            Instant::now(),
        )
        .unwrap();
        assert!(outcome.published && outcome.report.cancelled);
        let response = load_index(&data)
            .unwrap()
            .search("", &Default::default(), None)
            .unwrap();
        assert!(!response.coverage_complete && !response.complete);
        assert_eq!(response.total_relation, "at_least");
        assert_eq!(response.total, 2);
        let outcome = rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(outcome.published && outcome.report.complete);
        let fresh = load_index(&data).unwrap();
        assert!(fresh.coverage_complete());
        assert_eq!(fresh.len(), 7);
    }
}
