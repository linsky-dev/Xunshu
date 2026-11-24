# xs-query：统一 Rust 查询语义

纯 Rust、无文件读取、无网络。核心索引、CLI 与桌面端使用同一套解析、匹配和评分函数。`EntryRef` 只接收已授权范围内的元数据，不把查询表达式解释为文件操作。

## 已实现

| 能力 | 示例 | 明确语义 |
| --- | --- | --- |
| 文件名字面量、引号 | `年度 报告`、`"Annual Report"` | 默认文件名子串；引号包裹的值保持为数据，不会再次解释内部的 `file:` 等字段 |
| 布尔与分组 | `foo\|bar baz`、`(foo bar)\|baz`、`NOT ext:pdf` | `NOT/!` > `OR/\|` > 空格/`AND`；与旧版的主要运算优先级一致 |
| 文件和目录 | `file:报告`、`folder:`、`file:<foo\|bar>` | 裸类型过滤、带词或分组过滤均可 |
| 扩展名 | `ext:pdf;docx`、`ext:` | 末尾扩展名，目录不参与；空值表示无扩展名，`.gitignore` 也按无扩展名处理 |
| 文件名与路径 | `name:报告`、`path:工作资料`、`C:\Docs\` | Windows 路径分隔符在任意宿主系统上统一处理；POSIX 文件名中的反斜杠仍可用 `name:` 匹配 |
| 直接父目录 | `parent:"C:\Program Files"` | 精确父目录，排除更深层子目录；不访问该路径 |
| 来源 | `source:efu:archive` | 来源文字过滤；不连接离线来源 |
| 大小 | `size:>1MB`、`size:1.5KB`、`size:1KB..2MB` | 精确整数/有理数比较，支持开区间端点省略；KB/MB 与 KiB/MiB 均使用 1024 倍数，不使用浮点数 |
| 日期 | `dm:2026-11`、`dm:utc:2026-11`、`mtime:2026-11-08T10:00:00Z` | 日历范围默认按系统本地时区；`utc:` 指定 UTC；`>某日` 表示该日结束以后 |
| 相对日期 | `dm:today`、`dm:yesterday`、`dm:7days`、`dm:thisyear` | 本地日历边界在解析时确定，`7days` 为滚动时长；`Query::parse_at` 保留固定时刻与 UTC 日历语义 |
| 时间戳 | `mtime:@1794132000` | 明确的 Unix UTC 秒数 |
| 通配符 | `*.pdf`、`report_?.txt` | 对完整字段匹配，`?` 为一个 Unicode 标量；Unicode 组合音标按完整字面量片段归一化 |
| 大小写 | `case:README`、`case:<Report\|README>` | 仅该词或该组区分大小写；普通查询采用 NFKC + Unicode 小写 |
| 正则 | `regex:"^report.*\.pdf$"` | Rust regex 语法，带资源预算；不支持的 lookaround、反向引用明确报错 |
| 拼音 | `pinyin:xunshusheji`、`pinyin:xssj`、`pinyin:xun枢sj` | 全拼、简拼、混拼、多音字；NFA 匹配，不展开多音字笛卡尔积；允许末尾音节部分输入 |
| 模糊 | `fuzzy:rpt` | 文件名有序子序列，仿射 gap 惩罚和词边界奖励；不是拼写纠错/编辑距离 |
| 排序 | `matched_score` | 精确名称 > 精确主名 > 前缀 > 词边界 > 普通子串，附小幅路径深度调整 |
| 候选提示 | `candidate_plan` | 只返回必然出现在规范化名字中的字面量；NOT、路径、拼音、模糊、正则与大小写敏感分支保持保守 |

### 性能边界

`fold` 有 ASCII 快速路径。一次记录评估中，名字/路径/来源各自至多归一化一次；使用 `matched_score` 可避免 `matches` 和 `score` 再执行一遍。

引擎可用 `needs_path()` 判断是否需要完整路径，用 `matched_score_prepared(entry, folded_name, depth)` 借用已经验证的名字折叠结果，并传入缓存的父目录深度。`folded_name` 必须严格等于 `fold(entry.name)`，`depth` 必须等于 `xs_query::path_depth(真实完整路径)`；只有 `needs_path()==false` 时可在这个预处理接口中省去 `EntryRef.path` 的构造。普通接口不改变行为。84 条黄金查询和全部28条记录均验证预处理与普通接口的匹配和分数一致。

查询最长 8192 字节，最多 256 个词元，分组/修饰符最多 24 层；每个正则最长 2048 字节，编译预算 512 KiB、DFA 缓存预算 256 KiB、语法嵌套 32 层，一次查询至多 16 个正则或通配符。

拼音查询为 2–128 字符，模糊查询为 1–128 字符。两者仅针对文件名，输入名字最多 4096 字节。核心导入器另限制 1024 字符并显式拒绝超长 EFU 项；普通 Windows/POSIX 文件名在此范围内。模糊评分超过 262144 个 DP 单元时改用线性子序列评分，保持召回，不做指数回溯。

## 有意变更与尚未迁移

这不是旧版全部 3000 行查询语法的等价声明。与旧版不同，空布尔分支、空组、未闭合引号/分组、未知字段均明确报错，防止输入错误被扩大成全量搜索。应在迁移说明和界面错误中保留这个区别。

- 未支持的属性、去重查询、正文/内容权限表达式、自定义宏、持久 `?modifier:`、属性互相比值及特殊旧式日期写法不会默默退化。
- 正文/OCR/语义的召回由后续 worker 层接入；本 crate 不访问内容，也不将 `content:` 当成普通名字词。
- 拼音是显式 `pinyin:` 模式；普通 `xssj` 仍是字面量。尚无双拼、模糊音、词组读音概率和自动拼音倒排。
- NFKC + 小写有公开且确定的语义；不声称实现 Unicode 完整 case-fold（例如 `ß` 与 `ss` 不强制等价），不采用恢复得到的字符映射表。
- 日历条件按系统时区转换为起止时刻。Unix 秒数与 ISO 时分秒仍按 UTC 解释。
- 默认普通字面量和路径都不区分大小写；`case:` 提供精确大小写模式。

## 可重复验证

```sh
cargo test -p xs-query
cargo clippy -p xs-query --all-targets -- -D warnings
```

当前验证包含 4 项内部测试和 11 项集成测试，其中黄金 JSON 含 **84 条查询、46 条无效查询、28 条合成记录**。还遍历 1024 个语法组合，检查不 panic，并验证每个真正命中的记录都被候选计划保留。`tests/fixtures/query-golden-v1.json` 是语言无关用例，可用于新旧引擎逐条对照；不把旧版的已知错误作为新规范。

## 独立实现的公开依据

- Everything 公开搜索与分组语法：<https://www.voidtools.com/support/everything/searching/>，<https://www.voidtools.com/support/everything/search_syntax/>。
- Rust regex 官方资源预算和语法：<https://docs.rs/regex/latest/regex/>，<https://docs.rs/regex/latest/regex/struct.RegexBuilder.html>。
- Unicode 规范化由 `unicode-normalization` 提供：<https://docs.rs/unicode-normalization/>。
- 拼音使用开源 `pinyin` 0.11，关闭默认特性，仅启用 `plain` 和 `heteronym`：<https://docs.rs/pinyin/0.11.0/pinyin/trait.ToPinyinMulti.html>。

未读取或复制原软件恢复证据、商业字典及私有关键字表。具体依赖和对应许可证由工作区锁文件及第三方通知随包记录。
