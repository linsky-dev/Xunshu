> **当前工程版本：`0.18.0-alpha.6`（2026-11-24）**。本轮增加跨进程候选索引检查点、文件级事务、段落外键索引、可清除检查点和有界正文排名缓存；Linux 离线工作区 211 项独立测试通过，Clippy/Rustfmt/五个 Debug 原生二进制及 Xvfb 冒烟通过。详见 [本轮报告](QA_ALPHA6_20261124.md)。Windows、国产系统原生安装、真实模型、数字签名、长期运行与完全磁盘增量索引尚未完成；不是正式发行版。

# 寻枢 Xunshu · Rust M1

> 本页保留 M1 历史说明。恢复版 `0.18.0-alpha.2` 的当前入口为 [恢复版说明](docs/RECOVERY.md) 和 [验证记录](VALIDATION.md)。

**0.18.0-alpha.1：首轮可运行的 Rust 重写版本。** 文件索引、查询、命令行和桌面界面均由 Rust 实现。程序运行不启动 Python、Node.js、PyQt 或浏览器页面。原 0.17.0 保持为迁移对照，新程序使用独立设置与索引目录。

这是依据《寻枢（Xunshu）后期改造计划》推进的新实现。它已经可以完成指定目录收录、文件名检索、拼音检索、离线目录迁移和原生桌面操作；**尚未完成全部旧版功能与计划指标**。本轮验证结果以 [VALIDATION.md](VALIDATION.md) 为准，后续工作按 [MIGRATION_STATUS.md](MIGRATION_STATUS.md) 继续。

## 开始使用

Windows 可运行包内双击 `xs-ui.exe`，Linux 包运行 `./xs-ui`。首次运行依次完成：

1. 进入设置，添加希望检索的本地目录。
2. 选择全部文件、场景类型或自定义后缀，确认范围。
3. 保存并扫描，完成后输入文件名或查询表达式。

可以选择“稍后设置”；以后从“搜索范围与设置”重新进入。启动本身不扫描磁盘。取消或不完整扫描保留上一代索引；修改收录范围后，旧范围的查询会停用，直至新扫描完成。

两个程序默认共享数据目录：Windows 为 `%LOCALAPPDATA%\XunshuRust`，Linux 为 `$XDG_DATA_HOME/xunshu-rust` 或 `$HOME/.local/share/xunshu-rust`。可通过 `--data-dir` 指定另一个绝对本地路径。新目录应与旧版 schema4 存储分开。

当前使用标准目录扫描。界面没有全局双击 Ctrl、托盘或开机启动入口；MFT/USN 极速扫描、正文、OCR 与语义检索会明确显示未实现。

## 查询示例

| 输入 | 作用 |
|---|---|
| `报告` | 文件名包含“报告” |
| `ext:docx 报告` | Word 扩展名并包含“报告” |
| `pinyin:xssj` | 中文拼音简拼，例如“寻枢设计” |
| `pinyin:xunshu` | 中文全拼；支持全拼与简拼混合 |
| `path:项目 size:>1mb` | 路径条件与文件大小条件 |
| `budget \| meeting` | 两个名字条件的并集 |
| `regex:"^report_[0-9]+"` | Rust regex 语法的正则名字匹配 |
| `fuzzy:bdgt` | 按字符顺序的模糊子序列匹配 |

完整语法、预算和兼容性说明见 [查询契约](crates/xs-query/README.md)。无效字段或语法会报错。Unicode 使用 NFKC 与小写归一化。日历日期默认按系统时区解释，`dm:utc:today` 使用 UTC。拼音目前需要显式 `pinyin:`。

## 命令行与迁移

运行 `xunshu --help` 查看全部参数。以下是 PowerShell 示例，其中待扫描目录由你显式指定：

```powershell
.\xunshu.exe --data-dir "$env:LOCALAPPDATA\XunshuRust-Demo" init --root "C:\Users\你的用户名\Documents\待检索目录"
.\xunshu.exe --data-dir "$env:LOCALAPPDATA\XunshuRust-Demo" scan
.\xunshu.exe --data-dir "$env:LOCALAPPDATA\XunshuRust-Demo" search "ext:pdf"
```

