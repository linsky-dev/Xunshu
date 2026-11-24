# Xunshu 0.18.0-alpha.4 · 统一搜索与开源融合阶段 QA

- 日期：2026-11-23
- 环境：Debian Linux x86_64，Rust/Cargo 1.99.1，离线 Cargo vendor，本机 tmpfs 用于受保护数据测试。
- 范围：原已验收源码基础上进一步集成应用、已授权书签及 Git 仓库的主/快捷搜索；新增匹配预算与隐私防护；检查开源融合的许可证矩阵。
- **真实执行**：`cargo check --workspace --offline` 成功；`cargo test --workspace --offline --no-fail-fast` 使用项目 `scripts/verify_offline_linux.sh` 在 `tmpfs` 数据目录下运行，**197 项通过、0 项失败**（最终 alpha.4 版本，已重跑）；`cargo fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --offline -- -D warnings` 通过；`cargo build --offline -p xs-ui -p xs-cli` 通过；`xunshu --version` 回报 `0.18.0-alpha.4`；`xvfb-run` 原生 GUI 冒烟报告 `XS_UI_SMOKE_OK`，退出码 0。
- **尚未执行**：Windows 原生构建/安装与升级/回滚实测、麒麟/UOS GUI 实机、OCR/语义模型全量资源验收、百万级数据集性能、真实显示设备快捷键交互。
- **许可状态**：新增功能系自行实现，没有移植第三方仓库源码；仅调研对标，并制定将来引入时的许可台账。
- **发布状态**：工程预发布 alpha；不标记 ReleaseReady。
