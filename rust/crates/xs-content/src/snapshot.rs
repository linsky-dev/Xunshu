//! Integrity-checked, bounded two-slot snapshot storage. The publication
//! file is the sole commit point. A base is immutable while referenced; changes
//! are stored as a cumulative, bounded block overlay relative to that base.
//! A compaction writes the inactive base durably before replacing the pointer.
//! Digests detect corruption, not an adversary able to rewrite the entire profile.
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use xs_platform::{FileRevision, LocalWriteOutcome, PlatformError};

pub(crate) const MAGIC: [u8; 8] = *b"XSCONT02";
const PATCH_MAGIC: [u8; 8] = *b"XSCDEL03";
pub(crate) const HEADER_BYTES: usize = 56;
const OVERLAY_HEADER: usize = 120;
const BLOCK: usize = 32768;
pub(crate) const MAX_DB_SIZE: usize = 512 * 1024 * 1024;
const MAX_DELTA_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Base {
    slot: u8,
    generation: u64,
    size: usize,
    digest: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
struct Overlay {
    base: Base,
    blocks: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Head {
    pub generation: u64,
    pub size: usize,
    digest: [u8; 32],
    overlay: Option<Overlay>,
}

fn base_path(path: &Path, slot: u8) -> PathBuf {
    path.with_file_name(if slot == 0 {
        "content.base-a.idx"
    } else {
        "content.base-b.idx"
    })
}

pub(crate) fn encode_header(generation: u64, bytes: &[u8]) -> [u8; HEADER_BYTES] {
    let mut header = [0; HEADER_BYTES];
    header[..8].copy_from_slice(&MAGIC);
    header[8..16].copy_from_slice(&generation.to_le_bytes());
    header[16..24].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
    header[24..].copy_from_slice(&Sha256::digest(bytes));
    header
}

pub(crate) fn read_head(file: &mut File) -> Result<Head> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = [0; HEADER_BYTES];
    file.read_exact(&mut bytes)?;
    let generation = u64::from_le_bytes(bytes[8..16].try_into()?);
    let size = usize::try_from(u64::from_le_bytes(bytes[16..24].try_into()?))?;
    ensure!(
        (1..=i64::MAX as u64).contains(&generation) && (512..=MAX_DB_SIZE).contains(&size),
        "正文库头部或容量无效"
    );
    let digest = bytes[24..].try_into()?;
    let length = file.metadata()?.len();
    let overlay = if bytes[..8] == MAGIC {
        ensure!(length == (HEADER_BYTES + size) as u64, "正文库长度无效");
        None
    } else if bytes[..8] == PATCH_MAGIC {
        let mut extra = [0; OVERLAY_HEADER - HEADER_BYTES];
        file.read_exact(&mut extra)?;
        let slot = extra[0];
        let base_generation = u64::from_le_bytes(extra[8..16].try_into()?);
        let base_size = usize::try_from(u64::from_le_bytes(extra[16..24].try_into()?))?;
        let base_digest = extra[24..56].try_into()?;
        let blocks = u32::from_le_bytes(extra[56..60].try_into()?) as usize;
        ensure!(
            slot <= 1
                && extra[1..8] == [0; 7]
                && extra[60..64] == [0; 4]
                && (1..=generation).contains(&base_generation)
                && (512..=MAX_DB_SIZE).contains(&base_size)
                && blocks <= size.div_ceil(BLOCK)
                && length >= OVERLAY_HEADER as u64
                && length <= (OVERLAY_HEADER + blocks * (BLOCK + 4)) as u64
                && length <= (OVERLAY_HEADER + MAX_DELTA_BYTES) as u64,
            "正文增量头部无效，请完整重建"
        );
        Some(Overlay {
            base: Base {
                slot,
                generation: base_generation,
                size: base_size,
                digest: base_digest,
            },
            blocks,
        })
    } else {
        anyhow::bail!("无法识别正文库格式，请重建");
    };
    Ok(Head {
        generation,
        size,
        digest,
        overlay,
    })
}

fn open_base(path: &Path, descriptor: Base) -> Result<(File, FileRevision)> {
    let mut file = xs_platform::open_local_read(&base_path(path, descriptor.slot))?;
    let revision = FileRevision::from_file(&file)?;
    let head = read_head(&mut file)?;
    ensure!(
        head.overlay.is_none()
            && head.generation == descriptor.generation
            && head.size == descriptor.size
            && head.digest == descriptor.digest,
        "正文增量的基准快照不匹配，请完整校准"
    );
    Ok((file, revision))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub publication: FileRevision,
    pub base: Option<(u8, FileRevision)>,
}
impl Stamp {
    pub fn verify(&self, path: &Path) -> Result<()> {
        ensure!(
            FileRevision::at_path(path)? == self.publication,
            "正文索引已更新，请重新搜索"
        );
        if let Some((slot, revision)) = self.base {
            ensure!(
                slot <= 1 && FileRevision::at_path(&base_path(path, slot))? == revision,
                "正文基准索引已更新，请重新搜索"
            );
        }
        Ok(())
    }
}

pub(crate) struct OpenSnapshot {
    file: File,
    base_file: Option<File>,
    pub head: Head,
    pub stamp: Stamp,
}

impl OpenSnapshot {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = xs_platform::open_local_read(path)?;
        let publication = FileRevision::from_file(&file)?;
        let head = read_head(&mut file)?;
        let (base_file, base) = if let Some(overlay) = head.overlay {
            // A candidate checkpoint must always be a self-contained snapshot.
            ensure!(
                path.file_name().and_then(|n| n.to_str()) == Some(super::CONTENT_DB),
                "检查点不能引用外部基准"
            );
            let (file, revision) = open_base(path, overlay.base)?;
            (Some(file), Some((overlay.base.slot, revision)))
        } else {
            (None, None)
        };
        let stamp = Stamp { publication, base };
        stamp.verify(path)?;
        Ok(Self {
            file,
            base_file,
            head,
            stamp,
        })
    }

    /// Reconstruct using a single output allocation, hashing all base bytes
    /// (including a truncated tail), then validating the full reconstructed DB.
    pub fn read_into(&mut self, output: &mut [u8], cancel: &AtomicBool) -> Result<()> {
        ensure!(output.len() == self.head.size, "正文读取缓冲区长度错误");
        if let Some(overlay) = self.head.overlay {
            if output.len() > overlay.base.size {
                output[overlay.base.size..].fill(0);
            }
            let base = self.base_file.as_mut().context("正文基准句柄缺失")?;
            let mut digest = Sha256::new();
            let mut chunk = vec![0; BLOCK];
            for start in (0..overlay.base.size).step_by(BLOCK) {
                ensure!(!cancel.load(Ordering::Acquire), "正文库读取已取消");
                let n = BLOCK.min(overlay.base.size - start);
                base.read_exact(&mut chunk[..n])?;
                digest.update(&chunk[..n]);
                if start < output.len() {
                    let copy = n.min(output.len() - start);
                    output[start..start + copy].copy_from_slice(&chunk[..copy]);
                }
            }
            ensure!(
                digest.finalize()[..] == overlay.base.digest
                    && Some((overlay.base.slot, FileRevision::from_file(base)?)) == self.stamp.base,
                "正文基准已损坏或读取期间已更改"
            );
            let mut last = None;
            for _ in 0..overlay.blocks {
                ensure!(!cancel.load(Ordering::Acquire), "正文库读取已取消");
                let mut raw = [0; 4];
                self.file.read_exact(&mut raw)?;
                let index = u32::from_le_bytes(raw) as usize;
                ensure!(
                    index < output.len().div_ceil(BLOCK)
                        && last.is_none_or(|previous| index > previous),
                    "正文增量块顺序或范围无效"
                );
                let start = index * BLOCK;
                let end = (start + BLOCK).min(output.len());
                self.file.read_exact(&mut output[start..end])?;
                last = Some(index);
            }
            ensure!(
                self.file.stream_position()? == self.file.metadata()?.len(),
                "正文增量存在多余字节"
            );
        } else {
            for chunk in output.chunks_mut(1024 * 1024) {
                ensure!(!cancel.load(Ordering::Acquire), "正文库读取已取消");
                self.file.read_exact(chunk)?;
            }
        }
        let mut digest = Sha256::new();
        for chunk in output.chunks(1024 * 1024) {
            ensure!(!cancel.load(Ordering::Acquire), "正文库读取已取消");
            digest.update(chunk);
        }
        ensure!(
            digest.finalize()[..] == self.head.digest
                && FileRevision::from_file(&self.file)? == self.stamp.publication,
            "正文库已损坏或读取期间已更改，请重建或重试"
        );
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct PublishStats {
    pub bytes_written: u64,
    pub delta_blocks: usize,
    pub base_rewritten: bool,
    pub cleanup_warning: Option<String>,
}

fn write_chunks(file: &mut File, bytes: &[u8], cancel: &AtomicBool) -> io::Result<()> {
    for chunk in bytes.chunks(1024 * 1024) {
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::other("正文重建已取消"));
        }
        file.write_all(chunk)?;
    }
    Ok(())
}

