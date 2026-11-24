# 寻枢开源融合工程：来源、许可与可执行落地计划

> 审核时间：2026-11-23。面向免费开源、完全离线、可跨平台的 Xunshu。本文是技术评估与来源台账，不等于法律意见，也不代表已复制这些上游项目的源码。

## 1. 不以“非商业”绕过许可证

- 本仓库目前将自编 Rust 工作区按 **MIT** 许可发布。用户承诺未来免费开源是产品方向，但 GPL、Apache、MIT、EPL 等义务仍然有效，**免费不等于许可豁免**。
- 首选 MIT / Apache-2.0 / Unlicense 等可兼容整合的上游，先记录实际来源、commit、具体文件、许可正文、版权通知、第三方依赖，再引入；引用方案或公开算法无需假称“移植了源码”。
- 对 GPL-2.0-only、GPL-2.0-or-later、GPL-3.0-only 组件，不直接复制/链接进继续按 MIT-only 分发的同一个程序。若未来确有必要，要先做全项目兼容性判断和重新许可决策。仅作为独立进程或插件**不自动**使许可义务消失。
- 此次统一入口、来源路由、连接器 Top-K 匹配和缓存为**寻枢自身新增实现**，**没有复制**下表第三方仓库源码；不得在发布说明中虚构“已经集成/移植”它们。

## 2. 项目优先级矩阵（以具体上游仓库 LICENSE/README 为准）

