# 寻枢 Rust 历史验证与当前版本差异

## 文件类别筛选与覆盖验证

文件名工作台支持全部、文件、文件夹、文档、图片、音频、视频、压缩包与其他。类别复用查询引擎的后缀规则，在全局计数、排序和分页前生效。其他包含未分类或无扩展名的文件，切回全部可恢复原搜索结果。已新增 215 条同类命中的跨页验证和 Windows 原生分类按钮验收。

Windows 合成覆盖验证按完整路径核对 1,223 个授权范围内文件，含隐藏文件、无扩展名、空文件、同名文件、表情字符和 360 字符长路径，全部命中且无重复。扩展名限定、全拼与简拼、实时新增与改名也通过。独立正文范围内的 UTF-8、GB18030、Markdown、HTML 和 DOCX 五份支持文档全部命中。范围外文件、目录链接指向的未授权文件和 ZIP 内部成员按预期不收录。权限、排除规则与正文支持格式仍决定真实目录的覆盖范围。

2026-11-25 分类候选版在 Windows 11 通过锁定离线构建、严格 Clippy、工作区回归及原生 UIA 分类切换。使用便携包复验了托盘、向导、正文预览、EFU、扫描 PDF OCR、语义检索和取消恢复。UOS amd64 兼容包通过 GLIBC 2.28 的 ELF 检查，并在 Debian 隔离环境验证安装、升级、卸载保留数据与 Xvfb 原生界面。该安装包在 UOS Desktop 20 1070 amd64 测试虚拟机上解包并找到应用菜单图标，但 CLI 和安装管理器均以 139 退出，目标系统原生安装与 DDE 验收仍待完成。

## 搜索工作台与品牌图标

第 1 版原型已用于原生文件名搜索。搜索框位于顶部，结果表展示名称、所在位置、类型、大小与本地修改时间。宽窗口在右侧显示选中文件的路径和操作，窄窗口在列表下方提供操作。正文搜索保留独立入口。安装或升级无需迁移索引和配置。

品牌素材以用户选定的深绿定位图形为准。窗口和 Linux 应用菜单共用透明 PNG，Windows 安装程序与桌面快捷方式使用 16 至 256 像素多规格 ICO，通知区域使用单独的 32 像素 PNG。Windows 原生测试验证通知区域素材能被系统解码，`verify-host.ps1` 可复验通知图标、最小化和恢复。

Windows 启动阶段安排短时重绘，以覆盖桌面合成器接管窗口的时间差。实机复查时应在任何鼠标或键盘交互前确认搜索工作台可见。

## Windows 桌面入口更新

Windows 测试安装包新增桌面快捷方式和完成页启动选项，快捷方式经固定安装管理器解析当前有效版本。启动持有只读共享安装锁，安装、升级、回退及卸载持有独占锁。Windows 原生窗口最小化时保留托盘和文件名监听，正在运行的正文任务与未提交的向导留在隐藏窗口，恢复时接续同一状态。空闲窗口可释放渲染进程。

`crates/xs-ui/verify-host.ps1` 核验 Explorer 通知图标、实际系统最小化消息、空闲渲染进程回收、托盘重新唤起及向导状态保留。平台放行仍需分别覆盖 Windows 10 和统信 UOS 的原生环境。

## Alpha8 内部测试候选

本轮基于 `08713dbc5d3703dbb079cf9b4445aaefd64f6a47`，产品版本为 `0.18.0-alpha.7`。测试包用于目标系统验收，发行状态为内部测试候选。

