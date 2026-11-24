# Rust 核心与 CLI 独立对抗审查

审查时间：2026-11-08。范围：`xs-core` 的 index / persistence / operations / interchange，以及 `xs-cli` 的导入合并与发布路径。前文保留**修复前证据**，末尾记录修复后结果。本次发现的 R01–R10 均已通过对应回归；这不等同于全部旧功能或所有平台已经验收。

审查阶段没有修改这两个 crate。探针仅创建任务专属 `/tmp/xunshu-rust-validation-036c199d3807/rust-review-*` 合成样本；索引中的 Windows/UNC 名字是元数据，没有打开这些目录。没有访问真实用户目录、网络目录或进行全盘测试。后续根代理已明确授权本代理修复 persistence / operations / transactions 测试，代码修复与本报告的原始证据分开记录。

## 已验证的正确行为

- 对 300 条含中文、扩展区汉字、组合字符、NFKC、大小写、POSIX 反斜杠、Windows/UNC 词法路径的记录，37 条查询 × 5 个 offset，共 **185 组**候选、排序分值、完整总数和分页与独立全量语义 evaluator 一致。debug、release 各执行一次，均无差异。测试使用 Query::matched_score 计算所有记录的期望值，不复用 Index 的候选检索或分页。
- 干净 schema4 数据库导入前后的 SHA256 一致，未创建新 WAL/SHM；存在非空 WAL 的数据库被明确拒绝。
- 未终结 posting varint 的快照能够通过当前加载，但查询明确返回 `truncated posting list`，没有 panic 或伪装完整结果。此项记录为显式失败行为，不额外算一个问题。
- 常规完整查询的 `complete=true / total_relation=exact` 和中止查询的 `false / at_least` 标记正确；下面的问题发生于取消检查之前的候选构造，或未验证的快照不变量。

## 问题与风险优先级

P1：本轮发布/迁移闭环前应解决。P2：必须补齐相应边界与回归，不能据现有通过项声称完整验收。排序优先考虑真实副作用、用户数据语义及触发条件。

| ID | 优先级 | 已复现的问题 | 证据 |
|---|---|---|---|
| R01 | P1 | 快照创建在校验后仍按可变路径 `OpenOptions::open`，父目录被换成链接后发生越界写 | `rust-review-faults.json` / `parent_swap` |
| R02 | P1 | CURRENT rename 成功后目录 fsync 失败，API/CLI 报失败但 CURRENT 已切换新代次 | 同上 / `fsync` |
| R03 | P1 | EFU 往返丢失原始 FILETIME 精度、创建时间、属性、额外列，并把未知值改成 0 / 1970 | `rust-review-cli.json` |
| R04 | P1 | 同来源空 EFU 重新导入不能清空旧记录；schema4 中来源全部删除后重新导入仍残留 | CLI / legacy 两份 JSON |
| R05 | P1 | schema4 中 `filelists.enabled=0` 的目录被重新启用显示 | `rust-review-legacy.json` |
| R06 | P2 | 合法 CRC 的结构坏快照可通过未验证的 prepared flag / 空 posting 声称完整零命中；超长父路径绕过构建预算 | `rust-review-probe*.json` / `corruption` |
| R07 | P2 | 候选解码、OR 聚合/排序不响应取消或预算 | 同上 / `cancellation` |
| R08 | P1 | 空 scope token 被直接认作离线索引，却允许含 `source=local` 的范围外条目通过 `load_index` | 同上 / `scope` |
| R09 | P2 | `authorize_open` 在范围授权前执行路径校验和 metadata 读取，撤销范围后仍探测旧路径 | 同上 / `scope`；源码调用顺序 |
| R10 | P2 | EFU 输入 i128 最小值导致 debug 构建整数溢出 panic；release 错误行为不同 | 同上 / `efu_overflow` |

### R01：快照与锁文件创建应保留父目录句柄

`persistence::Index::write_snapshot` 先 `validate_local_path`，随后用普通 `OpenOptions::create_new(true).open(path)` 创建文件。`create_new` 只保护最终名字，不能保证父目录仍是刚验证的目录。`Store::lock` 有同类路径重解析方式。

探针在实际快照创建调用前，把任务合成的 `race-data/index` 重命名为 `index-saved`，再将原名字换为另一个合成目录的符号链接。CLI 最终因为 CURRENT 的安全父目录检查返回失败，但目标目录中已经出现 `generation-2-*.xsi`。这不是假设的网络攻击，也未打开网络：证据是同一专属 tmpfs 内可见的越界生成文件。

