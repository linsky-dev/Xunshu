//! Immutable snapshots and atomic CURRENT publication.
//! Errors before CURRENT commits preserve the previous pointer. A directory
//! sync failure after commit is a successful publication with a durability
//! warning, because readers may already observe the new generation.
use crate::catalog::{Catalog, CatalogFile, GenerationFile, MAX_DELETED, MAX_PARTS};
use crate::index::{Index, IndexBuilder, Manifest, Storage, MAX_SNAPSHOT};
use crate::records::{PackedRecords, RECORD_BYTES};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use xs_platform::LocalWriteOutcome;

const MAGIC: &[u8; 8] = b"XUNSHU18";
const HEADER: usize = 32;
const MAX_MANIFEST: usize = 64 * 1024 * 1024;
const MAX_POINTER: u64 = 4096;
const MAX_CATALOG: u64 = 4 * 1024 * 1024;
const CATALOG_MAGIC: &[u8; 8] = b"XSCAT001";
const HISTORY: usize = 3;
const MAX_PRUNE_QUEUE: usize = 256;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pointer {
    version: u32,
    file: String,
    generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history: Vec<GenerationFile>,
}

impl Pointer {
    fn current(&self) -> GenerationFile {
        GenerationFile {
            file: self.file.clone(),
            generation: self.generation,
        }
    }

