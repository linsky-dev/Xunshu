# 0.17.0 → Rust 迁移审计

审查完成：2026-11-08。审查对象为原始 Python 0.17.0 和上传的《后期改造计划》。所有执行仅使用合成数据，未扫描用户真实目录、磁盘、网络共享或 Windows USN 日志。原代码和 Rust 目录均未修改。

## 结果

本轮有两组基线，不能相加作为互不重复的测试数。

| 执行 | 结果 | 说明 |
|---|---|---|
| 原有核心/兼容测试集 | 78 项：50 通过，3 失败，24 项错误，1 跳过；另有 1 项测试类初始化错误 | 3 失败复现 Windows 词法路径依赖 Linux `os.sep`；环境错误按下面分类 |
| 排除已知平台依赖的纯核心测试集 | 39 项全部通过 | 拼音、收录规范化、后端分流、排序；是原集合的子集 |
| 新增纯内存探针 | 21 条既有行为契约通过；5 条增量一致性缺陷复现 | 探针测试旧行为并记录期望，不改源代码 |

25 条错误记录由 22 次 `LocalPathError`（工作区 overlay 后端无法确认为本地）、1 次缺少 PCRE1、2 次缺少 PyQt5（其中一次类初始化）构成。保留了原本地范围防护，未通过绕过边界让这些测试变绿。Windows 实际属性测试跳过。Windows/UOS 实机兼容、真实卷权限、托盘/热键、NTFS 实时性以及性能门槛均未验收。

## 已复现缺陷

| 编号 | 证据 | 影响 / Rust 必须做到 |
|---|---|---|
| PATH-01 | 原测试 compact path reconstruction 与 live append/rename 失败；`compact_index.py:62,108`、`live_index.py:57` | Windows 路径在 Linux 被当成本机路径，拼出重复盘符/混合分隔符。为 Windows/POSIX 建词法路径类型。 |
| USN-01 | `probes.json` 中 SAME-FRN-IDEMPOTENCY | 同一 FRN 再次到达产生重复可见结果。变更应按稳定文件标识 upsert。 |
| USN-02 | RENAME-CROSS-BATCH | `pending_rename` 每次调用重置，跨批改名旧名残留。维护跨批状态或根据文件 ID 直接替换现有名字。 |
| USN-03 | DIRECTORY-RENAME-CHILD | 父目录改名后子项仍链接旧槽位，完整路径继续含旧目录名。父子关系应引用稳定文件标识。 |
| USN-04 | APPEND-PATH-SEARCH | `append` 更新名字带但不更新路径带，新文件无法通过父路径查询。追加/重命名后所有搜索投影保持一致。 |
| USN-05 | DIRECTORY-DELETE-CHILD | 只收到父目录删除的一批后，子项仍可见。删除快照应使子树失效，不能依赖后续子项事件一定齐全。 |

增量探针均采用 POSIX 合成路径，因此后五项不依赖 PATH-01，也不依赖真实 Windows 事件来源。实际 USN 记录组合、断点续读、日志 ID 更换还需专门的目标机测试。

## 静态确认的计划风险

- `auto_index_ui.py:204–211` 的 `finished(result)` 在取消/关闭时仍写 `setup_wizard_completed=True`。`gui.py:4076,4116` 把“有索引根”当成向导已不需出现的依据。三态必须区分未开始、跳过、完成；成功应用范围策略后才算完成，提醒计数最多三次。
- `windows_support/runtime.py:144–150` 的内部极速服务固定读取 `SystemDrive`，未读取向导范围或扩展名策略。统一 `ScopeSnapshot(revision,roots,policies)`，普通扫描、极速采集、查询、统计与导出同源。
- `collection_policy.py` 对 `extensions=None`（全类型）与 `[]`（零普通文件）有刻意区别。目录无论扩展名仍保留。Rust 不能把空列表当作默认全量。
- 原查询默认 **OR 比 AND 优先**；空组或空 OR 分支为真；未闭合组在末尾闭合；未知前缀按字面量。若新语法改变这些行为，需要版本化或明确迁移说明，不能误称完全兼容。
- PCRE1 与 Rust 正则引擎语法能力不同。未实现的 lookaround/反向引用等必须明确返回能力错误，不得降级成普通搜索。

## 建议首批接口

`PathFlavor`；`ScopeSnapshot`；`CollectionPolicy { extensions: Option<Vec<String>>, revision, content_enabled, ocr, semantic }`；`WizardState`；`FileIdentity`；`ChangeBatch { journal_id, cursor, records }`；`QueryAst`；`QueryCapabilities`；`SearchSnapshot`。

游标和索引更新原子提交。策略缩窄与旧项清除同一事务。查询结果带 scope/index generation，异步 UI 丢弃过时结果。未知父节点、脱机根或越界路径不应拼造可打开位置。更细契约和黄金用例见 `CORE_CONTRACTS.md`。

## 可重复执行

在源目录执行：

```sh
TMPDIR="$PWD/migration-audit/tmp" python -m unittest -v tests.test_compact_index tests.test_live_index tests.test_pinyin tests.test_collection_policy tests.test_search_backend tests.test_ranking tests.test_unified_search tests.test_v017_bugfixes
python -m unittest -v tests.test_pinyin.PinyinUnitTests tests.test_pinyin.PinyinQueryTests.test_default_query_unchanged tests.test_pinyin.PinyinQueryTests.test_pinyin_only_for_ascii_initials tests.test_pinyin.PinyinQueryTests.test_or_branch_preserves_original_predicate tests.test_collection_policy.CollectionNormalizationTests tests.test_search_backend tests.test_ranking
python migration-audit/probe_original.py
```

执行记录：`core-baseline.log`、`pure-baseline.log`、`probes.json`。结构化摘要：`audit.json`。

## 目标门槛不能用语言替代

计划的三百万条文件名查询 P95 ≤16ms/P99 ≤50ms、NTFS 变更到可搜 P95 ≤1s、冷 mmap ≤300ms、内存约束以及零默认外连，都是待测验收门槛。Rust 重写本身不构成达标证据。本轮不产生任何这些门槛已通过的结论。
