# 寻枢本地正文索引

正文收录使用独立授权范围。保存 `content_enabled: true` 及明确的 `content_roots` 后，运行 `xunshu content-rebuild`，再运行 `xunshu content-search <查询>`。数据目录从收录范围中硬排除。默认关闭 OCR 和语义，可分别启用 `ocr_enabled`、`semantic_enabled`，再重建正文库。安装资源目录由工作进程位置决定。资源缺失或校验失败时提示修复。

- 普通正文支持 UTF-8、带 BOM 的 UTF-16、GB18030，以及 TXT、Markdown、HTML、DOCX、XLSX、PPTX、ODT、ODS、ODP 和 PDF 文字层。抽取忽略 HTML 脚本和外部资源，Office 公式按字面与已有缓存值收录。结果定位包括段落、幻灯片、PDF 页码、工作表及 A1 地址。
- OCR 支持 PNG、JPEG、TIFF、WebP，以及文字层为空、含直接嵌入 JPEG 或 8 位灰度/RGB 图像的 PDF 页。返回图片像素坐标。普通 PDF 文字层优先使用现成文字。带页面渲染才能还原的图形、嵌套图像及其他图像编码未纳入此抽取路径。
- 关键词查询使用 SQLite FTS5 和 BM25，按文档去重。中文连续词保留字符顺序，空格分隔的多组词需同时命中。语义查询对每个分窗向量计算得分，每个文档取最高分，再做全局分页。启用语义并重建后，通过 `ContentReader::search_mode(..., SearchMode::Semantic, ...)` 或常驻查询协议 `mode: "semantic"` 使用。
- `indexed_total` 表示当前索引快照里的命中文档数。查询前后校验设置文件和正文库代次，展示前用 `validate_response` 再校验范围及文件对象版本。分页里的旧文件会被省略并计入 `omitted_stale`。

正文库通过锚定路径写入独立的原子快照。目录遍历、进程异常或取消时保留原快照。某个文件无法解析时可发布其余文件并记录部分覆盖及失败计数。当前正文库上限 512 MiB，单文档输入上限 32 MiB，抽取文本上限 2 MiB。默认单次抽取或推理超时 10 秒。正文、OCR、语义进程的内存上限分别至少采用 384、768、1024 MiB 的对应默认配置，Windows 由 Job 管理，Linux 使用进程组和地址空间上限。

`xunshu content-serve` 通过继承的 stdin/stdout 运行 JSON 行协议。请求具有数字 `id`，`status` 返回协议版本，`search` 支持 `query`、`mode` 和 `options`（`offset`、`limit`），`cancel` 用 `request_id` 中断查询，`release` 释放正文与模型缓存，`shutdown` 退出。客户端一次发起一个查询，按响应 ID 接收结果。用户隐藏或切换到文件名模式时，宿主可释放常驻内容状态。

基准示例生成 10 万份合成文档并在运行后清理。先构建发布版 `xunshu`、`xs-content-worker` 和本 crate 的 `benchmark` 示例，再运行 `benchmark <本地测试父目录绝对路径> <发布版 xunshu 绝对路径>`。输出包含首次查询和三个各 30 次的热 IPC 查询场景。Linux CPU 数据是前台重建的子进程树用量；首次查询保留系统页缓存。