`init` 只保存范围，不执行扫描。已有配置需要显式 `--replace`。Linux 命令相同，换成 `./xunshu` 和绝对 POSIX 路径。

```text
xunshu import-efu /绝对路径/catalog.efu --name 我的目录
xunshu import-legacy /绝对路径/旧版数据库.db
xunshu export-efu /绝对路径/新导出文件.efu
xunshu migrate-settings /绝对路径/旧版settings.json
```

- EFU 和 schema4 导入只读原文件，记录中的路径仅作元数据，不打开这些路径。离线记录无法直接打开；真实本地文件应另行加入范围并扫描。
- 同名 EFU 再导入会替换该来源，空清单也会清除该来源旧条目。schema4 再导入替换此前的整个 schema4 来源集合，并尊重禁用目录。
- 原始 EFU 时间精度、创建时间、属性、自定义列及未知空值保存在稀疏元数据区，导出时保留字段值。列顺序、CSV 引号样式和字节布局不保证原样复现。
- EFU 日期输入当前要求无符号 64 位整数 FILETIME。旧 schema4 若保存了日历格式原文，会原样导出；该格式暂不能再次通过本版 EFU 导入器读取，不能把所有日期格式视为已兼容。
- 查询投影的大小/时间仍为整数；未知大小、未知时间的筛选语义未实现与旧版 NULL 的全量等价。导出保真不能替代查询兼容性验收。
- 旧 SQLite 必须是 schema4、已正常关闭且无非空 WAL；程序不替用户 checkpoint 或改写旧库。导入不能恢复未合入主库的 WAL 数据。
- 导出采用新文件名，拒绝覆盖已有文件。新设置迁移也拒绝覆盖源文件或已有目标。

`serve` 提供继承 stdin/stdout 的 JSON-lines 服务，支持 status、search、reload、shutdown。每条请求最多 64 KiB，不开 TCP/HTTP 端口。桌面界面目前通过后台线程调用同一套 Rust 核心；界面与常驻核心的进程隔离尚未完成。

## 工作区结构

| Crate | 职责 |
|---|---|
| `xs-core` | 紧凑索引、倒排候选、mmap 快照、事务发布、迁移和共享操作 |
| `xs-query` | 唯一查询语义执行器、Unicode、拼音、正则与评分 |
| `xs-platform` | 严格设置、本地路径边界、卷能力和可取消扫描 |
| `xs-cli` | 命令行、JSON-lines 服务入口、合成基准 |
| `xs-ui` | egui/eframe 原生桌面、设置向导、范围管理与结果展示 |

快照采用不可变代次。写入完成后才切换 CURRENT；已有读取者可继续使用旧映射。CRC 用于损坏检测，不是身份认证。不要在程序运行时修改或截断 `.xsi` 文件。当前保留历史代次，自动垃圾回收尚未实现；清理必须在所有程序退出并确认保留当前代次后手动进行。

## 构建与验证

本轮使用 Rust 1.99.1。`Cargo.lock` 固定依赖版本；声明的最低 Rust 版本尚未完成单独验证矩阵。首次开发构建需要工具链和依赖缓存，运行时不下载依赖或模型。

```sh
cargo fetch --locked
cargo build --locked --release -p xs-cli -p xs-ui
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
```

测试只使用合成数据。测试目录必须位于支持的本地文件系统；overlay、未知/FUSE、网络挂载被拒绝时，应选择已经确认的本地临时目录，不能降低边界规则。参见 [构建说明](docs/PLATFORM_BUILD.md) 和 `scripts/verify.*`。

Windows GNU 交叉编译产物不等于 Windows 实机验收。随附 Linux 二进制在 Debian 13 环境构建、验证，不能直接当作 UOS V20 1070 兼容包。目标平台仍需本机构建、桌面验证、安装与升级回退测试。

## 来源与许可

新 Rust 源码采用 MIT，见 [LICENSE](LICENSE)。依赖与字体的声明、版本和通知文件见 [THIRD_PARTY.md](THIRD_PARTY.md)、`licenses/inventory.json`。本 Rust 运行包不包含旧版 PyQt、恢复伪代码、专有 Everything 资源或逆向生成的查询表。
