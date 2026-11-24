# Rust 迁移：必须保留的契约与已发现风险

审查范围：原始 0.17.0；合成数据；Linux 工作区。此文件不代表 Windows/UOS 目标机验收。

## 建议核心接口

- `PathFlavor::{Windows,Posix}` + 根路径词法对象：解析/拼接由输入路径风格决定，不能使用当前宿主系统分隔符。Windows 的盘符、UNC、长路径与大小写规则要单独表示。
- `ScopeSnapshot { revision, roots, policies }`：普通扫描、极速索引、正文/OCR/语义、导出与统计全部接收同一份已授权快照；不能自行选择系统盘。查询输出带范围版本，策略更新按事务发布。
- `CollectionPolicy { extensions: Option<Vec<String>>, content_enabled, ocr, semantic, revision }`：`None` 表示所有扩展名，`Some([])` 表示普通文件一个也不采集；目录仍保留以便遍历。显式非法策略必须报错，不能降级成全量。
- `WizardState::{NotStarted,Skipped,Completed}` 与 `reminder_count`：取消、关闭、跳过均不能算完成；仅有效范围配置应用成功后记录完成；最多提醒三次。已有索引根不能替代完成状态。
- `FileIdentity(volume_id, file_id)` 与 `ChangeBatch { journal_id, cursor, records }`：同一文件 ID 的变更必须幂等；重命名可跨批次；父目录移动后子项路径立即一致；游标/索引状态原子提交。未知父节点暂停/补全或重建，不能拼造路径。
- `QueryAst` / `QueryCapabilities` / `SearchSnapshot`：语义、执行与能力展示分离。原语法默认 OR 优先于 AND；正则与高级语法没有实现时必须明确能力边界，不能把有意的查询变成普通文件名偷偷执行。旧 regex 用 PCRE1；Rust regex 的语法差异需公开说明。

## 已复现缺陷

原测集 78 项：50 通过、3 失败、24 项测试环境错误、1 跳过；另有 1 项 UI 测试类初始化错误。三项失败属于 Windows 路径在 Linux 上被 `os.sep` 误拆分/拼接。`tests.test_compact_index.test_path_reconstruction` 返回 `V:\\V:\\\\src\\annual_report.py`；两项 live_index 创建/重命名测试混入 `/`。

25 个环境错误：22 个测试被原始本地文件系统边界拦截（工作区为无法确认后端的 overlay），1 个缺少 PCRE1，1 项测试及 1 个测试类初始化缺少 PyQt5。没有绕过边界，也不能把这些项目算通过。

## 静态确认的问题

1. `linsearch/auto_index_ui.py` 的向导 `finished(result)` 无论接受还是取消都写 `setup_wizard_completed=True`。
2. `linsearch/gui.py` 用 `not wizard_done and len(prepared_index.roots()) == 0` 判断首次启动，已有根使未完成向导被压住。
3. `windows_support/runtime.py` 的 `--internal-fast-service` 分支读取 `SystemDrive` 并 `create_for_volume(drive)`，没有接收用户范围。

## 第一批黄金用例

1. Windows 输入 `V:\\src\\annual_report.py` 在 Linux/Windows 都还原成相同 Windows 路径；POSIX 输入独立验证。
2. `extensions=null` / `[]` / `[".TXT","pdf","txt"]` 分别代表全量 / 零普通文件 / 去重规范化；目录总能遍历。非法字段、`revision=true`、非法扩展名、显式 null 策略均失败。
3. 查询、总数、排序分页、导出、统计对同一范围快照返回一致集合；缩窄策略事务中断后保持旧快照完整；旧读者不混入新旧版本。
4. 向导首次未开始→取消→仍未完成；跳过→仅在提醒上限内提示；选择有效目录但策略提交失败→仍未完成；应用成功→已完成。
5. USN 同一 FRN 更新两次不重复；重命名前后分属两批也只剩新名；父目录改名后子文件路径更新；新增文件可通过父路径搜索；未知父项不泄露合成路径。
6. 普通查询大小写/变音折叠，名称/完整路径选项，默认 OR 高优先级，空组/未闭合组的原语义，拼音仅显式启用、非正则/重复树组合。
7. 设置文件与索引更新失败不截断原文件；失效设置保留可恢复副本；非法路径和网络路径拒绝；不联网下载。

详细探针与最终统计已写入同目录 `probes.json` / `audit.json` / `AUDIT.md`。纯核心基线另有 39 项全部通过（为原集合子集）；新增纯内存探针验证 21 条契约并独立复现 5 条增量一致性缺陷。
