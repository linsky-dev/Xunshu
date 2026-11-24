# 恢复依据与环境

- `SOURCE_FILES.json` 中的 746 个 M1 文件均已通过 SHA-256 校验。
- 恢复范围以桌面 `历史记录.md` 第 115 行之后的工程记录为依据。共享链接读取超时。
- Windows 11 x64 的工作盘 D: 为 NTFS，另有 Debian 13.5 WSL。当前 PowerShell 未发现 Cargo 或 MSVC C++ 工具链。
- PATH 中 E 盘 Git 启动失败，Codex 自带的 Git 2.56.0 可用。工作目录原为无 Git 元数据的源码归档。
- 历史首屏查询核心耗时不能代替键入到首屏。隐藏内存需测量包含索引校准在内的整棵进程树。
- Rust 1.99.1 的 Windows GNU 自带链接库可通过 `-C link-self-contained=yes` 使用。w64devkit 的 GCC 用于本地 C 依赖，裸链接缺少 `libgcc_eh`。工具链保留在工作区准备目录，Debian 工具链与测试位于本任务的 `/var/tmp` 本地目录。
- M1 Windows 基线复现了对已存在的 `D:\` 调用 `CreateDirectoryW` 失败。恢复版本只创建缺失路径分量，并保留目录句柄校验。
- 官方权重固定 BGE `7999e1d3359715c523056ef9478215996d62a620`、PP-OCR 检测 `0d63e78e2b680928f6b1747d76a08db6e645efb7`、识别 `682f20538d8c086cb2128e5cfac775e6c4904e85`。ONNX Runtime 1.24.4 的 Windows/Linux 发行压缩包按官方发行资产 SHA-256 校验。
