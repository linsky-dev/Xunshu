> 本轮开源项目对标及未来源码引入许可证矩阵：见 [OPEN_SOURCE_INTEGRATION_20261123.md](docs/OPEN_SOURCE_INTEGRATION_20261123.md)。本轮新增统一搜索与连接器缓存等代码为本项目自主实现，没有直接复制新的外部仓库源文件；不要将“参考架构”误写为“引入了开源代码”。

# 第三方来源与通知

恢复版 `0.18.0-alpha.2` 以当前 `Cargo.lock` 和 [验证记录](VALIDATION.md) 为准。正文处理依赖 calamine、lopdf、zip、quick-xml 和内置 SQLite。模型资源由 [构建脚本](scripts/prepare-models.py) 固定版本及摘要，安装载荷附上 BGE、PP-OCRv5、ONNX Runtime 和 Noto Sans SC 的许可通知。

Windows 当前 Cargo 解析范围的通知保存在 [恢复版库存](licenses/recovery-windows/inventory.json)。已缓存的包中，有 17 个未随 crate 提供单独通知文件，名单见 [采集摘要](evidence/recovery-license-inventory-windows.json)。M1 曾从上游补齐的通知保留在历史目录。完整发布包的通知覆盖仍需独立复核。

| 恢复版资源 | 来源与许可 |
| --- | --- |
| 中文 BGE 模型 | [BAAI 官方 BGE-small-zh-v1.5](https://huggingface.co/BAAI/bge-small-zh-v1.5)，MIT。对锁定的官方权重完成 ONNX 转换及数值对照 |
| PP-OCRv5 ONNX 检测和识别 | [检测发行](https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_det_onnx) 与 [识别发行](https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_rec_onnx)，Apache-2.0 |
| ONNX Runtime | MIT 及随附第三方通知。官方 GitHub 资产缺失时，使用固定 SHA-256 从同版本的 Microsoft PyPI wheel 提取原生库。 [GitHub 发行](https://github.com/microsoft/onnxruntime/releases/tag/v1.24.4) · [PyPI 1.24.4](https://pypi.org/project/onnxruntime/1.24.4/) |
| 中文界面字体 | [Noto Sans SC](https://github.com/notofonts/noto-cjk/tree/Sans2.004)，SIL OFL 1.1 |

## M1 历史通知

本文件对应 **0.18.0-alpha.1（Rust M1）**。新写的 Rust 工作区采用根目录 MIT 许可证。第三方代码、数据与字体保持各自许可证，不能由本项目的 MIT 声明替代。

## 依赖锁定与可追溯资料

- `Cargo.lock` 固定 Cargo registry 依赖版本和校验摘要。
- `licenses/inventory.json` 列出锁文件中 343 个 registry 包的名称、版本、声明的许可证表达式、源码仓库、registry checksum、通知文件及对应 SHA256。它包括本次未构建的平台依赖和开发依赖，因此不是某个可执行文件的链接清单。
- `evidence/dependencies-linux.txt` 与 `dependencies-windows.txt` 是两个实际构建 target 的 normal/build 依赖树；`license-native-coverage.json` 记录相应通知覆盖检查。Linux 190 个、Windows 159 个 registry 依赖均关联了已保存的通知文件。
- 从已下载 crate 中逐字复制的通知位于 `licenses/<crate>-<version>/`。crate 发布包缺失的通知，按其 `.cargo_vcs_info.json` 所记录的源码 commit 从声明的官方上游补齐，位于 `licenses/upstream/`；记录保留精确 URL、commit/tag 与摘要。

库存记录说明材料来源与覆盖，不等于法律审查结论；以后增加依赖、切换 target、引入模型或分发新平台时，必须重新检查对应构建的通知与义务。未构建的 Android、Apple、UEFI 等包虽在 lockfile 清单中，其全部上游通知未作为这些平台的发行验收完成。

## 主要组件

| 组件 | 使用目的 | 声明来源 |
|---|---|---|
| egui / eframe | 原生 Rust 桌面界面与绘制 | 锁文件、上游通知与 `licenses/inventory.json` |
| regex、unicode-normalization、pinyin | 查询、Unicode 与拼音数据 | 对应 crate 的原始通知 |
| memmap2、crc32fast、fs2 | 映射读取、损坏校验、文件锁 | 对应 crate 的原始通知 |
| rusqlite / libsqlite3-sys | 旧 schema4 的只读迁移 | SQLite 随 crate 提供的说明与对应通知 |
| csv、serde、serde_json、clap | EFU、设置、协议与 CLI 参数 | 对应 crate 的原始通知 |
| windows-sys、libc、winit、glutin | 系统调用与原生窗口 | 对应 crate 与上游通知 |

本实现没有复制 plocate 源码；只采用公开的 n-gram 倒排思路。未纳入原 0.17 的专有资源、恢复伪代码、逆向生成关键字表或 Python/PyQt 发行载荷。

## 字体

eframe 启用的默认字体来自 `epaint_default_fonts`。随包保留其 OFL、Ubuntu Font Licence、Hack 和 emoji 字体通知，以及 egui 项目许可证。中文显示优先从用户系统已安装字体读取；测试使用本机中文字体，没有把 Windows 或 Linux 系统字体文件复制进可运行包。

## Rust 标准库与 Windows 静态运行库

`licenses/rust-runtime/` 保留本轮官方 Rust 1.99.1 发行提供的 `COPYRIGHT-library.html`；其中的标准库相关通知不依赖 Cargo registry 清单。

Windows GNU 构建静态链接了相关 GNU/MinGW 运行库。`licenses/windows-runtime/` 保存来源 Debian 包的原始 copyright、GCC Runtime Library Exception 3.1、其引用的公共许可证，以及原包版本和摘要。该目录是通知材料，不包含 GCC、MinGW 开发工具或安装程序。具体来源、版本、历史文案和例外条件见该目录 README，许可证正文按原文保留。

Linux 产物的动态系统库由目标系统提供；其直接依赖和强/弱 GLIBC 需求记录在 `evidence/linux-binaries.json`。本包没有把整套操作系统库、图形驱动或系统字体重新分发。
