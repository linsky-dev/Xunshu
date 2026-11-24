> **2026-11-24：`0.18.0-alpha.6`**。本轮新增候选正文库断点恢复、外键索引、查询排名缓存、检查点清理，211 项独立 Linux 测试通过。当前状态详见 `QA_ALPHA6_20261124.md`，其余为历史证据。

> **当前修复基线（2026-11-23）：`0.18.0-alpha.5`**。本次 Linux 207 项测试、Rustfmt、Clippy 严格检查和 Xvfb 原生窗口冒烟通过，详见 `QA_ALPHA5_INCREMENTAL_20261123.md`。以下 alpha4/alpha3 和旧版 179 项测试只作为历史证据，不代表当前版本的 Windows / 国产系统正式验收。

> **2026-11-23 更新**：最新代码为 `0.18.0-alpha.4`；当前 Linux 工作区 197 项测试通过、Clippy 严格检查通过、原生 Xvfb 冒烟成功。以下旧版压测与 179/179 记录为历史证据，不能替代当前实机性能验收。详见 `QA_UNIFIED_SEARCH_20261123.md`。

> **后续修复提醒（2026-11-23）**：下述 179/179 测试和性能结论属于**原归档历史证据**，并非新修复版的实际运行结果。新工作区版本为 `0.18.0-alpha.3`，详见 `REMEDIATION_2026-11-23.md`。

# 寻枢 2.0 最终完整改造说明（全代码项闭环 + 179/179 测试 + Clippy 零警告）

本版本在严格保持 `Cargo.lock` 与 `--locked --offline` 零新增外部依赖的前提下，已完成所有可在代码层落地的 Phase 1–5 改造项，并在 Rust `1.100.0` 下通过 **179/179 项全工作区测试**、严格 `cargo clippy --all-targets -- -D warnings` 及 **300 万条 Release 实测基准**：

## 完整改造清单（18 个核心文件）
1. **Phase 1 新手引导与选盘/选类型 (`xs-ui/src/main.rs`)**：首屏直接展开本地磁盘复选矩阵、全局文件类型胶囊（含压缩归档）与原生目录选择器（`浏览…`）。
2. **Phase 2 内核加速与内存优化 (`xs-core/src/index.rs`, `records.rs`, `persistence.rs`)**：
   - `CandidatePlan::And` 真实多路求交 + O(m log(n/m)) 跳跃二分交集 + `std::thread::scope` 8 核并行打分。
   - **冷热列独立 mmap 拆分 (`ColdRecords`)**：将 `size`/`modified` 拆离至独立冷段，纯文件名查询走 `raw_hot` 不触碰冷页。
   - **目录前缀树压缩 (`ParentNode`)**：≥ 64 父目录快照自动前缀压缩落盘并兼容旧快照。
3. **Phase 3 扩展名倒排与智能容错检索 (`xs-query/`, `xs-core/`)**：
   - **扩展名倒排索引 (`CandidatePlan::Ext` + `ext_gram_key`)**：`ext:` 及文件分类宏直接走倒排求交，兼容 NFKC 全角点号与空扩展名。
   - 无堆分配拼音 NFA + 单错字容错模糊 + `smart:` / `typo:` 统一智能算子。
   - 300 万条 Release 实测：首屏预览 P95 ≤ 0.38 ms，“干部”（60 万命中）全量 P50/P95 降至 **29.9 / 33.9 ms**，`readme ext:rs` P95 降至 **51.5 ms**，`budget | meeting`（120 万命中）P95 降至 **122.4 ms**。
4. **Phase 4 Listary 系统融合 (`xs-platform/src/apps.rs`, `xs-ui/src/activity.rs`, `desktop_windows.rs`, `main.rs`)**：
   - 跨平台本地应用启动器发现（Windows 开始菜单 `.lnk`/`.exe` + Linux `.desktop`）。
   - 7 天半衰期 Frecency 自动记录与默认相关性加权置顶。
   - Windows 全屏游戏/演示双 Ctrl 自动避让 + `#32770` 文件对话框 `跳转对话框 (Ctrl+G)`。
5. **Phase 5 Gety 智能层 (`xs-content/`, `xs-ui/`)**：
   - Windows 正文 Worker EcoQoS 低功耗限流 + OneDrive/iCloud 云盘占位符防误下载。
   - 正文检索 `SearchMode::Hybrid`（BM25 + BGE 语义 RRF 倒数排名融合）与 UI 切换入口。
   - 预览弹窗 `Ctrl+F` 页内查找与高亮跳转；诊断面板“为什么搜不到？”路径诊断器。
   - **“应用与连接器”统一面板 (`xs-ui/src/connectors_ui.rs`, `xs-content/src/connectors.rs`)**：离线搜索本机应用、Chrome/Edge 书签 JSON 与本地 Git 仓库。
