# xs-platform: local filesystem and scope API

This crate provides strict settings, explicit local scopes, pinned file access, and cancellable metadata scanning. It opens no network listener and does not download resources.

- `pub type Result<T> = std::result::Result<T, PlatformError>`; errors implement std::error::Error and convert to anyhow through `?`.
- `pub struct FileEntry { pub path:String, pub size:u64, pub modified:i64, pub is_dir:bool, pub source:String }` serde Serialize/Deserialize, Clone.
- `pub struct Scope { pub path:String, pub extensions:Option<Vec<String>>, pub smart_excludes:bool }`; `Scope::new(path:impl Into<String>)->Self` defaults null/all + smart excludes.
- `pub struct Settings { pub version:u32, pub setup_state:SetupState, pub reminder_count:u8, pub mode:SpeedMode, pub roots:Vec<Scope>, pub content_roots:Vec<Scope>, pub content_enabled:bool, pub ocr_enabled:bool, pub semantic_enabled:bool }`. Implements Default, serde. Default empty roots, Standard, all enhancements disabled.
- `SetupState::{NotStarted,Deferred,Completed}`, `SpeedMode::{Standard,FastService,OnDemandElevated}`; serde snake_case.
- `Settings::load(path:impl AsRef<Path>)->Result<Self>`, `save(&self,path:impl AsRef<Path>)->Result<LocalWriteOutcome>`, `validate(&self)->Result<()>`, `scope_token(&self)->Result<String>`; token is deterministic JSON of sorted/normalized name scopes + mode, content settings do not alter name token.
- `Settings::migrate_legacy(old_path:impl AsRef<Path>,new_path:impl AsRef<Path>)->Result<(Self,LocalWriteOutcome)>` creates a distinct new file, never edits old file/SQLite; `load` also reads legacy settings in memory if setup_wizard_completed is present.
- `Settings::{should_remind(&self)->bool,record_reminder(&mut self)->bool,defer_setup(&mut self),complete_setup(&mut self),reopen_setup(&mut self)}`. Reopen doesn't reset reminder counter.
- `pub fn validate_local_path(path:&Path,allow_missing:bool)->Result<()>`; absolute explicit paths only; existing components reject links/reparse/network. `pub fn create_local_dir_all(path:&Path)->Result<()>` safe local directory creation.
- `pub struct ScanProgress { pub visited:u64, pub indexed:u64, pub errors:u64, pub skipped_links:u64, pub current_path:Option<String> }`.
- `pub struct ScanError { pub path:String, pub kind:String, pub message:String }` serde and Display.
- `pub struct ScanReport { pub entries:Vec<FileEntry>, pub errors:Vec<ScanError>, pub skipped_links:u64, pub skipped_non_utf8:u64, pub skipped_mounts:u64, pub skipped_excluded:u64, pub skipped_special:u64, pub cancelled:bool, pub complete:bool }`.
- `pub fn scan(settings:&Settings,cancel:&AtomicBool,progress:impl FnMut(ScanProgress))->Result<ScanReport>`.
- `pub fn scan_with_exclusions(settings:&Settings,cancel:&AtomicBool,progress:impl FnMut(ScanProgress),excluded_paths:&[PathBuf])->Result<ScanReport>`; exclude data directory plus descendants before opening/indexing regardless of smart_excludes. Must pass data dir here.
- `pub fn volumes()->Result<Vec<VolumeCapability>>` and `pub fn suggested_roots()->Vec<Scope>` (never scan automatically).
- `pub struct VolumeCapability { pub root:String, pub label:String, pub filesystem:String, pub local:bool, pub fixed:bool, pub standard_supported:bool, pub mft_supported:bool, pub reason:String }`.

**Complete is false for every scan IO error or cancellation.** Read/link/mount policy guards are not user-toggleable. No scanner opens file contents.

## 本轮平台接口