    fn retained(&self) -> impl Iterator<Item = GenerationFile> + '_ {
        std::iter::once(self.current()).chain(self.history.iter().cloned())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryPointer {
    newest_generation: u64,
    retained: Vec<GenerationFile>,
}

fn read_bounded(path: &Path, budget: u64) -> Result<Option<Vec<u8>>> {
    let file = match xs_platform::open_local_read(path) {
        Ok(file) => file,
        Err(xs_platform::PlatformError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        file.metadata()?.len() <= budget,
        "index manifest exceeds budget"
    );
    let mut bytes = Vec::new();
    file.take(budget + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= budget,
        "index manifest exceeds budget"
    );
    Ok(Some(bytes))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, budget: u64) -> Result<Option<T>> {
    read_bounded(path, budget)?
        .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
        .transpose()
}

fn validate_file(file: &GenerationFile, snapshot_only: bool) -> Result<()> {
    // 所有路径均为生成文件的单个名称，不能引入外部路径或 Windows ADS。
    let body = file
        .file
        .strip_prefix("generation-")
        .context("invalid generation filename")?;
    let stem = if snapshot_only {
        body.strip_suffix(".xsi")
    } else {
        body.strip_suffix(".xsi")
            .or_else(|| body.strip_suffix(".xsc"))
    }
    .context("invalid generation extension")?;
    let mut fields = stem.split('-');
    ensure!(
        file.file.len() < 128
            && fields.next().and_then(|value| value.parse::<u64>().ok()) == Some(file.generation)
            && fields
                .next()
                .is_some_and(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
            && fields
                .next()
                .is_some_and(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
            && fields.next().is_none(),
        "invalid generation filename"
    );
    Ok(())
}

fn validate_history(newest: u64, retained: &[GenerationFile]) -> Result<()> {
    ensure!(
        retained.len() <= HISTORY + 1,
        "too many retained generations"
    );
    let mut previous = newest;
    for file in retained {
        validate_file(file, false)?;
        ensure!(
            file.generation < previous,
            "invalid generation history order"
        );
        previous = file.generation;
    }
    Ok(())
}

/// Every successful outcome means CURRENT committed. Durability additionally
/// confirms synchronization of file data and directory entries.
#[derive(Debug, Clone, Serialize)]
pub struct PublicationOutcome {
    pub path: PathBuf,
    pub generation: u64,
    pub durable: bool,
    pub warning: Option<String>,
}

pub struct Store {
    dir: PathBuf,
}
impl Store {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        xs_platform::create_local_dir_all(&dir)?;
        // New private directories are created through pinned parents. Do not
        // chmod an existing directory by a mutable path after checking it.
        Ok(Self { dir })
    }
    /// 为诊断读取现有代次；数据目录不存在时也不会创建任何文件。
    pub fn open_read_only(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        xs_platform::validate_local_path(&dir, true)?;
        Ok(Self { dir })
    }
    pub fn directory(&self) -> &Path {
        &self.dir
    }
    pub(crate) fn read_m1(dir: &Path) -> Result<(Self, Index)> {
        let store = Self {
            dir: dir.to_path_buf(),
        };
        let pointer = store.read_pointer()?.context("M1 源目录中没有已发布索引")?;
        ensure!(pointer.version == 1, "迁移源必须为 M1 索引目录");
        let index = store.load_snapshot(&pointer.current())?;
        Ok((store, index))
    }
    pub(crate) fn reject_m1_directory(dir: &Path) -> Result<()> {
        Self {
            dir: dir.to_path_buf(),
        }
        .reject_m1_write()
    }
    fn reject_m1_write(&self) -> Result<()> {
        // 损坏的新版指针仍可用 PREVIOUS 恢复。能确认是 M1 时在建立锁文件前拒绝。
        ensure!(
            !matches!(self.read_pointer(), Ok(Some(pointer)) if pointer.version == 1),
            "M1 索引目录仅供读取，请迁移到全新目录"
        );
        Ok(())
    }
    fn lock(&self) -> Result<File> {
        let file = xs_platform::open_or_create_local_lock(&self.dir.join("writer.lock"))?;
        file.try_lock_exclusive()
            .context("another indexing writer owns the store; retry after it finishes")?;
        Ok(file)
    }
    pub fn has_snapshot(&self) -> Result<bool> {
        // Path::exists follows links and hides permission / invalid-path errors.
        // A checked read pins the current inode and distinguishes only absence.
        match xs_platform::open_local_read(&self.dir.join("CURRENT.json")) {
            Ok(_) => Ok(true),
            Err(xs_platform::PlatformError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }
    fn read_pointer(&self) -> Result<Option<Pointer>> {
        let Some(pointer) = read_json::<Pointer>(&self.dir.join("CURRENT.json"), MAX_POINTER)?
        else {
            return Ok(None);
        };
        ensure!(
            pointer.version == 2 || (pointer.version == 1 && pointer.history.is_empty()),
            "invalid CURRENT pointer"
        );
        validate_file(&pointer.current(), pointer.version == 1)?;
        validate_history(pointer.generation, &pointer.history)?;
        Ok(Some(pointer))
    }
    fn read_recovery(&self) -> Result<Option<RecoveryPointer>> {
        let recovery = read_json::<RecoveryPointer>(&self.dir.join("PREVIOUS.json"), MAX_POINTER)?;
        if let Some(recovery) = &recovery {
            validate_history(recovery.newest_generation, &recovery.retained)?;
        }
        Ok(recovery)
    }
    /// Read only the bounded, validated publication cursor. This deliberately
    /// does not map, checksum, or inspect the referenced snapshot. Readers must
    /// still call load to validate its contents before serving any records.
    pub fn current_generation(&self) -> Result<Option<u64>> {
        Ok(self.read_pointer()?.map(|pointer| pointer.generation))
    }
    pub fn load(&self) -> Result<Index> {
        let mut pointer = self
            .read_pointer()?
            .context("no snapshot has been published")?;
        for _ in 0..2 {
            match self.load_generation(&pointer.current()) {
                Ok(index) => return Ok(index),
                Err(error) => match self.read_pointer()? {
                    Some(newer) if pointer.generation < newer.generation => pointer = newer,
                    _ => return Err(error),
                },
            }
        }
        self.load_generation(&pointer.current())
    }

    fn load_snapshot(&self, file: &GenerationFile) -> Result<Index> {
        validate_file(file, true)?;
        let index = Index::open_snapshot(self.dir.join(&file.file))?;
        ensure!(
            index.generation() == file.generation,
            "CURRENT generation mismatch"
        );
        Ok(index)
    }

    fn read_catalog(&self, file: &GenerationFile) -> Result<CatalogFile> {
        validate_file(file, false)?;
        ensure!(file.file.ends_with(".xsc"), "invalid catalog filename");
        let bytes = read_bounded(&self.dir.join(&file.file), MAX_CATALOG)?
            .context("missing catalog generation")?;
        ensure!(
            bytes.len() >= 12 && &bytes[..8] == CATALOG_MAGIC,
            "invalid catalog header"
        );
        ensure!(
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()) == crc32fast::hash(&bytes[12..]),
            "catalog checksum mismatch"
        );
        let manifest: CatalogFile = serde_json::from_slice(&bytes[12..])?;
        ensure!(
            manifest.version == 1 && (2..=MAX_PARTS).contains(&manifest.parts.len()),
            "invalid catalog manifest"
        );
        ensure!(
            manifest
                .parts
                .last()
                .is_some_and(|part| part.generation == file.generation),
            "catalog generation mismatch"
        );
        for part in &manifest.parts {
            validate_file(part, true)?;
        }
        Ok(manifest)
    }

    fn load_generation(&self, file: &GenerationFile) -> Result<Index> {
        if file.file.ends_with(".xsi") {
            return self.load_snapshot(file);
        }
        let manifest = self.read_catalog(file)?;
        let parts = manifest
            .parts
            .iter()
            .map(|part| self.load_snapshot(part))
            .collect::<Result<_>>()?;
        Catalog::assemble(
            file.generation,
            manifest.scope_token,
            manifest.partial_coverage,
            manifest.journal,
            parts,
            manifest.deleted,
        )
    }

    pub fn publish(&self, index: &Index) -> Result<PublicationOutcome> {
        self.reject_m1_write()?;
        let _lock = self.lock()?;
        let previous = self.read_pointer()?;
        if let Some(previous) = &previous {
            ensure!(
                previous.version != 1,
                "M1 索引目录仅供读取，请迁移到全新目录"
            );
            ensure!(
                index.generation() > previous.generation,
                "stale generation; another scan has already published"
            );
        }
        let history = previous
            .iter()
            .flat_map(Pointer::retained)
            .take(HISTORY)
            .collect();
        self.publish_full(index, history)
    }

    fn allocate_file(&self, generation: u64, extension: &str) -> Result<GenerationFile> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let file = format!(
            "generation-{generation}-{}-{nonce}.{extension}",
            std::process::id()
        );
        Ok(GenerationFile { file, generation })
    }

    fn publish_full(
        &self,
        index: &Index,
        history: Vec<GenerationFile>,
    ) -> Result<PublicationOutcome> {
        let file = self.allocate_file(index.generation(), "xsi")?;
        let snapshot_write = index.write_snapshot(self.dir.join(&file.file))?;
        self.commit(file, history, snapshot_write)
    }

    fn commit(
        &self,
        file: GenerationFile,
        history: Vec<GenerationFile>,
        snapshot_write: LocalWriteOutcome,
    ) -> Result<PublicationOutcome> {
        let path = self.dir.join(&file.file);
        validate_history(file.generation, &history)?;
        let stale = self
            .read_pointer()
            .ok()
            .flatten()
            .into_iter()
            .flat_map(|pointer| pointer.retained().collect::<Vec<_>>())
            .chain(
                self.read_recovery()
                    .ok()
                    .flatten()
                    .into_iter()
                    .flat_map(|backup| backup.retained),
            )
            .collect::<Vec<_>>();
        // 备份只列曾发布过的代次。即使 CURRENT 断裂也不会激活未提交的载荷。
        atomic_write(
            &self.dir.join("PREVIOUS.json"),
            &serde_json::to_vec(&RecoveryPointer {
                newest_generation: file.generation,
                retained: history.clone(),
            })?,
        )?;
        let pointer = Pointer {
            version: 2,
            file: file.file,
            generation: file.generation,
            history,
        };
        let pointer_write = atomic_write(
            &self.dir.join("CURRENT.json"),
            &serde_json::to_vec_pretty(&pointer)?,
        )?;
        // A later successful fsync of this same directory also persists the
        // earlier generation-file entry. Warn only when the final directory
        // synchronization after CURRENT's commit is unconfirmed.
        let durable = pointer_write.is_durable();
        let warning = if durable {
            None
        } else {
            Some(match (snapshot_write.warning(), pointer_write.warning()) {
                (Some(snapshot), Some(pointer)) if snapshot != pointer => {
                    format!("snapshot: {snapshot}; CURRENT: {pointer}")
                }
                (_, Some(pointer)) => pointer.to_owned(),
                (Some(snapshot), None) => snapshot.to_owned(),
                (None, None) => "CURRENT 已提交，但目录持久化未确认。".to_owned(),
            })
        };
        if durable {
            // 只有 CURRENT 的目录持久化确认后才回收，以免掉电后旧指针引用已删除文件。
            self.prune_after_commit(&pointer, stale);
        }
        Ok(PublicationOutcome {
            path,
            generation: pointer.generation,
            durable,
            warning,
        })
    }

    fn retain_physical(&self, entry: &GenerationFile, files: &mut HashSet<String>) -> Result<()> {
        files.insert(entry.file.clone());
        if entry.file.ends_with(".xsc") {
            for part in self.read_catalog(entry)?.parts {
                files.insert(part.file);
            }
        }
        Ok(())
    }

    fn prune_after_commit(&self, pointer: &Pointer, stale: Vec<GenerationFile>) {
        let mut protected = HashSet::new();
        for entry in pointer.retained() {
            if self.retain_physical(&entry, &mut protected).is_err() {
                return;
            }
        }
        let queue_path = self.dir.join("PRUNE.json");
        let mut candidates = read_json::<Vec<GenerationFile>>(&queue_path, 64 * 1024)
            .ok()
            .flatten()
            .unwrap_or_default();
        candidates.truncate(MAX_PRUNE_QUEUE);
        for entry in stale {
            if !protected.contains(&entry.file) {
                if entry.file.ends_with(".xsc") {
                    if let Ok(catalog) = self.read_catalog(&entry) {
                        candidates.extend(catalog.parts);
                    }
                }
                candidates.push(entry);
            }
        }
        let had_candidates = !candidates.is_empty();
        let mut visited = HashSet::new();
        let mut retry = Vec::new();
        for entry in candidates {
            if validate_file(&entry, false).is_err()
                || entry.generation >= pointer.generation
                || protected.contains(&entry.file)
                || !visited.insert(entry.file.clone())
            {
                continue;
            }
            match xs_platform::remove_local_file(&self.dir.join(&entry.file)) {
                Ok(()) => {}
                Err(xs_platform::PlatformError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) if retry.len() < MAX_PRUNE_QUEUE => retry.push(entry),
                Err(_) => {}
            }
        }
        if had_candidates {
            if let Ok(bytes) = serde_json::to_vec(&retry) {
                let _ = atomic_write(&queue_path, &bytes);
            }
        }
    }

    /// 增量 ID 只属于 expected_generation。调用方以删除加新增表达原路径的替换。
    /// 达到段数或删除数量上限时按现有物理顺序合并，保留同分排序与分页。
    pub fn publish_delta(
        &self,
        expected_generation: u64,
        deleted_ids: &[u32],
        additions: &Index,
    ) -> Result<PublicationOutcome> {
        self.publish_delta_impl(expected_generation, deleted_ids, additions, false)
    }

    pub(crate) fn finish_initial_scan(
        &self,
        expected_generation: u64,
        completion: &Index,
    ) -> Result<PublicationOutcome> {
        self.publish_delta_impl(expected_generation, &[], completion, true)
    }

    fn publish_delta_impl(
        &self,
        expected_generation: u64,
        deleted_ids: &[u32],
        additions: &Index,
        initial_scan_finished: bool,
    ) -> Result<PublicationOutcome> {
        self.reject_m1_write()?;
        let _lock = self.lock()?;
        let pointer = self
            .read_pointer()?
            .context("增量发布需要完整基段或首扫批次")?;
        ensure!(
            pointer.version != 1,
            "M1 索引目录仅供读取，请迁移到全新目录"
        );
        ensure!(
            pointer.generation == expected_generation
                && additions.generation() > expected_generation,
            "增量基代次已过期"
        );
        let current = self.load_generation(&pointer.current())?;
        ensure!(
            current.scope_token() == additions.scope_token() && additions.catalog.is_none(),
            "增量范围或来源不一致"
        );
        ensure!(
            deleted_ids.windows(2).all(|pair| pair[0] < pair[1])
                && deleted_ids
                    .last()
                    .is_none_or(|id| (*id as usize) < current.len()),
            "增量删除 ID 必须有效并严格递增"
        );
        if let Some(previous) = current.journal_checkpoint() {
            let checkpoint = additions
                .journal_checkpoint()
                .context("日志增量缺少新游标")?;
            ensure!(
                checkpoint.volume_id == previous.volume_id
                    && checkpoint.journal_id == previous.journal_id
                    && checkpoint.cursor >= previous.cursor,
                "日志更换或游标退回，需要重新扫描"
            );
        }
        let history = pointer.retained().take(HISTORY).collect();
        let complete =
            (initial_scan_finished || current.coverage_complete()) && additions.coverage_complete();
        let mut deleted = Vec::new();
        let mut parts = if let Some(catalog) = &current.catalog {
            deleted.extend_from_slice(catalog.deleted());
            self.read_catalog(&pointer.current())?.parts
        } else {
            vec![pointer.current()]
        };
        let compact = parts.len() == MAX_PARTS
            || deleted.len().saturating_add(deleted_ids.len()) > MAX_DELETED;
        if compact {
            let mut builder = IndexBuilder::new(additions.scope_token().to_owned());
            builder.set_coverage_complete(complete);
            builder.set_journal_checkpoint(additions.journal_checkpoint().cloned());
            let mut deletes = deleted_ids.iter().copied().peekable();
            for id in 0..current.len() as u32 {
                if deletes.peek() == Some(&id) {
                    deletes.next();
                } else {
                    builder.add_from_index(&current, id)?;
                }
            }
            for id in 0..additions.len() as u32 {
                builder.add_from_index(additions, id)?;
            }
            return self.publish_full(&builder.finish(additions.generation())?, history);
        }
        let physical_count = current.len() + deleted.len();
        ensure!(
            physical_count.saturating_add(additions.len()) <= u32::MAX as usize,
            "增量 ID 空间耗尽，需要重新扫描"
        );
        deleted.extend(deleted_ids.iter().map(|&id| {
            current
                .catalog
                .as_ref()
                .map_or(id, |catalog| catalog.physical_id(id))
        }));
        deleted.sort_unstable();
        let segment = self.allocate_file(additions.generation(), "xsi")?;
        additions.write_snapshot(self.dir.join(&segment.file))?;
        parts.push(segment);
        let catalog = CatalogFile {
            version: 1,
            scope_token: additions.scope_token().to_owned(),
            partial_coverage: !complete,
            journal: additions.journal_checkpoint().cloned(),
            parts,
            deleted,
        };
        let file = self.allocate_file(additions.generation(), "xsc")?;
        let bytes = serde_json::to_vec(&catalog)?;
        ensure!(
            bytes.len() as u64 + 12 <= MAX_CATALOG,
            "incremental manifest exceeds budget"
        );
        let write = xs_platform::write_local_new(&self.dir.join(&file.file), |out| {
            out.write_all(CATALOG_MAGIC)?;
            out.write_all(&crc32fast::hash(&bytes).to_le_bytes())?;
            out.write_all(&bytes)
        })?;
        self.commit(file, history, write)
    }

    /// 从已提交、完整且仍符合当前范围的代次恢复。损坏或半扫代次不参与。
    pub fn recover(&self, scope_token: &str) -> Result<PublicationOutcome> {
        self.reject_m1_write()?;
        let _lock = self.lock()?;
        self.reject_m1_write()?;
        let pointer = self.read_pointer().ok().flatten();
        let recovery = self.read_recovery().ok().flatten();
        let newest = pointer
            .as_ref()
            .map(|p| p.generation)
            .into_iter()
            .chain(recovery.as_ref().map(|r| r.newest_generation))
            .max()
            .context("没有可验证的索引发布记录")?;
        let next = newest.checked_add(1).context("generation exhausted")?;
        let mut candidates: Vec<_> = pointer
            .iter()
            .flat_map(Pointer::retained)
            .chain(recovery.into_iter().flat_map(|r| r.retained))
            .collect();
        candidates.sort_by_key(|file| std::cmp::Reverse(file.generation));
        candidates.dedup();
        for candidate in &candidates {
            let Ok(index) = self.load_generation(candidate) else {
                continue;
            };
            if index.scope_token() != scope_token
                || !index.coverage_complete()
                || (scope_token.is_empty() && index.has_local_records())
            {
                continue;
            }
            let mut builder = IndexBuilder::new(scope_token.to_owned());
            builder.set_journal_checkpoint(index.journal_checkpoint().cloned());
            for id in 0..index.len() as u32 {
                builder.add_from_index(&index, id)?;
            }
            return self.publish_full(&builder.finish(next)?, vec![candidate.clone()]);
        }
        bail!("没有完整且符合当前范围的历史索引，请重新扫描")
    }
}

/// Write through a pinned sibling temporary file. A successful result always
/// means the name was committed, even when durability remains unconfirmed.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<LocalWriteOutcome> {
    let parent = path.parent().context("path must have a parent")?;
    xs_platform::create_local_dir_all(parent)?;
    Ok(xs_platform::atomic_write_local(path, bytes, true)?)
}

impl Index {
    pub fn write_snapshot(&self, path: impl AsRef<Path>) -> Result<LocalWriteOutcome> {
        ensure!(
            self.catalog.is_none(),
            "分段目录必须由 Store 写入或合并后导出"
        );
        let path = path.as_ref();
        xs_platform::validate_local_path(path, true)?;
        let manifest = if self.manifest.parents.len() >= 64 {
            let mut compact = self.manifest.clone();
            compact.parent_nodes = crate::index::encode_parent_trie(&compact.parents);
            compact.parents.clear();
            serde_json::to_vec(&compact)?
        } else {
            serde_json::to_vec(&self.manifest)?
        };
        ensure!(manifest.len() <= MAX_MANIFEST, "manifest budget exceeded");
        ensure!(
            self.payload().len() as u64 <= MAX_SNAPSHOT
                && HEADER as u64 + manifest.len() as u64 + self.payload().len() as u64
                    <= MAX_SNAPSHOT,
            "snapshot size outside budget"
        );
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&manifest);
        hasher.update(self.payload());
        let checksum = hasher.finalize();
        let mut header = Vec::with_capacity(HEADER);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&2u32.to_le_bytes());
        header.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        header.extend_from_slice(&(self.payload().len() as u64).to_le_bytes());
        header.extend_from_slice(&checksum.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        // Creation, commit and failure cleanup retain the verified parent.
        // Stream the payload without duplicating the complete snapshot in RAM.
        Ok(xs_platform::write_local_new(path, |file| {
            file.write_all(&header)?;
            file.write_all(&manifest)?;
            file.write_all(self.payload())
        })?)
    }
    pub fn open_snapshot(path: impl AsRef<Path>) -> Result<Self> {
        let file = xs_platform::open_local_read(path.as_ref())?;
        let len = file.metadata()?.len();
        ensure!(
            (HEADER as u64..=MAX_SNAPSHOT).contains(&len),
            "snapshot size outside budget"
        );
        // SAFETY: generations are immutable, private, create-only files. Writers
        // publish new files rather than modifying a mapped generation. External
        // processes must not modify/truncate .xsi files while readers are active.
        let map = unsafe { memmap2::MmapOptions::new().map(&file) }?;
        ensure!(&map[..8] == MAGIC, "not a Xunshu Rust snapshot");
        let u32at = |i| u32::from_le_bytes(map[i..i + 4].try_into().unwrap());
        ensure!(
            matches!(u32at(8), 1 | 2) && u32at(28) == 0,
            "unsupported snapshot version"
        );
        let manifest_len = u32at(12) as usize;
        let payload_len = u64::from_le_bytes(map[16..24].try_into().unwrap());
        ensure!(
            manifest_len <= MAX_MANIFEST
                && payload_len <= MAX_SNAPSHOT
                && HEADER as u64 + manifest_len as u64 + payload_len == len,
            "snapshot length mismatch"
        );
        ensure!(
            crc32fast::hash(&map[HEADER..]) == u32at(24),
            "snapshot checksum mismatch; original data retained"
        );
        let mut manifest: Manifest = serde_json::from_slice(&map[HEADER..HEADER + manifest_len])?;
        if manifest.parents.is_empty() && !manifest.parent_nodes.is_empty() {
            manifest.parents = crate::index::decode_parent_trie(&manifest.parent_nodes)?;
            manifest.parent_nodes.clear();
        }
        ensure!(
            u32at(8) != 1
                || (!manifest.rle_postings
                    && !manifest.partial_coverage
                    && manifest.journal.is_none()
                    && manifest.revisions.is_none()
                    && manifest.properties.is_none()
                    && manifest.packed_records.is_none()
                    && manifest.name_prefixes.is_empty()),
            "unsupported M1 manifest fields"
        );
        let parent_depths = crate::index::parent_depths(&manifest.parents);
        let record_width = manifest
            .packed_records
            .as_ref()
            .map_or(RECORD_BYTES, PackedRecords::width);
        let index = Self {
            storage: Storage::Mapped(map),
            payload_start: HEADER + manifest_len,
            manifest,
            parent_depths,
            catalog: None,
            record_width,
        };
        index.validate()?;
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileRecord, IndexBuilder, SearchOptions};
    use std::fs;

    fn index(generation: u64) -> Index {
        let mut b = IndexBuilder::default();
        b.add(FileRecord {
            path: "/demo/报告.docx".into(),
            size: 10,
            modified: 123,
            is_dir: false,
            source: "offline:test".into(),
        })
        .unwrap();
        b.finish(generation).unwrap()
    }
    #[test]
    fn published_snapshot_is_mapped_and_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(!store.has_snapshot().unwrap());
        assert_eq!(store.current_generation().unwrap(), None);
        let publication = store.publish(&index(1)).unwrap();
        assert!(publication.durable);
        assert!(publication.warning.is_none());
        assert!(store.has_snapshot().unwrap());
        assert_eq!(store.current_generation().unwrap(), Some(1));
        let i = store.load().unwrap();
        assert!(i.is_mapped());
        assert_eq!(
            i.search("报告", &SearchOptions::default(), None)
                .unwrap()
                .total,
            1
        );
    }
    #[test]
    fn stale_writer_cannot_replace_newer_generation() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.publish(&index(2)).unwrap();
        let pointer = fs::read(dir.path().join("CURRENT.json")).unwrap();
        assert!(s.publish(&index(1)).is_err());
        assert_eq!(pointer, fs::read(dir.path().join("CURRENT.json")).unwrap());
        assert_eq!(s.load().unwrap().generation(), 2);
    }
    #[test]
    fn mapped_reader_retains_its_generation_after_publish() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let first = s.publish(&index(1)).unwrap();
        let reader = s.load().unwrap();
        s.publish(&index(2)).unwrap();
        assert_eq!(reader.generation(), 1);
        assert_eq!(reader.record(0).unwrap().path, "/demo/报告.docx");
        assert!(first.path.exists());
        assert_eq!(s.load().unwrap().generation(), 2);
    }
    #[test]
    fn bit_flip_detected_and_immutable_write_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.xsi");
        index(1).write_snapshot(&p).unwrap();
        assert!(index(2).write_snapshot(&p).is_err());
        let mut bytes = fs::read(&p).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 1;
        fs::write(&p, bytes).unwrap();
        assert!(Index::open_snapshot(&p).is_err());
    }
    #[test]
    fn bad_pointer_cannot_escape_store() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        fs::write(
            dir.path().join("CURRENT.json"),
            br#"{"version":1,"file":"../secret","generation":1}"#,
        )
        .unwrap();
        assert!(s.load().is_err());
        assert!(s.current_generation().is_err());
    }
    #[test]
    fn publication_cursor_does_not_map_a_corrupt_previous_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let first = store.publish(&index(1)).unwrap();
        let mut bytes = fs::read(&first.path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&first.path, bytes).unwrap();
        assert_eq!(store.current_generation().unwrap(), Some(1));
        assert!(
            store.load().is_err(),
            "queries must still verify the snapshot"
        );
        store.publish(&index(2)).unwrap();
        assert_eq!(store.current_generation().unwrap(), Some(2));
        assert_eq!(store.load().unwrap().generation(), 2);
        assert!(first.path.exists(), "old generation retained for diagnosis");
    }
    #[test]
    fn publication_cursor_rejects_oversize_or_invalid_pointers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let pointer = dir.path().join("CURRENT.json");
        fs::write(&pointer, vec![b' '; MAX_POINTER as usize + 1]).unwrap();
        assert!(store.current_generation().is_err());
        for file in [
            "../escape.xsi",
            "generation-/escape.xsi",
            "generation-a\0.xsi",
            "generation-a:stream.xsi",
        ] {
            let bytes = serde_json::to_vec(&Pointer {
                version: 1,
                file: file.into(),
                generation: 9,
                history: Vec::new(),
            })
            .unwrap();
            fs::write(&pointer, bytes).unwrap();
            assert!(store.current_generation().is_err(), "{file:?}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn pointer_and_lock_symlinks_are_errors_not_absence() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"synthetic").unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        symlink(&target, store.directory().join("CURRENT.json")).unwrap();
        assert!(store.has_snapshot().is_err());
        assert!(store.current_generation().is_err());
        assert!(store.load().is_err());
        symlink(&target, store.directory().join("writer.lock")).unwrap();
        assert!(store.publish(&index(1)).is_err());
        assert_eq!(fs::read(target).unwrap(), b"synthetic");
    }
}
