# Alpha8 第9轮专业化开发（2026-11-24）

基线 cedf0a2100d1269fb84220ab51634b623ed38c22，版本仍为 0.18.0-alpha.7；代码提交 11c3ef2d7298c763a926c03eef3d9e1575c6647a，Rust 树 e8575559654bddb4b646a80a8da47ecadd98709e（1187个文件，与本地测试树一致）。非发行声明。

## 真实改动
- scripts/prepare-models.py：仅复用独立固定 SHA256 的大文件缓存，复制到私有临时构建目录后再次验证；不复用历史派生 ONNX 和未固定摘要的元数据，元数据从固定上游版本重新获取。转换从官方权重重新执行；检查 FP32/INT8 形状、有限值、零向量及余弦门槛0.98，低相似度仅回退已验证FP32；非法数值拒绝构建。ORT归档要求唯一运行库、LICENSE及ThirdPartyNotices，复制后摘要验证。失败不发布半成品。许可证URL固定 FlagEmbedding d04f7989eeec2c6db4ead7c77ddef656f9c9d3aa、PaddleOCR a00f633cef33a8192f853fdaf8decf24ce2240b0。新增 build-provenance.json 记录输入/输出摘要、转换工具版本及数值证据，不改变运行时 inference.json schema。
- xs-inference/src/embedding.rs：前后取消检查、UTF8边界、命名输出、512维有限非零检查、f64范数防溢出。
- xs-inference/src/ocr.rs：拒绝空、非有限和形状无效的模型输出；取消检查；检测超过512行预算明确失败而不是返回部分成功。
- scripts/test-prepare-models.py：8项离线构建合同（合成数据和mock下载，不是神经模型推理）。

## 本轮实际证据
Debian 13 Linux x86_64 容器，Rust 1.99.1，原依赖离线vendor。首次安装器单元测试在默认/tmp（overlay）被既有守卫拒绝，保留失败日志；随后使用既有 verify_offline_linux.sh 的专属真实tmpfs测试环境，未改变任何路径安全策略。全工作区 fmt/check/test/Clippy -D warnings/版本一致性均通过；251成功摘要（按上轮两项跨进程重复口径为249独立测试），0失败0忽略，新增4项Rust测试。Python 8项通过。完整工作区 Release 构建命令达到工具时限，不能判为通过；后续独立 xs-inference（含 xs-model-worker、xs-install）及 xs-cli Release 均通过。最终安装器12/12原生负向合同通过，非正常安装。真实 prepare-models.py 执行因 ModuleNotFoundError: onnx 阻塞；没有生成假资源包。真实模型精度、OCR/语义检索、真实安装生命周期仍待验证。

## 性能复测
最新Release CLI三次独立重新生成300万条合成文件名快照，8类查询各30次。全部正确性PASS，宽OR均120万命中。第1/2/3轮宽OR P95为38.041336/57.350114/54.834470ms；严格50ms门槛分别PASS/FAIL/FAIL。没有修改阈值或隐瞒失败。合成tmpfs非SSD；尾延迟稳定性仍未关闭。

## 评分与阻塞
暂定工程成熟度81.25/100（安全18.75/20，+0.25，其他维度沿用17.5/20、12/15、12.5/15、13/15、7.5/15），非独立认证。距离96为14.75；95+且发行门槛同时通过才允许正式包。真实模型和独立固定小文件摘要仍缺；当前重新从固定URL获取小文件并记录摘要不等同于预先独立锁定其哈希。CI分支查询实际运行数 0；连接器无首次 workflow_dispatch 动作，不能声称 Windows/UOS/麒麟已验证。整套桌面 Release、本机签名、跨平台安装、72小时稳定性、真实SSD及真实模型验收均开放。继续性能尾延迟剖析、真实资源可信验证和真实平台验收，不放松安全或发布私有仓库。
