# Rust M1 构建、验证与平台边界

> 下文是 M1 的历史构建证据。恢复版构建与安装验收见 [恢复版说明](RECOVERY.md)。

版本：**0.18.0-alpha.1（Rust M1）**。这是首轮可运行的 Rust 重写 alpha。当前提供源码构建和本地验收产物；构建成功不代表安装包、签名发行版或全部旧功能已迁移。整体结论和最终产物摘要以 [VALIDATION.md](../VALIDATION.md) 为唯一入口；功能差距见 [FEATURE_PARITY.md](FEATURE_PARITY.md)。

## 产物与依赖

| 项目 | 当前实现 |
| --- | --- |
| 命令行 | `xunshu` / `xunshu.exe`，crate 为 `xs-cli` |
| 原生桌面 | `xs-ui` / `xs-ui.exe`，eframe / egui，crate 为 `xs-ui` |
| 运行时语言 | Rust；不启动 Python/PyQt、Node.js 或 WebView 作为应用运行时 |
| 系统依赖 | 操作系统、原生 C/CRT 运行库、图形驱动、桌面服务与字体仍需满足；外部打开使用系统关联程序 |
| 开发工具 | 已安装的 Rust/Cargo、本机 C 编译器与链接器；Linux ELF 检查使用 GNU readelf 等工具 |
| 开发验证 | `verify.sh` 使用 Python 3 标准库检查 JSON；它不属于运行时，也不会进入二进制产物目录。PowerShell 版本直接解析 JSON |
| 构建缓存 | 必须事先准备 Cargo.lock 对应依赖；脚本使用 `--locked --offline`，缺少工具、缓存或 target 时明确失败，不自动安装或下载 |

本轮实际工具链为 rustc 1.99.1。workspace 声明的 1.88 不是本轮已验证的最低工具链保证；完整依赖版本以 [Cargo.lock](../Cargo.lock) 为准。MinGW-w64、Rust 标准库 target、C 编译器等属于开发构建环境，不随应用作为运行时工具链分发。

两个 build 脚本都从自身位置定位 `rust/Cargo.toml`，不依赖调用者当前目录。输出根默认为用户目录中的 `Xunshu-builds`，每次创建唯一的新 package 子目录，拒绝复用已有包；目录内只有两个二进制和 `README.txt`。不会复制开发工具链、索引、合成样本、模型或恢复资料，不删除旧包或用户文件。最终交付归档及锁定依赖许可材料由发行打包步骤另行汇总。

## Linux x64 本机构建

在已准备依赖的本地源码树运行：

```sh
bash rust/scripts/build-linux.sh
bash rust/scripts/build-linux.sh --output-root "$HOME/Xunshu-builds"
```

核心构建命令为：

```sh
cargo build --locked --release -p xs-cli -p xs-ui --offline \
  --target x86_64-unknown-linux-gnu --target-dir /absolute/source/rust/target
```

显式 target 使复制来源确定，即 `target/x86_64-unknown-linux-gnu/release/`。首次使用该目录时，Cargo 可能重新编译此前只在 `target/release/` 中构建过的依赖。低内存并行构建环境可在调用脚本前设置 `CARGO_BUILD_JOBS=3`。

脚本对**复制后的实际 ELF 文件**检查 x64 类型、SHA-256、解释器、DT_NEEDED 和 GLIBC 版本需求，并写入包内 README。也可以设置自己核实的强符号版本上限：

```sh
bash rust/scripts/build-linux.sh --max-glibc 2.35
```

超过上限时退出非零；保留并标明未完成的本次输出目录，不把它报告为成功包。该参数不是 UOS 版本映射，也不自动改变工具链或链接结果。

### GLIBC 不能只取最高字符串

本轮 Debian GNU/Linux 13、宿主 GLIBC 2.41 环境中的实际 ELF 检查结果：

