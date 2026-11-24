//! 独立的正文收录与查询。内容范围和文件对象版本是发布与展示的共同约束。

pub mod connectors;
mod extract;
mod ipc;
mod search;
mod semantic;
mod snapshot;
mod transport;
mod worker;

pub use extract::Location;
pub use ipc::serve;
pub use search::{
    ContentHit, ContentReader, Highlight, MatchEvidence, SearchMode, SearchOptions, SearchResponse,
};
pub use worker::{extract_worker, extract_worker_serve, extract_worker_with_ocr, WorkerOptions};

use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use rusqlite::{ffi, params, serialize::OwnedData, Connection, OptionalExtension, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};
use xs_platform::{FileRevision, PlatformError, Settings, SpeedMode};

use snapshot::{encode_header, OpenSnapshot, MAX_DB_SIZE};
// 增量路径直接在旧快照的私有可写副本中更新，不再同时保留新旧两个 SQLite 连接。
// 最大复用大小与受保护的索引容量相同，以免大型索引退化为全量重建。
const MAX_REUSE_SNAPSHOT_BYTES: u64 = MAX_DB_SIZE as u64;
const MAX_REPORTED_FAILURES: usize = 100;
const CONTENT_DB: &str = "content.idx";
const CONTENT_LOCK: &str = "content.lock";
// 私有候选库：只供重建器恢复，不是查询入口。
const CONTENT_CHECKPOINT: &str = "content.pending.idx";
const CONTENT_REQUESTS: &str = "content.requests.json";
const MAX_REQUESTS: usize = 4096;
const MAX_REQUEST_BYTES: u64 = 4 * 1024 * 1024;

/// Durable, coalesced file-change requests, bound to the exact content scope.
/// An uncertain event (deletion, directory, overflow) requires full reconciliation.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingRequests {
    scope: String,
    full: bool,
    paths: BTreeSet<String>,
}

fn read_requests(dir: &Path) -> Result<Option<PendingRequests>> {
    let file = match xs_platform::open_local_read(&dir.join(CONTENT_REQUESTS)) {
        Ok(file) => file,
        Err(PlatformError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_REQUEST_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_REQUEST_BYTES,
        "正文变更队列超过容量限制"
    );
    let value: PendingRequests = serde_json::from_slice(&bytes)?;
    ensure!(
        value.paths.len() <= MAX_REQUESTS,
        "正文变更队列超过条数限制"
    );
    Ok(Some(value))
}

/// Record notifications before processing; concurrent writers receive a retryable lock error.
/// Empty input is an explicit request for complete reconciliation (e.g. watcher overflow).
pub fn enqueue_changes(dir: &Path, paths: &[PathBuf]) -> Result<usize> {
    let lock = xs_platform::open_or_create_local_lock(&dir.join(CONTENT_LOCK))?;
    FileExt::try_lock_exclusive(&lock).context("正文任务正在运行，请稍后重试提交变更")?;
    let policy = Policy::load(dir)?;
    let mut queue = match read_requests(dir) {
        Ok(Some(value)) if value.scope == policy.token => value,
        Ok(None) => PendingRequests {
            scope: policy.token.clone(),
            ..Default::default()
        },
        // Unreadable/obsolete data is not trusted as a partial event list.
        _ => PendingRequests {
            scope: policy.token.clone(),
            full: true,
            paths: BTreeSet::new(),
        },
    };
    if paths.is_empty() || paths.len() > MAX_REQUESTS {
        queue.full = true;
    }
    for path in paths.iter().take(MAX_REQUESTS) {
        ensure!(
            policy.allows(dir, path)?
                || policy.settings.allows_content(path, true)?
                    && !xs_platform::local_path_within(path, dir)?,
            "变更路径不在正文授权范围"
        );
        let text = path.to_str().context("变更路径不是有效 UTF-8")?;
        if !queue.full {
            queue.paths.insert(text.to_owned());
        }
    }
    if queue.paths.len() > MAX_REQUESTS {
        queue.full = true;
    }
    if queue.full {
        queue.paths.clear();
    }
    let mut bytes = serde_json::to_vec(&queue)?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        queue.full = true;
        queue.paths.clear();
        bytes = serde_json::to_vec(&queue)?;
    }
    policy.verify(dir)?;
    let result = xs_platform::atomic_write_local(&dir.join(CONTENT_REQUESTS), &bytes, true)?;
    ensure!(result.is_durable(), "变更队列持久化未确认，请重试");
    Ok(queue.paths.len())
}

