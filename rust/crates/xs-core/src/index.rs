//! Immutable compact filename index. Query semantics live exclusively in xs-query.
use crate::catalog::Catalog;
use crate::names::{self, MAX_NAME_BYTES, MAX_PREFIXES, MAX_PREFIX_BYTES, PREFIX_FLAG};
use crate::records::{ColdRecords, PackedRecords, RECORD_BYTES};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashMap},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use xs_platform::{FileIdentity, FileRevision, FileTimestamp};
use xs_query::{CandidatePlan, EntryRef, Query, QueryMetadata};

pub(crate) const GRAM_BYTES: usize = 24;
const REVISION_BYTES: usize = 52;
const PROPERTY_BYTES: usize = 21;
pub(crate) const MAX_SNAPSHOT: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub path: String,
    pub size: u64,
    pub modified: i64,
    pub is_dir: bool,
    pub source: String,
}

/// Sparse catalog values retained verbatim for lossless EFU/schema4 migration.
/// Local filename records do not allocate or store this optional payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct CatalogMetadata {
    pub fields: BTreeMap<String, String>,
}

/// 原生文件属性独立于离线清单的原始字段和文件修订。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileProperties {
    pub created: Option<FileTimestamp>,
    pub attributes: Option<u32>,
}
const MAX_METADATA: usize = 1024 * 1024;
fn validate_metadata(metadata: &CatalogMetadata) -> Result<()> {
    ensure!(
        metadata.fields.len() <= 256,
        "catalog has more than 256 columns"
    );
    for name in metadata.fields.keys() {
        ensure!(
            !name.is_empty() && name.len() <= 4096 && !name.contains('\0'),
            "invalid catalog column name"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct Section {
    pub start: u64,
    pub len: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParentNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    pub segment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub generation: u64,
    pub count: u32,
    pub scope_token: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_nodes: Vec<ParentNode>,
    pub sources: Vec<String>,
    pub records: Section,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packed_records: Option<PackedRecords>,
    pub names: Section,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub name_prefixes: Vec<String>,
    pub grams: Section,
    pub postings: Section,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Section>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revisions: Option<Section>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<Section>,
    /// M1 没有覆盖标志，其已发布快照等同于完整覆盖。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial_coverage: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rle_postings: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<JournalCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_records: Option<Section>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packed_cold: Option<ColdRecords>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ext_postings: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinyin_postings: bool,
}

/// 与可查询索引同批提交的卷日志位置。日志替换或截断时由采集器重建。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JournalCheckpoint {
    pub volume_id: String,
    pub journal_id: u64,
    pub cursor: i64,
}
impl JournalCheckpoint {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.volume_id.is_empty()
                && self.volume_id.len() <= 4096
                && !self.volume_id.contains('\0')
                && self.cursor >= 0,
            "invalid journal checkpoint"
        );
        Ok(())
    }
}

pub(crate) enum Storage {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}
impl Storage {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Owned(v) => v,
            Self::Mapped(v) => v,
        }
    }
}

pub struct Index {
    pub(crate) storage: Storage,
    pub(crate) payload_start: usize,
    pub(crate) manifest: Manifest,
    pub(crate) parent_depths: Vec<usize>,
    pub(crate) catalog: Option<Catalog>,
    pub(crate) record_width: usize,
}

struct PostingBuilder {
    data: Vec<u8>,
    previous: u32,
    count: u32,
    gap: u32,
    run: u32,
}
impl PostingBuilder {
    fn flush_run(&mut self) {
        if self.run >= 4 {
            self.data.push(0);
            put_varint(self.run, &mut self.data);
            put_varint(self.gap, &mut self.data);
        } else {
            for _ in 0..self.run {
                put_varint(self.gap, &mut self.data);
            }
        }
        self.run = 0;
    }

    fn add(&mut self, id: u32) {
        if self.count == 0 {
            // 第一个 ID 可为零，后续零保留给等间隔重复编码。
            put_varint(id, &mut self.data);
        } else {
            let gap = id - self.previous;
            if gap != self.gap {
                self.flush_run();
                self.gap = gap;
            }
            self.run += 1;
        }
        self.previous = id;
        self.count += 1;
    }
}
pub struct IndexBuilder {
    records: Vec<u8>,
    names: Vec<u8>,
    metadata: Vec<u8>,
    parents: Vec<String>,
    sources: Vec<String>,
    parent_ids: HashMap<String, u32>,
    source_ids: HashMap<String, u32>,
    postings: BTreeMap<u64, PostingBuilder>,
    count: u32,
    scope_token: String,
    partial_coverage: bool,
    journal: Option<JournalCheckpoint>,
    revisions: Vec<u8>,
    properties: Vec<u8>,
}