| 产物 | 最高非弱 GLIBC 版本需求 | 最高 GLIBC 标记（含弱引用） | 对应解释 |
| --- | --- | --- | --- |
| CLI `xunshu` | **GLIBC_2.34** | GLIBC_2.39 | 2.39 的 pidfd 相关符号是 WEAK，不能直接当作最低版本 |
| 桌面 `xs-ui` | **GLIBC_2.35** | GLIBC_2.39 | 强引用包含 `hypotf@GLIBC_2.35`；2.39 仍是弱引用 |

脚本读取 readelf 版本需求段，分别列出非弱需求与弱版本标记。最高非弱需求只是必要的符号条件，不能独立证明完整兼容性：传递依赖、运行时动态加载的 X11/Wayland、EGL/OpenGL、键盘库、字体、驱动和内核环境还需要目标机检查。

**这些 Debian 产物没有被证明可运行于统信 UOS V20 1070 AMD64。** UOS 应在目标系统或等价基线 sysroot 中重新构建，重新测量产物，并完成原生窗口、中文输入和显示、搜索、范围设置、文件打开、权限拒绝、取消及重启恢复的实机验收。

## Windows x64 本机构建

使用已安装 Rust 和相应 C/Windows 链接工具的普通用户 PowerShell。脚本的语法目标为 Windows PowerShell 5.1 及 PowerShell 7，不修改执行策略、不提权、不注册服务。若本机策略阻止脚本，应按本机既定策略处理；脚本不会自行绕过。

```powershell
& D:\source\Xunshu\rust\scripts\build-windows.ps1
& D:\source\Xunshu\rust\scripts\build-windows.ps1 -Target x86_64-pc-windows-msvc -OutputRoot C:\Users\you\Xunshu-builds
```

默认使用当前 Rust host；支持 `x86_64-pc-windows-msvc` 和 `x86_64-pc-windows-gnu`。MSVC 路线需要相应 Windows SDK/链接环境，GNU 路线需要已准备的 MinGW-w64。缺少工具时不会切换路线或自动下载。

实际构建命令为：

```text
cargo build --locked --release -p xs-cli -p xs-ui --offline --target <selected-target> --target-dir <source>\rust\target
```

构建、复制失败，或非 x64 PE、缺失产物，都会以非零退出码报告。成功包包含 `xunshu.exe`、`xs-ui.exe`、`README.txt`，README 记录 target、工具链、锁文件与二进制摘要。检查 PE 头不等于执行程序，不证明 Win10/Win11 图形或文件系统行为通过。

### GNU 交叉构建与实机运行分开记录

本轮 Linux 上已完成 `0.18.0-alpha.1` 的 Windows GNU **完整链接构建**，生成两个 EXE；不只停留在 cargo check。最终 CLI 为 CUI 子系统 3，UI 为 GUI 子系统 2。可追溯 [Windows 产物记录](../evidence/windows-binaries.json)、[构建环境](../evidence/windows-build-environment.json) 与 [构建说明](../evidence/windows-build-notes.md)。最终载荷摘要和验收结论仍以 VALIDATION 为准。

该交叉构建使用开发侧准备的官方发行 MinGW-w64 工具，不将工具链复制进产品。它验证代码和链接链路，没有运行 Windows 程序。Windows PowerShell 构建/验证脚本的实机执行，以及 Win10/Win11 的原生桌面、文件锁、reparse point 拒绝、中文字体、外部打开和安装体验都需要独立验收。

最终交叉产物使用了记录在构建环境中的静态 CRT/GCC 辅助库选项，直接导入表未出现 GNU 运行库 DLL。源码构建脚本继承调用者已准备的链接配置，不能仅凭“两个 EXE 已复制”推断另一套本机工具链也生成相同的 DLL 依赖；应复核实际产物的直接导入表。

## 可重复验证脚本

Linux 默认在工作区创建合成样本：

```sh
bash rust/scripts/verify.sh
```

若源码工作区为 overlay，脚本明确拒绝将其用作产品数据测试目录，不降低本地边界。当前云任务获准的 tmpfs 位置必须显式指定：

```sh
CARGO_BUILD_JOBS=3 bash rust/scripts/verify.sh \
  --temp-root /tmp/xunshu-rust-validation-036c199d3807
```

