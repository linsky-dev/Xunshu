# 寻枢 Xunshu

当前工程基线：`0.18.0-alpha.7`。免费、开源方向的本地文件/正文搜索工具，原生 Rust，运行时离线。当前成熟度评估 76/100（非竞品实测评分），未达到正式发行门槛。未经完整验收，不发布最终安装包。

- 完整 Rust 工作区：`rust/`（8 个成员，Rust 1.99.1，Cargo.lock 锁定）。
- 最新开发证据：[alpha.7 进展](checkpoints/alpha7/Professional_Progress.md)、[QA JSON](checkpoints/alpha7/QA_Evidence.json)、[测试日志](checkpoints/alpha7/Test_Log.txt)。
- [云端交接记录](CLOUD_HANDOFF.md)。当前构建与验证在本地执行。
- 旧版文档、迁移审计及历史清单仅用于追溯，不证明当前版本已经通过全部平台验收。

在 `rust/` 下执行 `cargo fetch --locked` 后，可进行 `cargo check --workspace --locked --offline` 和对应平台验证脚本。构建阶段允许下载锁定依赖，产品运行时不需要联网。禁止绕过文件系统和权限保护。

GitHub Actions 的官方 Windows Runner 为 Server；UOS、麒麟须在目标环境验证。未取得实机、模型、许可证、签名和安装证据前，禁止标记为 ReleaseReady。
