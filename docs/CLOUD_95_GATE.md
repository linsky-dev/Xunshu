# Xunshu 云端专业化 95+ 放行门槛

基线 main: b0f707d20afffaad28b899c7492102e29237b659；产品版本 0.18.0-alpha.7。开发分支 cloud/alpha8。95+ 为目标，不是完成声明。沿用 alpha.7 六维权重，不以测试数量或源码提交代替可交付性。

## 本轮实证（本地 Linux，不是 Actions）
- 搜索内核路径加权复用字符串缓冲，避免每个命中重复分配；并行预算/取消轮询间隔从 4096 条改为 128 条。
- 新增带路径过滤及全局 Top-K 排序的回归用例；明确保持既有 OR 优先于 AND 的查询语义。
- 代码提交 a63adc74cadebba556dfe6d003466535f8fd9978，index.rs blob 108846ae8cdb44ba9d6d7fd1ea5b4e8cda72c883 与本地测试版本一致。
- Rust 1.99.1: Rustfmt、全工作区 check、全目标 Clippy -D warnings 通过；47 项 xs-core 测试通过；全工作区原始测试摘要 221 次成功（其中独立进程恢复子测试重复计数一次），220 项独立测试，0 失败、0 忽略。测试使用受安全策略允许的 tmpfs 数据目录及前一轮同源的已构建 worker；未关闭文件系统安全保护。
- 尚未重新测量宽 OR 及真实个性化性能，不宣称达到 50 ms。
- CI 已配置 Windows Server 2022 和 Ubuntu 24.04；包含锁定依赖、严格检查、测试、Release 编译、CLI 恢复、Linux 窗口烟测，另有手动真实模型/安装测试选项。当前尚无可读取的实际 Actions run 或 Check，不算云端验收通过。

## 权重与 95+ 目标（建议达到 96 分，不能预先记分）

| 维度 | 权重 | alpha.7 基线贡献 | 目标贡献 | 必须提供的证据 |
|---|---:|---:|---:|---|
| 搜索正确性与索引 | 20 | 17 | 19 | 真实 NTFS USN/MFT、多磁盘、Linux 监听、删除/改名/重启、中文检索真值与稳定分页，零关键漏检 |
| 安全及恢复 | 20 | 17 | 19 | 磁盘满、强制退出、权限撤销、损坏快照/模型、目录替换、更新事务故障注入；无越权与数据丢失 |
| 桌面交互 | 15 | 11.25 | 14.5 | Win10/11、UOS、麒麟桌面中文输入法、多屏、托盘、键盘、文件打开/系统对话框、无障碍人工用例 |
| 功能与扩展 | 15 | 12 | 14.5 | OCR 混合 PDF、复杂 Office、语义召回真值、模型隔离和授权撤销、可解释结果、插件边界 |
| 性能及资源 | 15 | 12 | 14.5 | Release 同机 300 万元数据、10 万真实/合成正文分别报告、宽 OR P95 <= 50ms（沿用原目标）；真实 SSD 扫描、峰值内存、常驻 CPU/磁盘、ANN 精度和性能权衡、72 小时稳定性 |
| 发行与安装 | 15 | 6.75 | 14.5 | 每个声明支持平台原生安装、覆盖升级、回滚、卸载、保留数据、完整依赖/模型/字体、许可清单、可验证签名、离线运行 |
| 合计 | 100 | 76 | 96 | 复审实际证据才允许上调 |

## 分阶段执行
1. GitHub Actions：先运行基础 CI，保留每个平台日志，逐个修复失败；任何失败、不运行或跳过均不得算通过。
2. 手动模型验证：勾选 model_validation，使用已有 prepare-models.py、make-install-payload.py、verify-recovery.py --with-models、verify-install.py。模型转换依赖与权重、原生 ONNX Runtime 版本锁定；字体来源固定 Noto CJK Sans2.004 提交，最终发行前完成摘要/许可证复核。合成 OCR 自检不等同真实文档语料验收。仅在隔离 Runner 上构建测试载荷，不发布安装包。
3. UOS/麒麟：记录具体版本/架构和 GLIBC 基线，可信容器兼容检查后使用专用 UOS/麒麟自托管 Runner；真实桌面和 Win10/11 单独验证。不得关闭文件系统防护来适配 OverlayFS；禁止在不可信 PR 上运行自托管 Runner。
4. 性能攻坚：先采样剖析 OR 热路径，保证全量真值；独立优化 ANN（与精确检索对照召回）、磁盘写放大、长期内存；加入真实文档、文件系统故障矩阵及对标软件同机测试。
5. 产品验收：全部已声明平台实际完成安装与回滚、签名、离线测试；第三方/模型授权核查。95+ 是必要但非充分条件，P0 或平台门槛未过禁止发行。仅保留工程证据。

## 操作入口
Actions: https://github.com/linsky-dev/Xunshu/actions
草稿 PR: https://github.com/linsky-dev/Xunshu/pull/1
GitHub 官方 Runner: https://docs.github.com/en/actions/reference/runners/github-hosted-runners
手动触发须有默认分支工作流: https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow

当前没有新平台证据，保守维持 76/100（距离严格超过 95 需超过 19 个加权分；建议目标 96，需补 20 分）。不可将已配置 CI 当作 CI 已执行，不可将本轮本地测试当作 Windows 或 UOS 通过。
