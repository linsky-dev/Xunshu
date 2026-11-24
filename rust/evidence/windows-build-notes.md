# Windows 交叉构建记录

日期：2026-11-08。宿主为 Debian 13 x86_64，目标为 `x86_64-pc-windows-gnu`。本记录描述交叉编译和 PE 检查，不表示 Windows 10/11 实机运行、安装、IME、热键、文件操作或性能验收通过。

## 工具链修复与来源

- 原准备脚本此前因一次 HTTP 503 中止；主控已下载的5个完整包全部复用，只有缺失的 gcc-mingw-w64-base 重新下载。没有修改系统软件源、网络规则或权限。
- 使用 Debian 官方 HTTPS 分发站。InRelease 的三个归档签名通过系统 Debian 公钥验证；Packages.xz 对照 InRelease 中 SHA256 验证；全部6个包对照 Packages 中大小及 SHA256 验证。
- 解包进入用户态目录，未系统安装。宿主 cc1 的共享库依赖已解析，目标 GCC 与 binutils 可实际执行。
- 修订下载脚本只重试相同官方URL的临时服务器错误，完整包按校验复用，下载先写 .part 后校验改名；明确拒绝不改域名绕过。

| 包 | 版本 | 字节数 | SHA256 |
|---|---|---:|---|
| binutils-mingw-w64-x86-64 | 2.44-3+12+b1 | 3473344 | `b53f4a091b16a0d0952b8e3bd6027931641386258f96b37db1f0c62c787221d3` |
| gcc-mingw-w64-base | 14.2.0-19+27+b1 | 194268 | `8d2c64b886ab4a435a78ddc4cd3b510f149b65fbd6be8472e8f460627562718a` |
| gcc-mingw-w64-x86-64-posix | 14.2.0-19+27+b1 | 37567656 | `e32d24cfd1482394f16d9a84c1d0a9f5aa08ee826a79acc383c70cc8c23c4fe5` |
| gcc-mingw-w64-x86-64-posix-runtime | 14.2.0-19+27+b1 | 13037152 | `c898d96177574a8a7238564a1e799d8ddfed5a3d2ea005510305f9ad466a6f59` |
| mingw-w64-common | 12.0.0-5 | 5792084 | `97dce5d0d8aff1cade4786ad4ef5078345477a598829afcac2e7f2fbc5399990` |
| mingw-w64-x86-64-dev | 12.0.0-5 | 4871860 | `0bf89cf7454cccb49cd2a46a6f7b33896f6ae3a1a2fe33e02c40be6155bbb385` |

具体官方包 URL 保存在 `windows-toolchain-downloads.json`；签名日志及索引摘要分别在 `windows-debian-signature.log`、`windows-toolchain-metadata.json`。

## 构建配置

```sh
CARGO_TARGET_DIR=<project>/rust/target-windows
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=<mingw>/usr/bin/x86_64-w64-mingw32-gcc-posix
CC_x86_64_pc_windows_gnu=<mingw>/usr/bin/x86_64-w64-mingw32-gcc-posix
AR_x86_64_pc_windows_gnu=<mingw>/usr/bin/x86_64-w64-mingw32-ar
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS="-Ctarget-feature=+crt-static -Clink-arg=-static-libgcc"
cargo build --release --target x86_64-pc-windows-gnu -p xs-cli -p xs-ui --locked
```

这些机器路径仅通过本次构建环境传入，没有写入项目永久配置。单独 target-windows 避免与 Linux 构建争用目录锁。实际路径、限定的构建环境变量、rustc/cargo/GCC/objdump版本、Cargo.lock摘要记录在 `windows-build-environment.json`；未导出完整进程环境。

## 首次完整构建

首次完整构建 exit_code=0，耗时89.921秒，前后Rust源/清单/资产摘要相同。CLI和UI均成功生成 PE32+ x86-64 EXE。首次UI仍是 Windows CUI 子系统，已向主控报告并请求以源码属性修复；不通过修改已生成二进制伪造GUI构建。首次导入表和摘要保留在 windows-binaries-attempt-1.json、windows-xs-ui-attempt-1.pe.txt。

完整构建尝试逐次追加在 windows-build.log 和 windows-build-attempts.jsonl，先前Windows语法检查记录 windows-core-check.log 也保留。

## 运行时与许可证

最终两个EXE都没有导入 libwinpthread-1.dll、libgcc_s_seh-1.dll 或 libstdc++ DLL；无需为本次产物附带这些GNU DLL。仍依赖Windows系统DLL和UI的OpenGL/桌面系统支持，完整直接导入清单见 `windows-binaries.json` 及两个 `.pe.txt` 文件。此检查覆盖 PE 直接导入表，不把宿主系统动态加载的 DLL 或设备驱动作为随包组件。

构建时用到的MinGW运行库与GCC静态辅助代码，应在交付第三方通知中保留工具链相应的copyright/运行库例外文本。来源位于工具链 usr/share/doc/mingw-w64-common/copyright 及 usr/share/doc/gcc-mingw-w64-base/copyright。

## 最终 Rust M1 构建

版本 `0.18.0-alpha.1`，最终构建于 2026-11-08 06:03:22 UTC 开始，27.138 秒完成，退出码 0。完整执行 CLI 与 UI 的 release 构建；Cargo 复用不变的依赖缓存。前后源码、清单、测试及资产的组合 SHA256 相同：

`6e6fe6d002e78524de3f895e33eecc7ce1397e5d1b8ff8e749596885d7606135`

最终产物位于 `target-windows/x86_64-pc-windows-gnu/release/`：

| 文件 | 字节数 | PE 子系统 | SHA256 |
|---|---:|---|---|
| xunshu.exe | 5582336 | 3 / Windows CUI | `607dc06abe3b31b47185371c2470fe5b4efa4b646132512a55779f8e239c3c73` |
| xs-ui.exe | 6992896 | 2 / Windows GUI | `fae3a3193e63cc9ed4441364d9f5343e84e5b2646f15a1f6d0120d3461c1314e` |

UI 子系统由源码 `cfg_attr(target_os = "windows", windows_subsystem = "windows")` 指定，经最终链接产物复核，没有后处理 EXE。两者均为 PE32+ x86-64。所有构建尝试和第一次产物证据保留，可区分初次 CUI UI 与最终 GUI UI。

本轮没有在 Windows 或 Wine 中执行这些 EXE。构建与导入表检查能够证明目标代码可编译、链接和识别产物结构，不能证明 Windows 10/11 实际启动、窗口、字体、中文输入、驱动、文件操作权限、部署兼容性或性能。后续仍需按 Windows 实机验收清单执行。
