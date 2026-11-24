//! 受限常驻模型进程与完整向量检索，查询期间可取消并回收进程。

use crate::{
    transport::{Framing, Transport},
    WorkerOptions,
};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const DIMENSION: usize = 512;
const MAX_REQUEST: usize = 128 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const DEFAULT_MODEL_MEMORY_MIB: u32 = 1024;
const PASSAGE_CHARS: usize = 128;
const LONG_SENTENCE_OVERLAP: usize = 24;

/// 按句子分片后再由模型做 token 分窗。长段落尾句单独保留，避免前文稀释短主题。
pub(crate) fn passages(text: &str) -> Vec<(usize, &str)> {
    let mut ranges = Vec::new();
    let mut current = None;
    let mut last_sentence = None;
    for (offset, sentence) in text.split_sentence_bound_indices() {
        let trimmed = sentence.trim();
        if trimmed.is_empty() {
            continue;
        }
        let start = offset + (sentence.len() - sentence.trim_start().len());
        let end = start + trimmed.len();
        let length = trimmed.chars().count();
        last_sentence = Some((start, end, length));
        if length > PASSAGE_CHARS {
            ranges.extend(current.take());
            let mut tail = trimmed;
            let mut position = start;
            while let Some((index, _)) = tail.char_indices().nth(PASSAGE_CHARS) {
                ranges.push((position, position + index));
                let (next, _) = tail
                    .char_indices()
                    .nth(PASSAGE_CHARS - LONG_SENTENCE_OVERLAP)
                    .expect("重叠应小于窗口");
                tail = &tail[next..];
                position += next;
            }
            ranges.push((position, end));
            continue;
        }
        current = match current.take() {
            Some((left, right)) if text[left..end].chars().count() > PASSAGE_CHARS => {
                ranges.push((left, right));
                Some((start, end))
            }
            Some((left, _)) => Some((left, end)),
            None => Some((start, end)),
        };
    }
    ranges.extend(current);
    if let Some((start, end, _)) = last_sentence.filter(|(start, _, length)| {
        ranges.len() > 1
            && ranges.last().is_some_and(|(last, _)| start > last)
            && (8..=PASSAGE_CHARS).contains(length)
    }) {
        ranges.push((start, end));
    }
    ranges
        .into_iter()
        .map(|(start, end)| (start, &text[start..end]))
        .collect()
}

#[derive(Deserialize)]
struct Hello {
    protocol: u32,
    model: String,
    dimension: usize,
    fingerprint: String,
}

#[derive(Deserialize)]
pub(crate) struct Window {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) vector: Vec<f32>,
}

impl Window {
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.vector
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }
}

#[derive(Deserialize)]
struct Reply {
    id: u64,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    windows: Vec<Window>,
}

pub(crate) fn adjacent_to(executable: &Path) -> PathBuf {
    executable.with_file_name(format!("xs-model-worker{}", std::env::consts::EXE_SUFFIX))
}

pub(crate) struct ModelClient {
    process: Transport,
    fingerprint: String,
    next: u64,
}

impl ModelClient {
    pub(crate) fn start(
        executable: &Path,
        options: &WorkerOptions,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        ensure!(!cancel.load(Ordering::Acquire), "语义任务已取消");
        xs_platform::validate_local_path(executable, false)
            .context("内置语义程序缺失，请修复安装")?;
        let mut command = Command::new(executable);
        command.arg("serve");
        let memory = options.memory_mib.max(DEFAULT_MODEL_MEMORY_MIB);
        let process = Transport::start(
            command,
            memory,
            options.timeout_ms,
            Framing::Line,
            MAX_RESPONSE,
        )?;
        let hello: Hello = serde_json::from_slice(&process.receive(cancel)?)?;
        ensure!(
            hello.protocol == 1
                && hello.model == "bge-small-zh-v1.5"
                && hello.dimension == DIMENSION
                && hello
                    .fingerprint
                    .split_once(':')
                    .is_some_and(|(model, tokenizer)| {
                        [model, tokenizer].iter().all(|sha| {
                            sha.len() == 64 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                    }),
            "内置语义程序能力不匹配，请修复安装"
        );
        Ok(Self {
            process,
            fingerprint: format!("sentence-v1:{}", hello.fingerprint),
            next: 0,
        })
    }

    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(crate) fn embed(&mut self, text: &str, cancel: &AtomicBool) -> Result<Vec<Window>> {
        ensure!(!cancel.load(Ordering::Acquire), "语义任务已取消");
        self.next = self.next.checked_add(1).context("语义请求编号耗尽")?;
        let mut frame = serde_json::to_vec(&json!({"id": self.next, "text": text}))?;
        frame.push(b'\n');
        ensure!(frame.len() <= MAX_REQUEST, "语义请求过大");
        let reply: Reply =
            serde_json::from_slice(&self.process.request(frame, Vec::new(), cancel)?)?;
        ensure!(reply.id == self.next, "语义进程返回了错位响应");
        if !reply.ok {
            bail!("{}", reply.error.unwrap_or_else(|| "语义推理失败".into()));
        }
        for window in &reply.windows {
            ensure!(
                window.start < window.end
                    && text.is_char_boundary(window.start)
                    && text.is_char_boundary(window.end)
                    && window.vector.len() == DIMENSION
                    && window
                        .vector
                        .iter()
                        .all(|value| value.is_finite() && value.abs() <= 1.01),
                "语义进程返回了无效的分窗向量"
            );
        }
        Ok(reply.windows)
    }
}

/// Validate model output once per exact scan, not once per indexed vector.
/// Decode each persisted vector once even for multi-window queries. Keep the
/// existing sequential f32 accumulation (including tie behavior).
pub(crate) struct PreparedQueries<'a> {
    vectors: Vec<&'a [f32; DIMENSION]>,
}

impl<'a> PreparedQueries<'a> {
    pub(crate) fn new(queries: &'a [Window]) -> Result<Self> {
        ensure!(!queries.is_empty(), "语义查询没有有效向量");
        let vectors = queries
            .iter()
            .map(|q| {
                let v: &[f32; DIMENSION] = q
                    .vector
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("查询向量长度无效"))?;
                ensure!(
                    v.iter().all(|v| v.is_finite() && v.abs() <= 1.01),
                    "查询向量无效"
                );
                Ok(v)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { vectors })
    }

