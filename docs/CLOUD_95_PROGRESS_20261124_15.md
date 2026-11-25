# 寻枢 Alpha8 第15轮开发记录（2026-11-24）

基线 e142bdcd98e82d99de990dbb582b83769f66b787，Rust 树 47dc4ee9b05ff68c272b831bdd5f31332effa0a7。1620个交接文件摘要全部匹配，原始Rust1.99.1与vendor归档摘要匹配，1192个Rust跟踪文件重建Git树与基线一致。版本0.18.0-alpha.7，未发行。

## 源码实改
- `xs-content/src/extract.rs`: ODS 在 Calamine 展开之前先有界验证 content.xml。限制 XML 深度、行列范围、重复计数、非空矩形/中间展开总单元格槽位100万、重复内容展开64MiB及空格展开2MiB；禁止 DTD，拒绝极端或无效计数，不把静默截断当作成功。保留正常尾部重复空行/空单元格；仍使用既有Calamine解析，不执行公式、不放宽输入或工作进程安全策略、不新增依赖。大而合法的ODS可能明确超限，不声称支持所有稀疏ODS。
- `xs-content/src/lib.rs`: 新正文抽取版本指纹 `ods-budget-v1`，旧正文索引重建，源文档、授权范围及文件名索引不变。回归测试比较第14轮指纹；两版实际Release迁移本轮未单独复测。
- `xs-ui/src/desktop_windows.rs`、`xs-content/src/lib.rs`: 根据两次真实Windows Runner Clippy报错，移动测试模块至文件末尾，不抑制警告、不修改生产逻辑。
- `.github/workflows/cloud-ci.yml`: 实际发现ENOSPC环境故障导致独立恢复、监听、UI和模型测试被跳过，修复为依赖成功Release构建且未取消；模型三阶段仍按成功先决条件执行。不使用continue-on-error、不伪造ENOSPC通过、不改变Runner权限。

## 云端原始证据
- [初始运行](https://github.com/linsky-dev/Xunshu/actions/runs/45381454446)：基线Ubuntu格式/类型/Clippy/测试/Release/ELF/安装负向通过；ENOSPC `unshare: write failed /proc/self/uid_map: Operation not permitted` 失败，后续测试跳过。Windows类型检查通过、Clippy失败。性能三轮720次独立真值通过，但严格50ms为0/3，宽OR P95 103.004356/103.122496/103.619673ms，组合查询75.77095/76.390546/76.031932ms。原始Linux证据与性能ZIP下载并核对SHA256，三组性能门槛本地独立复核均正确性PASS、延迟FAIL。详见qa/round15。
- [第一处Windows修复](https://github.com/linsky-dev/Xunshu/commit/741fd2d68b2da7a79a56c99f5534b9ce324eb971)；[复测](https://github.com/linsky-dev/Xunshu/actions/runs/45382279661)发现正文模块同类错误。
- [完整源码修复](https://github.com/linsky-dev/Xunshu/commit/e506516a10fc3f7151c1b8de8fd13983e7c8e512)，实改Rust树 `76253ce748c6e47a9fc4044e143614c74746e2c0`（本地和远端一致）。[独立源码验证](https://github.com/linsky-dev/Xunshu/actions/runs/45383054686)。
- [CI修复](https://github.com/linsky-dev/Xunshu/commit/d62653bc98488d6cc9ed73fc7fe5521a10fe845d)，工作流blob `0a3bef11e1a48568b4f29b0b468e6c9da22ab987`，已非强制快进至cloud/alpha8；[CI运行](https://github.com/linsky-dev/Xunshu/actions/runs/45383330870)。后续实际结论以记录时状态为准。

## 本地验证
Debian13 x86_64，Rust1.99.1、Cargo锁定vendor、真实tmpfs测试目录（不放宽生产路径守卫），4 CPU/4GiB cgroup。六个ODS专项单测通过，含旧Calamine静默截断复现；早期测试缺少ODS manifest失败已修正fixture并保留日志。全工作区270成功摘要（按历史口径约268独立）、0失败0忽略；四组Python合同28/28；fmt/check/严格Clippy/版本一致性通过。384MiB RLIMIT_AS原生Debug正文worker合成专项6/6，含正常文档、稀疏矩形、行重复超限、空格放大、重复内容以及同进程异常恢复。后续测试模块仅移动位置；最终fmt通过。真实模型本地构建实际失败（缺onnx），未生成真实模型资源。

## 评分及放行
暂维持83.50/100（搜索18/20、安全19/20、桌面12/15、功能13/15、性能13.25/15、发行8.25/15）。云端性能失败与Windows/模型验收仍开放，不自动加分。未达到95+且独立发行门槛未通过，禁止正式安装包。目标平台实机、真实SSD、72h、真实模型、正常安装升级回滚及签名仍开放。用户需在Actions手动运行修复后的cloud/alpha8，model_validation=true、fault_validation=false先取得真实模型证据；独立ENOSPC另次执行，或使用可信支持隔离命名空间的Runner。连接器无首次workflow_dispatch能力。不以云端通用Runner代替目标系统。