fn gram_key(chars: &[char]) -> u64 {
    let mut h = 0xcbf29ce484222325u64 ^ chars.len() as u64;
    for c in chars {
        for b in (*c as u32).to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    h
}

fn ext_gram_key(ext: &str) -> u64 {
    let mut h = 0xaf63dc4c8601ec8cu64 ^ ext.len() as u64;
    for b in ext.as_bytes() {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

fn pinyin_gram_key(bytes: &[u8]) -> u64 {
    let mut h = 0x517cc1b727220a95u64 ^ bytes.len() as u64;
    for b in bytes {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}

fn pinyin_initial_keys(text: &str) -> Vec<u64> {
    let bytes = text.as_bytes();
    let mut keys = Vec::new();
    if bytes.len() >= 2 {
        for w in bytes.windows(2) {
            keys.push(pinyin_gram_key(w));
        }
    }
    if bytes.len() >= 3 {
        for w in bytes.windows(3) {
            keys.push(pinyin_gram_key(w));
        }
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}
fn cjk(c: char) -> bool {
    matches!(c as u32,0x3400..=0x9fff|0x20000..=0x3134f|0xf900..=0xfaff)
}
fn grams(text: &str) -> Vec<u64> {
    let chars: Vec<_> = text.chars().collect();
    let mut out: Vec<u64> = chars.windows(3).map(gram_key).collect();
    for c in &chars {
        if cjk(*c) {
            out.push(gram_key(&[*c]));
        }
    }
    for pair in chars.windows(2) {
        if pair.iter().all(|c| cjk(*c)) {
            out.push(gram_key(pair));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}
fn put_varint(mut value: u32, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}
fn take_varint(data: &mut &[u8]) -> Result<u32> {
    let mut value = 0;
    for shift in (0..=28).step_by(7) {
        let (b, tail) = data.split_first().context("truncated posting varint")?;
        *data = tail;
        ensure!(shift != 28 || *b <= 15, "invalid posting varint");
        value |= ((*b & 127) as u32) << shift;
        if *b & 128 == 0 {
            ensure!(shift == 0 || *b != 0, "overlong posting varint");
            return Ok(value);
        }
    }
    unreachable!("最后一个字节的取值已受限")
}
fn visit_postings(
    mut data: &[u8],
    count: u32,
    max: u32,
    rle: bool,
    budget: Option<&SearchBudget<'_>>,
    mut visit: impl FnMut(u32),
) -> Result<()> {
    let mut found = 0u32;
    let mut previous = 0u32;
    while !data.is_empty() {
        let value = take_varint(&mut data)?;
        let (run, gap) = if rle && found != 0 && value == 0 {
            let run = take_varint(&mut data)?;
            let gap = take_varint(&mut data)?;
            ensure!(run >= 4, "invalid posting run length");
            (run, gap)
        } else {
            (1, value)
        };
        ensure!(
            run <= count - found && (found == 0 || gap > 0),
            "invalid posting count or gap"
        );
        let last = (run as u64) * (gap as u64) + u64::from(previous);
        ensure!(last < u64::from(max), "posting ID outside record range");
        // 快照校验无需逐项展开已验证边界的长重复段。
        if budget.is_none() {
            found += run;
            previous = last as u32;
            continue;
        }
        for _ in 0..run {
            if found.is_multiple_of(1024) {
                if let Some(budget) = budget {
                    budget.check()?;
                }
            }
            previous += gap;
            visit(previous);
            found += 1;
        }
    }
    ensure!(found == count, "truncated posting list");
    Ok(())
}
fn decode_postings(
    data: &[u8],
    count: u32,
    max: u32,
    rle: bool,
    budget: &SearchBudget<'_>,
) -> Result<Vec<u32>> {
    budget.check()?;
    let mut out = Vec::with_capacity(count.min(max) as usize);
    visit_postings(data, count, max, rle, Some(budget), |id| out.push(id))?;
    Ok(out)
}
fn read32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn read64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

/// Path identity is lexical and independent of the operating system hosting an EFU catalog.
pub fn split_path(path: &str) -> Result<(&str, &str)> {
    ensure!(
        !path.is_empty() && !path.contains('\0'),
        "empty path or NUL"
    );
    // A backslash is a legal character in a POSIX filename. Only Windows
    // lexical paths use it as a separator, independent of the host OS.
    let windows = path.starts_with("\\\\")
        || path.starts_with("//")
        || (path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && path.as_bytes().get(1) == Some(&b':'));
    let separator = if windows {
        path.rfind(['/', '\\'])
    } else {
        path.rfind('/')
    };
    match separator {
        Some(i) => Ok((&path[..=i], &path[i + 1..])),
        None => Ok(("", path)),
    }
}
fn intern(value: &str, items: &mut Vec<String>, ids: &mut HashMap<String, u32>) -> Result<u32> {
    if let Some(id) = ids.get(value) {
        return Ok(*id);
    }
    let id = u32::try_from(items.len()).context("too many interned strings")?;
    items.push(value.to_owned());
    ids.insert(value.to_owned(), id);
    Ok(id)
}
impl Default for IndexBuilder {
    fn default() -> Self {
        Self::new(String::new())
    }
}
impl IndexBuilder {
    pub fn new(scope_token: String) -> Self {
        Self {
            records: Vec::new(),
            names: Vec::new(),
            metadata: Vec::new(),
            parents: Vec::new(),
            sources: Vec::new(),
            parent_ids: HashMap::new(),
            source_ids: HashMap::new(),
            postings: BTreeMap::new(),
            count: 0,
            scope_token,
            partial_coverage: false,
            journal: None,
            revisions: Vec::new(),
            properties: Vec::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.count as usize
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn set_coverage_complete(&mut self, complete: bool) {
        self.partial_coverage = !complete;
    }
    pub fn set_journal_checkpoint(&mut self, checkpoint: Option<JournalCheckpoint>) {
        self.journal = checkpoint;
    }
    pub fn add(&mut self, entry: FileRecord) -> Result<()> {
        self.add_with_metadata(entry, None)
    }
    pub fn add_from_index(&mut self, index: &Index, id: u32) -> Result<()> {
        self.add_record_with_properties(
            index.record(id)?,
            index.record_metadata(id)?,
            index.record_revision(id)?,
            index.record_properties(id)?,
        )
    }
    pub fn add_with_metadata(
        &mut self,
        entry: FileRecord,
        metadata: Option<CatalogMetadata>,
    ) -> Result<()> {
        self.add_record(entry, metadata, None)
    }
    pub fn add_record(
        &mut self,
        entry: FileRecord,
        metadata: Option<CatalogMetadata>,
        revision: Option<FileRevision>,
    ) -> Result<()> {
        self.add_record_with_properties(entry, metadata, revision, FileProperties::default())
    }

    pub fn add_record_with_properties(
        &mut self,
        entry: FileRecord,
        metadata: Option<CatalogMetadata>,
        revision: Option<FileRevision>,
        properties: FileProperties,
    ) -> Result<()> {
        ensure!(self.count < u32::MAX, "record ID space exhausted");
        ensure!(
            entry.path.len() <= 32768 && !entry.source.contains('\0') && entry.source.len() <= 4096,
            "path/source too long or invalid"
        );
        if let Some(revision) = &revision {
            ensure!(entry.source == "local", "离线记录不能携带本地文件身份");
            ensure!(revision.size == entry.size, "文件大小与修订不一致");
            ensure!(
                revision.modified.nanoseconds < 1_000_000_000
                    && revision.changed.nanoseconds < 1_000_000_000,
                "invalid file revision timestamp"
            );
        }
        if properties != FileProperties::default() {
            ensure!(entry.source == "local", "离线记录不能携带本地属性");
            ensure!(
                properties
                    .created
                    .is_none_or(|date| date.nanoseconds < 1_000_000_000),
                "invalid created timestamp"
            );
        }
        let (parent, name) = split_path(&entry.path)?;
        ensure!(
            name.len() <= MAX_NAME_BYTES && name.chars().count() <= 1024,
            "filename exceeds supported 4096-byte / 1024-character budget"
        );
        let name_len = u32::try_from(name.len())?;
        let name_start = u32::try_from(self.names.len()).context("name arena exceeds 4 GiB")?;
        ensure!(
            self.names
                .len()
                .checked_add(name.len())
                .is_some_and(|n| n <= u32::MAX as usize),
            "name arena exceeds 4 GiB"
        );
        let extra = metadata
            .filter(|m| !m.fields.is_empty())
            .map(|m| -> Result<Vec<u8>> {
                validate_metadata(&m)?;
                let bytes = serde_json::to_vec(&m)?;
                ensure!(
                    bytes.len() <= MAX_METADATA,
                    "catalog row metadata exceeds 1 MiB"
                );
                Ok(bytes)
            })
            .transpose()?;
        let metadata_offset = if let Some(bytes) = extra {
            let offset = u32::try_from(self.metadata.len())?
                .checked_add(1)
                .context("metadata arena exceeds 4 GiB")?;
            ensure!(
                self.metadata
                    .len()
                    .checked_add(bytes.len() + 4)
                    .is_some_and(|n| n <= u32::MAX as usize),
                "metadata arena exceeds 4 GiB"
            );
            self.metadata
                .extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            self.metadata.extend(bytes);
            offset
        } else {
            0
        };
        let pid = intern(parent, &mut self.parents, &mut self.parent_ids)?;
        let sid = intern(&entry.source, &mut self.sources, &mut self.source_ids)?;
        self.names.extend_from_slice(name.as_bytes());
        for value in [name_start, name_len, pid, sid] {
            self.records.extend_from_slice(&value.to_le_bytes());
        }
        self.records.extend_from_slice(&entry.size.to_le_bytes());
        self.records
            .extend_from_slice(&entry.modified.to_le_bytes());
        let folded = xs_query::fold(name);
        let flags = (entry.is_dir as u32) | if folded == name { 2 } else { 0 };
        self.records.extend_from_slice(&flags.to_le_bytes());
        self.records
            .extend_from_slice(&metadata_offset.to_le_bytes());
        let mut ext_keys = [0u64; 2];
        let mut ext_len = 0usize;
        for candidate_name in [name, folded.as_str()] {
            if let Some((_, raw_ext)) = candidate_name.rsplit_once('.') {
                if !raw_ext.is_empty() && raw_ext.len() <= 32 {
                    let ext_folded = xs_query::fold(raw_ext);
                    if !ext_folded.is_empty() {
                        let k = ext_gram_key(&ext_folded);
                        if !ext_keys[..ext_len].contains(&k) {
                            ext_keys[ext_len] = k;
                            ext_len += 1;
                        }
                    }
                }
            }
        }
        let py_keys = xs_query::pinyin_initials(name)
            .map(|init| pinyin_initial_keys(&init))
            .unwrap_or_default();
        for key in grams(&folded)
            .into_iter()
            .chain(ext_keys[..ext_len].iter().copied())
            .chain(py_keys)
        {
            let p = self.postings.entry(key).or_insert_with(|| PostingBuilder {
                data: Vec::new(),
                previous: 0,
                count: 0,
                gap: 0,
                run: 0,
            });
            p.add(self.count);
        }
        if let Some(revision) = revision {
            self.revisions.extend(self.count.to_le_bytes());
            self.revisions
                .extend(revision.identity.volume.to_le_bytes());
            self.revisions.extend(revision.identity.object);
            self.revisions
                .extend(revision.modified.seconds.to_le_bytes());
            self.revisions
                .extend(revision.modified.nanoseconds.to_le_bytes());
            self.revisions
                .extend(revision.changed.seconds.to_le_bytes());
            self.revisions
                .extend(revision.changed.nanoseconds.to_le_bytes());
        }
        if properties != FileProperties::default() {
            let flags = u8::from(properties.created.is_some())
                | (u8::from(properties.attributes.is_some()) << 1);
            let created = properties.created.unwrap_or(FileTimestamp {
                seconds: 0,
                nanoseconds: 0,
            });
            self.properties.extend(self.count.to_le_bytes());
            self.properties.push(flags);
            self.properties.extend(created.seconds.to_le_bytes());
            self.properties.extend(created.nanoseconds.to_le_bytes());
            self.properties
                .extend(properties.attributes.unwrap_or(0).to_le_bytes());
        }
        self.count += 1;
        Ok(())
    }
    pub fn finish(self, generation: u64) -> Result<Index> {
        let mut buffer = self.records;
        let mut name_data = self.names;
        let name_prefixes = names::compact(&mut buffer, &mut name_data);
        let cold_extracted = ColdRecords::extract(&buffer);
        if cold_extracted.is_some() {
            for row in buffer.as_chunks_mut::<RECORD_BYTES>().0 {
                row[16..32].fill(0);
            }
        }
        let packed_records = PackedRecords::compact(&mut buffer);
        let record_width = packed_records
            .as_ref()
            .map_or(RECORD_BYTES, PackedRecords::width);
        let record_len = buffer.len() as u64;
        let names = Section {
            start: buffer.len() as u64,
            len: name_data.len() as u64,
        };
        buffer.extend(name_data);
        let grams = Section {
            start: buffer.len() as u64,
            len: (self.postings.len() * GRAM_BYTES) as u64,
        };
        let mut posts = Vec::new();
        for (key, mut list) in self.postings {
            list.flush_run();
            buffer.extend_from_slice(&key.to_le_bytes());
            buffer.extend_from_slice(&(posts.len() as u64).to_le_bytes());
            buffer.extend_from_slice(&u32::try_from(list.data.len())?.to_le_bytes());
            buffer.extend_from_slice(&list.count.to_le_bytes());
            posts.extend(list.data);
        }
        let postings = Section {
            start: buffer.len() as u64,
            len: posts.len() as u64,
        };
        buffer.extend(posts);
        let metadata = if self.metadata.is_empty() {
            None
        } else {
            let section = Section {
                start: buffer.len() as u64,
                len: self.metadata.len() as u64,
            };
            buffer.extend(self.metadata);
            Some(section)
        };
        let revisions = if self.revisions.is_empty() {
            None
        } else {
            let section = Section {
                start: buffer.len() as u64,
                len: self.revisions.len() as u64,
            };
            buffer.extend(self.revisions);
            Some(section)
        };
        let properties = if self.properties.is_empty() {
            None
        } else {
            let section = Section {
                start: buffer.len() as u64,
                len: self.properties.len() as u64,
            };
            buffer.extend(self.properties);
            Some(section)
        };
        let (cold_records, packed_cold) = match cold_extracted {
            Some((layout, cold_bytes)) if packed_records.is_some() => {
                let section = Section {
                    start: buffer.len() as u64,
                    len: cold_bytes.len() as u64,
                };
                buffer.extend(cold_bytes);
                (Some(section), Some(layout))
            }
            _ => (None, None),
        };
        buffer.shrink_to_fit();
        let manifest = Manifest {
            generation,
            count: self.count,
            scope_token: self.scope_token,
            parents: self.parents,
            parent_nodes: Vec::new(),
            sources: self.sources,
            records: Section {
                start: 0,
                len: record_len,
            },
            packed_records,
            names,
            name_prefixes,
            grams,
            postings,
            metadata,
            revisions,
            properties,
            partial_coverage: self.partial_coverage,
            rle_postings: true,
            journal: self.journal,
            cold_records,
            packed_cold,
            ext_postings: true,
            pinyin_postings: true,
        };
        let parent_depths = parent_depths(&manifest.parents);
        let index = Index {
            storage: Storage::Owned(buffer),
            payload_start: 0,
            manifest,
            parent_depths,
            catalog: None,
            record_width,
        };
        index.validate()?;
        Ok(index)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchOptions {
    pub limit: usize,
    pub offset: usize,
    pub time_budget_ms: u64,
}
impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 100,
            offset: 0,
            time_budget_ms: 5000,
        }
    }
}
/// 全局列排序。相关度排序继续使用原有查询接口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Path,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnSort {
    pub column: SortColumn,
    pub descending: bool,
}

struct ColumnRank {
    sort: ColumnSort,
    path: String,
    name_offset: usize,
    source: String,
    size: u64,
    modified: i64,
    id: u32,
    score: i64,
}

impl ColumnRank {
    fn cmp_entry(&self, entry: &EntryRef<'_>) -> std::cmp::Ordering {
        let primary = match self.sort.column {
            SortColumn::Name => self.path[self.name_offset..].cmp(entry.name),
            SortColumn::Path => self.path.as_str().cmp(entry.path),
            SortColumn::Size => self.size.cmp(&entry.size),
            SortColumn::Modified => self.modified.cmp(&entry.modified),
        };
        let primary = if self.sort.descending {
            primary.reverse()
        } else {
            primary
        };
        primary
            .then_with(|| self.path.as_str().cmp(entry.path))
            .then_with(|| self.source.as_str().cmp(entry.source))
    }
}

impl Ord for ColumnRank {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        debug_assert_eq!(self.sort, other.sort);
        self.cmp_entry(&EntryRef {
            name: &other.path[other.name_offset..],
            path: &other.path,
            size: other.size,
            modified: other.modified,
            source: &other.source,
            is_dir: false,
        })
        .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for ColumnRank {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ColumnRank {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for ColumnRank {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub id: u32,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub modified: i64,
    pub is_dir: bool,
    pub source: String,
    pub score: i64,
}
/// 搜索结果跨代次的来源与路径标识。数字 ID 只在其索引代次内有效。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordKey {
    pub path: String,
    pub source: String,
}
impl SearchHit {
    pub fn key(&self) -> RecordKey {
        RecordKey {
            path: self.path.clone(),
            source: self.source.clone(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub generation: u64,
    pub results: Vec<SearchHit>,
    pub total: usize,
    pub complete: bool,
    pub total_relation: String,
    pub candidate_count: usize,
    pub candidate_count_exact: bool,
    pub examined: usize,
    pub elapsed_ms: f64,
    pub index_entries: usize,
    #[serde(default = "complete_coverage")]
    pub coverage_complete: bool,
}
fn complete_coverage() -> bool {
    true
}
#[derive(Debug)]
struct SearchInterrupted;
impl std::fmt::Display for SearchInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("search interrupted")
    }
}
impl std::error::Error for SearchInterrupted {}
struct SearchBudget<'a> {
    start: Instant,
    milliseconds: u64,
    cancel: Option<&'a AtomicBool>,
}
impl SearchBudget<'_> {
    fn expired(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
            || self.start.elapsed().as_millis() >= self.milliseconds as u128
    }
    fn check(&self) -> Result<()> {
        if self.expired() {
            Err(SearchInterrupted.into())
        } else {
            Ok(())
        }
    }
}
impl Index {
    pub fn len(&self) -> usize {
        self.manifest.count as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn generation(&self) -> u64 {
        self.manifest.generation
    }
    pub fn scope_token(&self) -> &str {
        &self.manifest.scope_token
    }
    pub fn coverage_complete(&self) -> bool {
        !self.manifest.partial_coverage
    }
    pub fn journal_checkpoint(&self) -> Option<&JournalCheckpoint> {
        self.manifest.journal.as_ref()
    }
    pub fn storage_bytes(&self) -> usize {
        self.catalog
            .as_ref()
            .map_or_else(|| self.storage.bytes().len(), Catalog::storage_bytes)
    }
    pub fn is_mapped(&self) -> bool {
        self.catalog.as_ref().map_or(
            matches!(self.storage, Storage::Mapped(_)),
            Catalog::is_mapped,
        )
    }
    pub fn has_local_records(&self) -> bool {
        self.catalog.as_ref().map_or_else(
            || self.manifest.sources.iter().any(|s| s == "local"),
            Catalog::has_local_records,
        )
    }
    pub(crate) fn payload(&self) -> &[u8] {
        &self.storage.bytes()[self.payload_start..]
    }
    fn section(&self, s: &Section) -> &[u8] {
        &self.payload()[s.start as usize..(s.start + s.len) as usize]
    }
    pub(crate) fn validate(&self) -> Result<()> {
        let length = self.payload().len() as u64;
        ensure!(length <= MAX_SNAPSHOT, "snapshot exceeds limit");
        let m = &self.manifest;
        if let Some(layout) = &m.packed_records {
            layout.validate_layout()?;
        }
        ensure!(
            m.cold_records.is_some() == m.packed_cold.is_some(),
            "cold_records and packed_cold must both be present or absent"
        );
        if let (Some(section), Some(cold)) = (&m.cold_records, &m.packed_cold) {
            cold.validate_layout()?;
            ensure!(
                section.len == u64::from(m.count) * cold.width() as u64,
                "invalid cold_records section length"
            );
        }
        let mut end = 0;
        for s in [&m.records, &m.names, &m.grams, &m.postings]
            .into_iter()
            .chain(m.metadata.iter())
            .chain(m.revisions.iter())
            .chain(m.properties.iter())
            .chain(m.cold_records.iter())
        {
            ensure!(s.start == end, "snapshot sections must be contiguous");
            end = s.start.checked_add(s.len).context("section overflow")?;
            ensure!(end <= length, "section outside file");
        }
        ensure!(
            end == length
                && m.records.len == m.count as u64 * self.record_width as u64
                && m.grams.len.is_multiple_of(GRAM_BYTES as u64),
            "invalid snapshot lengths"
        );
        ensure!(
            m.parents.len() <= m.count as usize && m.sources.len() <= m.count as usize,
            "too many interned strings"
        );
        for parent in &m.parents {
            ensure!(
                parent.len() <= 32768 && !parent.contains('\0'),
                "invalid parent path"
            );
        }
        for source in &m.sources {
            ensure!(
                source.len() <= 4096 && !source.contains('\0'),
                "invalid source"
            );
        }
        ensure!(
            m.name_prefixes.len() <= MAX_PREFIXES,
            "invalid filename prefix dictionary"
        );
        for prefix in &m.name_prefixes {
            ensure!(
                (8..=MAX_PREFIX_BYTES).contains(&prefix.len()) && !prefix.contains('\0'),
                "invalid filename prefix"
            );
        }
        let prefix_shapes: Vec<_> = m
            .name_prefixes
            .iter()
            .map(|prefix| (prefix.chars().count(), xs_query::fold(prefix) == *prefix))
            .collect();
        let extra = m
            .metadata
            .as_ref()
            .map(|s| self.section(s))
            .unwrap_or_default();
        let mut metadata_end = 0;
        let mut name_scratch = String::new();
        let checked_arithmetic = m
            .packed_records
            .as_ref()
            .is_some_and(|layout| !layout.arithmetic_safe());
        for id in 0..m.count {
            let r = &if checked_arithmetic {
                self.raw_record::<true>(id)?
            } else {
                self.raw(id)
            };
            let (prefix, suffix) = self.name_parts(r)?;
            let len = prefix.map_or(0, |id| m.name_prefixes[id].len()) + suffix.len();
            ensure!(
                read32(r, 8) < m.parents.len() as u32
                    && read32(r, 12) < m.sources.len() as u32
                    && read32(r, 32) <= 3 + PREFIX_FLAG,
                "invalid record flags or strings"
            );
            ensure!(
                len <= MAX_NAME_BYTES
                    && prefix.map_or(0, |id| prefix_shapes[id].0) + suffix.chars().count() <= 1024
                    && !suffix.contains('\0'),
                "filename exceeds supported budget"
            );
            let path_len = m.parents[read32(r, 8) as usize].len() + len;
            ensure!(
                path_len > 0 && path_len <= 32768,
                "path exceeds supported budget"
            );
            // A persisted normalization hint must never silently change matching.
            if read32(r, 32) & 2 != 0 {
                let normalized = if suffix.is_ascii() {
                    prefix.is_none_or(|id| prefix_shapes[id].1)
                        && !suffix.bytes().any(|b| b.is_ascii_uppercase())
                } else {
                    let name = self.name(r, &mut name_scratch)?;
                    xs_query::fold(name) == name
                };
                ensure!(normalized, "invalid prepared normalization flag");
            }
            let offset = read32(r, 36);
            if offset != 0 {
                let offset = (offset - 1) as usize;
                ensure!(
                    offset == metadata_end
                        && offset.checked_add(4).is_some_and(|n| n <= extra.len()),
                    "invalid metadata offset"
                );
                let len = read32(extra, offset) as usize;
                ensure!(
                    len <= MAX_METADATA
                        && offset
                            .checked_add(4 + len)
                            .is_some_and(|n| n <= extra.len()),
                    "invalid metadata length"
                );
                let metadata: CatalogMetadata =
                    serde_json::from_slice(&extra[offset + 4..offset + 4 + len])?;
                validate_metadata(&metadata)?;
                ensure!(
                    !metadata.fields.is_empty(),
                    "empty catalog metadata must use zero offset"
                );
                metadata_end = offset + 4 + len;
            }
        }
        ensure!(metadata_end == extra.len(), "unreferenced catalog metadata");
        if let Some(revisions) = &m.revisions {
            let bytes = self.section(revisions);
            ensure!(
                bytes.len().is_multiple_of(REVISION_BYTES),
                "invalid file revision section"
            );
            let mut previous = None;
            for revision in bytes.as_chunks::<REVISION_BYTES>().0 {
                let id = read32(revision, 0);
                ensure!(
                    id < m.count
                        && previous.is_none_or(|previous| id > previous)
                        && read32(revision, 36) < 1_000_000_000
                        && read32(revision, 48) < 1_000_000_000,
                    "invalid file revision record"
                );
                ensure!(
                    self.source(&self.raw(id)) == "local",
                    "invalid file revision source"
                );
                previous = Some(id);
            }
        }
        if let Some(properties) = &m.properties {
            let bytes = self.section(properties);
            ensure!(
                bytes.len().is_multiple_of(PROPERTY_BYTES),
                "invalid file property section"
            );
            let mut previous = None;
            for row in bytes.as_chunks::<PROPERTY_BYTES>().0 {
                let id = read32(row, 0);
                let flags = row[4];
                ensure!(
                    id < m.count
                        && previous.is_none_or(|previous| id > previous)
                        && (1..=3).contains(&flags)
                        && read32(row, 13) < 1_000_000_000
                        && (flags & 1 != 0 || (read64(row, 5) == 0 && read32(row, 13) == 0))
                        && (flags & 2 != 0 || read32(row, 17) == 0),
                    "invalid file properties"
                );
                ensure!(
                    self.source(&self.raw(id)) == "local",
                    "invalid file properties source"
                );
                previous = Some(id);
            }
        }
        if let Some(journal) = &m.journal {
            journal.validate()?;
        }
        let mut previous = None;
        let mut posting_end = 0;
        for g in self.section(&m.grams).as_chunks::<GRAM_BYTES>().0 {
            let key = read64(g, 0);
            ensure!(previous.is_none_or(|p| key > p), "gram keys not sorted");
            previous = Some(key);
            let start = read64(g, 8);
            let len = read32(g, 16) as u64;
            let count = read32(g, 20);
            ensure!(
                start == posting_end
                    && start
                        .checked_add(len)
                        .is_some_and(|end| end <= m.postings.len)
                    && count > 0
                    && count <= m.count
                    && (m.rle_postings || len >= count as u64)
                    && len <= count as u64 * 5,
                "posting section invalid"
            );
            posting_end = start + len;
            visit_postings(
                &self.section(&m.postings)[start as usize..posting_end as usize],
                count,
                m.count,
                m.rle_postings,
                None,
                |_| {},
            )?;
        }
        ensure!(posting_end == m.postings.len, "unreferenced posting bytes");
        Ok(())
    }
    fn raw_hot_record<const CHECKED: bool>(&self, id: u32) -> Result<[u8; RECORD_BYTES]> {
        let records = self.section(&self.manifest.records);
        if let Some(layout) = &self.manifest.packed_records {
            let width = self.record_width;
            let start = id as usize * width;
            layout.decode::<CHECKED>(&records[start..start + width])
        } else {
            let start = id as usize * RECORD_BYTES;
            Ok(records[start..start + RECORD_BYTES].try_into().unwrap())
        }
    }
    fn raw_record<const CHECKED: bool>(&self, id: u32) -> Result<[u8; RECORD_BYTES]> {
        let mut row = self.raw_hot_record::<CHECKED>(id)?;
        if let (Some(section), Some(cold)) =
            (&self.manifest.cold_records, &self.manifest.packed_cold)
        {
            let width = cold.width();
            let start = id as usize * width;
            let bytes = self.section(section);
            ensure!(start + width <= bytes.len(), "cold record out of bounds");
            cold.decode_into(&bytes[start..start + width], &mut row);
        }
        Ok(row)
    }
    fn raw(&self, id: u32) -> [u8; RECORD_BYTES] {
        self.raw_record::<false>(id).expect("validated row")
    }

    #[inline]
    fn hot_fields(&self, id: u32) -> (u32, u32, u32, u32, u32) {
        let records = self.section(&self.manifest.records);
        if let Some(layout) = &self.manifest.packed_records {
            let width = self.record_width;
            let start = id as usize * width;
            layout.decode_hot_fields(&records[start..start + width])
        } else {
            let start = id as usize * RECORD_BYTES;
            let r = &records[start..start + RECORD_BYTES];
            (
                read32(r, 0),
                read32(r, 4),
                read32(r, 8),
                read32(r, 12),
                read32(r, 32),
            )
        }
    }

    #[inline]
    fn name_hot<'a>(
        &'a self,
        start: u32,
        len: u32,
        flags: u32,
        scratch: &'a mut String,
    ) -> &'a str {
        let s = start as usize;
        let e = s + len as usize;
        let stored = &self.section(&self.manifest.names)[s..e];
        if flags & PREFIX_FLAG == 0 {
            // Index::validate() 已在加载时逐条校验所有文件名段为合法 UTF-8。
            return std::str::from_utf8(stored).unwrap_or("");
        }
        let id = u16::from_le_bytes([stored[0], stored[1]]) as usize;
        let prefix = &self.manifest.name_prefixes[id];
        let suffix = std::str::from_utf8(&stored[2..]).unwrap_or("");
        scratch.clear();
        scratch.push_str(prefix);
        scratch.push_str(suffix);
        scratch.as_str()
    }

    fn raw_hot(&self, id: u32) -> [u8; RECORD_BYTES] {
        self.raw_hot_record::<false>(id).expect("validated hot row")
    }
    fn source(&self, row: &[u8; RECORD_BYTES]) -> &str {
        &self.manifest.sources[read32(row, 12) as usize]
    }
    fn name<'a>(&'a self, r: &[u8; RECORD_BYTES], scratch: &'a mut String) -> Result<&'a str> {
        let (prefix, suffix) = self.name_parts(r)?;
        let Some(prefix) = prefix.map(|id| &self.manifest.name_prefixes[id]) else {
            return Ok(suffix);
        };
        let length = prefix.len() + suffix.len();
        ensure!(
            length <= MAX_NAME_BYTES,
            "reconstructed filename outside budget"
        );
        scratch.clear();
        scratch.push_str(prefix);
        scratch.push_str(suffix);
        Ok(scratch)
    }
    fn name_parts<'a>(&'a self, r: &[u8; RECORD_BYTES]) -> Result<(Option<usize>, &'a str)> {
        let start = read32(r, 0) as usize;
        let len = read32(r, 4) as usize;
        let end = start.checked_add(len).context("filename length overflow")?;
        let stored = self
            .section(&self.manifest.names)
            .get(start..end)
            .context("name outside arena")?;
        if read32(r, 32) & PREFIX_FLAG == 0 {
            return Ok((
                None,
                std::str::from_utf8(stored).context("invalid filename UTF-8")?,
            ));
        }
        let (id, suffix) = stored
            .split_at_checked(2)
            .context("truncated filename prefix identifier")?;
        let id = u16::from_le_bytes(id.try_into().unwrap()) as usize;
        ensure!(
            id < self.manifest.name_prefixes.len(),
            "unknown filename prefix identifier"
        );
        Ok((
            Some(id),
            std::str::from_utf8(suffix).context("invalid filename UTF-8")?,
        ))
    }
    fn view_raw<'a>(
        &'a self,
        r: &[u8; RECORD_BYTES],
        name: &'a str,
        path: &'a str,
    ) -> EntryRef<'a> {
        EntryRef {
            name,
            path,
            size: read64(r, 16),
            modified: read64(r, 24) as i64,
            is_dir: read32(r, 32) & 1 == 1,
            source: self.source(r),
        }
    }
    pub fn record(&self, id: u32) -> Result<FileRecord> {
        ensure!(id < self.manifest.count, "record ID outside index");
        if let Some(catalog) = &self.catalog {
            let (part, id) = catalog.locate(id);
            return part.record(id);
        }
        let r = &self.raw(id);
        let mut scratch = String::new();
        let v = self.view_raw(r, self.name(r, &mut scratch)?, "");
        let path = format!("{}{}", self.manifest.parents[read32(r, 8) as usize], v.name);
        Ok(FileRecord {
            path,
            size: v.size,
            modified: v.modified,
            is_dir: v.is_dir,
            source: v.source.to_owned(),
        })
    }
    pub fn record_metadata(&self, id: u32) -> Result<Option<CatalogMetadata>> {
        ensure!(id < self.manifest.count, "record ID outside index");
        if let Some(catalog) = &self.catalog {
            let (part, id) = catalog.locate(id);
            return part.record_metadata(id);
        }
        let offset = read32(&self.raw(id), 36);
        if offset == 0 {
            return Ok(None);
        }
        let bytes = self.section(
            self.manifest
                .metadata
                .as_ref()
                .context("missing metadata arena")?,
        );
        let start = (offset - 1) as usize;
        let length = read32(bytes, start) as usize;
        Ok(Some(serde_json::from_slice(
            &bytes[start + 4..start + 4 + length],
        )?))
    }
    pub fn record_revision(&self, id: u32) -> Result<Option<FileRevision>> {
        ensure!(id < self.manifest.count, "record ID outside index");
        if let Some(catalog) = &self.catalog {
            let (part, id) = catalog.locate(id);
            return part.record_revision(id);
        }
        let Some(section) = &self.manifest.revisions else {
            return Ok(None);
        };
        let rows = self.section(section).as_chunks::<REVISION_BYTES>().0;
        let Ok(position) = rows.binary_search_by_key(&id, |row| read32(row, 0)) else {
            return Ok(None);
        };
        let row = &rows[position];
        Ok(Some(FileRevision {
            identity: FileIdentity {
                volume: read64(row, 4),
                object: row[12..28].try_into().unwrap(),
            },
            size: read64(&self.raw(id), 16),
            modified: FileTimestamp {
                seconds: read64(row, 28) as i64,
                nanoseconds: read32(row, 36),
            },
            changed: FileTimestamp {
                seconds: read64(row, 40) as i64,
                nanoseconds: read32(row, 48),
            },
        }))
    }
    pub fn record_properties(&self, id: u32) -> Result<FileProperties> {
        ensure!(id < self.manifest.count, "record ID outside index");
        if let Some(catalog) = &self.catalog {
            let (part, id) = catalog.locate(id);
            return part.record_properties(id);
        }
        let Some(section) = &self.manifest.properties else {
            return Ok(FileProperties::default());
        };
        let rows = self.section(section).as_chunks::<PROPERTY_BYTES>().0;
        let Ok(position) = rows.binary_search_by_key(&id, |row| read32(row, 0)) else {
            return Ok(FileProperties::default());
        };
        let row = &rows[position];
        Ok(FileProperties {
            created: (row[4] & 1 != 0).then(|| FileTimestamp {
                seconds: read64(row, 5) as i64,
                nanoseconds: read32(row, 13),
            }),
            attributes: (row[4] & 2 != 0).then(|| read32(row, 17)),
        })
    }

    fn query_metadata(&self, id: u32) -> Result<QueryMetadata> {
        if let Some(metadata) = self.record_metadata(id)? {
            let field = |key| metadata.fields.get(key).map(|value| value.trim());
            let created = field("Date Created")
                .and_then(|value| value.parse::<u64>().ok())
                .map(|ticks| {
                    ((i128::from(ticks) - 116_444_736_000_000_000).div_euclid(10_000_000)) as i64
                });
            let attributes = field("Attributes").and_then(|value| {
                if let Some(hex) = value
                    .strip_prefix("0x")
                    .or_else(|| value.strip_prefix("0X"))
                {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    value.parse::<u32>().ok()
                }
            });
            return Ok(QueryMetadata {
                size_known: field("Size").is_none_or(|value| !value.is_empty()),
                modified_known: field("Date Modified").is_none_or(|value| !value.is_empty()),
                created,
                attributes: attributes.map(u64::from),
            });
        }
        let properties = self.record_properties(id)?;
        Ok(QueryMetadata {
            created: properties.created.map(|created| created.seconds),
            attributes: properties.attributes.map(u64::from),
            ..Default::default()
        })
    }
    pub fn records(&self) -> impl Iterator<Item = FileRecord> + '_ {
        (0..self.manifest.count).map(|id| self.record(id).expect("validated record"))
    }
    fn posting(&self, key: u64) -> Option<&[u8]> {
        let data = self.section(&self.manifest.grams);
        let mut left = 0;
        let mut right = data.len() / GRAM_BYTES;
        while left < right {
            let mid = (left + right) / 2;
            let g = &data[mid * GRAM_BYTES..(mid + 1) * GRAM_BYTES];
            match read64(g, 0).cmp(&key) {
                std::cmp::Ordering::Less => left = mid + 1,
                std::cmp::Ordering::Greater => right = mid,
                std::cmp::Ordering::Equal => return Some(g),
            }
        }
        None
    }
    fn literal_candidates(
        &self,
        text: &str,
        budget: &SearchBudget<'_>,
    ) -> Result<Option<Vec<u32>>> {
        budget.check()?;
        let Some(keys) = literal_keys(text) else {
            return Ok(None);
        };
        let mut lists = Vec::new();
        for key in keys {
            let Some(g) = self.posting(key) else {
                return Ok(Some(Vec::new()));
            };
            lists.push(g);
        }
        lists.sort_unstable_by_key(|g| (read32(g, 20), read64(g, 0)));
        lists.dedup_by_key(|g| read64(g, 0));
        let Some(first) = lists.first() else {
            return Ok(None);
        };
        let decode = |g: &[u8]| {
            let start = read64(g, 8) as usize;
            let len = read32(g, 16) as usize;
            decode_postings(
                &self.section(&self.manifest.postings)[start..start + len],
                read32(g, 20),
                self.manifest.count,
                self.manifest.rle_postings,
                budget,
            )
        };
        let mut selected = decode(first)?;
        // Intersect selective grams first. Very broad lists cost more than
        // verifying the already-small candidate set, so skip them conservatively.
        for g in lists.iter().skip(1) {
            if selected.is_empty() {
                break;
            }
            if selected.len() <= 32 || read32(g, 20) as usize > selected.len().saturating_mul(8) {
                break;
            }
            let right = decode(g)?;
            selected = intersect_sorted(&selected, &right, budget)?;
        }
        Ok(Some(selected))
    }

    fn candidate_upper_bound(&self, plan: &CandidatePlan) -> usize {
        match plan {
            CandidatePlan::All => self.len(),
            CandidatePlan::Pinyin(needle) => {
                if !self.manifest.pinyin_postings {
                    return self.len();
                }
                let keys = pinyin_initial_keys(needle);
                if keys.is_empty() {
                    return self.len();
                }
                keys.into_iter()
                    .map(|k| {
                        self.posting(k)
                            .map_or(0, |posting| read32(posting, 20) as usize)
                    })
                    .min()
                    .unwrap_or(self.len())
            }
            CandidatePlan::Ext(exts) => {
                if !self.manifest.ext_postings {
                    return self.len();
                }
                exts.iter()
                    .map(|ext| {
                        self.posting(ext_gram_key(ext))
                            .map_or(0, |posting| read32(posting, 20) as usize)
                    })
                    .fold(0usize, usize::saturating_add)
                    .min(self.len())
            }
            CandidatePlan::Literal(text) => literal_keys(text).map_or(self.len(), |keys| {
                keys.into_iter()
                    .map(|key| {
                        self.posting(key)
                            .map_or(0, |posting| read32(posting, 20) as usize)
                    })
                    .min()
                    .unwrap_or(self.len())
            }),
            CandidatePlan::And(parts) => parts
                .iter()
                .map(|part| self.candidate_upper_bound(part))
                .min()
                .unwrap_or(self.len()),
            CandidatePlan::Or(parts) => parts
                .iter()
                .map(|part| self.candidate_upper_bound(part))
                .fold(0usize, usize::saturating_add)
                .min(self.len()),
        }
    }

    fn candidates(
        &self,
        plan: &CandidatePlan,
        budget: &SearchBudget<'_>,
    ) -> Result<Option<Vec<u32>>> {
        budget.check()?;
        match plan {
            CandidatePlan::All => Ok(None),
            CandidatePlan::Pinyin(needle) => {
                if !self.manifest.pinyin_postings {
                    return Ok(None);
                }
                let keys = pinyin_initial_keys(needle);
                if keys.is_empty() {
                    return Ok(None);
                }
                let mut lists: Vec<&[u8]> = Vec::with_capacity(keys.len());
                for key in keys {
                    let Some(posting) = self.posting(key) else {
                        return Ok(Some(Vec::new()));
                    };
                    lists.push(posting);
                }
                lists.sort_unstable_by_key(|p| read32(p, 20));
                let decode = |p: &[u8]| -> Result<Vec<u32>> {
                    let start = read64(p, 8) as usize;
                    let len = read32(p, 16) as usize;
                    let count = read32(p, 20);
                    decode_postings(
                        &self.section(&self.manifest.postings)[start..start + len],
                        count,
                        self.manifest.count,
                        self.manifest.rle_postings,
                        budget,
                    )
                };
                let mut selected = decode(lists[0])?;
                for g in lists.iter().skip(1) {
                    if selected.is_empty()
                        || selected.len() <= 32
                        || read32(g, 20) as usize > selected.len().saturating_mul(8)
                    {
                        break;
                    }
                    let right = decode(g)?;
                    selected = intersect_sorted(&selected, &right, budget)?;
                }
                Ok(Some(selected))
            }
            CandidatePlan::Ext(exts) => {
                if !self.manifest.ext_postings {
                    return Ok(None);
                }
                let mut all = Vec::new();
                for ext in exts {
                    let Some(posting) = self.posting(ext_gram_key(ext)) else {
                        continue;
                    };
                    let start = read64(posting, 8) as usize;
                    let len = read32(posting, 16) as usize;
                    let count = read32(posting, 20);
                    let list = decode_postings(
                        &self.section(&self.manifest.postings)[start..start + len],
                        count,
                        self.manifest.count,
                        self.manifest.rle_postings,
                        budget,
                    )?;
                    if all.is_empty() {
                        all = list;
                    } else {
                        all = union_sorted(&all, &list, budget)?;
                    }
                    if all.len() == self.len() {
                        return Ok(None);
                    }
                }
                Ok(Some(all))
            }
            CandidatePlan::Literal(s) => self.literal_candidates(s, budget),
            CandidatePlan::And(plans) => {
                let mut ordered: Vec<(usize, &CandidatePlan)> = Vec::new();
                for (n, p) in plans.iter().enumerate() {
                    if plans[..n].contains(p) || matches!(p, CandidatePlan::All) {
                        continue;
                    }
                    let bound = self.candidate_upper_bound(p);
                    if bound == 0 {
                        return Ok(Some(Vec::new()));
                    }
                    ordered.push((bound, p));
                }
                ordered.sort_unstable_by_key(|(bound, _)| *bound);
                let mut selected: Option<Vec<u32>> = None;
                for (bound, p) in ordered {
                    if let Some(current) = &selected {
                        if current.is_empty() {
                            return Ok(selected);
                        }
                        if current.len() <= 32 || bound > current.len().saturating_mul(8) {
                            break;
                        }
                    }
                    if let Some(v) = self.candidates(p, budget)? {
                        if v.is_empty() {
                            return Ok(Some(v));
                        }
                        selected = match selected {
                            None => Some(v),
                            Some(current) => Some(intersect_sorted(&current, &v, budget)?),
                        };
                    }
                }
                Ok(selected)
            }
            CandidatePlan::Or(plans) => {
                let mut all = Vec::new();
                for (n, p) in plans.iter().enumerate() {
                    if plans[..n].contains(p) {
                        continue;
                    }
                    match self.candidates(p, budget)? {
                        Some(v) => all = union_sorted(&all, &v, budget)?,
                        None => return Ok(None),
                    }
                    if all.len() == self.len() {
                        return Ok(None);
                    }
                }
                Ok(Some(all))
            }
        }
    }
    /// 借助文件名倒排限制监听查找，并核对完整路径与来源。
    pub(crate) fn find_local_path(
        &self,
        path: &std::path::Path,
        start: Instant,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<u32>>> {
        let Some(path_text) = path.to_str() else {
            return Ok(None);
        };
        let folded = xs_query::fold(split_path(path_text)?.1);
        let budget = SearchBudget {
            start,
            milliseconds: 100,
            cancel: Some(cancel),
        };
        let catalog = self.catalog.as_ref();
        let mut matches = Vec::new();
        let mut name_scratch = String::new();
        for part in 0..catalog.map_or(1, Catalog::part_count) {
            let (start, segment) = catalog.map_or((0, self), |catalog| catalog.part(part));
            let candidates = match segment.literal_candidates(&folded, &budget) {
                Ok(candidates) => candidates,
                Err(error) if error.is::<SearchInterrupted>() => return Ok(None),
                Err(error) => return Err(error),
            };
            let ids: Box<dyn Iterator<Item = u32>> = match candidates {
                Some(ids) => Box::new(ids.into_iter()),
                None if segment.len() <= 10_000 => Box::new(0..segment.manifest.count),
                None => return Ok(None),
            };
            for (n, id) in ids.enumerate() {
                if n.is_multiple_of(128) && budget.expired() {
                    return Ok(None);
                }
                let logical = if let Some(catalog) = catalog {
                    let Some(id) = catalog.logical_id(start + id) else {
                        continue;
                    };
                    id
                } else {
                    id
                };
                let raw = &segment.raw(id);
                let view = segment.view_raw(raw, segment.name(raw, &mut name_scratch)?, "");
                if view.source != "local" || xs_query::fold(view.name) != folded {
                    continue;
                }
                let parent = &segment.manifest.parents[read32(raw, 8) as usize];
                let record_path = format!("{parent}{}", view.name);
                let same = if cfg!(windows) {
                    record_path.replace('\\', "/").to_lowercase()
                        == path_text.replace('\\', "/").to_lowercase()
                } else {
                    record_path == path_text
                };
                if same {
                    matches.push(logical);
                }
            }
        }
        Ok(Some(matches))
    }

    fn interrupted_response(&self, start: Instant) -> SearchResponse {
        SearchResponse {
            generation: self.generation(),
            results: Vec::new(),
            total: 0,
            complete: false,
            total_relation: "at_least".into(),
            candidate_count: self.len(),
            candidate_count_exact: false,
            examined: 0,
            elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
            index_entries: self.len(),
            coverage_complete: self.coverage_complete(),
        }
    }
    pub fn search(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Option<&AtomicBool>,
    ) -> Result<SearchResponse> {
        self.search_with_mode::<false>(query, options, cancel, false, None, None)
    }

    /// 在候选集合的全局 Top-K 选择之前应用历史打开加权，避免仅重排当前页。
    pub fn search_personalized(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Option<&AtomicBool>,
        boosts: &HashMap<String, i64>,
    ) -> Result<SearchResponse> {
        self.search_with_mode::<false>(query, options, cancel, false, None, Some(boosts))
    }

    /// 对全部匹配项按指定列排序，再从同一索引代次分页。
    pub fn search_sorted(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Option<&AtomicBool>,
        sort: ColumnSort,
    ) -> Result<SearchResponse> {
        self.search_with_mode::<true>(query, options, cancel, false, Some(sort), None)
    }

    /// 限时首屏跳过昂贵的候选集合并，返回已验证命中的下界。
    /// 完整搜索仍使用相同的查询、排序与全局分页规则。
    pub fn search_preview(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Option<&AtomicBool>,
    ) -> Result<SearchResponse> {
        self.search_with_mode::<false>(query, options, cancel, true, None, None)
    }

    /// 有界预览同样使用全局 Frecency，防止预览与完整列表顺序跳跃。
    pub fn search_preview_personalized(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Option<&AtomicBool>,
        boosts: &HashMap<String, i64>,
    ) -> Result<SearchResponse> {
        self.search_with_mode::<false>(query, options, cancel, true, None, Some(boosts))
    }

    // 分别编译相关度与列排序的热循环，避免默认查询处理列排序状态。
    fn search_with_mode<const COLUMN_SORT: bool>(
        &self,
        query: &str,
        options: &SearchOptions,
        cancel: Option<&AtomicBool>,
        preview: bool,
        sort: Option<ColumnSort>,
        boosts: Option<&HashMap<String, i64>>,
    ) -> Result<SearchResponse> {
        ensure!(
            options.limit > 0 && options.limit <= 1000 && options.offset <= 10000,
            "limit must be 1..1000, offset <=10000"
        );
        ensure!(
            options.time_budget_ms > 0 && options.time_budget_ms <= 60000,
            "time budget must be 1..60000 ms"
        );
        let start = Instant::now();
        let budget = SearchBudget {
            start,
            milliseconds: options.time_budget_ms,
            cancel,
        };
        if budget.expired() {
            return Ok(self.interrupted_response(start));
        }
        let query = Query::parse(query).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let needs_path = query.needs_path() || COLUMN_SORT;
        let needs_cold = query.needs_metadata() || COLUMN_SORT;
        let plan = query.candidate_plan();
        let catalog = self.catalog.as_ref();
        let segment_count = catalog.map_or(1, Catalog::part_count);
        let capacity = options.limit + options.offset;
        let mut heap: BinaryHeap<Reverse<(i64, Reverse<u32>)>> = BinaryHeap::new();
        let mut column_heap: BinaryHeap<ColumnRank> = BinaryHeap::new();
        let mut total = 0;
        let mut examined = 0usize;
        let mut complete = true;
        let mut candidate_count = 0;
        let mut candidate_count_exact = true;
        let mut name_scratch = String::new();
        let mut path = String::new();
        'parts: for part in 0..segment_count {
            let (start, segment) = catalog.map_or((0, self), |catalog| catalog.part(part));
            let sample = preview && segment.candidate_upper_bound(&plan) > 32_768;
            let candidates = if sample {
                None
            } else {
                match segment.candidates(&plan, &budget) {
                    Ok(value) => value,
                    Err(error) if error.is::<SearchInterrupted>() => {
                        if examined == 0 {
                            return Ok(self.interrupted_response(budget.start));
                        }
                        complete = false;
                        candidate_count_exact = false;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            };
            let workers = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
                .clamp(1, 8);
            let candidate_len = candidates
                .as_ref()
                .map_or(segment.manifest.count as usize, Vec::len);
            if !COLUMN_SORT && !sample && workers > 1 && candidate_len >= 16_384 {
                struct ChunkOutcome {
                    heap: BinaryHeap<Reverse<(i64, Reverse<u32>)>>,
                    total: usize,
                    examined: usize,
                    candidates: usize,
                    complete: bool,
                }
                let ids_vec = candidates;
                let chunk_size = candidate_len.div_ceil(workers);
                let part_results: Result<Vec<ChunkOutcome>> = std::thread::scope(|scope| {
                    let mut handles = Vec::with_capacity(workers);
                    for worker_idx in 0..workers {
                        let range_start = worker_idx * chunk_size;
                        if range_start >= candidate_len {
                            break;
                        }
                        let range_end = (range_start + chunk_size).min(candidate_len);
                        let ids_ref = ids_vec.as_deref();
                        let query_ref = &query;
                        let budget_ref = &budget;
                        handles.push(scope.spawn(move || -> Result<ChunkOutcome> {
                            let mut local_heap: BinaryHeap<Reverse<(i64, Reverse<u32>)>> =
                                BinaryHeap::with_capacity(capacity.min(1024));
                            let mut local_total = 0usize;
                            let mut local_examined = 0usize;
                            let mut local_candidates = 0usize;
                            let mut local_complete = true;
                            let mut local_name = String::new();
                            let mut local_path = String::new();
                            for offset in range_start..range_end {
                                if (offset - range_start).is_multiple_of(4096)
                                    && budget_ref.expired()
                                {
                                    local_complete = false;
                                    break;
                                }
                                let id = match ids_ref {
                                    Some(slice) => slice[offset],
                                    None => offset as u32,
                                };
                                let id_in_catalog = if let Some(catalog) = catalog {
                                    let Some(live) = catalog.logical_id(start + id) else {
                                        continue;
                                    };
                                    live
                                } else {
                                    id
                                };
                                local_candidates += 1;
                                local_examined += 1;
                                let (view, folded_name, parent_id, metadata) = if !needs_cold {
                                    let (name_off, name_len, pid, sid, flags) =
                                        segment.hot_fields(id);
                                    let name = segment.name_hot(
                                        name_off,
                                        name_len,
                                        flags,
                                        &mut local_name,
                                    );
                                    let parent_id = pid as usize;
                                    if needs_path {
                                        local_path.clear();
                                        local_path.push_str(&segment.manifest.parents[parent_id]);
                                        local_path.push_str(name);
                                    }
                                    let view = EntryRef {
                                        name,
                                        path: &local_path,
                                        size: 0,
                                        modified: 0,
                                        is_dir: flags & 1 != 0,
                                        source: &segment.manifest.sources[sid as usize],
                                    };
                                    let folded_name = (flags & 2 != 0).then_some(view.name);
                                    (view, folded_name, parent_id, QueryMetadata::default())
                                } else {
                                    let r = segment.raw(id);
                                    let name_view = segment.view_raw(
                                        &r,
                                        segment.name(&r, &mut local_name)?,
                                        "",
                                    );
                                    let parent_id = read32(&r, 8) as usize;
                                    if needs_path {
                                        local_path.clear();
                                        local_path.push_str(&segment.manifest.parents[parent_id]);
                                        local_path.push_str(name_view.name);
                                    }
                                    let view = EntryRef {
                                        path: &local_path,
                                        ..name_view
                                    };
                                    let folded_name =
                                        (read32(&r, 32) & 2 != 0).then_some(view.name);
                                    let metadata = segment.query_metadata(id)?;
                                    (view, folded_name, parent_id, metadata)
                                };
                                if let Some(mut score) = query_ref.matched_score_with_metadata(
                                    &view,
                                    folded_name,
                                    segment.parent_depths[parent_id],
                                    metadata,
                                ) {
                                    if let Some(boosts) = boosts {
                                        let full_path = if needs_path {
                                            local_path.clone()
                                        } else {
                                            format!(
                                                "{}{}",
                                                segment.manifest.parents[parent_id], view.name
                                            )
                                        };
                                        score =
                                            score.saturating_add(path_boost(boosts, &full_path));
                                    }
                                    local_total += 1;
                                    let rank = (score, Reverse(id_in_catalog));
                                    if local_heap.len() < capacity {
                                        local_heap.push(Reverse(rank));
                                    } else if local_heap.peek().is_some_and(|last| rank > last.0) {
                                        local_heap.pop();
                                        local_heap.push(Reverse(rank));
                                    }
                                }
                            }
                            Ok(ChunkOutcome {
                                heap: local_heap,
                                total: local_total,
                                examined: local_examined,
                                candidates: local_candidates,
                                complete: local_complete,
                            })
                        }));
                    }
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .map_err(|_| anyhow::anyhow!("parallel search worker panicked"))?
                        })
                        .collect()
                });
                for outcome in part_results? {
                    total += outcome.total;
                    examined += outcome.examined;
                    candidate_count += outcome.candidates;
                    if !outcome.complete {
                        complete = false;
                        candidate_count_exact = false;
                    }
                    for Reverse(rank) in outcome.heap {
                        if heap.len() < capacity {
                            heap.push(Reverse(rank));
                        } else if heap.peek().is_some_and(|last| rank > last.0) {
                            heap.pop();
                            heap.push(Reverse(rank));
                        }
                    }
                }
                if !complete {
                    break 'parts;
                }
                continue 'parts;
            }
            let iter: Box<dyn Iterator<Item = u32>> = match candidates {
                Some(v) => Box::new(v.into_iter()),
                None => Box::new(0..segment.manifest.count),
            };
            for (checked, id) in iter.enumerate() {
                if checked.is_multiple_of(128) && budget.expired() {
                    complete = false;
                    candidate_count_exact = false;
                    break 'parts;
                }
                let id_in_catalog = if let Some(catalog) = catalog {
                    let Some(live) = catalog.logical_id(start + id) else {
                        continue;
                    };
                    live
                } else {
                    id
                };
                candidate_count += 1;
                examined += 1;
                let r = if needs_cold {
                    segment.raw(id)
                } else {
                    segment.raw_hot(id)
                };
                let r = &r;
                let name_view = segment.view_raw(r, segment.name(r, &mut name_scratch)?, "");
                let parent_id = read32(r, 8) as usize;
                if needs_path {
                    path.clear();
                    path.push_str(&segment.manifest.parents[parent_id]);
                    path.push_str(name_view.name);
                }
                let view = EntryRef {
                    path: &path,
                    ..name_view
                };
                let folded_name = (read32(r, 32) & 2 != 0).then_some(view.name);
                let metadata = if query.needs_metadata() {
                    segment.query_metadata(id)?
                } else {
                    QueryMetadata::default()
                };
                if let Some(mut score) = query.matched_score_with_metadata(
                    &view,
                    folded_name,
                    segment.parent_depths[parent_id],
                    metadata,
                ) {
                    if let Some(boosts) = boosts {
                        let full_path = if needs_path {
                            path.clone()
                        } else {
                            format!("{}{}", segment.manifest.parents[parent_id], view.name)
                        };
                        score = score.saturating_add(path_boost(boosts, &full_path));
                    }
                    total += 1;
                    if COLUMN_SORT {
                        let sort = sort.expect("列排序调用必须提供排序列");
                        if column_heap.len() < capacity
                            || column_heap.peek().is_some_and(|worst| {
                                worst
                                    .cmp_entry(&view)
                                    .then_with(|| worst.id.cmp(&id_in_catalog))
                                    .is_gt()
                            })
                        {
                            if column_heap.len() == capacity {
                                column_heap.pop();
                            }
                            column_heap.push(ColumnRank {
                                sort,
                                path: path.clone(),
                                name_offset: path.len() - view.name.len(),
                                source: view.source.to_owned(),
                                size: view.size,
                                modified: view.modified,
                                id: id_in_catalog,
                                score,
                            });
                        }
                    } else {
                        let rank = (score, Reverse(id_in_catalog));
                        if heap.len() < capacity {
                            heap.push(Reverse(rank));
                        } else if heap.peek().is_some_and(|last| rank > last.0) {
                            heap.pop();
                            heap.push(Reverse(rank));
                        }
                    }
                    if sample && total >= capacity.saturating_mul(4) {
                        complete = false;
                        candidate_count_exact = false;
                        break 'parts;
                    }
                }
            }
        }
        let sorted: Vec<_> = if COLUMN_SORT {
            column_heap
                .into_sorted_vec()
                .into_iter()
                .map(|r| (r.score, Reverse(r.id)))
                .collect()
        } else {
            let mut sorted: Vec<_> = heap.into_iter().map(|r| r.0).collect();
            sorted.sort_unstable_by(|a, b| b.cmp(a));
            sorted
        };
        let mut results = Vec::new();
        for (score, Reverse(id)) in sorted.into_iter().skip(options.offset) {
            let r = self.record(id)?;
            let name = split_path(&r.path)?.1.to_owned();
            results.push(SearchHit {
                id,
                path: r.path,
                name,
                size: r.size,
                modified: r.modified,
                is_dir: r.is_dir,
                source: r.source,
                score,
            });
        }
        let complete = complete && self.coverage_complete();
        Ok(SearchResponse {
            generation: self.generation(),
            results,
            total,
            complete,
            total_relation: if complete { "exact" } else { "at_least" }.into(),
            candidate_count: if candidate_count_exact {
                candidate_count
            } else {
                self.len()
            },
            candidate_count_exact,
            examined,
            elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
            index_entries: self.len(),
            coverage_complete: self.coverage_complete(),
        })
    }
}

fn path_boost(boosts: &HashMap<String, i64>, path: &str) -> i64 {
    if cfg!(windows) {
        boosts.get(&path.to_lowercase()).copied().unwrap_or(0)
    } else {
        boosts.get(path).copied().unwrap_or(0)
    }
}

fn literal_keys(text: &str) -> Option<Vec<u64>> {
    let chars: Vec<_> = text.chars().collect();
    if chars.len() >= 3 {
        Some(chars.windows(3).map(gram_key).collect())
    } else if !chars.is_empty() && chars.iter().all(|c| cjk(*c)) {
        Some(vec![gram_key(&chars)])
    } else {
        None
    }
}

pub(crate) fn parent_depths(parents: &[String]) -> Vec<usize> {
    parents
        .iter()
        .map(|path| xs_query::path_depth(path))
        .collect()
}

fn gallop_lower_bound(slice: &[u32], mut start: usize, target: u32) -> usize {
    if start >= slice.len() || slice[start] >= target {
        return start;
    }
    let mut step = 1usize;
    let mut prev = start;
    while start < slice.len() && slice[start] < target {
        prev = start;
        start = start.saturating_add(step);
        step = step.saturating_mul(2);
    }
    let end = start.min(slice.len());
    prev + slice[prev..end].partition_point(|&value| value < target)
}

fn intersect_sorted(left: &[u32], right: &[u32], budget: &SearchBudget<'_>) -> Result<Vec<u32>> {
    let (small, large) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    if small.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::with_capacity(small.len());
    if large.len() > small.len().saturating_mul(8) {
        let mut pos = 0usize;
        for (steps, &needle) in small.iter().enumerate() {
            if steps.is_multiple_of(256) {
                budget.check()?;
            }
            pos = gallop_lower_bound(large, pos, needle);
            if pos >= large.len() {
                break;
            }
            if large[pos] == needle {
                result.push(needle);
                pos += 1;
            }
        }
        return Ok(result);
    }
    let mut a = 0;
    let mut b = 0;
    let mut steps = 0usize;
    while a < small.len() && b < large.len() {
        if steps.is_multiple_of(1024) {
            budget.check()?;
        }
        steps += 1;
        match small[a].cmp(&large[b]) {
            std::cmp::Ordering::Less => a += 1,
            std::cmp::Ordering::Greater => b += 1,
            std::cmp::Ordering::Equal => {
                result.push(small[a]);
                a += 1;
                b += 1;
            }
        }
    }
    Ok(result)
}
fn union_sorted(left: &[u32], right: &[u32], budget: &SearchBudget<'_>) -> Result<Vec<u32>> {
    let mut a = 0;
    let mut b = 0;
    let mut result = Vec::with_capacity(left.len().max(right.len()));
    while a < left.len() || b < right.len() {
        if result.len().is_multiple_of(1024) {
            budget.check()?;
        }
        if b == right.len() || (a < left.len() && left[a] < right[b]) {
            result.push(left[a]);
            a += 1;
        } else if a == left.len() || right[b] < left[a] {
            result.push(right[b]);
            b += 1;
        } else {
            result.push(left[a]);
            a += 1;
            b += 1;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Index {
        let mut b = IndexBuilder::default();
        for path in [
            "C:\\资料\\干部任免报告.docx",
            "C:\\资料\\预算.pdf",
            "/tmp/project/budget_final.txt",
            "/tmp/project/readme.md",
        ] {
            b.add(FileRecord {
                path: path.into(),
                size: 1234,
                modified: 1700000000,
                is_dir: false,
                source: "offline:test".into(),
            })
            .unwrap();
        }
        b.finish(1).unwrap()
    }
    #[test]
    fn posix_backslash_is_a_filename_character() {
        assert_eq!(split_path("/tmp/a\\b.txt").unwrap(), ("/tmp/", "a\\b.txt"));
    }
    #[test]
    fn root_path_survives() {
        assert_eq!(split_path("C:\\").unwrap(), ("C:\\", ""));
        assert_eq!(split_path("/").unwrap(), ("/", ""));
    }
    #[test]
    fn windows_paths_survive_linux() {
        let idx = fixture();
        assert_eq!(idx.record(0).unwrap().path, "C:\\资料\\干部任免报告.docx");
    }
    #[test]
    fn missing_word_does_not_scan() {
        let r = fixture()
            .search("nonexistent", &SearchOptions::default(), None)
            .unwrap();
        assert_eq!((r.total, r.examined), (0, 0));
    }
    #[test]
    fn cjk_short_uses_posting() {
        let r = fixture()
            .search("干部", &SearchOptions::default(), None)
            .unwrap();
        assert_eq!((r.total, r.examined), (1, 1));
    }
    #[test]
    fn or_does_not_drop_branch() {
        let r = fixture()
            .search("budget | readme", &SearchOptions::default(), None)
            .unwrap();
        assert_eq!(r.total, 2);
    }
    #[test]
    fn mixed_not_path_is_conservative() {
        let r = fixture()
            .search("path:project !readme", &SearchOptions::default(), None)
            .unwrap();
        assert_eq!(r.total, 1);
    }
    #[test]
    fn cancellation_is_honest() {
        let c = AtomicBool::new(true);
        let r = fixture()
            .search("", &SearchOptions::default(), Some(&c))
            .unwrap();
        assert!(!r.complete);
        assert_eq!(r.total_relation, "at_least");
    }
    #[test]
    fn bad_pagination_rejected() {
        assert!(fixture()
            .search(
                "",
                &SearchOptions {
                    limit: 0,
                    ..Default::default()
                },
                None
            )
            .is_err());
    }
    #[test]
    fn u32_varint_roundtrip() {
        let mut b = Vec::new();
        for d in [0, 1, 127, 128, 100000] {
            put_varint(d, &mut b);
        }
        let budget = SearchBudget {
            start: Instant::now(),
            milliseconds: 5000,
            cancel: None,
        };
        assert_eq!(
            decode_postings(&b, 5, 200000, false, &budget).unwrap(),
            vec![0, 1, 128, 256, 100256]
        );
    }
    #[test]
    fn malformed_postings_fail() {
        let budget = SearchBudget {
            start: Instant::now(),
            milliseconds: 5000,
            cancel: None,
        };
        assert!(decode_postings(&[255, 255, 255, 255, 255, 1], 1, 5, false, &budget).is_err());
        assert!(decode_postings(&[0, 0, 20, 1], 5, 5, true, &budget).is_err());
    }
    #[test]
    fn repeated_postings_are_compact_and_exact() {
        let mut posting = PostingBuilder {
            data: Vec::new(),
            previous: 0,
            count: 0,
            gap: 0,
            run: 0,
        };
        for id in (0..100_000).step_by(5) {
            posting.add(id);
        }
        posting.flush_run();
        assert!(posting.data.len() <= 8);
        let budget = SearchBudget {
            start: Instant::now(),
            milliseconds: 5000,
            cancel: None,
        };
        assert_eq!(
            decode_postings(&posting.data, posting.count, 100_000, true, &budget).unwrap(),
            (0..100_000).step_by(5).collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod intersect_tests {
    use super::*;

    #[test]
    fn galloping_and_linear_intersection_match_exactly() {
        let budget = SearchBudget {
            start: Instant::now(),
            milliseconds: 5_000,
            cancel: None,
        };
        let small = vec![2, 10, 500, 9_999, 100_000];
        let large: Vec<u32> = (0..120_000).step_by(2).collect();
        let out = intersect_sorted(&small, &large, &budget).unwrap();
        assert_eq!(out, vec![2, 10, 500, 100_000]);
        let rev = intersect_sorted(&large, &small, &budget).unwrap();
        assert_eq!(rev, vec![2, 10, 500, 100_000]);
        let sym = intersect_sorted(&[1, 3, 5, 7], &[2, 3, 6, 7], &budget).unwrap();
        assert_eq!(sym, vec![3, 7]);
    }
}

pub(crate) fn encode_parent_trie(parents: &[String]) -> Vec<ParentNode> {
    let mut nodes: Vec<ParentNode> = Vec::with_capacity(parents.len());
    let mut prefix_map: HashMap<String, u32> = HashMap::with_capacity(parents.len());
    for (idx, path) in parents.iter().enumerate() {
        let trimmed = path.trim_end_matches(['/', '\\']);
        let split_pos = trimmed.rfind(['/', '\\']);
        let node = if let Some(pos) = split_pos {
            let parent_prefix = &path[..=pos];
            if let Some(&parent_idx) = prefix_map.get(parent_prefix) {
                ParentNode {
                    parent: Some(parent_idx),
                    segment: path[pos + 1..].to_owned(),
                }
            } else {
                ParentNode {
                    parent: None,
                    segment: path.clone(),
                }
            }
        } else {
            ParentNode {
                parent: None,
                segment: path.clone(),
            }
        };
        prefix_map.entry(path.clone()).or_insert(idx as u32);
        nodes.push(node);
    }
    nodes
}

pub(crate) fn decode_parent_trie(nodes: &[ParentNode]) -> Result<Vec<String>> {
    let mut parents: Vec<String> = Vec::with_capacity(nodes.len());
    for (idx, node) in nodes.iter().enumerate() {
        let full = match node.parent {
            None => node.segment.clone(),
            Some(parent_idx) => {
                let p = parent_idx as usize;
                ensure!(p < idx, "invalid parent trie back-reference");
                let mut combined = String::with_capacity(parents[p].len() + node.segment.len());
                combined.push_str(&parents[p]);
                combined.push_str(&node.segment);
                combined
            }
        };
        parents.push(full);
    }
    Ok(parents)
}

#[cfg(test)]
mod parent_trie_tests {
    use super::*;

    #[test]
    fn parent_trie_roundtrips_windows_and_posix_trees_compactly() {
        let sample = vec![
            "C:\\".to_owned(),
            "C:\\Users\\".to_owned(),
            "C:\\Users\\alice\\".to_owned(),
            "C:\\Users\\alice\\Documents\\".to_owned(),
            "C:\\Users\\alice\\Documents\\Reports\\".to_owned(),
            "/home/".to_owned(),
            "/home/alice/".to_owned(),
            "/home/alice/projects/xunshu/".to_owned(),
        ];
        let encoded = encode_parent_trie(&sample);
        let decoded = decode_parent_trie(&encoded).unwrap();
        assert_eq!(decoded, sample);
        let raw_bytes: usize = sample.iter().map(String::len).sum();
        let trie_bytes: usize = encoded.iter().map(|n| n.segment.len()).sum();
        assert!(trie_bytes < raw_bytes);
    }
}

#[cfg(test)]
mod pinyin_inverted_tests {
    use super::*;

    #[test]
    fn smart_pinyin_hits_inverted_index_without_full_scan() {
        let mut builder = IndexBuilder::new("scope-py".to_string());
        let target = FileRecord {
            path: "/docs/寻枢设计方案.docx".into(),
            size: 100,
            modified: 10,
            is_dir: false,
            source: "local".into(),
        };
        builder.add(target).unwrap();
        for i in 0..500 {
            let entry = FileRecord {
                path: format!("/docs/english_file_{i}.txt"),
                size: 50,
                modified: 10,
                is_dir: false,
                source: "local".into(),
            };
            builder.add(entry).unwrap();
        }
        let index = builder.finish(1).unwrap();
        let resp = index
            .search(
                "smart:xssj",
                &SearchOptions {
                    limit: 20,
                    offset: 0,
                    time_budget_ms: 5_000,
                },
                None,
            )
            .unwrap();
        assert_eq!(resp.total, 1);
        assert!(
            resp.examined < 10,
            "expected inverted recall, examined={}",
            resp.examined
        );
    }
}
