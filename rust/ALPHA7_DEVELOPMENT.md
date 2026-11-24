# Alpha.7 开发检查点（不放行正式发行）

基线：0.18.0-alpha.6；工具链：Rust 1.99.1，原有锁定 Vendor。未增加第三方依赖。

- 正文持久变更请求：content-enqueue / content-apply / content-watch。已知文件增改采用局部更新；未知、目录、删除或异常回到完整校准，避免静默漏收。实时监控重启先全量校准，不承诺 OS 持久日志。局部 `scope_reconciled=false`。桌面提供显式启动/停止实时收录。
- 原子发布正文数据库：`content.idx` 作为提交点，`content.base-a.idx` / `content.base-b.idx` 为两槽基准，32KiB 累计差分，达到预算则压缩；删除文件时强制重新生成基准。读取校验基准身份、基准与重构 SHA-256、发布身份及授权。兼容旧自包含格式。新格式旧程序不能直接读，降级需独立备份或重建。未完成磁盘 WAL、真正局部读取、加密与可证明擦除。提交统计不含检查点及 OS/存储硬件写放大。
- 来源授权：显式记住、恢复和撤销，绑定来源路径与内容摘要；不存导入正文和 URL 令牌。来源变化要求重新授权。
- 安装器：确认激活状态持久化后才清理旧版本，验证程序，保留无清单未知文件；卸载先持久提交状态。真实模型安装、断电、Windows 升级及卸载未验收。

Linux 当前证据：全工作区 219 个独立测试通过（原始日志 220 次成功，跨进程子测试重复一次）；Rustfmt、Clippy `-D warnings`、全工作区类型检查、版本一致性、Debug 原生构建与 Xvfb 冒烟通过。Release CLI/正文 worker 和正文 benchmark 构建通过；桌面 Release 未重新构建。3,000,000 合成文件名每组 30 次，首屏核心 P95 最大 0.462ms（3 宽查询为部分首屏），完整查询 P95 最大 105.744ms（仍超 50ms 门槛）；20,000 合成文档初次建库 11.771s，常驻查询 P95 0.788/5.129/4.617ms；增量 1/100/1000 文件分别 59.408/171.174/1057.898ms，发布 557244/1016052/1966440 字节，命中真值验证通过。合成、tmpfs、非竞品同机跑分；不含真实模型及真实多格式文件。

复现：`bash scripts/verify_offline_linux.sh`；`cargo clippy --workspace --all-targets --locked --offline -- -D warnings`；`cargo build --release --locked --offline -p xs-cli -p xs-content --bins --example benchmark`。把 CLI 和正文 worker 放在安全策略允许的本地目录，同目录有 `xs-content-worker`。用专用空数据目录运行 `xunshu --data-dir <dir> benchmark --entries 3000000 --iterations 30`；`target/release/examples/benchmark <合成父目录> <CLI绝对路径> 20000`。不禁用任何本地文件系统保护。

下一阶段必须完成真实 OCR/语义资源、Windows 原生发行/升级/回滚/卸载、国产系统实机测试、长期故障注入和资源预算，且验证宽查询完整耗时。未达到 80 分门槛，禁止将本开发检查点称为最终发布包。
