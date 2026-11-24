# 寻枢 0.18.0-alpha.5 本轮 Linux 离线质量检查

**2026-11-23 / Linux x86_64 / Rust 1.99.1 / 离线 Cargo vendor**

| 检查 | 结果 | 备注 |
| --- | --- | --- |
| 版本一致性 | PASS | `scripts/check_release_consistency.py`；8 个 workspace 成员版本一致 |
| `cargo check --workspace --offline` | PASS | 所有成员通过类型检查 |
| `cargo fmt --all --check` | PASS | Rustfmt 无差异 |
| `cargo clippy --workspace --all-targets --offline -- -D warnings` | PASS | 无告警 |
| `bash scripts/verify_offline_linux.sh` | PASS | **207 passed / 0 failed**，包含新增的空文档、链接与失败回滚测试 |
| `cargo build --workspace --bins --locked --offline` | PASS | Linux CLI、UI、工作进程可执行文件完成 Debug 构建 |
| `xvfb-run ... xs-ui --standalone --smoke-test` | PASS | 输出 `XS_UI_SMOKE_OK: native window rendered`，仅验证 Linux 窗口启动/退出 |
| Windows/UOS/麒麟实机安装升级 | NOT_RUN | 当前没有相应真实运行环境 |
| 软件签名、跨版本升级/回滚、长期性能压测 | NOT_RUN | 不得宣称正式发行完成 |

测试期间一次中途失败的原因：原书签隐私测试预期接受 `https://user:password@host`，与更严格的导入拒绝策略不一致。已更新安全契约与测试；**最终重跑结果 207/207**，未通过忽略测试达标。

新增/补强用例：

- 空文档不重复索引，内容清空清除旧命中，后续删除仍被移除。
- 一个无效 Word 抽取失败时，既不发布候选删除，也不覆盖旧索引代次。
- HTTP(S) URL 拒绝空 host、userinfo、控制字符、反斜杠和无效链接，合法网页链接仍可载入。
- Chromium 书签缺少标题时只展示站点名，路径和 URL 查询密钥不会出现在 UI 摘要。

**验证局限**：测试使用合成文件与 Linux 虚拟显示，不能证明真实 PDF/OCR 模型、Windows 安装器、国产系统 WPS 对接、大规模数据或多显示器场景已经通过。测试报告不替代源代码审计和最终发版复核。
