//! 不可变基段和增量段。删除 ID 属于物理目录，查询 ID 在当前代次内连续。
use crate::index::{Index, JournalCheckpoint, Manifest, Storage};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_PARTS: usize = 8;
pub(crate) const MAX_DELETED: usize = 250_000;

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationFile {
    pub file: String,
    pub generation: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogFile {
    pub version: u32,
    pub scope_token: String,
    pub partial_coverage: bool,
    pub journal: Option<JournalCheckpoint>,
    pub parts: Vec<GenerationFile>,
    pub deleted: Vec<u32>,
}

pub(crate) struct Catalog {
    starts: Vec<u32>,
    parts: Vec<Index>,
    deleted: Vec<u32>,
    physical_count: u32,
}

impl Catalog {
    pub(crate) fn assemble(
        generation: u64,
        scope_token: String,
        partial_coverage: bool,
        journal: Option<JournalCheckpoint>,
        parts: Vec<Index>,
        deleted: Vec<u32>,
    ) -> Result<Index> {
        ensure!(
            (2..=MAX_PARTS).contains(&parts.len()),
            "invalid segment count"
        );
        ensure!(deleted.len() <= MAX_DELETED, "too many deleted IDs");
        if let Some(checkpoint) = &journal {
            checkpoint.validate()?;
        }
        let mut starts = Vec::with_capacity(parts.len());
        let mut physical_count = 0u32;
        let mut previous = None;
        for index in &parts {
            ensure!(
                index.catalog.is_none() && index.scope_token() == scope_token,
                "incompatible index segment"
            );
            ensure!(
                previous.is_none_or(|id| index.generation() > id),
                "segments must be ordered by generation"
            );
            previous = Some(index.generation());
            starts.push(physical_count);
            physical_count = physical_count
                .checked_add(u32::try_from(index.len())?)
                .context("segment IDs exhausted")?;
        }
        ensure!(previous == Some(generation), "catalog generation mismatch");
        ensure!(
            deleted.last().is_none_or(|id| *id < physical_count)
                && deleted.windows(2).all(|pair| pair[0] < pair[1]),
            "invalid deletion mask"
        );
        Ok(Index {
            storage: Storage::Owned(Vec::new()),
            payload_start: 0,
            manifest: Manifest {
                generation,
                count: physical_count - deleted.len() as u32,
                scope_token,
                partial_coverage,
                journal,
                ..Default::default()
            },
            parent_depths: Vec::new(),
            record_width: 0,
            catalog: Some(Self {
                starts,
                parts,
                deleted,
                physical_count,
            }),
        })
    }

    pub(crate) fn part_count(&self) -> usize {
        self.parts.len()
    }

    pub(crate) fn part(&self, i: usize) -> (u32, &Index) {
        (self.starts[i], &self.parts[i])
    }

    pub(crate) fn storage_bytes(&self) -> usize {
        self.parts.iter().map(Index::storage_bytes).sum::<usize>()
            + self.deleted.len() * std::mem::size_of::<u32>()
    }

    pub(crate) fn is_mapped(&self) -> bool {
        self.parts.iter().all(Index::is_mapped)
    }

    pub(crate) fn has_local_records(&self) -> bool {
        // 空范围禁止包含任何本地段，即使坏目录声称已遮蔽其中所有记录。
        self.parts.iter().any(Index::has_local_records)
    }

    pub(crate) fn logical_id(&self, physical: u32) -> Option<u32> {
        self.deleted
            .binary_search(&physical)
            .err()
            .map(|deleted_before| physical - deleted_before as u32)
    }

    pub(crate) fn deleted(&self) -> &[u32] {
        &self.deleted
    }

    pub(crate) fn physical_id(&self, logical: u32) -> u32 {
        let mut low = logical;
        let mut high = (logical + self.deleted.len() as u32).min(self.physical_count - 1);
        while low < high {
            let middle = low + (high - low) / 2;
            let skipped = self.deleted.partition_point(|id| *id <= middle) as u32;
            if middle + 1 - skipped <= logical {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    pub(crate) fn locate(&self, logical: u32) -> (&Index, u32) {
        let physical = self.physical_id(logical);
        let part = self.starts.partition_point(|start| *start <= physical) - 1;
        (&self.parts[part], physical - self.starts[part])
    }
}
