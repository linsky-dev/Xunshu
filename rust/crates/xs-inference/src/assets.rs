use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
};

#[cfg(windows)]
const LIBRARY: &str = "onnxruntime.dll";
#[cfg(not(windows))]
const LIBRARY: &str = "libonnxruntime.so";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    files: BTreeMap<String, DigestEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DigestEntry {
    bytes: u64,
    sha256: String,
    source: String,
    license: String,
}

pub struct Assets {
    root: PathBuf,
    embedding_fingerprint: String,
}

impl Assets {
    /// 普通运行只接受当前安装程序旁的固定资源目录。
    pub fn installed() -> Result<Self> {
        let exe = std::env::current_exe()?;
        Self::verify(&exe.parent().context("无法定位程序目录")?.join("resources"))
    }

    /// 安装器暂存校验和实际推理共用同一清单。
    pub fn verify(root: &Path) -> Result<Self> {
        let manifest_file = xs_platform::open_local_read(&root.join("inference.json"))
            .context("缺少内置模型清单，请修复安装")?;
        let mut bytes = Vec::new();
        manifest_file.take(65537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65536, "内置模型清单过大，请修复安装");
        let manifest: Manifest =
            serde_json::from_slice(&bytes).context("内置模型清单损坏，请修复安装")?;
        let required = [
            LIBRARY,
            "bge.onnx",
            "bge-tokenizer.json",
            "ppocr-det.onnx",
            "ppocr-rec.onnx",
            "ppocr-dictionary.txt",
        ];
        ensure!(
            manifest.schema == 1 && manifest.files.len() == required.len(),
            "内置模型清单版本或资源集合无效"
        );
        for name in required {
            let record = manifest
                .files
                .get(name)
                .with_context(|| format!("缺少内置资源 {name}，请修复安装"))?;
            ensure!(
                !record.source.is_empty()
                    && !record.license.is_empty()
                    && record.bytes <= 512 * 1024 * 1024,
                "资源清单记录无效：{name}"
            );
            let mut file = xs_platform::open_local_read(&root.join(name))
                .with_context(|| format!("缺少内置资源 {name}，请修复安装"))?;
            ensure!(
                file.metadata()?.len() == record.bytes,
                "内置资源长度异常：{name}，请修复安装"
            );
            let mut hash = Sha256::new();
            std::io::copy(&mut file, &mut hash)?;
            ensure!(
                format!("{:x}", hash.finalize()) == record.sha256,
                "内置资源摘要异常：{name}，请修复安装"
            );
        }
        let embedding_fingerprint = format!(
            "{}:{}",
            manifest.files["bge.onnx"].sha256, manifest.files["bge-tokenizer.json"].sha256
        );
        Ok(Self {
            root: root.to_owned(),
            embedding_fingerprint,
        })
    }

    pub fn embedding_fingerprint(&self) -> &str {
        &self.embedding_fingerprint
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub(crate) fn initialize(&self) -> Result<()> {
        static ORT: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();
        let library = self.path(LIBRARY);
        let loaded = ORT.get_or_init(|| {
            let env = ort::init_from(&library).map_err(|error| error.to_string())?;
            env.with_name("寻枢离线推理").with_telemetry(false).commit();
            Ok(library.clone())
        });
        match loaded {
            Ok(path) if *path == library => Ok(()),
            Ok(_) => anyhow::bail!("当前进程已经加载另一版本的内置推理库"),
            Err(error) => anyhow::bail!("内置推理库无法加载，请修复安装：{error}"),
        }
    }
}