| 项目/仓库 | 观察到的许可 | 最值得借鉴/未来可复用部分 | 集成方式与风险 | 状态 |
| --- | --- | --- | --- | --- |
| [UniversalSearch](https://github.com/syuumaimikan/UniversalSearch) | MIT OR Apache-2.0（仓库宣称） | 来源插件、统一查询规划、书签/应用/文件融合 | 需要锁定正确仓库、提交和模块；README 内安装 clone 地址与现仓库存在差异 | **设计已参考；未复制** |
| [Kensaku](https://github.com/yjid/kensaku) | MIT | 四层检索、查询词法/规划、OCR 失败回退、MCP、资源预算 | 只提取经压测验证的能力；不要整体替换现有 UI/索引 | **规划中，未复制** |
| [Everyfind](https://github.com/kyo5uke/everyfind) | MIT OR Apache-2.0 | NTFS MFT + USN Journal、持久更新恢复 | Windows 特有；重视低权限失败、日记溢出和反向校验 | **规划中，未复制** |
| [Tantivy](https://github.com/quickwit-oss/tantivy) | MIT | 增量全文索引、BM25、段合并、读取热身 | 必须与 SQLite FTS5 实测内存/磁盘/中文命中率；先做可替换实验 | **候选依赖，未引入** |
| [ripgrep](https://github.com/BurntSushi/ripgrep) | MIT OR Unlicense | 代码正文扫描、忽略文件规则、快速 fallback | 仅对显式授权目录做受控查询，禁止后台无界递归 | **规划中，未引入** |
| [fd](https://github.com/sharkdp/fd) | MIT OR Apache-2.0 | glob/ignore、安全路径遍历和 CLI 体验 | 与自有扫描器比较差异，再按模块引入 | **规划中，未引入** |
| [Yazi](https://github.com/sxyazi/yazi) | MIT | 异步预览、图片/文件操作交互 | 重点学调度与预算，不应复制整套 TUI | **规划中，未引入** |
| [Flow Launcher](https://github.com/Flow-Launcher/Flow.Launcher) | MIT（仓库 LICENSE） | 键盘优先 UI、搜索来源分组与插件生态 | 主要为 .NET 实现，最适合学习交互和扩展契约 | **设计已参考；未复制** |
| [FSearch](https://github.com/cboxdoerfer/fsearch) | GPL-2.0（上游标示） | Linux 文件树刷新与过滤器 | **仅参考原理；当前不直接复制/链接** | 待许可决策 |
| [Recoll](https://www.recoll.org/) | GPL-2.0-or-later（Linux 发行包） | 失效文档重试、抽取器模块化、增量全文索引 | **仅参考架构；当前不直接复制/链接** | 待许可决策 |
| [DocFetcher](https://github.com/docfetcher/DocFetcher) | GitHub 仓库提示 Apache-2.0；历史文档提到 EPL | 正文范围、索引目录、内容提取 UI | 版本/来源许可证不一致，**先锁定准确源码标签和许可证** | 待核查 |

## 3. 已落地能力（0.18.0-alpha.4）

1. 普通搜索可并行显示**文件名索引 + 本机应用 + 用户主动载入的书签/Git 仓库**（不访问互联网、不自动读取未授权浏览器档案）。
2. `app:` / `>`、`bm:` / `bookmark:`、`git:` 作为来源限定。限定来源不会被文件搜索语法误解析。
3. 快捷面板支持文件、应用、书签和仓库共同方向键选择与回车打开；主面板可点击相应数据源；应用由系统启动，书签仅允许 HTTP(S) 并由用户点击打开，Git 仓库只打开本地目录。
4. 已载入书签使用内存中的有界 **Top-K**，重复查询结果缓存，手动重新载入/添加仓库立即失效缓存；最大提示数严格受限。
5. 书签搜索和界面提示只保留标题、站点域名，不显示 URL 中用户名、密码、路径、查询字符串和片段；实际链接仅在用户明确点击后交给系统浏览器。
6. 新增来源路由、分组排序、2 万条书签 Top-K 与 URL 去敏相关回归测试。

## 4. 后续优先级与可验收标准

**P0：正确性与安全验收**
- 建立每类来源 100+ 真实用例的真值集，覆盖拼音、多语言、路径权限、文件变更、符号链接、异常书签、旧索引；搜索结果附**“来源/上次验证/为什么命中”**，以人能理解的方式解释候选。
- 强制 1,000 次打开文件/书签/应用动作正确性测试；禁止 URL 参数/凭据进入日志、截屏提示和异常栈。

**P1：索引与性能**
- 从现有正文快照复用推进到持久差分队列、批次提交、可取消重试；分批 OCR/向量计算、CPU/内存预算；变更后只更新相关文件。
- Tantivy 仅用于隔离基准原型，对照 SQLite FTS5 的中文召回、P50/P95、300 万文件与 10 万正文文档索引体积；达标才替换，且必须保留一键回退。
- Windows 每卷 NTFS MFT/USN 与 Linux inotify 分别做溢出/挂载故障/热插拔模拟，不能用“最近扫描过”冒充实时完整性。

**P2：用户体验与开放扩展**
- 增加授权来源配置存储与可撤销权限、索引资源消耗设置、同名文件去重可视化；设计签名插件 manifest + 进程沙箱协议；保持核心离线。
- 设计独特亮点：**可解释检索**（为什么命中）、**可撤销来源授权**、**受控按需深搜**（只在用户启动时执行）、**索引健康度与低开销自动修复**。

**P3：公开发行**
- Windows 10/11、麒麟、UOS 的真正原生安装、增量升级、回滚、卸载、数字签名（如具备证书）实机验证，长时运行与多语言 UI 验收。
- 开源发布时输出 SBOM、第三方版权声明、锁定源 commit/文件级来源、构建可复现说明；发布版本一致性强校验。

## 5. 上游代码移植台账格式（适用于之后的每次引入）

```text
upstream_name:
repo_url:
commit_sha:
source_paths:
license_expression:
license_text_path:
notice_paths:
original_author:
local_target_paths:
transformation_and_changes:
verification_tests:
security_review:
commit_of_integration:
```

> 本轮没有记录实际移植 SHA，原因是**没有直接复制任何新增外部项目源码**。今后如引入必须先填写此记录，再合并到默认分支。