/// Compare against the immutable base. Keep only block numbers, not duplicated
/// data. The cumulative overlay is compacted before it exceeds 25% of the base
/// (at least eight blocks, capped at 64 MiB).
fn changed_blocks(
    path: &Path,
    base: Base,
    bytes: &[u8],
    cancel: &AtomicBool,
) -> Result<Option<(Vec<u32>, FileRevision)>> {
    let (mut file, revision) = open_base(path, base)?;
    let budget = (base.size / 4).clamp(8 * BLOCK, MAX_DELTA_BYTES);
    let mut changed = Vec::new();
    let mut patch_size = 0;
    let mut old = vec![0; BLOCK];
    let mut digest = Sha256::new();
    for start in (0..base.size.max(bytes.len())).step_by(BLOCK) {
        ensure!(!cancel.load(Ordering::Acquire), "正文增量比较已取消");
        let n = base.size.saturating_sub(start).min(BLOCK);
        file.read_exact(&mut old[..n])?;
        digest.update(&old[..n]);
        if start >= bytes.len() {
            continue;
        }
        let current = &bytes[start..(start + BLOCK).min(bytes.len())];
        if n < current.len() || old[..current.len()] != *current {
            patch_size += current.len() + 4;
            if patch_size > budget {
                return Ok(None);
            }
            changed.push(u32::try_from(start / BLOCK)?);
        }
    }
    ensure!(
        digest.finalize()[..] == base.digest && FileRevision::from_file(&file)? == revision,
        "正文基准在增量比较期间损坏或被替换"
    );
    Ok(Some((changed, revision)))
}