- `FileEntry.revision`、`created` 和 `attributes` 为可选字段，旧 JSON 仍可读。`FileRevision::at_path` 使用固定对象读取元数据，`FileRevision::from_file` 从调用者已校验的句柄读取。Windows 使用 128 位文件标识和变更时间，Linux 使用设备、inode 及纳秒修改/变更时间。硬链接共享对象身份，路径独立记录。无法取得身份时，Windows 标准扫描仍可收录文件名，需身份的调用者应拒绝使用缓存。Windows 属性位不套用到 Linux 权限值。
- `scan_with_batches` 逐批交付记录，`ScanReport.entries` 为空，保留取消和完整性语义。`inspect_scoped_entry` 对通知路径重新检查范围、类型、硬排除和对象边界。缺失或不收录返回 `None`，无法验证范围或路径时返回错误并要求校准。`Settings::content_scope_token` 独立绑定正文范围及增强功能开关。
- `ScopeWatcher::poll` 返回空闲、变更或需要校准。`take_changed_paths` 取出本批至多 512 条路径提示。Linux 使用固定目录描述符注册 inotify，目录拓扑变化后要求重订阅。Windows 使用 IOCP 和重叠目录通知。过载、根对象变更、读取错误或订阅失效时，调用方保留已有索引并重新订阅后校准。
- `OnDemandElevated` 只接受显式授权的单个本地固定 NTFS/ReFS 卷根，类型筛选和智能排除须关闭，硬排除和应用数据目录须位于卷外。原生采集分页枚举 MFT/USN，通过文件标识定位对象，按硬链接分路径收录，并排除卷的保留元记录、受保护的卷元数据和重解析点。只有日志在采集期间一致且扫描完整，才返回 `ScanReport.native_checkpoint`。ReFS 明确报告原生接口不支持时使用目录扫描并给出原因。`FastService` 暂保留显式错误。卷能力是候选标记，权限和可用日志只在实际授权扫描时验证。
- `read_scoped_journal_changes` 从已提交的卷身份、日志标识及游标读出有界路径提示。日志缺口、无法定位的父目录、目录或硬链接拓扑变化要求重建并保留旧游标。普通文件改名包含旧名和新名。消费端须重新核验路径，把变更和新游标同一代次提交。
- `write_local_replace` 使用固定父目录流式写入并原子替换。`remove_local_file` 用于持有写锁的应用自有目录内清理单个代次文件，拒绝目录和链接。Linux 使用固定父描述符的 `unlinkat`，Windows 通过打开的文件句柄设置删除状态。
- 迁移可通过 `create_local_dir_new` 建立不覆盖的暂存目录，关闭内容句柄后用 `publish_local_dir_new` 向同一父目录下未占用的目标发布。Linux 使用 `renameat2(RENAME_NOREPLACE)`，Windows 对暂存目录句柄提交重命名并返回持久性未确认提示。调用者须控制暂存名称及父目录的写入生命周期。

Windows NTFS 原生验收可从已准备 Rust 和 Hyper-V 工具的管理员 PowerShell 执行 `verify-native-vhd.ps1`。脚本只初始化本次在工作区创建并核对过的文件型虚拟盘，测试结束后卸载并清理磁盘镜像。

## Local access contracts

- `FileEntry.source` is always `"local"`; source root identity stays in settings policy, not record source.
- `Settings::allows_filename(&self, path: &Path, is_dir: bool) -> Result<bool>` is an alias of `allows_name`; both enforce the same longest-root, whitelist and smart exclusions.
- `open_local_read(&Path) -> Result<File>` reads a verified, pinned regular file.
- `create_local_new(&Path) -> Result<File>` creates a read/write regular file under a pinned local parent, refusing every existing object.
- `open_or_create_local_lock(&Path) -> Result<File>` opens or creates a read/write regular lock file without truncating it; links, reparse points and special files are refused.
- `write_local_new(&Path, impl FnOnce(&mut File)->std::io::Result<()>) -> Result<LocalWriteOutcome>` streams to a private sibling, syncs its data, then commits create-only. Failed writes clean only the temporary name under the pinned parent; callers must not retain file clones or independently publish the temporary file.
- `atomic_write_local(&Path, &[u8], overwrite:bool) -> Result<LocalWriteOutcome>` uses the same pinned-parent stream and commit boundary.
- `LocalWriteOutcome::{Durable, Committed { warning:String }}` implements Debug/Clone/Serialize/Deserialize and provides `is_durable()->bool` and `warning()->Option<&str>`. **Both variants mean the destination was committed.** Only pre-commit failures return Err. A directory fsync failure after rename/link returns Ok(Committed), never deletes the committed target, and never implies that old contents remain current.
- `validate_local_path` alone is a point-in-time check; do not follow it with unprotected path-based open/create/unlink operations. Use the file APIs above.
- Linux overlay is excluded from the data filesystem allowlist. Existing overlay directories are permitted only for O_PATH ancestor traversal to a separately mounted local target; no fstat, readdir, reads, writes, or mkdir on overlay.
- Synthetic validation uses `/tmp/xunshu-rust-validation-036c199d3807` (tmpfs), per root coordination; no user roots are scanned.
- `Settings::save`/migration create their parents through `create_local_dir_all`.

## Verification update

- `TMPDIR=/tmp/xunshu-rust-validation-036c199d3807 cargo test -p xs-platform --offline`: **33 passed** (5 unit tests + 28 regression contracts), 0 failed, 0 ignored. Only task-owned synthetic fixtures were used and removed.
- New regression cases cover a directory swapped after its descriptor was pinned, lock files without truncation, FIFO/link refusal, failed-stream cleanup preserving an outside decoy, pre-commit ENOSPC, and post-commit directory fsync EIO for both replacement and create-only publication.
- Windows `cargo check -p xs-platform --target x86_64-pc-windows-gnu` passed after removing `Iter::eq_by`; no Windows runtime testing has occurred.
- Windows scan now checks cancellation before opening directory streams, reports missing child handles as errors, and stops a failed directory stream. Removed the unused Guard.path field.
- Windows scope comparison now uses native `CompareStringOrdinal` UTF-16 case comparison, avoiding ASCII-only case aliasing of hard-excluded data directories; `Win32_Globalization` is the added API feature. `cargo check -p xs-platform --tests --target x86_64-pc-windows-gnu` completed successfully with no warnings. This verifies compilation only, not Windows runtime behavior.

Whole-product platform status is recorded in rust/VALIDATION.md. These crate checks do not establish Windows or UOS runtime acceptance.