**修复/回归要点：**通过已验证父目录 descriptor/handle 创建快照和 writer.lock；清理失败临时文件同样不能重新按可变路径 unlink。回归须证明故障注入确实发生，且替代目录没有文件。原 shim 拦截 `open/open64`；改为 `openat` 后应更新注入位置或由平台受控测试钩子复现同一父目录置换，不能把“钩子没有触发”误判为防护通过。

### R02：提交后持久化失败不能报告成未发布

注入器仅让 CURRENT 父目录的 fsync 返回 EIO；写入与文件 fsync 已成功。CLI 返回 exit 1 / `ok:false`，但 CURRENT 从 generation 1 变成 2。`atomic_write_local` 的 rename 是可见性提交点，后续 fsync 失败无法归类为“当前指针未变”。

**修复/回归要点：**区分提交前错误与“已提交，但落盘持久性未确认”；调用者的 published/generation 与实际 CURRENT 保持一致，并向 CLI/UI 显示明确警告。故障测试要同时断言返回状态、CURRENT、当前可加载代次。不要用失败后盲目路径回滚替代提交状态，因为可能覆盖后续发布。

### R03：EFU 信息应保留，而非重新构造近似值

输入两行，一行包含：`Date Modified=133444736000000001`、`Date Created=133444736123456789`、`Attributes=34`、`Project=项目甲`；另一行 Size / 日期 / 属性全部未知。

当前输出第一行修改时间变成 `133444736000000000`，创建时间为空，属性变成 `0`，Project 列消失。第二行 Size 变 `0`，修改时间变 `116444736000000000`（Unix epoch）。原 EFU 本身没有被修改。

旧 `linsearch/formats.py` 的 `make_efu_record` / `record_fields` 保存原始日期字符串、属性和额外列；schema4 也保存 `mtime_filetime`、`birthtime_filetime`、`attributes`、`extra_json`。因此这里是迁移的静默信息丢失。

**回归要点：**EFU→索引→保存→mmap→导出，原字段值精确保留；再次合并别的来源与本地重扫后仍保留；未知值保持未知。秒级查询字段与导出原始字段可以分离，但不能声称秒级派生值是精确原值。

### R04：替换身份不能从新导入记录推导

`merge_catalog` 从新记录提取 sources，再删除旧索引中相同来源。新来源没有任何行时，来源集合为空，旧数据全部保留。已复现同名 EFU 从 2 行变 0 行后 `imported=0,total=2`；schema4 从 3 行删除某来源全部记录后 `imported=2,total=3`，已删除条目仍能搜索。

**回归要点：**用导入请求的明确来源身份/命名空间替换，覆盖空输入与完全消失的子来源，同时保留确实无关的来源。本问题在 EFU 和 legacy 两个入口都需要验证。

### R05：旧目录的禁用状态应保持

原版 `linsearch/index.py` 的查询条件会排除 `filelists.enabled=0`。合成 schema4 中标记禁用的 `efu:disabled`，新 importer 不读取 filelists，导入后查询 `source:offline:schema4:efu:disabled` 返回一条完整命中。

**回归要点：**导入应保留/尊重禁用状态，或者采用明确且可审阅的迁移选择；默认导入不应重新显示用户已关闭的来源。

### R06：快照验证应覆盖运行时会信任的不变量

以下变体均基于本工具生成的快照修改元数据并重新计算 CRC。它们测结构契约，不把 CRC 当作防篡改认证；普通单 bit 意外损坏的 CRC 拒绝不受质疑。

1. 将 `ABC.txt` 的记录 flag 设为 2，宣称名字等于 fold(name)。加载通过，`abc` 从 1 条变成 `complete=true,total_relation=exact,total=0`。prepared 查询 API 明确要求传入 hint 等于实际折叠值，加载器应验证此不变量。
2. 将 gram 项的 posting length/count 置 0，加载通过，原来命中的查询变为完整零命中。构建器不会生成空 posting，应拒绝这种结构。
3. 将 parent 改为 262 KiB 字符串，加载及查询均通过，返回 path 长达 262163 字节；构建入口对 path 限 32768 字节，但加载不执行该限制。相同父路径可被大量结果重复分配，放大内存。没有进行 OOM 测试。

**回归要点：**校验父路径/来源/单条名字/完整路径预算、NUL、UTF-8 边界、prepared flag、非空 posting 等读取假设；损坏应明确拒绝或返回错误，不可静默宣称完整结果。无需把本项扩展成对所有恶意内容的认证设计。

### R07：候选阶段也属于查询预算

