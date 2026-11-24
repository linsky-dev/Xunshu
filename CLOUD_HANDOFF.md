# 寻枢 alpha.7 — GitHub 云端接续与 80 分验收

基线：0.18.0-alpha.7。此前 Debian 13 x64 实际工程记录：219 项独立测试通过、0 失败，Rustfmt/Clippy 通过，Release 文件名 300 万条合成索引首屏最高 P95 0.462ms、宽 OR 完整查询 P95 105.744ms（目标 50ms，未达）；2 万合成正文增量发布通过。依据 checkpoints/alpha7 中原始证据，沿用 20/20/15/15/15/15 六维权重，成熟度暂为 76/100。不是竞品实测得分。距离 80 差 4 个加权分，严格超过 80 需再多于 4 分。当前禁止最终发行打包。

## GitHub 当前边界
本仓库已初始化；alpha.7 的差异补丁、原始报告、QA 数据和测试日志已保存在 checkpoints/alpha7。完整 alpha.6 + alpha.7 源码树仍需导入，不能仅用补丁启动 CI。导入后先验证 rust/Cargo.lock SHA-256 b322a5e3baf465cb2ab462259a44969425fc5eb41793814b3b775bcb88ca969e 与 8 个成员；禁止覆盖其他提交或强制推送。

## 路线
1. GitHub Actions 官方 windows-2022 (MSVC)、ubuntu-24.04，Rust 1.99.1；先 cargo fetch --locked，后 cargo check/test/clippy --locked --offline。保留现有本地文件系统防护；使用独立合成数据。先处理真实编译失败，再上传本次 commit 的日志与未签名测试二进制，不创建 Release。
2. 在独立可信构建任务中使用 rust/scripts/prepare-models.py 下载并校验 BGE、PP-OCRv5 与原生 ONNX Runtime 1.24.4，锁定来源、许可与哈希；用 make-install-payload.py、verify-recovery.py --with-models、verify-install.py 验证真实推理与安装事务。正式包必须离线运行，不上传私人文档。
3. Windows Server Runner 可以做原生编译、CLI、NTFS 和隔离安装测试；Windows 10/11 桌面输入法、多屏、托盘和真实升级另用对应虚拟机/自托管 Runner。
4. GitHub 不提供标准 UOS/麒麟托管镜像。先用许可合规且摘要锁定的目标容器做兼容构建及 ABI 检查（OverlayFS 不能绕过安全策略），后用 UOS/麒麟虚拟机注册专用 self-hosted runner，并用独立用户/数据、真实文件系统测试。不要把容器通过算作桌面实机通过。
5. 优化宽 OR 查询，Release 同机重复 300 万元数据与 10 万正文；测量首屏、完整查询、峰值内存、后台占用及真实 OCR/语义召回；故障注入磁盘满、权限撤销、强制退出等。

## 评分及门槛
现有加权分：索引 17/20、安全 17/20、桌面 11.25/15、功能 12/15、性能 12/15、发行 6.75/15。目标示例：原生安装证据使发行至 10.5（+3.75），真实模型使功能至 13（+1），故障矩阵使安全至 18（+1），条件式总计 81.75；再补性能/桌面证据争取 83–85。每项需重新审查，不预先计分。所有目标平台发布门槛独立于总分。未通过 P0、许可、签名及安装/回滚验收不得发布；仅保留 CI 工程产物与证据。

参考： https://docs.github.com/en/actions/reference/runners/github-hosted-runners ； https://docs.github.com/en/actions/reference/runners/self-hosted-runners ； https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/run-jobs-in-a-container