/// Apply the durable request queue, or reconcile the whole scope when no reliable
/// event list exists. A failed/cancelled run never acknowledges the queue.
pub fn apply_queued_changes(
    dir: &Path,
    executable: &Path,
    cancel: &AtomicBool,
) -> Result<BuildReport> {
    rebuild_inner(dir, executable, &WorkerOptions::default(), cancel, true)
}
const CHECKPOINT_BATCH: usize = 256;
const CHECKPOINT_MAX_BATCH: usize = 4096;
const CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Serialize)]
pub struct BuildFailure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Default, Serialize)]
pub struct BuildReport {
    pub published: bool,
    /// True only for a successful full scope traversal, not a targeted update.
    pub scope_reconciled: bool,
    pub targeted_update: bool,
    pub request_warning: Option<String>,
    /// Actual publication payload bytes written (excludes temporary checkpoints).
    pub publication_bytes_written: u64,
    pub delta_blocks: usize,
    pub base_rewritten: bool,
    pub snapshot_cleanup_warning: Option<String>,
    pub generation: Option<u64>,
    pub durable: bool,
    pub durability_warning: Option<String>,
    pub complete: bool,
    pub cancelled: bool,
    pub documents: usize,
    pub segments: usize,
    pub semantic_vectors: usize,
    pub reused_documents: usize,
    pub reused_segments: usize,
    pub reused_vectors: usize,
    /// 本轮真正重新抽取的文档数。
    pub updated_documents: usize,
    /// 本轮从旧索引中移除的文件数。
    pub removed_documents: usize,
    /// 未变更时复用原有持久快照而不产生新代次。
    pub unchanged_snapshot: bool,
    /// 恢复了同一授权设置、发布基线和模型对应的候选快照。
    pub resumed_checkpoint: bool,
    pub checkpoints_written: usize,
    pub checkpoint_warning: Option<String>,
    pub unsupported: usize,
    pub failed: usize,
    pub failures: Vec<BuildFailure>,
}

impl BuildReport {
    fn failure(&mut self, path: String, error: String) {
        self.failed += 1;
        if self.failures.len() < MAX_REPORTED_FAILURES {
            self.failures.push(BuildFailure { path, error });
        }
    }
}

struct Policy {
    settings: Settings,
    revision: FileRevision,
    token: String,
}

impl Policy {
    fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("settings.json");
        let revision = FileRevision::at_path(&path)?;
        let settings = Settings::load(&path)?;
        ensure!(settings.content_enabled, "正文检索尚未启用");
        ensure!(
            revision == FileRevision::at_path(&path)?,
            "正文授权设置正在更改，请重试"
        );
        let token = format!(
            "{:x}",
            Sha256::digest(settings.content_scope_token()?.as_bytes())
        );
        Ok(Self {
            settings,
            revision,
            token,
        })
    }

    fn verify(&self, dir: &Path) -> Result<()> {
        ensure!(
            self.revision == FileRevision::at_path(&dir.join("settings.json"))?,
            "正文授权设置已更改，请重试"
        );
        Ok(())
    }

    fn allows(&self, dir: &Path, path: &Path) -> Result<bool> {
        Ok(!xs_platform::local_path_within(path, dir)?
            && self.settings.allows_content(path, false)?)
    }
}

/// Opt-in foreground watch. Subscribe before reconciliation; notification loss
/// and topology changes force a full scan, and every restart does the same.
/// This is not an OS-persistent journal: an unclean stop is recovered by that scan.
pub fn watch_changes(
    dir: &Path,
    executable: &Path,
    cancel: &AtomicBool,
    mut on_report: impl FnMut(&BuildReport),
) -> Result<()> {
    use std::time::{Duration, Instant};
    use xs_platform::{ScopeWatcher, WatchEvent};
    let policy = Policy::load(dir)?;
    let scan_settings = Settings {
        roots: policy.settings.content_roots.clone(),
        mode: SpeedMode::Standard,
        ..Settings::default()
    };
    let mut watcher = ScopeWatcher::new(&scan_settings, &[dir.into()])?;
    policy.verify(dir)?;
    enqueue_changes(dir, &[])?;
    let mut pending = true;
    let mut next_attempt = Instant::now();
    let mut reconcile_at = Instant::now() + Duration::from_secs(600);
    while !cancel.load(Ordering::Acquire) {
        policy.verify(dir)?;
        if Instant::now() >= reconcile_at {
            enqueue_changes(dir, &[])?;
            pending = true;
            reconcile_at = Instant::now() + Duration::from_secs(600);
        }
        if pending && Instant::now() >= next_attempt {
            let report = apply_queued_changes(dir, executable, cancel)?;
            let done = report.published
                && report.durable
                && report.complete
                && report.request_warning.is_none();
            if done && report.scope_reconciled {
                reconcile_at = Instant::now() + Duration::from_secs(600);
            }
            on_report(&report);
            if report.cancelled || cancel.load(Ordering::Acquire) {
                break;
            }
            pending = !done;
            next_attempt = Instant::now()
                + if done {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(10)
                };
        }
        match watcher.poll(Duration::from_secs(1)) {
            Ok(WatchEvent::Idle) => (),
            Ok(WatchEvent::Changed) => {
                policy.verify(dir)?;
                if let Some(paths) = watcher.take_changed_paths() {
                    // Watch notifications can include files excluded by an extension filter.
                    let mut accepted = Vec::new();
                    for path in paths {
                        if policy.allows(dir, &path)?
                            || (policy.settings.allows_content(&path, true)?
                                && !xs_platform::local_path_within(&path, dir)?)
                        {
                            accepted.push(path);
                        }
                    }
                    if !accepted.is_empty() {
                        enqueue_changes(dir, &accepted)?;
                        pending = true;
                    }
                } else {
                    enqueue_changes(dir, &[])?;
                    pending = true;
                }
            }
            _ => {
                policy.verify(dir)?;
                // Re-subscribe before scheduling a full reconciliation.
                watcher = ScopeWatcher::new(&scan_settings, &[dir.into()])?;
                enqueue_changes(dir, &[])?;
                pending = true;
            }
        }
    }
    Ok(())
}

