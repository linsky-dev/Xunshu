# 寻枢原生桌面界面

`xs-ui` 使用 Rust 和 eframe / egui。文件名搜索与 CLI 共用核心索引和授权规则。正文查询按需启动同目录的 `xunshu content-serve`，通过继承管道协商、搜索和取消。文件抽取由 `xs-content-worker` 执行。

## 启动与操作

```sh
cargo run -p xs-ui --release -- --data-dir /absolute/local/data-directory
```

`--data-dir` 接受本地绝对路径。新用户选择收录目录和文件类型后才开始扫描。可选参数如下。

| 参数 | 用途 |
| --- | --- |
| `--query TEXT` | 启动时带入查询 |
| `--quick` | 打开快捷搜索框 |
| `--content` | 在独立主窗口直接打开正文页，可与 `--query` 同用 |
| `--font PATH` | 指定本地中文字体，上限 64 MiB |
| `--standalone` | 单独打开界面，便于诊断 |
| `--background` | Windows 仅启动常驻宿主和托盘 |
| `--smoke-test` | 运行原生窗口烟测，使用现有索引 |

Windows 默认启动每配置单实例的托盘宿主。双击 Ctrl 打开或隐藏快捷搜索框。方向键选择，Enter 打开已授权的本地结果，Esc 隐藏。输入法组合期间快捷框让出方向键、Enter 和 Esc。同一会话中主窗口和快捷框分别保留查询。宿主监督后台监听和手工扫描，隐藏后关闭渲染进程，退出后回收子进程。托盘可显式选择当前用户登录时启动。

Linux 使用 StatusNotifierItem 接入桌面托盘。支持该协议的面板可显示寻枢图标，关闭窗口可收起到托盘，点击托盘或再次打开同配置下的应用可恢复原窗口。托盘菜单提供退出。有桌面会话但面板未提供托盘时，保留系统原有的窗口行为。

主窗口支持按文件类型过滤、分页、复制路径、重新加载、选择收录范围。文件名页的“搜索历史与书签”保留最近 24 条显式搜索，提供查询收藏、移除和清空历史。两类记录存于独立的本地文件。首屏与完整查询分开提交，后者按来源和路径保留选中项。打开文件前重新检查范围、记录身份和修改版本。

在“正文”页单独设置正文范围并更新正文索引。内置资源校验通过后，可启用图片及扫描 PDF 文字识别和语义检索。命中预览显示正文片段、段落、页码、图像位置或工作表单元格。切回文件名或隐藏窗口时释放正文查询进程。重新校验授权或文件版本失败时清除失效的结果。`本地工具` 提供 EFU 行检查与另存、离线清单对比、已收录目录对比，以及需要显式核对的 Agent 一次性授权。

界面优先加载安装目录下的 `resources/fonts/NotoSansSC-Regular.otf`，也支持系统自带中文字体。加载前检查字体格式和关键简体字形，损坏或缺失时给出中文提示。

## 本地验证

```sh
cargo test -p xs-ui --locked --offline
cargo clippy -p xs-ui --locked --offline --no-deps -- -D warnings
```

Windows PowerShell 7 交互桌面上，将下列参数指向同一构建目录中的程序。正文检查还需要 `xs-content-worker.exe`，脚本使用独立的合成目录和配置。

```powershell
& rust/crates/xs-ui/verify-host.ps1 -Ui rust/target/debug/xs-ui.exe -Cli rust/target/debug/xunshu.exe
```

脚本验证原生窗口、同配置单实例、已识别热键消息的显隐分派、隐藏后监听及索引更新、退出进程清理、正文预览、中文字体与损坏字体提示。系统注入的键盘事件不会计入双 Ctrl，实体按键、输入法候选操作和高 DPI 外观需要独立交互验收。完整平台和性能状态见 [VALIDATION.md](../../VALIDATION.md)。

Linux 的 Xvfb 原生烟测使用普通用户、本地合成数据目录和同目录构建产物。

```sh
bash rust/crates/xs-ui/verify-xvfb.sh rust/target/debug/xs-ui rust/target/debug/xunshu /absolute/local/test-root
```