fn overlay_header(
    generation: u64,
    bytes: &[u8],
    base: Base,
    blocks: usize,
) -> Result<[u8; OVERLAY_HEADER]> {
    let mut header = [0; OVERLAY_HEADER];
    header[..HEADER_BYTES].copy_from_slice(&encode_header(generation, bytes));
    header[..8].copy_from_slice(&PATCH_MAGIC);
    header[56] = base.slot;
    header[64..72].copy_from_slice(&base.generation.to_le_bytes());
    header[72..80].copy_from_slice(&(base.size as u64).to_le_bytes());
    header[80..112].copy_from_slice(&base.digest);
    header[112..116].copy_from_slice(&u32::try_from(blocks)?.to_le_bytes());
    Ok(header)
}

pub(crate) fn publish(
    path: &Path,
    bytes: &[u8],
    generation: u64,
    allow_delta: bool,
    cancel: &AtomicBool,
    validate: impl Fn() -> Result<()>,
) -> Result<(LocalWriteOutcome, PublishStats)> {
    ensure!(
        (512..=MAX_DB_SIZE).contains(&bytes.len()),
        "正文库超出容量上限"
    );
    validate()?;
    ensure!(!cancel.load(Ordering::Acquire), "正文重建已取消");
    let old_base = xs_platform::open_local_read(path)
        .ok()
        .and_then(|mut file| read_head(&mut file).ok())
        .and_then(|head| head.overlay.map(|overlay| overlay.base));
    let diff = if allow_delta {
        if let Some(base) = old_base {
            changed_blocks(path, base, bytes, cancel)?
                .map(|(blocks, revision)| (base, blocks, revision))
        } else {
            None
        }
    } else {
        None
    };
    let (base, blocks, base_revision, base_rewritten) = if let Some((base, blocks, revision)) = diff
    {
        (base, blocks, revision, false)
    } else {
        // Never overwrite the base referenced by the current publication.
        let slot = old_base.map_or(0, |old| 1 - old.slot);
        let base = Base {
            slot,
            generation,
            size: bytes.len(),
            digest: Sha256::digest(bytes).into(),
        };
        let path_base = base_path(path, slot);
        let outcome = xs_platform::write_local_replace(&path_base, |file| {
            file.write_all(&encode_header(generation, bytes))?;
            write_chunks(file, bytes, cancel)?;
            validate().map_err(io::Error::other)
        })?;
        ensure!(
            outcome.is_durable(),
            "正文基准已写入但持久化未确认，未切换正式索引"
        );
        (base, Vec::new(), FileRevision::at_path(&path_base)?, true)
    };
    let header = overlay_header(generation, bytes, base, blocks.len())?;
    let mut bytes_written = (if base_rewritten {
        HEADER_BYTES + bytes.len()
    } else {
        0
    }) + OVERLAY_HEADER;
    for &index in &blocks {
        bytes_written += 4 + (bytes.len() - index as usize * BLOCK).min(BLOCK);
    }
    let outcome = xs_platform::write_local_replace(path, |file| {
        file.write_all(&header)?;
        for &index in &blocks {
            if cancel.load(Ordering::Acquire) {
                return Err(io::Error::other("正文增量发布已取消"));
            }
            file.write_all(&index.to_le_bytes())?;
            let start = index as usize * BLOCK;
            file.write_all(&bytes[start..(start + BLOCK).min(bytes.len())])?;
        }
        validate().map_err(io::Error::other)?;
        if FileRevision::at_path(&base_path(path, base.slot)).map_err(io::Error::other)?
            != base_revision
        {
            return Err(io::Error::other("正文基准在提交前发生变化"));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::other("正文重建已取消"));
        }
        Ok(())
    })?;
    let mut cleanup_warning = None;
    if outcome.is_durable() {
        let obsolete = base_path(path, 1 - base.slot);
        match xs_platform::remove_local_file(&obsolete) {
            Ok(()) => (),
            Err(PlatformError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => (),
            Err(error) => cleanup_warning = Some(format!("旧正文基准未清理：{error}")),
        }
    }
    Ok((
        outcome,
        PublishStats {
            bytes_written: bytes_written as u64,
            delta_blocks: blocks.len(),
            base_rewritten,
            cleanup_warning,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs};

    fn read(path: &Path) -> Result<Vec<u8>> {
        let mut snapshot = OpenSnapshot::open(path)?;
        let mut output = vec![0xff; snapshot.head.size];
        snapshot.read_into(&mut output, &AtomicBool::new(false))?;
        Ok(output)
    }

    #[test]
    fn legacy_overlay_roundtrip_and_corruption() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(super::super::CONTENT_DB);
        let cancel = AtomicBool::new(false);
        let initial = vec![42; BLOCK * 64];
        let mut legacy = encode_header(1, &initial).to_vec();
        legacy.extend_from_slice(&initial);
        fs::write(&path, legacy)?;
        assert_eq!(read(&path)?, initial);
        let (_, first) = publish(&path, &initial, 2, true, &cancel, || Ok(()))?;
        assert!(first.base_rewritten);
        let old = OpenSnapshot::open(&path)?.stamp;
        let mut updated = initial.clone();
        updated[BLOCK * 5 + 10] = 99;
        updated.extend_from_slice(&[17; 100]);
        let (_, delta) = publish(&path, &updated, 3, true, &cancel, || Ok(()))?;
        assert!(!delta.base_rewritten);
        assert_eq!(delta.delta_blocks, 2);
        assert!(delta.bytes_written < initial.len() as u64 / 10);
        assert_eq!(read(&path)?, updated);
        assert_eq!(OpenSnapshot::open(&path)?.stamp.base, old.base);
        assert!(old.verify(&path).is_err());
        updated.truncate(BLOCK * 63 + 15);
        publish(&path, &updated, 4, true, &cancel, || Ok(()))?;
        assert_eq!(read(&path)?, updated);
        let pointer = fs::read(&path)?;
        let mut bad = pointer.clone();
        *bad.last_mut().unwrap() ^= 1;
        fs::write(&path, bad)?;
        assert!(read(&path).is_err());
        fs::write(&path, pointer)?;
        let stamp = OpenSnapshot::open(&path)?.stamp;
        let base = base_path(&path, stamp.base.unwrap().0);
        let mut bad = fs::read(&base)?;
        *bad.last_mut().unwrap() ^= 1;
        fs::write(base, bad)?;
        assert!(stamp.verify(&path).is_err());
        assert!(read(&path).is_err());
        Ok(())
    }

    // Test real kernel write refusal without filling a shared disk or changing
    // the test runner's process-wide resource limits. EFBIG is not ENOSPC.
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_file_size_limit_preserves_published_snapshot() -> Result<()> {
        const CHILD_PATH: &str = "XS_TEST_FSIZE_SNAPSHOT_PATH";
        const CHILD_DELTA: &str = "XS_TEST_FSIZE_SNAPSHOT_DELTA";
        let cancel = AtomicBool::new(false);
        let initial = vec![12; BLOCK * 16];
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = std::path::PathBuf::from(path);
            let delta = std::env::var_os(CHILD_DELTA).is_some();
            let mut updated = initial.clone();
            updated[BLOCK + 10] = 23;
            // SAFETY: this branch runs in the isolated test child; the limit
            // affects only this process. SIG_IGN lets write report EFBIG.
            unsafe {
                assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
                let limit = libc::rlimit {
                    rlim_cur: 8192,
                    rlim_max: 8192,
                };
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            }
            let error = publish(&path, &updated, 2, delta, &cancel, || Ok(())).unwrap_err();
            assert!(
                error.chain().any(|cause| cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.raw_os_error() == Some(libc::EFBIG))),
                "{error:#}"
            );
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(super::super::CONTENT_DB);
        publish(&path, &initial, 1, false, &cancel, || Ok(()))?;
        let before = OpenSnapshot::open(&path)?.stamp;
        let file_count = fs::read_dir(dir.path())?.count();
        for delta in [false, true] {
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command
                .args([
                    "--exact",
                    "snapshot::tests::kernel_file_size_limit_preserves_published_snapshot",
                    "--nocapture",
                ])
                .env(CHILD_PATH, &path)
                .env_remove(CHILD_DELTA);
            if delta {
                command.env(CHILD_DELTA, "1");
            }
            let result = command.output()?;
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
            before.verify(&path)?;
            assert_eq!(read(&path)?, initial);
            assert_eq!(fs::read_dir(dir.path())?.count(), file_count);
        }
        let mut updated = initial;
        updated[BLOCK + 10] = 23;
        publish(&path, &updated, 2, true, &cancel, || Ok(()))?;
        assert_eq!(read(&path)?, updated);
        Ok(())
    }

    #[test]
    fn interrupted_compaction_preserves_current_snapshot() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(super::super::CONTENT_DB);
        let cancel = AtomicBool::new(false);
        let initial = vec![12; BLOCK * 16];
        publish(&path, &initial, 1, false, &cancel, || Ok(()))?;
        let before = OpenSnapshot::open(&path)?.stamp;
        let checks = Cell::new(0);
        let updated = vec![23; BLOCK * 16];
        let result = publish(&path, &updated, 2, false, &cancel, || {
            checks.set(checks.get() + 1);
            ensure!(checks.get() < 3, "injected interruption before publication");
            Ok(())
        });
        assert!(result.is_err());
        before.verify(&path)?;
        assert_eq!(read(&path)?, initial);
        publish(&path, &updated, 2, false, &cancel, || Ok(()))?;
        assert_eq!(read(&path)?, updated);
        assert!(!base_path(&path, before.base.unwrap().0).exists());
        Ok(())
    }
}
