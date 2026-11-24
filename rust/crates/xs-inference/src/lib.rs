//! 只从固定本地安装目录加载校验过的 ONNX、字典和 tokenizer。
mod assets;
mod embedding;
mod ocr;

pub use assets::Assets;
pub use embedding::{Embedder, EmbeddingWindow};
pub use ocr::{Ocr, TextLine};

use anyhow::Result;
use ort::session::Session;
use std::path::Path;

fn session(path: &Path) -> Result<Session> {
    Ok(Session::builder()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .with_intra_threads(1)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .with_inter_threads(1)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .commit_from_file(path)?)
}
