# 2026-11-24 续建 07：回滚自检隔离、卸载续清理、完整源码恢复

代码提交：3c350ab8bb07488a0aa4838c9296d7b7ed0b9b91，父提交 99d73ac6ecab6e7d4676de7559ef54c798ab1080。产品版本 0.18.0-alpha.7，开发分支 cloud/alpha8。main 未修改。

## 实质修改
- 回滚使用全新随机自检目录，归属标记持久化成功后才能自检；不使用旧 settings/index；不触碰旧无归属数据。
- cleanup 前兼容旧 active/previous 身份；历史自检数据无法安全清理时保留 retained 记录，支持重试。
- uninstall 先持久化停用状态，未清理完退出非成功状态，不抹去可重试记录，保留用户数据。
- 新增两个安装器回归；未增加第三方依赖。

## 实际验收
恢复全部 1186 个 rust/ 文件，原 Git 树 dbfdc5e566c3eb7291d4b691ec604bccfa2d8b0b 与远端一致。新 Git 树 c9c256db6a604c7353454172201159b91272f53f 与远端一致。安装器 blob 3c0e0468327a9dc0ab3d6e451e63c066aa467580。
Rust 1.99.1 Linux x86_64，锁定离线依赖。最新完整源码 cargo fmt --all --check、verify_offline_linux.sh（workspace check、worker build、全量 test）与 cargo clippy --workspace --all-targets --locked --offline -- -D warnings 均退出 0。原始日志 242 次通过摘要，去掉两个跨进程子测试重复计数为 240 项独立测试，0 失败、0 忽略。安装器专项 6/6，Debug 构建及专项 Clippy 通过。
原生 verify-install-negative.py 九项合同通过；独立 1 MiB tmpfs 实际 ENOSPC 通过（可用 1036288 bytes，写入尝试 1101824 bytes，errno 28；旧指针、合成用户数据、失败目录清理与可用空间恢复）。实际卸载残留回归确认未知文件保留、卸载 exit 1、停用状态持久化、retained 仍保留，人工确认合成样本后 cleanup 可完成。
未重建全量 Release，未验证真实模型成功安装、Windows/UOS/麒麟、物理断电、72h、竞品同机、签名和正式发行。

## 未关闭问题
1. cleanup 在仍存在非 active/previous retained 项时可能返回 0 并输出错误的完成计数；须修复部分/完整清理语义和原生断言，但不将成功安装因旧残留而误报成失败。
2. install 失败补偿中 remove_version 成功而 clean_probe 失败时，仍可能丢失 retained 记录；应修复并测试。
3. 随机回滚 probe 在进程被杀/清理失败时可能留下无持久登记的临时目录；安全回收需独立可信归属机制，不能删除未知目录。

评分维持 80.25/100（搜索 17.5/20，安全 18.5/20，桌面 12/15，功能 12.5/15，性能 12.5/15，发行 7.25/15）。距 95 差 14.75，建议 96 目标差 15.75。内部主观工程评分，不等于竞品测评；本轮不提前给未通过的发行能力加分。

## 新会话接续
先核对 cloud/alpha8 HEAD 及 rust/ 树；若有更新应从最新核实的代码接续。离线交接包 Xunshu_alpha8_Round07_Offline_Handoff.zip 包含完整 rust/ 树、原始证据及逐文件 SHA-256 清单；不是全 Git 仓库或安装包。配合此前用户上传的 Rust 1.99.1 Linux x64 tar 和 Xunshu_Cargo_Vendor_Offline.tar.gz 复现。完整操作手册 Xunshu_Alpha8_Continuation.md 随交接包提供。
依 docs/CLOUD_95_GATE.md：先修安装残余，再真实模型资源（BGE、PP-OCRv5、ORT 1.24.4、合法字体）、成功安装/升级/回滚/卸载，Windows 与 UOS/麒麟原生，最新 Release 规模性能/资源，72h 稳定性、竞品、签名许可。运行时必须纯本地，不能用假资源、绕过自检或放开路径安全边界。Actions 此前无实际运行记录，重新核验；不擅自更改仓库权限或强推。每轮真实记录 PASS/FAIL/BLOCKED/NOT_RUN。95+ 及独立发行门槛未全部通过前禁止正式发行。