    pub(crate) fn cosine(&self, embedding: &[u8]) -> Result<f64> {
        ensure!(embedding.len() == DIMENSION * 4, "正文库向量长度无效");
        let chunks: &[[u8; 4]; DIMENSION] = embedding
            .as_chunks::<4>()
            .0
            .try_into()
            .map_err(|_| anyhow::anyhow!("正文库向量长度无效"))?;
        let decoded = chunks.map(f32::from_le_bytes);
        ensure!(
            decoded.iter().all(|v| v.is_finite() && v.abs() <= 1.01),
            "正文库向量包含无效数据"
        );
        let mut best = f32::NEG_INFINITY;
        for query in &self.vectors {
            let score: f32 = decoded.iter().zip(query.iter()).map(|(v, q)| v * q).sum();
            ensure!(score.is_finite(), "正文库向量包含无效数据");
            best = best.max(score);
        }
        Ok(f64::from(best))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_queries_match_legacy_exact_scores_and_reject_invalid_vectors() {
        let make = |seed: usize| Window {
            start: 0,
            end: 1,
            vector: (0..DIMENSION)
                .map(|i| (((i * 17 + seed * 11) % 101) as f32 - 50.0) / 50.0)
                .collect(),
        };
        let queries: Vec<_> = (0..7).map(make).collect();
        let prepared = PreparedQueries::new(&queries).unwrap();
        for seed in 0..128 {
            let data = make(seed).bytes();
            let old = queries
                .iter()
                .map(|q| {
                    data.as_chunks::<4>()
                        .0
                        .iter()
                        .zip(&q.vector)
                        .map(|(bytes, q)| f32::from_le_bytes(*bytes) * q)
                        .sum::<f32>()
                })
                .fold(f32::NEG_INFINITY, f32::max);
            assert_eq!(prepared.cosine(&data).unwrap(), f64::from(old));
        }
        assert!(prepared.cosine(&[0; 4]).is_err());
        let mut data = make(0);
        data.vector[0] = f32::NAN;
        assert!(prepared.cosine(&data.bytes()).is_err());
        assert!(PreparedQueries::new(&[data]).is_err());
        let mut data = make(1);
        data.vector[0] = 2.0;
        assert!(prepared.cosine(&data.bytes()).is_err());
        assert!(PreparedQueries::new(&[data]).is_err());
        let mut data = make(2);
        data.vector.pop();
        assert!(PreparedQueries::new(&[data]).is_err());
        assert!(PreparedQueries::new(&[]).is_err());
    }

    #[test]
    fn passages_keep_long_utf8_tail_at_its_original_offset() {
        let terminal = "超导量子计算设备利用低温控制芯片。";
        let text = format!("{}{terminal}", "普通会议纪要按月整理并归档。".repeat(90));
        let segments = passages(&text);
        assert!(segments.len() > 2);
        assert!(segments.iter().all(|(start, part)| {
            part.chars().count() <= PASSAGE_CHARS && &text[*start..*start + part.len()] == *part
        }));
        let (offset, last) = segments.last().unwrap();
        assert_eq!(*last, terminal);
        assert_eq!(*offset + last.len(), text.len());
    }
}