Windows 默认使用 `rust\test-tmp`，也可显式指定本地合成样本父目录：

```powershell
& D:\source\Xunshu\rust\scripts\verify.ps1
& D:\source\Xunshu\rust\scripts\verify.ps1 -TempRoot D:\source\Xunshu\rust\test-tmp
```

两个脚本执行 Rust workspace 测试、构建 CLI/UI，并使用本次新建的少量文件验证：保存范围、扫描、扩展名过滤、Unicode 名字、智能排除、数据目录硬排除、EFU 导入/导出、离线来源与未知查询的明确错误。所有 CLI 操作都传入专用 `--data-dir`。仅执行 UI 的 `--help`，不计作窗口验收。

不运行旧 Python 全量测试，不启动全盘扫描，不跑大规模文件系统基准。Linux 必须使用普通用户，因为权限拒绝回归不能在 root 下被绕过。清理需要匹配本次随机目录与所有权标记；Linux 另核对设备/inode 并限制跨文件系统删除；Windows 发现遗留 reparse point 时保留目录供检查。用户提供的父目录本身绝不会作为递归清理对象。

日志和 CLI JSON 保留在 `rust/evidence/verify-linux.*` 或 `verify-windows-*` 的本次证据目录中。可通过 `--evidence-root` / `-EvidenceRoot` 指定证据父目录，仍只创建新子目录。失败退出非零；当前默认数据目录和旧设置不会被测试覆盖。

本轮 [脚本验收记录](../evidence/delivery-script-validation.json) 包含 Linux 验证脚本的 125 项 Rust 测试和合成 CLI 流程、构建脚本实际生成的三文件目录及摘要、过低 GLIBC 上限的拒绝结果，以及带末尾斜杠的链接目录保护。脚本验收产物与最终经过桌面验收的运行包分别记录；Windows PS1 尚未实际执行。

## 运行与迁移

```sh
./xunshu --help
./xs-ui --data-dir /absolute/local/new-xunshu-data
```

Windows 对应 `.\xunshu.exe` 和 `.\xs-ui.exe`，数据路径使用本地驱动器绝对路径。默认数据目录为 Windows `LOCALAPPDATA\XunshuRust`；Linux 为 `XDG_DATA_HOME/xunshu-rust` 或 `HOME/.local/share/xunshu-rust`。首次验收建议显式选择独立新目录。

启动只加载保存的设置/索引，不自动全盘扫描。旧 settings 通过 `migrate-settings` 创建新文件；旧 schema4 通过 `import-legacy` 只读导入为不可打开的离线元数据。不要将旧用户库目录当成新 Rust 数据目录，也不要修改旧 SQLite 来尝试回退。正文、OCR、语义、MFT/USN 和系统服务尚未完成。

## 状态判定

| 验证种类 | 能证明什么 | 不能据此声称什么 |
| --- | --- | --- |
| Linux 合成回归和 CLI 运行 | 当前 Linux 环境的对应行为与边界 | Win10、Win11 或 UOS 已实测 |
| Linux 原生桌面交互 | 本轮云桌面所执行并记录的窗口、向导、输入与搜索流程 | 未执行的交互、Win10/11/UOS 或发行安装链路通过 |
| Windows GNU cargo check | 该 target 的代码类型检查 | EXE 链接或 Windows 运行成功 |
| Windows GNU 完整 build | 两个 EXE 实际链接产出 | 原生 PowerShell 脚本或目标系统已执行 |
| 构建脚本三文件目录 | 可复核的本地源码构建输出 | 正式安装器、签名、更新器或完整旧功能等价 |

## 公开工具依据

- [Cargo build：target、locked 与 offline](https://doc.rust-lang.org/cargo/commands/cargo-build.html)
- [GNU Binutils readelf：ELF 动态依赖与版本段](https://sourceware.org/binutils/docs/binutils/readelf.html)
- [PowerShell：本地程序退出码 LASTEXITCODE](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_automatic_variables#lastexitcode)