25,000 条合成记录，查询为 128 个 `common` 的 OR。索引仅约数万条，无真实文件访问。

| 构建 | 预先 cancel=true | 1 ms 预算 | 实际 examined |
|---|---:|---:|---:|
| debug | 2095.6 ms | 2016.9 ms | 0 |
| release | 65.5 ms | 63.2 ms | 0 |

时间花在候选解码、重复 OR 集合聚合与排序，第一次取消检查发生于候选生成完成之后。这些数字是本次合成样本观测，不是跨机器性能承诺。

**回归要点：**候选阶段传递取消/截止时间，分块检查；重复分支可以消重；聚合要有空间上界，超出预算时返回诚实不完整状态或保守扫描。已有布尔逻辑的候选超集语义必须保持。

### R08 / R09：来源身份与打开顺序

独立调用公开 IndexBuilder/Store，构造空 token + local 来源的任务合成范围外条目，随后 `operations::load_index` 与搜索成功，尽管设置只授权另一个目录。常规 CLI importer 会打 offline 标签，因此此项不声称已有正常 CLI 输入能伪造 local；它暴露的是“空 token ⇒ 元数据离线”的未落实不变量。应在共享加载/范围校验处强制空 token 不得含 local 来源，避免未来写入路径或坏快照扩大范围。

`authorize_open` 最终拒绝范围外记录，但先验证本地路径并读取 metadata。对不存在的范围外路径，它先返回 NotFound，而不是范围拒绝。修复应先按记录类型做纯词法范围/类型策略判断，再验证本地路径与实际类型，必要时再次检查。实际系统调用追踪因云环境禁止 ptrace 而未执行，不能声称已有 strace 证据；源码顺序与独立探针的错误行为已记录。

### R10：EFU 整数计算必须受界

`timestamp` 将任意 i128 整数减 FILETIME epoch，再转秒。输入 `-170141183460469231731687303715884105728` 导致 debug subtraction overflow panic；release 最后报转换错误。用 FILETIME 合法范围解析或 checked arithmetic，让不合法输入在所有构建下明确返回错误。

## 复用与执行

所有证据文件是修复前结果，后续写到单独的 `*-after.*`，不覆盖这些文件。

- `rust-review-probe/src/main.rs`：`differential()` 可转成核心集成测试；`mutate_snapshot()` + `corruption()` 可复用为加载器坏快照回归；`cancellation()` 验证预取消与预算；`scope_invariants()` 验证空 token 和授权顺序；`overflow()` 验证 importer 不 panic。
- `rust_review_cli.py`：EFU 全字段与空来源替换的 CLI 回归。
- `rust_review_legacy.py`：schema4 字节不变、无 sidecar、WAL 拒绝、disabled 和删除来源重导入。
- `fs_faults.c` / `rust_review_faults.py`：提交后目录 fsync 和父目录置换；只接受任务专属前缀，不访问真实目录。新 pinned API 后必须确认父目录置换钩子仍实际触发。

```sh
env XS_REVIEW_LABEL=after XS_REVIEW_EXPECT_FIXED=1 python migration-audit/rust_review_cli.py
env XS_REVIEW_LABEL=after XS_REVIEW_EXPECT_FIXED=1 python migration-audit/rust_review_legacy.py
cc -shared -fPIC migration-audit/fs_faults.c -ldl -o migration-audit/fs_faults.so
env XS_REVIEW_LABEL=after XS_REVIEW_EXPECT_FIXED=1 python migration-audit/rust_review_faults.py

env RUSTUP_HOME=/workspace/scratch/036c199d3807/.toolchain/rustup \
  CARGO_HOME=/workspace/scratch/036c199d3807/.toolchain/cargo \
  PATH=/workspace/scratch/036c199d3807/.toolchain/cargo/bin:$PATH \
  CARGO_TARGET_DIR=/workspace/scratch/036c199d3807/xunshu/rust/target \
  cargo run --release --offline \
  --manifest-path migration-audit/rust-review-probe/Cargo.toml
```

初版 CLI 二进制 SHA256：`94b74320023a6215624baf0ed63e7ea637d7132543478da99c4fc2a7d76b4a1f`。审查中的 prepared 查询探针链接当时新 core 源码，区别于尚未重编的初版 CLI；debug/release 的具体输出分别保存在对应 JSON。

## 当前关闭状态

2026-11-08 本报告建立时，R01–R10 均已向根代理报告并进入修复。以下是随后新增的验证记录；前文与没有 `after` 后缀的 JSON 仍是原始失败证据。

