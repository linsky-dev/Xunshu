use crate::{session, Assets};
use anyhow::{ensure, Context, Result};
use ort::{session::Session, value::Tensor};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use tokenizers::{Tokenizer, TruncationParams};

const MAX_TOKENS: usize = 512;
const OVERLAP: usize = 96;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingWindow {
    pub start: usize,
    pub end: usize,
    pub vector: Vec<f32>,
}

pub struct Embedder {
    tokenizer: Tokenizer,
    session: Session,
}

impl Embedder {
    pub fn new(assets: &Assets) -> Result<Self> {
        assets.initialize()?;
        let mut tokenizer = Tokenizer::from_file(assets.path("bge-tokenizer.json"))
            .map_err(|error| anyhow::anyhow!("内置 tokenizer 无法加载：{error}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                stride: OVERLAP,
                ..Default::default()
            }))
            .map_err(|error| anyhow::anyhow!("内置 tokenizer 无法设置分窗：{error}"))?;
        Ok(Self {
            tokenizer,
            session: session(&assets.path("bge.onnx"))?,
        })
    }

    /// tokenizer 负责保留每窗的起止偏移以及特殊 token，覆盖长文尾部。
    pub fn embed(&mut self, text: &str, cancel: &AtomicBool) -> Result<Vec<EmbeddingWindow>> {
        ensure!(!cancel.load(Ordering::Relaxed), "语义推理已取消");
        ensure!(text.len() <= 1024 * 1024, "语义片段超出长度限制");
        let encoded = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        ensure!(!cancel.load(Ordering::Relaxed), "语义推理已取消");
        let mut result = Vec::with_capacity(1 + encoded.get_overflowing().len());
        for window in std::iter::once(&encoded).chain(encoded.get_overflowing()) {
            ensure!(!cancel.load(Ordering::Relaxed), "语义推理已取消");
            let range = window
                .get_offsets()
                .iter()
                .copied()
                .filter(|(start, end)| start < end)
                .fold(None, |range: Option<(usize, usize)>, (start, end)| {
                    Some(range.map_or((start, end), |(left, right)| {
                        (left.min(start), right.max(end))
                    }))
                });
            let Some((start, end)) = range else {
                continue;
            };
            ensure!(
                start < end
                    && end <= text.len()
                    && text.is_char_boundary(start)
                    && text.is_char_boundary(end),
                "模型分窗偏移无效"
            );
            let length = window.len();
            ensure!((1..=MAX_TOKENS).contains(&length), "模型分窗长度无效");
            let tensor = |values: &[u32]| {
                Tensor::from_array((
                    [1, length],
                    values
                        .iter()
                        .map(|value| i64::from(*value))
                        .collect::<Vec<_>>(),
                ))
            };
            let outputs = self.session.run(ort::inputs! {
                "input_ids" => tensor(window.get_ids())?,
                "attention_mask" => tensor(window.get_attention_mask())?,
                "token_type_ids" => tensor(window.get_type_ids())?,
            })?;
            ensure!(!cancel.load(Ordering::Relaxed), "语义推理已取消");
            let (shape, vector) = outputs
                .get("embedding")
                .context("语义模型缺少 embedding 输出")?
                .try_extract_tensor::<f32>()?;
            ensure!(shape.as_ref() == [1, 512], "语义模型输出形状无效");
            result.push(EmbeddingWindow {
                start,
                end,
                vector: normalize(vector)?,
            });
        }
        ensure!(!cancel.load(Ordering::Relaxed), "语义推理已取消");
        Ok(result)
    }
}

fn normalize(vector: &[f32]) -> Result<Vec<f32>> {
    ensure!(
        vector.len() == 512 && vector.iter().all(|v| v.is_finite()),
        "语义模型输出长度或数值无效"
    );
    let norm = vector
        .iter()
        .map(|&v| f64::from(v).powi(2))
        .sum::<f64>()
        .sqrt();
    ensure!(norm.is_finite() && norm > 1e-12, "语义模型输出空向量");
    Ok(vector
        .iter()
        .map(|&v| (f64::from(v) / norm) as f32)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalization_rejects_invalid_outputs() {
        for value in [0.0, f32::NAN, f32::INFINITY, 1e-30] {
            assert!(normalize(&[value; 512]).is_err());
        }
        assert!(normalize(&[1.0; 511]).is_err());
    }

    #[test]
    fn normalization_handles_large_finite_values() {
        for value in [1.0, f32::MAX] {
            let vector = normalize(&[value; 512]).unwrap();
            let length_squared: f64 = vector.iter().map(|&v| f64::from(v).powi(2)).sum();
            assert!((length_squared - 1.0).abs() < 1e-6);
        }
    }
}