- **正文恢复**的确定性实验复现了 Linux 并发 fork 继承同一打开文件描述时的 `EAGAIN`。正文任务结束时显式解锁，并通过持有额外描述的回归验证。
- Windows Server 2022 的**候选分支检查**通过。Ubuntu 的 X11 窗口冒烟缺少 `libxkbcommon-x11`，已补平台依赖。本地 Debian 完成 Xvfb、中文字体和正文预览检查。 [候选运行日志](https://github.com/linsky-dev/Xunshu/actions/runs/45391368879)
- **BGE INT8** 的最低对照余弦为 0.994016，门槛为 0.98。Linux 在隔离网络命名空间中通过原生 ONNX 语义、中文 OCR、扫描 PDF、混合 PDF、在途取消和模型篡改拒绝测试。
- 专用 1 MiB Linux tmpfs 触发真实**磁盘满**。安装器保留原活动版本和合成用户数据，并清理失败载荷。
- **文件名性能**使用同一 300 万条合成快照，在 Debian WSL2 进行 12 次交替测量及 2,880 次独立真值核对。固定 4 核时，宽 OR 候选 P95 为 17.68–18.44 ms，组合查询为 10.03–10.63 ms。交付证据保存原始样本和采样范围。

最终产品源码在 Windows 11 x64 和 Debian 13 x86_64 上通过格式检查、锁定离线类型检查、严格 Clippy、工作区回归和 Release 构建。测试日志分别统计 255 和 274 个成功摘要，含跨进程子测试。Windows 本机构建使用 GNU x64，候选基线另有 Windows Server MSVC 通过证据。两端原生模型专项通过。Windows 向导完成中文试扫、确认后授权、发布和合成文件命中，Linux 完成窗口及正文预览。

Windows 11 测试安装包通过安装、窗口启动、同版本升级、回退、损坏修复和卸载保留数据。Linux 测试包以 GLIBC 2.28 为兼容目标，5 个程序及 ONNX Runtime 通过 ABI 检查，在 Debian 完成安装和模型验证。

统信 UOS Desktop 20 1070 amd64 虚拟机实测 GLIBC 2.28。该环境的新编译探针、Rust 工具链和候选安装管理器均在启动时出现段错误，原生安装、运行和桌面验收待完成。Windows 10、目标平台人工项目、有效签名和 72 小时稳定性仍需单独取证。六维复评暂为 88.25/100。见 [双平台实机验收](docs/PLATFORM_ACCEPTANCE.md)。

## 历史验证

> **2026-11-24 Alpha8 第15轮开发检查点（未放行）**：见 [第15轮开发记录](../docs/CLOUD_95_PROGRESS_20261124_15.md)。ODS有界展开、正文指纹及两处Windows测试模块顺序已修复。Linux全工作区270成功摘要（含跨进程重复计数）、28项Python合同、ODS原生Debug受限进程6/6通过；基线云端Linux Release及安装负向通过，ENOSPC环境故障未过，Windows首次两轮Clippy失败已修复待复测。云端3M严格50ms为0/3，独立真值全过。真实模型、目标平台、正常安装签名与72小时未放行。旧正文索引需重建；未制作正式安装包。

> **2026-11-24 Alpha8 第14轮开发检查点（未放行）**：见 [第14轮开发记录](../docs/CLOUD_95_PROGRESS_20261124_14.md)。XLSX稀疏抽取和共享公式内存故障修复，真实前后对照2/2及正文索引迁移5/5通过；264个Rust成功摘要（含重复计数）、28项Python合同、完整Linux Release、Xvfb、7类恢复、12项安装负向及8项监听合同通过。三轮3M正确性通过，严格50ms仅2/3通过；真实模型、目标系统、正常安装、签名、72小时稳定性未验收。旧正文索引需重建，文件名索引不变。以下为历史证据。

> **2026-11-24 Alpha8 第13轮开发检查点（未放行）**：见 [第13轮开发记录](../docs/CLOUD_95_PROGRESS_20261124_13.md)。PDF绘制资源发现和旧OCR索引版本绑定已修复；260个Rust成功摘要（含重复计数）、28项Python合同、完整Linux Release、Xvfb、7类恢复、12项安装负向及8项原生监听重启合同通过。三轮3M正确性均通过，严格50ms仅2/3通过；真实模型、目标系统、正常安装、签名、72小时稳定性尚未验收。以下为历史记录。

> **2026-11-24 Alpha8 第12轮开发检查点（未放行）**：最新证据见 [第12轮开发记录](../docs/CLOUD_95_PROGRESS_20261124_12.md)。完整 Linux Release、Xvfb 窗口、7类恢复及12项安装负向合同通过；254个Rust成功摘要（含跨进程重复），28项Python合同通过。三轮独立3M基准新增排名真值与原始样本校验均通过；历史尾延迟超标仍保留。五个原生程序直接ABI已核验，真实模型、目标系统、正常安装生命周期、签名、72小时稳定性均未放行。以下为历史证据。

> **2026-11-24 Alpha8 第10轮开发检查点（未放行）**：最新证据见 [第10轮开发记录](../docs/CLOUD_95_PROGRESS_20261124_10.md)。完整 Linux Release 工作区构建、Xvfb 原生窗口、7类恢复合同、12项安装负向合同均通过；253 个 Rust 成功摘要（含跨进程重复计数），14 项 Python 构建/载荷合同通过。独立三轮 300 万合成文件名严格延迟门槛均通过，交替 A/B 仍有超标，不代表真实磁盘稳定性。真实模型、目标平台原生验收、正常安装生命周期、数字签名及72小时稳定性仍未通过；禁止正式发行。以下均为历史记录。

> **2026-11-24 alpha.7 开发检查点（未放行）**：当前源码版本为 `0.18.0-alpha.7`。本轮 Linux 工作区 219 项独立测试通过、0 失败、0 忽略；Rustfmt、严格 Clippy、版本一致性、Debug 原生构建和 Xvfb 窗口冒烟通过。Release CLI 和正文抽取程序实际构建，并完成 300 万条合成文件名、2 万份合成正文及 1/100/1000 文件增量发布基准。Windows、国产系统、真实模型、长时稳定性及同机竞品对跑仍未验收。参见 [ALPHA7_DEVELOPMENT.md](ALPHA7_DEVELOPMENT.md)，以下为历史资料，不用于当前发布放行。

> **2026-11-23 新一轮修复注记**：当前源码版本已提高至 `0.18.0-alpha.3`，本页以下历史记录仅适用于原先的 `0.18.0-alpha.2` 或标注的特定提交。新一轮修复源码改动、验证缺口和实机指令参见 [REMEDIATION_2026-11-23.md](REMEDIATION_2026-11-23.md)。**本轮未在无 Rust/Cargo 的环境中完成重新编译或实机验收。** 不可把历史通过情况转移认定为修复版通过。

最后核对日期为 2026-11-10。源码从原始 Rust M1 检查点 `a27613c` 恢复。原始 M1 报告可用 `git show a27613c:rust/VALIDATION.md` 查看，历史测试和性能数据单独保留。

## M2 升级基线

2026-11-10 在现有 Windows 11 x64 主机（版本 26200）复测 `834cfc5` 的发布构建。语料为 300 万条确定性中英文合成元数据，每个查询 100 次，包含未命中、稀有词、中文和跨分支宽查询。快照由新进程打开并校验，操作系统页缓存未清除。验收阈值以用户确认的 M2–M6 计划为准。

| 测量 | 基线结果与证据 |
| --- | --- |
| 核心快照加载 | 209.60 ms，快照 86,022,857 字节。[原始数据](evidence/m2-baseline-core-windows.json) |
| 核心完整宽查询 | “干部”、`readme ext:rs`、`budget \| meeting` 的 P95 分别为 57.29、95.87、186.99 ms，均超过 50 ms。[原始数据](evidence/m2-baseline-core-windows.json) |
| 桌面烟测 | 原生窗口、单实例、系统消息激活、隐藏监听及退出回收通过。小样本隐藏宿主私有内存 3,031,040 字节。[原始数据](evidence/m2-baseline-desktop-windows.json) |
| 待验 | 物理双 Ctrl、真实中文输入法、键入到绘制、300 万条校准期间整棵隐藏进程树峰值、首装到首次命中、5 名用户及同机竞品评测。 |

```powershell
# 在 Rust 工作区运行，数据目录使用专用空目录
xunshu --data-dir <合成数据目录> benchmark --entries 3000000 --iterations 100 --output <本地证据文件>
pwsh -File crates/xs-ui/verify-host.ps1 -Ui target/release/xs-ui.exe -Cli target/release/xunshu.exe -SkipContent
```

基线 CLI SHA-256 为 `ae3a6e934ed1137c1ee0b16acace407b75b39e951a95f9919fd52c01fa84044ff`，桌面 SHA-256 为 `ea262868b901ac20ae10991b14c933f172eb76ffe4548c624d793775a27c8b42`。核心查询时间只含索引查询调用。桌面烟测使用原生消息分派，内存读数来自小样本宿主。

### M2 首个实现检查点

源码版本保留 `0.18.0-alpha.2`，本检查点对应 M2 的已实施项。Rust `1.99.1` 使用提交内锁文件。Windows 构建为原生 GNU x64，Debian WSL2 在 ext4 工作副本上以普通用户执行，并设置 `XS_REQUIRE_UNPRIVILEGED_TESTS=1`。

| 项目 | 本轮结果与证据 |
| --- | --- |
| Windows 工作区 | 154 项通过，严格 Clippy 与发布构建通过。[测试](evidence/m2-tests-windows.log) · [Clippy](evidence/m2-clippy-windows.log) · [构建](evidence/m2-release-windows.log) |
| Debian WSL2 工作区 | 167 项通过，严格 Clippy 与发布构建通过。[测试](evidence/m2-tests-debian.log) · [Clippy](evidence/m2-clippy-debian.log) · [构建](evidence/m2-release-debian.log) |
| 日期、设置与诊断 | 覆盖 UTC+8 跨日、夏令时、原子替换与损坏设置、撤权取消，以及诊断只读和脱敏导出。发布程序通过两端 CLI 合成流程。[Windows](evidence/m2-cli-windows.log) · [Debian](evidence/m2-cli-debian.log) |
| Windows 桌面 | 原生烟测通过，UI Automation 读到中文标题和结果，正文预览、CJK 字体、隐藏监听与退出回收通过。小样本隐藏私有内存为 3,186,688 字节。[原始数据](evidence/m2-desktop-windows.json) |
| 模糊测试接线 | Debian 上用 `cargo-fuzz 0.13.2` 和 `nightly-2026-11-09` 分别执行约 16 秒，查询 45,543 次、EFU/CSV 54,791 次、USN 1,894,181 次，均以 0 退出。8 小时及正式 24 小时门槛待验。[查询](evidence/m2-fuzz-query.log) · [EFU/CSV](evidence/m2-fuzz-efu_csv.log) · [USN](evidence/m2-fuzz-usn_decode.log) |
| 新构建的 300 万条核心测量 | 首屏核心调用 P95 最高约 0.44 ms，P99 最高约 0.48 ms。新进程校验加载约 251.58 ms。宽查询完整响应 P95 最高约 244.10 ms，超过 50 ms 门槛。本次与短时模糊测试有重叠，另做空闲复测。[原始数据](evidence/m2-release-core-windows.json) |

同一快照、两版二进制各 100 次的空闲复测中，最宽查询的 P95 从约 222.90 ms 增至 253.86 ms。已知宽查询超标仍存在，新增退化需要单独处理。[基线复测](evidence/m2-baseline-core-windows-repeat.json) · [新构建复测](evidence/m2-release-core-windows-repeat.json)

将默认相关度查询与列排序热循环分开编译后，Windows 单独构建 `xs-cli` 的同快照 P95 为 221.81 ms（`budget | meeting`）、129.65 ms（`readme ext:rs`）和 78.86 ms（“干部”）。最宽查询接近原版复测，中文宽查询仍比原版慢，三条均超 50 ms。此项仅测核心。[优化后数据](evidence/m2-specialized-core-windows.json) · [CLI 单包构建](evidence/m2-specialized-release-windows.log)

修正后 Windows 154 项、Debian WSL2 167 项工作区测试均通过，两端严格 Clippy 和发布构建通过。Windows CLI、UI Automation 和 Debian Xvfb 烟测也通过。Windows 的实际本地日期与 `utc:` 查询分别命中正确日历日期。Windows 最终工作区构建的 CLI 和桌面 SHA-256 收录在 CLI 日志。 [Windows 测试](evidence/m2-specialized-tests-windows.log) · [Windows Clippy](evidence/m2-specialized-clippy-windows.log) · [Windows 构建](evidence/m2-specialized-release-workspace-windows.log) · [Windows CLI](evidence/m2-specialized-cli-windows.log) · [Windows 桌面](evidence/m2-specialized-desktop-windows.json) · [Windows 日期](evidence/m2-date-windows.json) · [Debian 测试](evidence/m2-specialized-tests-debian.log) · [Debian Clippy](evidence/m2-specialized-clippy-debian.log) · [Debian 构建](evidence/m2-specialized-release-debian.log) · [Debian 桌面](evidence/m2-specialized-desktop-debian.log)

阶段出口仍待验。原生文件拖放、向导范围耗时估算、完整诊断面板、旧缺陷 L-01–L-09 全量映射尚需补齐。首装到首次命中、五人向导测试、NVDA 实读、实际中文输入法、物理双 Ctrl、系统缩放及端到端绘制时延需各自取证。M3–M6 仍待实施与验收，正式发布门禁单独保留。

工作区门禁使用 `cargo test --locked --offline --workspace`、`cargo clippy --locked --offline --workspace --all-targets -- -D warnings`、`cargo build --locked --offline --release --workspace`。两端的 `verify.ps1 -BinDir` 和 `verify.sh --bin-dir` 可对同一批构建单独复跑合成 CLI 契约。Windows 原生烟测增加 `-CheckAccessibility`。从 Linux 本地副本运行 `bash scripts/fuzz.sh query 15`，目标也可选 `efu_csv` 或 `usn_decode`，省略时长即运行 8 小时。

### CLI 能力开关

| `status` 字段 | 预期值 |
| --- | --- |
| `filename_search` | `true` |
| `stdio_ipc` | `true` |
| `stdio_ipc_v2` | `true` |
| `mmap_snapshot` | `true` |
| `native_ui` | `true` |
| `directory_watch` | `true` |
| `content_search` | `true` |
| `mcp_content` | `true` |
| `fast_service` | `false` |
| `ocr` | `true` |
| `semantic` | `true` |
| `legacy_query_full_parity` | `false` |

运行 `python scripts/check-validation.py --status <verify 生成的 status.json>` 核对证据链接及能力开关。`mft_usn` 随目标平台变化，Windows 的已验路径见下方原生 VHD 证据。

## alpha.2 恢复阶段验证证据

| 验证项目 | 结果与证据 |
| --- | --- |
| Windows 11 x64 工作区 | 148 项通过，含 M1 只读迁移、代次恢复、范围撤销、硬链接、正文 worker 和桌面逻辑。严格 Clippy 与发布构建通过。见 [测试日志](evidence/recovery-final-tests-windows.log)、[Clippy](evidence/recovery-final-clippy-windows.log)、[构建](evidence/recovery-release-windows.log) |
| Debian 13.5 WSL2 工作区 | 161 项通过，含普通用户执行、IPv4/IPv6 TCP/UDP 拒绝、Unix IPC 保留、子进程超时清理。严格 Clippy 与发布构建通过。见 [全流程日志](evidence/recovery-final-debian.log) |
| Windows CLI 端到端 | 扫描、监听、v2 常驻协议、EFU 审阅/另存/比较、空值、正文工作进程、MCP 授权与撤销。见 [端到端日志](evidence/recovery-cli-e2e-windows.log) |
| Debian CLI 和桌面 | 发布程序通过相同的合成 CLI 流程，Xvfb 原生窗口通过正文 IPC、预览和字体验收。见 [构建和 CLI](evidence/recovery-final-build-debian.log)、[安装和 Xvfb](evidence/recovery-install-debian-final.log) |
| Windows 11 原生文件系统 | 独立 160 MiB NTFS VHD 实测 MFT 枚举、双硬链接身份、改名旧/新路径、改写双路径、游标推进、日志不匹配回退和取消。真实用户卷未扫描。见 [VHD 日志](evidence/recovery-native-vhd-windows.log) |
| Windows 11 原生桌面 | 快捷框与主窗口切换、单实例、隐藏释放、隐藏时监听、退出回收、正文和字体。键盘手势逻辑由单测覆盖，原生烟测使用系统消息分派。见 [最终日志](evidence/recovery-desktop-windows.log) 和 [验证脚本](crates/xs-ui/verify-host.ps1) |
| 模型真实推理 | BGE 量化对照的最小余弦为 0.994016。原生 ONNX 自检识别“本地文件”和“XUNSHU OFFLINE”，并覆盖长文尾部。安装自检要求扫描 PDF OCR 命中。见 [模型日志](evidence/recovery-model-selftest-windows.log) |
| 隔离安装生命周期 | Windows 和 Debian 均通过真实模型自检、新装、同版本重新安装、清单摘要拒绝、回退、损坏修复和卸载保留数据。Windows 安装验证应用出站规则在 ActiveStore 生效，卸载后测试路径规则数为 0。见 [Windows](evidence/recovery-install-windows-final.log)、[Debian](evidence/recovery-install-debian-final.log) |

Windows 原始 M1 复跑曾因对已存在的卷根目录调用创建 API 而返回拒绝访问。恢复版验证已存在的目录句柄，只创建缺失分量。原始复现与最终通过记录可分别查看 [M1 日志](evidence/recovery-m1-windows.log) 和本页工作区测试。

依赖审计先发现 9 项公告，涉及 calamine、lopdf、zip、quick-xml 和 libsqlite3-sys。升级并锁定修复版本后，同一 RustSec 数据库复验为 0 项漏洞、3 项停止维护提示。见 [升级前](evidence/recovery-rustsec.json) 和 [复验](evidence/recovery-rustsec-final.json)。

## 性能测量

Debian WSL2，Intel i7-11800H，发布构建。合成语料为 300 万条中英混合文件名，独立读取进程，每条查询 30 次，包含 CRC 和结构校验，系统页缓存未强制清除。

| 测量 | 结果 |
| --- | --- |
| 快照大小和核心读取 | 86,022,857 字节，约 82.04 MiB。加载约 199.07 ms |
| 读取进程内存 | 查询后 RSS 89,928 KiB，查询峰值 103,696 KiB |
| 首屏核心调用 | 所测查询 P95 最高约 0.20 ms，P99 最高约 0.24 ms |
| 完整宽查询 | “干部”P95 约 51.48 ms，`readme ext:rs` 约 74.15 ms，`budget \| meeting` 约 177.49 ms |
| 构建 | 约 3.74 秒，构建进程峰值 209,392 KiB |

原始数据和环境见 [300 万基准](evidence/recovery-catalog-3m-debian.json) 和 [环境](evidence/recovery-catalog-3m-debian-environment.txt)。首屏核心调用与桌面键入到绘制首屏分别记账。Windows 发布构建小样本隐藏私有内存为 2,945,024 字节。包含 300 万条校准的隐藏进程树峰值仍待达标。

Debian 另以 10 万份合成 TXT 验证常驻正文 IPC，每种查询 30 次。稀有词、宽泛中文和宽泛英文 P95 分别约 1.48、60.79 和 56.73 ms，三组均达到 100 ms。首次查询含加载约 76.56 ms，系统页缓存未强制清除。前台重建约 108.61 秒，平均占单逻辑 CPU 的 92.65%。后台收录 15% 门槛尚无通过证据。见 [正文基准](evidence/recovery-content-100k-debian.log) 和 [复现程序](crates/xs-content/examples/benchmark.rs)。

## 当前保留的验收项

- Windows 提权服务生命周期、Explorer 菜单注册维护与外部文件对话框切换仍待接通。当前有显式提权的卷采集、目录范围启动入口和继承式本地 IPC。
- Windows 物理双 Ctrl、实际中文输入法、高 DPI、ReFS、Windows 10 和统信 UOS V20 1070 实机矩阵仍需补证。通用 Debian 结果不作为 UOS 兼容证明。
- Windows 已验证应用出站规则实际安装及移除。全链路 Windows IPv4、IPv6、DNS 出站探针和采集证据待补。
- OCR 已验证图片与嵌入 JPEG 等栅格图像的扫描 PDF。一般 PDF 页面光栅化及所有编码仍待覆盖，OCR 内容保留原识别文本。首轮样本曾把“搜”识别为“捜”。
- 正式签名、全目标系统安装、依赖许可覆盖和完整性能门槛继续阻挡正式发布。当前载荷仅用于本地隔离验收。

使用方式及重新验证命令见 [恢复版说明](docs/RECOVERY.md)。