| 范围 | 修复后验证 | 状态 |
|---|---|---|
| R01 | 新 CLI 的 openat 故障注入确实触发（`XS_REVIEW_SWAPPED_PARENT_OPENAT`），替代目录没有生成文件，原 CURRENT 保持 generation 1 | Linux 合成竞态回归通过 |
| R02 | 新 CLI 在 parent fsync EIO 后返回成功提交、generation 2、durable=false 和警告；实际 CURRENT 为 generation 2 | Linux 提交后错误语义回归通过 |
| 最终核心测试 | 41 unit + 2 adversarial + 13 interchange fidelity + 4 snapshot validation + 10 transactions，共 **70 项全部通过** | `rust-core-final.log` |
| R03 | EFU 原始 100 ns 时间、Created、Attributes、Project、未知空值逐字段一致；原文件未变 | `rust-review-cli-after.json`，断言通过 |
| R04 | 同来源 EFU 清空后 total=0；旧库中原来实际可搜的 enabled 来源删除后重导入 total=0 | CLI / legacy after JSON，断言通过 |
| R05 | disabled 来源不再出现；原 db SHA256 不变，无新 sidecar，非空 WAL 继续被拒绝 | `rust-review-legacy-after.json`，断言通过 |
| R06 | 错误 prepared flag、空 posting、超长 parent、坏 varint 都在加载阶段拒绝 | debug / release 独立 after 探针通过 |
| R07 | 25k / 128 OR，release 预取消 0.000201 ms，1 ms 预算耗时 1.078 ms；debug 分别 0.002954 / 1.031 ms，均诚实标为不完整 | `rust-review-probe-after.json` 与 release after JSON |
| R08 | 空 token + local 行在 load_index 阶段拒绝 | 独立 after + transactions 通过 |
| R09 | 存在与不存在的范围外记录均先返回授权拒绝；不再先返回 NotFound | 独立 after + transactions 通过 |
| R10 | i128::MIN 在 debug 与 release 均明确返回 unsigned 64-bit FILETIME 输入错误，没有 panic | 两份独立 after + importer 回归通过 |

最终 debug、release 各 **185 组**查询/排序/分页全量对照仍无差异。post-commit 故障后实际重新运行 CLI 搜索，成功加载 generation 2 且得到 second.txt；不仅检查了 CURRENT 的文本。父目录替换故障在新的 pinned openat 路径实际触发，未把未触发钩子算成通过。脚本设置 `XS_REVIEW_EXPECT_FIXED=1` 时包含相应断言，默认输出到 `*-rerun.json`，不会覆盖原始失败证据。

发布 API 现返回 `PublicationOutcome { path, generation, durable, warning }`；重建返回 published 与 durability 警告，设置保存传播 `LocalWriteOutcome`。离线记录跨重扫使用 `add_from_index` 保留稀疏原始字段。CURRENT 存在性检查只将安全打开得到的 NotFound 视为不存在，链接/权限/坏路径错误不再被吞掉。没有加入按路径删除旧代次的回收机制。

根代理的 300 万条复测发现 publish 仅为比较代次而映射旧 snapshot 会放大峰值内存；随后增加 `Store::current_generation()->Result<Option<u64>>`。它只通过固定句柄读取最多 4096 字节的 CURRENT 游标，不映射旧 payload；publish 在 writer lock 内用该值拒绝旧代次。真正查询仍走完整 load/CRC/结构验证。新增回归确认损坏的旧 payload 不被游标读取，但 load 仍拒绝；新代次发布成功后旧文件保留。8 项 persistence 回归通过，记录在 `rust-persistence-cursor-after.log`。最终 RSS/发布时间采用根代理后续基准，不在这里推算。

仍保留明确的阶段边界：新 EFU 日期输入只接受 unsigned FILETIME；schema4 原有 calendar/ISO 日期原文本会保真导出，但以这种文本重新导入为新 EFU 会明确拒绝。没有把此边界包装成完整旧版 EFU 语法兼容。原始索引/旧数据未被原地改写。

本代理尝试的 Windows core `--tests` check 因未设置 MinGW 编译器路径而阻塞在 libsqlite3-sys（缺 `x86_64-w64-mingw32-gcc`），不能记为代码通过或代码失败。最终 Windows 交叉构建由根代理协调使用已验证的 MinGW 环境统一进行。本审查不证明 Windows/UOS 实机运行通过，也不证明硬断电后的物理持久性。根代理的 `rust/VALIDATION.md` 仍是整体验证状态入口。
