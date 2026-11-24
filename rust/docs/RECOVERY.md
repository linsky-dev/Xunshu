# 寻枢 Rust 恢复版使用与验证

`0.18.0-alpha.2` 使用独立数据目录。Windows 默认为 `%LOCALAPPDATA%\XunshuRustV2`，Linux 默认为 `$XDG_DATA_HOME/xunshu-rust-v2` 或 `~/.local/share/xunshu-rust-v2`。CLI 和原生界面均可显式传入 `--data-dir`。

## 常用入口

```text
xunshu --data-dir <新目录> init --root <本地目录>
xunshu --data-dir <新目录> scan
xunshu --data-dir <新目录> search "ancestor:<目录> document:"
xunshu --data-dir <新目录> watch
xunshu --data-dir <新目录> content-configure --root <正文目录> --ocr --semantic
xunshu --data-dir <新目录> content-rebuild
xunshu --data-dir <新目录> content-search "查找本地文档" --semantic
xunshu --data-dir <新目录> status --verify-assets
xunshu --data-dir <新目录> doctor --json --export <新的本地诊断文件>
```

界面可保存查询书签，保留最近的显式搜索，并从正文页启动独立索引和查询。Windows 托盘宿主维护快捷框与主窗口，隐藏后释放渲染进程。双 Ctrl、托盘与开机启动属于 Windows 桌面入口。模型只从程序旁的 `resources` 加载，缺损时通过完整载荷修复安装。

Windows 标准扫描适用于显式选择的目录。`init --fast-volume --root <本地卷根目录>` 选择原生 MFT/USN 模式，需要具备该卷的访问权限和独立于所选卷的数据目录。卷日志缺口触发完整校准。`volumes` 仅报告本地卷能力，用户态常驻提权服务仍单独验收。

使用 `xunshu --data-dir <未创建的目标目录> migrate-m1 <M1 数据目录>` 迁移设置和快照。旧版仍可使用自己的目录。现有 Rust M1 快照在恢复版中可以读取，写入、删除和恢复均要求新目录。schema4 和 EFU 导入只产生离线来源。EFU 审阅、另存和比较入口分别为 `efu-inspect`、`efu-edit`、`efu-compare`。`folder-compare` 比较当前完整索引中的两个已授权目录。

`serve` 保留 v1 标准输入输出协议。`serve-v2` 先用 `{"id":1,"op":"hello","protocol":2}` 协商，之后接受 `search` 和 `cancel`，并将首屏与最终响应分开。`content-serve` 提供独立正文会话。MCP 工具 `search_content` 返回 `approval_id` 后，由用户在本地界面或通过 `agent-inspect` 和 `agent-approve` 审阅并授权，再以相同参数重试。会话退出或范围变化后重新授权。

## 本地构建和安装验收

在 `rust` 目录准备 Rust `1.99.1` 和锁定依赖。Windows 本轮使用 GNU 工具链和 `RUSTFLAGS='-C link-self-contained=yes'`，使用 w64devkit 的 GCC 编译 C 依赖。Debian WSL 在本地 Linux 文件系统用普通用户执行测试。

```text
cargo test --locked --offline --workspace
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo build --locked --offline --workspace --release
python scripts/verify-recovery.py --bin-dir target/release --temp-root <合成测试父目录>
```

准备模型只发生在构建阶段。`scripts/prepare-models.py` 校验官方 BGE 权重和 PaddlePaddle 的 PP-OCRv5 ONNX 发行模型。BGE 量化转换在 Python 中完成并对照官方权重输出。准备环境用到 PyTorch CPU 2.8.0、Transformers 4.51.3、ONNX 1.17.0、ONNX Runtime Python 1.20.1 和 PyYAML。产品的 Rust 推理工作进程使用锁定的原生 ONNX Runtime 1.24.4。资源清单包含发行来源、许可、大小和摘要。

`scripts/make-install-payload.py` 接收构建程序、资源目录、获许可的 Noto Sans SC 字体及其许可，生成带校验值的本地载荷和合成自检图片/PDF。`xs-install --root <独立安装目录> install <载荷目录> --sha256 <清单摘要>` 在实际推理通过后激活。再次执行同一命令可修复。`rollback` 复核上一版本，`uninstall` 保留独立的数据目录。`launch --data-dir <数据目录> --folder <已授权目录>` 是目录搜索入口。测试脚本 `scripts/verify-install.py` 使用独立根目录，Windows 测试仅对本轮隔离路径管理应用防火墙规则。

最终可用性、模型命中范围、性能及目标系统状态以 [验证记录](../VALIDATION.md) 为准。