/// 使用相邻的受信任抽取程序重建独立正文库，默认每份文档 384 MiB／10 秒。
pub fn rebuild(dir: &Path, executable: &Path, cancel: &AtomicBool) -> Result<BuildReport> {
    rebuild_with_worker_options(dir, executable, &WorkerOptions::default(), cancel)
}

pub fn rebuild_with_worker_options(
    dir: &Path,
    executable: &Path,
    options: &WorkerOptions,
    cancel: &AtomicBool,
) -> Result<BuildReport> {
    rebuild_inner(dir, executable, options, cancel, false)
}

fn rebuild_inner(
    dir: &Path,
    executable: &Path,
    options: &WorkerOptions,
    cancel: &AtomicBool,
    use_queue: bool,
) -> Result<BuildReport> {
    let policy = Policy::load(dir)?;
    options.validate()?;
    xs_platform::validate_local_path(executable, false)?;
    ensure!(
        xs_platform::open_local_read(executable)?
            .metadata()?
            .is_file(),
        "正文抽取程序不是普通文件"
    );
    if policy.settings.ocr_enabled {
        let resources = executable
            .parent()
            .context("无法定位正文程序目录")?
            .join("resources");
        xs_inference::Assets::verify(&resources)?;
    }
    let lock = xs_platform::open_or_create_local_lock(&dir.join(CONTENT_LOCK))?;
    FileExt::try_lock_exclusive(&lock).context("另一个正文重建任务正在运行")?;

    let mut report = BuildReport::default();
    policy.verify(dir)?;
    if cancel.load(Ordering::Acquire) {
        report.cancelled = true;
        return Ok(report);
    }
    let scan_settings = Settings {
        roots: policy.settings.content_roots.clone(),
        mode: SpeedMode::Standard,
        ..Settings::default()
    };
    let target = dir.join(CONTENT_DB);
    let base_revision = optional_revision(&target)?;
    let generation = next_generation(&target)?;
    let checkpoint = dir.join(CONTENT_CHECKPOINT);
    let checkpoint_key = serde_json::to_string(&(base_revision, policy.revision))?;
    let mut extractor =
        worker::ExtractClient::start(executable, options, policy.settings.ocr_enabled, cancel)?;
    let mut model = if policy.settings.semantic_enabled {
        Some(semantic::ModelClient::start(
            &semantic::adjacent_to(executable),
            options,
            cancel,
        )?)
    } else {
        None
    };
    // 只有授权 token 和语义模型指纹均一致、快照校验通过时允许复用。
    // 文件身份/时间戳/长度还要在逐文件复制前后再次确认。
    let fingerprint = model.as_ref().map(semantic::ModelClient::fingerprint);
    let mut published_base = if use_queue {
        reusable_snapshot(&target, &policy.token, fingerprint, cancel)
    } else {
        None
    };
    let mut entries = None;
    if use_queue && published_base.is_some() {
        if let Ok(Some(queue)) = read_requests(dir) {
            if queue.scope == policy.token && !queue.full && !queue.paths.is_empty() {
                let mut selected = Vec::new();
                for path in &queue.paths {
                    // Missing, renamed or directory events must reconcile the whole scope;
                    // never infer deletion from a transient failure or a single directory event.
                    match xs_platform::inspect_scoped_entry(
                        &scan_settings,
                        Path::new(path),
                        &[dir.into()],
                    ) {
                        Ok(Some(entry))
                            if !entry.is_dir
                                && entry.path == *path
                                && policy.allows(dir, Path::new(path))?
                                && extract::supports(
                                    &Path::new(path)
                                        .extension()
                                        .and_then(std::ffi::OsStr::to_str)
                                        .unwrap_or_default()
                                        .to_ascii_lowercase(),
                                    policy.settings.ocr_enabled,
                                ) =>
                        {
                            selected.push(entry)
                        }
                        _ => {
                            selected.clear();
                            break;
                        }
                    }
                }
                if selected.len() == queue.paths.len() {
                    entries = Some(selected);
                }
            }
        }
    }
    report.targeted_update = entries.is_some();
    let entries = match entries {
        Some(entries) => entries,
        None => {
            let scan =
                xs_platform::scan_with_exclusions(&scan_settings, cancel, |_| {}, &[dir.into()])?;
            if !scan.complete {
                report.cancelled = scan.cancelled;
                for failure in scan.errors {
                    report.failure(failure.path, failure.message);
                }
                return Ok(report);
            }
            scan.entries
        }
    };
    // Bind a candidate to the exact selected event set; partial candidates must
    // never be mistaken for full-scope reconciliation or a different event set.
    let checkpoint_key = if report.targeted_update {
        serde_json::to_string(&(
            checkpoint_key,
            entries.iter().map(|e| &e.path).collect::<Vec<_>>(),
        ))?
    } else {
        checkpoint_key
    };
    // Avoid holding two large snapshots in memory at once.
    if optional_revision(&checkpoint)?.is_some() {
        published_base = None;
    }
    let pending =
        reusable_snapshot(&checkpoint, &policy.token, fingerprint, cancel).filter(|loaded| {
            loaded.generation == generation
                && loaded
                    .connection
                    .query_row("SELECT base FROM checkpoint_binding LIMIT 1", [], |row| {
                        row.get::<_, String>(0)
                    })
                    .ok()
                    .as_deref()
                    == Some(checkpoint_key.as_str())
        });
    report.resumed_checkpoint = pending.is_some();
    if pending.is_none() {
        // A stale or damaged candidate must never be resumed. Do not follow links.
        clear_checkpoint(&checkpoint)?;
    }
    let previous = pending
        .or(published_base)
        .or_else(|| reusable_snapshot(&target, &policy.token, fingerprint, cancel));
    ensure!(
        !report.targeted_update || previous.is_some(),
        "正文局部更新的发布基线失效，请完整校准"
    );
    let incremental = previous.is_some();
    let previous_generation = previous.as_ref().map(|loaded| loaded.generation);
    let previous_stamp = previous.as_ref().map(|loaded| loaded.stamp);
    let mut db = if let Some(previous) = previous {
        previous.connection
    } else {
        Connection::open_in_memory()?
    };
    db.execute_batch(
        "PRAGMA trusted_schema=OFF;
         PRAGMA temp_store=MEMORY;
         PRAGMA foreign_keys=ON;
         PRAGMA max_page_count=16384;",
    )?;
    if !incremental {
        db.execute_batch(
            "PRAGMA page_size=32768;
             CREATE TABLE metadata(scope TEXT NOT NULL, generation INTEGER NOT NULL, model TEXT);
             CREATE TABLE documents(id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, revision TEXT NOT NULL);
             CREATE TABLE segments(id INTEGER PRIMARY KEY, doc INTEGER NOT NULL REFERENCES documents(id),
                                   location TEXT NOT NULL, body TEXT NOT NULL);
             CREATE TABLE vectors(doc INTEGER NOT NULL REFERENCES documents(id),
                                  segment INTEGER NOT NULL REFERENCES segments(id),
                                  start INTEGER NOT NULL, end INTEGER NOT NULL, embedding BLOB NOT NULL,
                                  PRIMARY KEY(doc, segment, start)) WITHOUT ROWID;
             CREATE VIRTUAL TABLE terms USING fts5(tokens, tokenize='unicode61 remove_diacritics 0');"
        )?;
    }
    // Index the foreign key used by reuse, removal and result grouping.
    // Existing snapshots receive this index on their next successful publication.
    db.execute_batch(
        "CREATE INDEX IF NOT EXISTS segments_doc ON segments(doc);
        CREATE TEMP TABLE scanned(path TEXT PRIMARY KEY) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS checkpoint_binding(base TEXT NOT NULL);
        DELETE FROM checkpoint_binding;",
    )?;
    db.execute(
        "INSERT INTO checkpoint_binding(base) VALUES (?1)",
        [&checkpoint_key],
    )?;
    if incremental {
        db.execute("UPDATE metadata SET generation=?1", [generation])?;
    } else {
        db.execute(
            "INSERT INTO metadata(scope,generation,model) VALUES (?,?,?)",
            params![policy.token, generation, fingerprint],
        )?;
    }
    let mut dirty = 0;
    let mut last_checkpoint = std::time::Instant::now();
    for entry in entries.into_iter().filter(|entry| !entry.is_dir) {
        if cancel.load(Ordering::Acquire) {
            report.cancelled = true;
            return Ok(report);
        }
        let path = PathBuf::from(&entry.path);
        let extension = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !extract::supports(&extension, policy.settings.ocr_enabled) {
            report.unsupported += 1;
            continue;
        }
        policy.verify(dir)?;
        // 只认文件对象修订号，不认扫描时的文件名或缓存时间戳。
        if incremental {
            if let Ok(current) = FileRevision::at_path(&path) {
                if policy.allows(dir, &path)? {
                    if let Some((segments, vectors)) =
                        unchanged_document(&db, &entry.path, &current)?
                    {
                        ensure!(
                            FileRevision::at_path(&path)? == current,
                            "复用正文索引期间文件已变更，请重试"
                        );
                        db.execute(
                            "INSERT OR IGNORE INTO scanned(path) VALUES (?1)",
                            [&entry.path],
                        )?;
                        report.documents += 1;
                        report.segments += segments;
                        report.semantic_vectors += vectors;
                        report.reused_documents += 1;
                        report.reused_segments += segments;
                        report.reused_vectors += vectors;
                        continue;
                    }
                }
            }
        }
        let document = extract_authorized(&policy, dir, &path, &mut extractor, &extension, cancel);
        let (revision, segments) = match document {
            Ok(document) => document,
            Err(_) if cancel.load(Ordering::Acquire) => {
                report.cancelled = true;
                return Ok(report);
            }
            Err(error) if error.is::<worker::WorkerFault>() => {
                report.failure(entry.path, error.to_string());
                return Ok(report);
            }
            Err(error) => {
                report.failure(entry.path, error.to_string());
                continue;
            }
        };
        // 内容变更的旧段落、FTS 倒排项和向量必须作为一组删除，防止重复命中。
        // 即使新文件没有可索引正文，也必须删去其旧索引。
        // Each document is atomic, including its FTS entries and vectors.
        let tx = db.transaction()?;
        if incremental {
            remove_document(&tx, &entry.path)?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO scanned(path) VALUES (?1)",
            [&entry.path],
        )?;
        let mut insert_document =
            tx.prepare_cached("INSERT INTO documents(path,revision) VALUES (?,?)")?;
        let mut insert_segment =
            tx.prepare_cached("INSERT INTO segments(doc,location,body) VALUES (?,?,?)")?;
        let mut insert_terms = tx.prepare_cached("INSERT INTO terms(rowid,tokens) VALUES (?,?)")?;
        let mut insert_vector = tx.prepare_cached(
            "INSERT INTO vectors(doc,segment,start,end,embedding) VALUES (?,?,?,?,?)",
        )?;
        report.updated_documents += 1;
        // 即使没有可检索正文，也记录已验证的文件修订号。否则空文本、
        // 无可抽取内容的 PDF 等文件会在每轮增量任务中重复抽取，并错误推进代次。
        // documents 行允许零个 segments；它不会出现在全文命中结果中。
        let doc = insert_document.insert(params![entry.path, serde_json::to_string(&revision)?])?;
        report.documents += 1;
        for segment in segments {
            if cancel.load(Ordering::Acquire) {
                report.cancelled = true;
                return Ok(report);
            }
            let tokens = search::index_text(&segment.text);
            if tokens.is_empty() {
                continue;
            }
            let id = insert_segment.insert(params![
                doc,
                serde_json::to_string(&segment.location)?,
                segment.text,
            ])?;
            insert_terms.execute(params![id, tokens])?;
            if let Some(model) = model.as_mut() {
                for (offset, passage) in semantic::passages(&segment.text) {
                    policy.verify(dir)?;
                    let windows = match model.embed(passage, cancel) {
                        Ok(windows) => windows,
                        Err(_) if cancel.load(Ordering::Acquire) => {
                            report.cancelled = true;
                            return Ok(report);
                        }
                        Err(error) => return Err(error.context("正文向量构建失败")),
                    };
                    for window in windows {
                        insert_vector.execute(params![
                            doc,
                            id,
                            offset + window.start,
                            offset + window.end,
                            window.bytes()
                        ])?;
                        report.semantic_vectors += 1;
                    }
                }
            }
            report.segments += 1;
        }
        drop((insert_document, insert_segment, insert_terms, insert_vector));
        tx.commit()?;
        dirty += 1;
        if dirty >= CHECKPOINT_MAX_BATCH
            || (dirty >= CHECKPOINT_BATCH && last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL)
        {
            save_checkpoint(
                &mut db,
                &checkpoint,
                generation,
                &policy,
                dir,
                cancel,
                &mut report,
            )?;
            dirty = 0;
            last_checkpoint = std::time::Instant::now();
        }
    }
    // 任何已支持文件的抽取失败都不得发布覆盖旧库的缺文档快照。
    // 保留上一完整索引，返回失败清单供用户修复后重试。
    if report.failed != 0 {
        // Preserve successful work so correcting a bad file need not re-extract it.
        if dirty != 0 {
            save_checkpoint(
                &mut db,
                &checkpoint,
                generation,
                &policy,
                dir,
                cancel,
                &mut report,
            )?;
        }
        report.complete = false;
        return Ok(report);
    }
    let tx = db.transaction()?;
    if incremental && !report.targeted_update {
        let missing = {
            let mut stmt = tx.prepare(
                "SELECT path FROM documents WHERE path NOT IN (SELECT path FROM scanned)",
            )?;
            let paths = stmt.query_map([], |row| row.get::<_, String>(0))?;
            paths.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for path in &missing {
            ensure!(!cancel.load(Ordering::Acquire), "正文增量清理已取消");
            remove_document(&tx, path)?;
        }
        report.removed_documents = missing.len();
    }
    tx.commit()?;
    db.execute_batch("DROP TABLE scanned; DROP TABLE checkpoint_binding;")?;
    // 零变化时保持原来的代次，避免无意义的整库序列化和查询缓存失效。
    if incremental
        && !report.resumed_checkpoint
        && report.updated_documents == 0
        && report.removed_documents == 0
    {
        policy.verify(dir)?;
        ensure!(!cancel.load(Ordering::Acquire), "正文索引更新已取消");
        ensure!(
            optional_revision(&target)? == base_revision,
            "正文发布基线已变化，请重试"
        );
        if let Some(stamp) = previous_stamp {
            stamp.verify(&target)?;
        }
        report.published = true;
        report.unchanged_snapshot = true;
        report.generation = previous_generation;
        report.complete = true;
        report.durable = true;
        clear_checkpoint(&checkpoint)?;
        report.scope_reconciled = !report.targeted_update;
        acknowledge_requests(dir, &mut report);
        return Ok(report);
    }
    if cancel.load(Ordering::Acquire) {
        report.cancelled = true;
        return Ok(report);
    }
    let bytes = db.serialize(MAIN_DB)?;
    ensure!(bytes.len() <= MAX_DB_SIZE, "正文库超出容量上限");
    let published = snapshot::publish(
        &target,
        &bytes,
        generation,
        incremental && !report.resumed_checkpoint && report.removed_documents == 0,
        cancel,
        || {
            policy.verify(dir)?;
            ensure!(
                optional_revision(&target)? == base_revision,
                "正文发布基线已变化，请重试"
            );
            if !report.resumed_checkpoint {
                if let Some(stamp) = previous_stamp {
                    stamp.verify(&target)?;
                }
            }
            ensure!(!cancel.load(Ordering::Acquire), "正文重建已取消");
            Ok(())
        },
    );
    let (outcome, stats) = match published {
        Err(_) if cancel.load(Ordering::Acquire) => {
            report.cancelled = true;
            return Ok(report);
        }
        other => other?,
    };
    report.publication_bytes_written = stats.bytes_written;
    report.delta_blocks = stats.delta_blocks;
    report.base_rewritten = stats.base_rewritten;
    report.snapshot_cleanup_warning = stats.cleanup_warning;
    report.published = true;
    report.generation = Some(generation);
    report.durable = outcome.is_durable();
    report.durability_warning = outcome.warning().map(str::to_owned);
    report.complete = report.failed == 0;
    report.scope_reconciled = report.complete && !report.targeted_update;
    if report.durable && report.complete {
        acknowledge_requests(dir, &mut report);
    }
    if let Err(error) = clear_checkpoint(&checkpoint) {
        report.checkpoint_warning = Some(format!("已发布，但候选检查点清理失败：{error}"));
    }
    Ok(report)
}

fn acknowledge_requests(dir: &Path, report: &mut BuildReport) {
    if let Err(error) = clear_checkpoint(&dir.join(CONTENT_REQUESTS)) {
        report.request_warning = Some(format!("已发布，但变更队列未清理（可安全重放）：{error}"));
    }
}

/// Discard only the private candidate. Never touches the published index or user files.
/// Acquires the same writer lock as rebuild; an active writer cannot be interrupted.
pub fn discard_pending_checkpoint(dir: &Path) -> Result<()> {
    let lock = xs_platform::open_or_create_local_lock(&dir.join(CONTENT_LOCK))?;
    FileExt::try_lock_exclusive(&lock).context("正文任务正在运行，请取消任务并等待结束后重试")?;
    clear_checkpoint(&dir.join(CONTENT_CHECKPOINT))
}

/// Clear pending candidate and queued paths on explicit authorization change.
pub fn discard_pending_work(dir: &Path) -> Result<()> {
    let lock = xs_platform::open_or_create_local_lock(&dir.join(CONTENT_LOCK))?;
    FileExt::try_lock_exclusive(&lock).context("正文任务正在运行，请取消任务并等待结束后重试")?;
    clear_checkpoint(&dir.join(CONTENT_CHECKPOINT))?;
    clear_checkpoint(&dir.join(CONTENT_REQUESTS))
}

fn optional_revision(path: &Path) -> Result<Option<FileRevision>> {
    match FileRevision::at_path(path) {
        Ok(revision) => Ok(Some(revision)),
        Err(PlatformError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn clear_checkpoint(path: &Path) -> Result<()> {
    match xs_platform::remove_local_file(path) {
        Ok(()) => Ok(()),
        Err(PlatformError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn save_checkpoint(
    db: &mut Connection,
    path: &Path,
    generation: u64,
    policy: &Policy,
    dir: &Path,
    cancel: &AtomicBool,
    report: &mut BuildReport,
) -> Result<()> {
    policy.verify(dir)?;
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let bytes = db.serialize(MAIN_DB)?;
    ensure!(bytes.len() <= MAX_DB_SIZE, "正文检查点超出容量上限");
    let header = encode_header(generation, &bytes);
    // The existing durable publication primitive pins the parent and rejects links.
    // A failed optional checkpoint does not invalidate the live index or current work.
    match xs_platform::write_local_replace(path, |file| {
        file.write_all(&header)?;
        for chunk in bytes.chunks(1024 * 1024) {
            if cancel.load(Ordering::Acquire) {
                return Err(io::Error::other("正文检查点已取消"));
            }
            file.write_all(chunk)?;
        }
        policy.verify(dir).map_err(io::Error::other)?;
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::other("正文检查点已取消"));
        }
        Ok(())
    }) {
        Ok(outcome) => {
            report.checkpoints_written += 1;
            if let Some(warning) = outcome.warning() {
                report.checkpoint_warning = Some(warning.to_owned());
            }
        }
        Err(error) => report.checkpoint_warning = Some(format!("正文检查点未写入：{error}")),
    }
    policy.verify(dir)?;
    Ok(())
}

fn reusable_snapshot(
    target: &Path,
    scope_token: &str,
    model: Option<&str>,
    cancel: &AtomicBool,
) -> Option<LoadedDb> {
    let snapshot = OpenSnapshot::open(target).ok()?;
    if snapshot.head.size as u64 > MAX_REUSE_SNAPSHOT_BYTES {
        return None;
    }
    let loaded = load_db(snapshot, cancel, true).ok()?;
    if loaded.scope == scope_token && loaded.model.as_deref() == model {
        Some(loaded)
    } else {
        None
    }
}

/// 精确复用旧数据库中的文档/段落/词项/向量，避免 O(N) 的重复 INSERT。
fn unchanged_document(
    db: &Connection,
    path: &str,
    revision: &FileRevision,
) -> Result<Option<(usize, usize)>> {
    let stored: Option<(i64, String)> = db
        .query_row(
            "SELECT id,revision FROM documents WHERE path=?1",
            [path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((doc, stored_revision)) = stored else {
        return Ok(None);
    };
    if serde_json::from_str::<FileRevision>(&stored_revision)? != *revision {
        return Ok(None);
    }
    let segments: i64 =
        db.query_row("SELECT count(*) FROM segments WHERE doc=?1", [doc], |row| {
            row.get(0)
        })?;
    let vectors: i64 = db.query_row("SELECT count(*) FROM vectors WHERE doc=?1", [doc], |row| {
        row.get(0)
    })?;
    Ok(Some((
        usize::try_from(segments)?,
        usize::try_from(vectors)?,
    )))
}

/// 先删除 FTS，再删向量/段落和文档；整个操作只作用于未发布的事务副本。
fn remove_document(db: &Connection, path: &str) -> Result<bool> {
    let doc: Option<i64> = db
        .query_row("SELECT id FROM documents WHERE path=?1", [path], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(doc) = doc else { return Ok(false) };
    db.execute(
        "DELETE FROM terms WHERE rowid IN (SELECT id FROM segments WHERE doc=?1)",
        [doc],
    )?;
    db.execute("DELETE FROM vectors WHERE doc=?1", [doc])?;
    db.execute("DELETE FROM segments WHERE doc=?1", [doc])?;
    db.execute("DELETE FROM documents WHERE id=?1", [doc])?;
    Ok(true)
}

fn extract_authorized(
    policy: &Policy,
    dir: &Path,
    path: &Path,
    extractor: &mut worker::ExtractClient,
    extension: &str,
    cancel: &AtomicBool,
) -> Result<(FileRevision, Vec<extract::Segment>)> {
    ensure!(policy.allows(dir, path)?, "文件不在正文授权范围");
    let file = xs_platform::open_local_read(path)?;
    ensure!(
        !is_cloud_placeholder(&file),
        "跳过未下载的云盘占位符文件，避免触发自动下载"
    );
    let revision = FileRevision::from_file(&file)?;
    ensure!(
        revision.size <= extract::MAX_INPUT as u64,
        "文件超出正文抽取大小上限"
    );
    let mut bytes = Vec::with_capacity(revision.size as usize);
    let mut input = (&file).take(extract::MAX_INPUT as u64 + 1);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        ensure!(!cancel.load(Ordering::Acquire), "正文任务已取消");
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    ensure!(
        bytes.len() as u64 == revision.size
            && FileRevision::from_file(&file)? == revision
            && FileRevision::at_path(path)? == revision,
        "读取正文期间文件已替换或更改"
    );
    policy.verify(dir)?;
    let ocr = policy.settings.ocr_enabled && (extension == "pdf" || extract::is_image(extension));
    let segments = extractor.extract(extension, bytes, ocr, cancel)?;
    policy.verify(dir)?;
    ensure!(
        FileRevision::at_path(path)? == revision,
        "抽取正文期间文件已替换或更改"
    );
    Ok((revision, segments))
}

fn next_generation(path: &Path) -> Result<u64> {
    match xs_platform::open_local_read(path) {
        Ok(mut file) => {
            // 显式重建可以恢复受损的快照。响应仍以打开的文件对象和代次双重校验。
            let previous = snapshot::read_head(&mut file).map_or(0, |header| header.generation);
            Ok(previous.checked_add(1).context("正文索引代次耗尽")?)
        }
        Err(PlatformError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(1),
        Err(error) => Err(error.into()),
    }
}

struct LoadedDb {
    stamp: snapshot::Stamp,
    generation: u64,
    scope: String,
    model: Option<String>,
    connection: Connection,
}

fn load_db(mut snapshot: OpenSnapshot, cancel: &AtomicBool, writable: bool) -> Result<LoadedDb> {
    let generation = snapshot.head.generation;
    let size = snapshot.head.size;
    // SQLite owns this one reconstructed buffer; no second full DB allocation.
    let ptr = NonNull::new(unsafe { ffi::sqlite3_malloc64(size as u64) }.cast::<u8>())
        .context("正文库超出可用内存")?;
    let data = unsafe { OwnedData::from_raw_nonnull(ptr, size) };
    let bytes = unsafe {
        ptr.as_ptr().write_bytes(0, size);
        std::slice::from_raw_parts_mut(ptr.as_ptr(), size)
    };
    snapshot.read_into(bytes, cancel)?;
    let mut db = Connection::open_in_memory()?;
    db.deserialize(MAIN_DB, data, !writable)?;
    if writable {
        db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;")?;
    } else {
        db.execute_batch(
            "PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; PRAGMA temp_store=MEMORY;",
        )?;
    }
    let (scope, indexed_generation, model) = db.query_row(
        "SELECT scope,generation,model FROM metadata LIMIT 1",
        [],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        },
    )?;
    ensure!(
        generation == indexed_generation,
        "正文库的代次不一致，请重建"
    );
    Ok(LoadedDb {
        stamp: snapshot.stamp,
        generation,
        scope,
        model,
        connection: db,
    })
}

pub fn validate_response(dir: &Path, response: &SearchResponse) -> Result<()> {
    let policy = Policy::load(dir)?;
    ensure!(
        response.scope_token == policy.token && response.settings_revision == policy.revision,
        "正文授权已更改"
    );
    let path = dir.join(CONTENT_DB);
    let snapshot = OpenSnapshot::open(&path)?;
    ensure!(
        response.catalog_revision == snapshot.stamp.publication
            && response.catalog_base_revision == snapshot.stamp.base
            && snapshot.head.generation == response.generation,
        "正文索引已更新，请重新搜索"
    );
    for hit in &response.hits {
        let path = Path::new(&hit.path);
        if !policy.allows(dir, path)? || FileRevision::at_path(path).ok() != Some(hit.revision) {
            bail!("正文结果中的文件已移动、更改或撤权，请重新搜索");
        }
    }
    policy.verify(dir)?;
    snapshot.stamp.verify(&path)?;
    Ok(())
}

#[cfg(windows)]
fn is_cloud_placeholder(file: &File) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    const CLOUD_MASK: u32 = FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    file.metadata()
        .map(|meta| meta.file_attributes() & CLOUD_MASK != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_cloud_placeholder(_file: &File) -> bool {
    false
}
