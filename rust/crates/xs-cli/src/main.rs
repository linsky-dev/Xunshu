use anyhow::{bail, ensure, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::Instant,
};
use xs_core::{operations, persistence::Store, FileRecord, Index, IndexBuilder, SearchOptions};
use xs_platform::{Scope, Settings, SetupState};

mod doctor;
mod efu;
mod mcp;
mod service_v2;
mod watch;

#[derive(Parser)]
#[command(
    name = "xunshu",
    version,
    about = "寻枢 · Rust 原生离线文件搜索（显式范围，无联网）"
)]
struct App {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// 创建收录配置；不会扫描，已有配置需显式 --replace
    Init {
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
        #[arg(long)]
        ext: Option<String>,
        #[arg(long)]
        directories_only: bool,
        #[arg(long)]
        defer: bool,
        #[arg(long)]
        replace: bool,
        #[cfg(windows)]
        #[arg(long, conflicts_with_all = ["ext", "directories_only", "defer"])]
        fast_volume: bool,
    },
    /// 显示本地卷能力；不会扫描卷
    Volumes,
    /// 显示设置与实际索引代次
    Status {
        /// 同时校验已安装的模型资源摘要，不加载推理库
        #[arg(long)]
        verify_assets: bool,
    },
    /// 只读检查本机配置和索引，默认输出脱敏诊断摘要
    Doctor {
        /// 以 JSON 输出脱敏结果
        #[arg(long)]
        json: bool,
        /// 将同一份脱敏 JSON 另存为新文件
        #[arg(long)]
        export: Option<PathBuf>,
    },
    /// 只扫描当前已保存范围，Ctrl-C 取消，失败保留原索引
    Scan,
    /// 监听当前文件名范围，在变更、通知丢失或范围调整时校准索引
    Watch,
    /// 从当前授权范围的历史完整代次恢复文件名索引
    Recover,
    /// 独立设置正文收录范围，保留文件名范围
    ContentConfigure {
        #[arg(
            long = "root",
            required_unless_present = "disable",
            conflicts_with = "disable"
        )]
        roots: Vec<PathBuf>,
        #[arg(long)]
        disable: bool,
        #[arg(long, conflicts_with = "disable")]
        ocr: bool,
        #[arg(long, conflicts_with = "disable")]
        semantic: bool,
    },
    /// 由独立工作进程抽取正文并提交索引，Ctrl-C 取消
    ContentRebuild,
    /// 持久登记已知文件变更；不带路径表示要求完整校准
    ContentEnqueue { paths: Vec<PathBuf> },
    /// 处理持久变更队列；删除、目录或不确定事件回退到完整校准
    ContentApply,
    /// 前台监听已授权正文范围；退出后下次启动先完整校准
    ContentWatch,
    /// 删除未发布的正文检查点，不影响正式索引与用户文件
    ContentDiscardCheckpoint,
    /// 清除未发布检查点及变更队列，不影响正式索引
    ContentDiscardPending,
    /// 搜索已经发布的正文索引
    ContentSearch {
        query: String,
        #[arg(long, default_value_t = 40)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        semantic: bool,
    },
    #[command(hide = true)]
    ContentSelfTest { query: String },
    /// 为桌面提供继承式正文查询进程
    ContentServe,
    /// 启动本地 MCP 标准输入输出服务
    Mcp,
    /// 查看需要逐次授权的 Agent 请求
    AgentInspect { request_id: String },
    /// 授权一条已核对的 Agent 请求，仅限当前会话一次
    AgentApprove { request_id: String },
    /// 搜索现有索引；查询语法错误显式返回
    Search {
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
    },
    /// EFU 目录作为不可打开的离线元数据导入，保留其他来源
    ImportEfu {
        file: PathBuf,
        #[arg(long, default_value = "catalog")]
        name: String,
    },
    /// 只读导入旧 schema4；替换上次 schema4 导入的全部来源，不改旧数据库
    ImportLegacy { file: PathBuf },
    /// 导出当前有效范围内全部元数据
    ExportEfu { file: PathBuf },
    /// 查看 EFU 行号及当前文件版本，供另存编辑时复核
    EfuInspect {
        file: PathBuf,
        #[arg(long)]
        row: usize,
    },
    /// 修改或删除一行并另存为新的 EFU 文件
    EfuEdit {
        input: PathBuf,
        output: PathBuf,
        #[arg(long)]
        row: usize,
        #[arg(long)]
        revision: String,
        #[arg(
            long = "set",
            required_unless_present = "delete",
            conflicts_with = "delete"
        )]
        fields: Vec<String>,
        #[arg(long)]
        delete: bool,
    },
    /// 比较两个离线清单中的新增、删除与属性差异
    EfuCompare {
        left: PathBuf,
        right: PathBuf,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// 比较两个已授权目录在当前完整文件名索引中的内容
    FolderCompare {
        left: PathBuf,
        right: PathBuf,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// 迁移旧设置到独立新文件，旧文件保持不变
    MigrateSettings { file: PathBuf },
    /// 将 M1 的设置和索引只读复制到当前全新数据目录
    MigrateM1 { source: PathBuf },
    /// 纯本地 JSON-lines 子进程服务，不开放 TCP/HTTP
    Serve,
    /// 可取消并自动重载的本地文件名搜索服务，协议版本 2
    ServeV2,
    /// 工作区合成基准，不访问文件名对应路径
    Benchmark {
        #[arg(long, default_value_t = 100000)]
        entries: usize,
        #[arg(long, default_value_t = 20)]
        iterations: usize,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    #[command(hide = true)]
    BenchmarkRead {
        snapshot: PathBuf,
        #[arg(long, default_value_t = 20)]
        iterations: usize,
    },
}
fn absolute(path: PathBuf) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}
fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{}", json!({"ok":false,"error":format!("{error:#}")}));
        std::process::exit(1)
    }
}
fn run() -> Result<()> {
    xs_runtime::enforce_offline()?;
    let app = App::parse();
    let dir = absolute(match app.data_dir {
        Some(p) => p,
        None => operations::default_data_dir()?,
    })?;
    let value = match app.command {
        Command::Init {
            roots,
            ext,
            directories_only,
            defer,
            replace,
            #[cfg(windows)]
            fast_volume,
        } => {
            ensure!(
                !(ext.is_some() && directories_only),
                "--ext 与 --directories-only 不能同时使用"
            );
            let path = operations::settings_path(&dir);
            ensure!(
                replace || !path.exists(),
                "设置已存在，修改时需 --replace；旧索引会在重新扫描前停用"
            );
            let extensions = if directories_only {
                Some(Vec::new())
            } else {
                ext.map(|s| {
                    s.split([',', ';'])
                        .map(str::trim)
                        .map(str::to_owned)
                        .collect()
                })
            };
            let mut settings = Settings::default();
            for root in roots {
                let mut scope = Scope::new(absolute(root)?.to_string_lossy().into_owned());
                scope.extensions = extensions.clone();
                settings.roots.push(scope);
            }
            #[cfg(windows)]
            if fast_volume {
                ensure!(
                    settings.roots.len() == 1
                        && Path::new(&settings.roots[0].path).parent().is_none(),
                    "极速模式需要显式选择一个本地卷根目录"
                );
                ensure!(
                    !xs_platform::local_path_within(&dir, Path::new(&settings.roots[0].path))?,
                    "极速模式的数据目录必须位于所选卷之外"
                );
                settings.mode = xs_platform::SpeedMode::OnDemandElevated;
            }
            if defer {
                settings.defer_setup()
            } else {
                ensure!(
                    !settings.roots.is_empty(),
                    "请通过 --root 明确添加目录，或 --defer 稍后设置"
                );
                settings.setup_state = SetupState::Completed;
            }
            let commit = operations::save_settings(&dir, &settings)?;
            json!({"ok":true,"settings":settings,"commit":commit,"message":"范围已保存；运行 scan 后生效。正文、OCR、语义默认关闭。"})
        }
        Command::Volumes => serde_json::to_value(xs_platform::volumes()?)?,
        Command::Doctor { json, export } => {
            let report = doctor::report(&dir);
            if let Some(path) = export {
                doctor::export(&absolute(path)?, &report)?;
            }
            if json {
                return print_json(&report);
            }
            doctor::print_summary(&report);
            return Ok(());
        }
        Command::Status { verify_assets } => {
            let settings = operations::load_settings(&dir)?;
            let store = Store::open(dir.join("index"))?;
            let index = if store.has_snapshot()? {
                match store.load() {
                    Ok(index) => {
                        json!({"entries":index.len(),"generation":index.generation(),"mapped":index.is_mapped(),"snapshot_bytes":index.storage_bytes(),"coverage_complete":index.coverage_complete(),"scope_matches":operations::validate_index_scope(&index,&settings).is_ok()})
                    }
                    Err(e) => json!({"error":format!("{e:#}")}),
                }
            } else {
                Value::Null
            };
            let model = if verify_assets {
                match xs_inference::Assets::installed() {
                    Ok(_) => json!({"state":"verified"}),
                    Err(error) => json!({"state":"repair_required","message":error.to_string()}),
                }
            } else {
                json!({"state":"checked_on_use"})
            };
            json!({"version":env!("CARGO_PKG_VERSION"),"runtime":"Rust","data_dir":dir,"settings":settings,"index":index,"network":false,"model_resources":model,"capabilities":{"filename_search":true,"stdio_ipc":true,"stdio_ipc_v2":true,"mmap_snapshot":true,"native_ui":true,"directory_watch":true,"content_search":true,"mcp_content":true,"mft_usn":cfg!(windows),"fast_service":false,"ocr":true,"semantic":true,"legacy_query_full_parity":false}})
        }
        Command::Scan => {
            let cancel = Arc::new(AtomicBool::new(false));
            let c = cancel.clone();
            ctrlc::set_handler(move || c.store(true, std::sync::atomic::Ordering::Relaxed))?;
            let mut last = Instant::now();
            let outcome = operations::rebuild(&dir, &cancel, |p| {
                if last.elapsed().as_millis() > 500 {
                    eprintln!(
                        "已查看 {} 项，收录 {} 项，错误 {} 项",
                        p.visited, p.indexed, p.errors
                    );
                    last = Instant::now();
                }
            })?;
            let value = serde_json::to_value(&outcome)?;
            if !outcome.published {
                print_json(&value)?;
                bail!("扫描未完整完成，已有索引保持不变")
            }
            value
        }
        Command::Watch => {
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            ctrlc::set_handler(move || signal.store(true, std::sync::atomic::Ordering::Relaxed))?;
            watch::run(&dir, &cancel, |event| println!("{event}"))?;
            return Ok(());
        }
        Command::Recover => {
            let settings = operations::load_settings(&dir)?;
            let store = Store::open(dir.join("index"))?;
            let publication = store.recover(&settings.scope_token()?)?;
            json!({"ok":true,"generation":store.current_generation()?,"durable":publication.durable,"warning":publication.warning})
        }
        Command::ContentConfigure {
            roots,
            disable,
            ocr,
            semantic,
        } => {
            let mut settings = operations::load_settings(&dir)?;
            let previous_scope = settings.content_scope_token()?;
            if !disable {
                settings.content_roots = roots
                    .into_iter()
                    .map(|path| {
                        let path = absolute(path)?;
                        xs_platform::validate_local_path(&path, false)?;
                        ensure!(path.is_dir(), "正文范围必须是本地目录");
                        Ok(Scope::new(
                            path.to_str()
                                .ok_or_else(|| anyhow::anyhow!("目录编码不受支持"))?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            settings.content_enabled = !disable;
            settings.ocr_enabled = ocr;
            settings.semantic_enabled = semantic;
            let commit = operations::save_settings(&dir, &settings)?;
            let checkpoint_warning = if previous_scope != settings.content_scope_token()? {
                xs_content::discard_pending_work(&dir).err().map(|error| format!("授权已保存，但待处理任务未清理：{error:#}；任务结束后执行 content-discard-pending"))
            } else {
                None
            };
            json!({"ok":true,"content_enabled":settings.content_enabled,"commit":commit,"checkpoint_warning":checkpoint_warning})
        }
        Command::ContentDiscardPending => {
            xs_content::discard_pending_work(&dir)?;
            json!({"ok":true})
        }
        Command::ContentDiscardCheckpoint => {
            xs_content::discard_pending_checkpoint(&dir)?;
            json!({"ok":true})
        }
        Command::ContentEnqueue { paths } => {
            let paths = paths
                .into_iter()
                .map(absolute)
                .collect::<Result<Vec<_>>>()?;
            let queued = xs_content::enqueue_changes(&dir, &paths)?;
            json!({"ok":true,"queued_paths":queued})
        }
        Command::ContentWatch => {
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            ctrlc::set_handler(move || signal.store(true, std::sync::atomic::Ordering::Relaxed))?;
            let worker = std::env::current_exe()?
                .with_file_name(format!("xs-content-worker{}", std::env::consts::EXE_SUFFIX));
            xs_content::watch_changes(&dir, &worker, &cancel, |report| {
                if let Ok(line) = serde_json::to_string(report) {
                    println!("{line}");
                }
            })?;
            json!({"ok":true,"stopped":true})
        }
        command @ (Command::ContentRebuild | Command::ContentApply) => {
            let cancel = Arc::new(AtomicBool::new(false));
            let signal = cancel.clone();
            ctrlc::set_handler(move || signal.store(true, std::sync::atomic::Ordering::Relaxed))?;
            let worker = std::env::current_exe()?
                .with_file_name(format!("xs-content-worker{}", std::env::consts::EXE_SUFFIX));
            xs_platform::validate_local_path(&worker, false)?;
            let report = if matches!(command, Command::ContentApply) {
                xs_content::apply_queued_changes(&dir, &worker, &cancel)?
            } else {
                xs_content::rebuild(&dir, &worker, &cancel)?
            };
            serde_json::to_value(report)?
        }
        Command::ContentSearch {
            query,
            limit,
            offset,
            semantic,
        } => {
            let mut reader = xs_content::ContentReader::new(&dir);
            let options = xs_content::SearchOptions { limit, offset };
            let cancel = AtomicBool::new(false);
            let response = if semantic {
                reader.search_mode(&query, xs_content::SearchMode::Semantic, &options, &cancel)?
            } else {
                reader.search(&query, &options, &cancel)?
            };
            xs_content::validate_response(&dir, &response)?;
            serde_json::to_value(response)?
        }
        Command::ContentSelfTest { query } => {
            let mut reader = xs_content::ContentReader::new(&dir);
            let response = reader.search(
                &query,
                &xs_content::SearchOptions::default(),
                &AtomicBool::new(false),
            )?;
            xs_content::validate_response(&dir, &response)?;
            ensure!(
                response
                    .hits
                    .iter()
                    .any(|hit| matches!(hit.location, xs_content::Location::PageRegion { .. })),
                "扫描 PDF 未通过图片文字检索自检"
            );
            json!({"ok":true,"scanned_pdf_ocr":true,"hits":response.hits.len()})
        }
        Command::ContentServe => {
            xs_content::serve(
                &dir,
                std::io::BufReader::new(std::io::stdin()),
                std::io::stdout(),
            )?;
            return Ok(());
        }
        Command::Mcp => {
            mcp::serve(&dir, std::io::stdin().lock(), std::io::stdout().lock())?;
            return Ok(());
        }
        Command::AgentInspect { request_id } => mcp::inspect(&dir, &request_id)?,
        Command::AgentApprove { request_id } => mcp::approve(&dir, &request_id)?,
        Command::Search {
            query,
            limit,
            offset,
            timeout_ms,
        } => {
            let index = operations::load_index(&dir)?;
            let response = index.search(
                &query,
                &SearchOptions {
                    limit,
                    offset,
                    time_budget_ms: timeout_ms,
                },
                None,
            )?;
            operations::validate_index_scope(&index, &operations::load_settings(&dir)?)?;
            serde_json::to_value(response)?
        }
        Command::ImportEfu { file, name } => {
            ensure!(
                !name.is_empty()
                    && name.len() < 128
                    && !name.contains('\0')
                    && !name.starts_with("schema4:"),
                "目录名无效或使用了保留的 schema4: 前缀"
            );
            let store = Store::open(dir.join("index"))?;
            let next = next_generation(&store)?;
            let source = format!("offline:{name}");
            let imported = xs_core::interchange::import_efu(&absolute(file)?, &source, next)?;
            merge_catalog(&store, imported, &source, false)?
        }
        Command::ImportLegacy { file } => {
            let store = Store::open(dir.join("index"))?;
            let next = next_generation(&store)?;
            let imported = xs_core::interchange::import_schema4(&absolute(file)?, next)?;
            merge_catalog(&store, imported, "offline:schema4:", true)?
        }
        Command::ExportEfu { file } => {
            let index = operations::load_index(&dir)?;
            let path = absolute(file)?;
            let commit = xs_platform::write_local_new(&path, |file| {
                xs_core::interchange::export_efu(&index, file).map_err(std::io::Error::other)
            })?;
            json!({"exported":index.len(),"path":path,"commit":commit})
        }
        Command::EfuInspect { file, row } => efu::inspect(&absolute(file)?, row)?,
        Command::EfuEdit {
            input,
            output,
            row,
            revision,
            fields,
            delete,
        } => efu::edit(
            &absolute(input)?,
            &absolute(output)?,
            row,
            &revision,
            &fields,
            delete,
        )?,
        Command::EfuCompare { left, right, limit } => {
            efu::compare(&absolute(left)?, &absolute(right)?, limit)?
        }
        Command::FolderCompare { left, right, limit } => {
            efu::compare_folders(&dir, &absolute(left)?, &absolute(right)?, limit)?
        }
        Command::MigrateSettings { file } => {
            xs_platform::create_local_dir_all(&dir)?;
            let (settings, commit) =
                Settings::migrate_legacy(absolute(file)?, operations::settings_path(&dir))?;
            json!({"migrated":true,"settings":settings,"commit":commit,"original_modified":false})
        }
        Command::MigrateM1 { source } => {
            serde_json::to_value(operations::migrate_m1(&absolute(source)?, &dir)?)?
        }
        Command::Serve => {
            xs_core::service::serve(&dir, std::io::stdin().lock(), std::io::stdout().lock())?;
            return Ok(());
        }
        Command::ServeV2 => {
            service_v2::serve(&dir, std::io::stdin().lock(), std::io::stdout())?;
            return Ok(());
        }
        Command::Benchmark {
            entries,
            iterations,
            output,
        } => {
            let result = benchmark(&dir, entries, iterations)?;
            if let Some(output) = output {
                xs_core::persistence::atomic_write(
                    &absolute(output)?,
                    &serde_json::to_vec_pretty(&result)?,
                )?;
            }
            result
        }
        Command::BenchmarkRead {
            snapshot,
            iterations,
        } => benchmark_read(&snapshot, iterations)?,
    };
    print_json(&value)
}
fn next_generation(store: &Store) -> Result<u64> {
    if let Some(generation) = store.current_generation()? {
        generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("generation exhausted"))
    } else {
        Ok(1)
    }
}
fn merge_catalog(store: &Store, imported: Index, replacement: &str, prefix: bool) -> Result<Value> {
    let count = imported.len();
    let old = if store.has_snapshot()? {
        Some(store.load()?)
    } else {
        None
    };
    let token = old
        .as_ref()
        .map(|i| i.scope_token().to_owned())
        .unwrap_or_default();
    let mut b = IndexBuilder::new(token);
    let mut deleted = Vec::new();
    if let Some(old) = &old {
        b.set_coverage_complete(old.coverage_complete());
        b.set_journal_checkpoint(old.journal_checkpoint().cloned());
        for id in 0..old.len() as u32 {
            let row = old.record(id)?;
            let replace = if prefix {
                row.source.starts_with(replacement)
            } else {
                row.source == replacement
            };
            if replace {
                deleted.push(id);
            }
        }
    }
    for id in 0..imported.len() as u32 {
        b.add_from_index(&imported, id)?;
    }
    let additions = b.finish(imported.generation())?;
    let (publication, total) = if let Some(old) = old {
        (
            store.publish_delta(old.generation(), &deleted, &additions)?,
            old.len() - deleted.len() + count,
        )
    } else {
        (store.publish(&additions)?, count)
    };
    Ok(
        json!({"imported":count,"total":total,"generation":additions.generation(),"replaced_source":replacement,"replaced_source_prefix":prefix,"durable":publication.durable,"durability_warning":publication.warning,"mode":"metadata_only","opened_catalog_paths":0}),
    )
}
fn memory() -> Value {
    #[cfg(target_os = "linux")]
    {
        let content = fs::read_to_string("/proc/self/status").unwrap_or_default();
        let get = |name: &str| {
            content
                .lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u64>().ok())
        };
        json!({"rss_kib":get("VmRSS:"),"peak_rss_kib":get("VmHWM:")})
    }
    #[cfg(not(target_os = "linux"))]
    {
        json!({"rss_kib":null,"peak_rss_kib":null})
    }
}
fn benchmark(dir: &Path, entries: usize, iterations: usize) -> Result<Value> {
    ensure!(
        (1..=10_000_000).contains(&entries),
        "entries must be 1..10000000"
    );
    ensure!((1..=100).contains(&iterations), "iterations must be 1..100");
    let store = Store::open(dir.join("benchmark"))?;
    let generation = next_generation(&store)?;
    let start = Instant::now();
    let mut b = IndexBuilder::default();
    for i in 0..entries {
        let name = match i % 5 {
            0 => format!("干部任免报告_{i:07}.docx"),
            1 => format!("project_budget_final_{i:07}.pdf"),
            2 => format!("photo_archive_{i:07}.jpg"),
            3 => format!("readme_source_{i:07}.rs"),
            _ => format!("meeting_notes_{i:07}.txt"),
        };
        b.add(FileRecord {
            path: format!(
                "/synthetic/department_{:03}/year_{}/{}",
                (i / 1000) % 1000,
                2020 + (i % 7),
                name
            ),
            size: (i as u64 * 7919) % 10_000_000,
            modified: 1700000000 + (i % 86400) as i64,
            is_dir: false,
            source: "offline:benchmark".into(),
        })?;
    }
    let index = b.finish(generation)?;
    let build_ms = start.elapsed().as_secs_f64() * 1000.;
    let start = Instant::now();
    let publication = store.publish(&index)?;
    let snapshot = publication.path;
    let persist_ms = start.elapsed().as_secs_f64() * 1000.;
    let size = fs::metadata(&snapshot)?.len();
    let build_memory = memory();
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["benchmark-read", "--iterations", &iterations.to_string()])
        .arg(&snapshot)
        .output()?;
    ensure!(
        output.status.success(),
        "benchmark query child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let read: Value = serde_json::from_slice(&output.stdout)?;
    Ok(
        json!({"version":1,"dataset":"deterministic synthetic mixed Chinese/English filenames; no real files scanned","entries":entries,"build_ms":build_ms,"persist_ms":persist_ms,"snapshot_bytes":size,"bytes_per_entry":size as f64/entries as f64,"build_process_memory":build_memory,"read_process":read,"platform":std::env::consts::OS,"arch":std::env::consts::ARCH,"target_metrics_are_not_acceptance":true}),
    )
}
fn percentile(values: &[f64], fraction: f64) -> f64 {
    values[((values.len() as f64 * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(values.len() - 1)]
}
fn benchmark_read(snapshot: &Path, iterations: usize) -> Result<Value> {
    ensure!((1..=100).contains(&iterations), "iterations must be 1..100");
    let before = memory();
    let start = Instant::now();
    let index = Index::open_snapshot(snapshot)?;
    let load_ms = start.elapsed().as_secs_f64() * 1000.;
    let after_load = memory();
    let queries = [
        "never_present_xyz",
        "0001234",
        "预算决算不存在",
        "干部",
        "project_budget_final_0000121",
        "name:report ext:pdf",
        "readme ext:rs",
        "budget | meeting",
    ];
    let mut rows = Vec::new();
    let mut first_screen = Vec::new();
    for query in queries {
        let mut preview_times = Vec::with_capacity(iterations);
        let mut partial_samples = 0;
        for _ in 0..iterations {
            let result = index.search_preview(
                query,
                &SearchOptions {
                    time_budget_ms: 8,
                    ..Default::default()
                },
                None,
            )?;
            preview_times.push(result.elapsed_ms);
            partial_samples += usize::from(!result.complete);
        }
        preview_times.sort_by(f64::total_cmp);
        first_screen.push(json!({"query":query,"p50_ms":percentile(&preview_times,0.5),"p95_ms":percentile(&preview_times,0.95),"p99_ms":percentile(&preview_times,0.99),"partial_samples":partial_samples}));
        let mut times = Vec::new();
        let mut summary = None;
        for _ in 0..iterations {
            let response = index.search(
                query,
                &SearchOptions {
                    time_budget_ms: 60000,
                    ..Default::default()
                },
                None,
            )?;
            times.push(response.elapsed_ms);
            summary = Some(response);
        }
        times.sort_by(f64::total_cmp);
        let s = summary.unwrap();
        rows.push(json!({"query":query,"p50_ms":percentile(&times,0.5),"p95_ms":percentile(&times,0.95),"p99_ms":percentile(&times,0.99),"total":s.total,"candidates":s.candidate_count,"examined":s.examined,"complete":s.complete}));
    }
    Ok(
        json!({"mapped":index.is_mapped(),"load_ms":load_ms,"load_note":"separate fresh process; OS page cache not forcibly evicted; includes CRC and structural validation","iterations":iterations,"baseline_memory":before,"after_load_memory":after_load,"after_queries_memory":memory(),"queries":rows,"first_screen_core_queries":first_screen,"first_screen_note":"core only; excludes input, IPC and presentation"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use xs_core::CatalogMetadata;

    #[test]
    fn empty_import_replaces_its_identity_and_preserves_unrelated_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut builder = IndexBuilder::default();
        for source in [
            "offline:catalog",
            "offline:other",
            "offline:schema4:deleted",
            "offline:schema4:retained",
        ] {
            builder
                .add_with_metadata(
                    FileRecord {
                        path: format!("/virtual/{source}.txt"),
                        size: 1,
                        modified: 0,
                        is_dir: false,
                        source: source.into(),
                    },
                    Some(CatalogMetadata {
                        fields: BTreeMap::from([("Tag".into(), source.into())]),
                    }),
                )
                .unwrap();
        }
        store.publish(&builder.finish(1).unwrap()).unwrap();
        let response = merge_catalog(
            &store,
            IndexBuilder::default().finish(2).unwrap(),
            "offline:catalog",
            false,
        )
        .unwrap();
        assert_eq!(response["imported"], 0);
        assert_eq!(response["total"], 3);
        assert!(!store
            .load()
            .unwrap()
            .records()
            .any(|r| r.source == "offline:catalog"));

        let mut replacement = IndexBuilder::default();
        replacement
            .add(FileRecord {
                path: "/virtual/new.txt".into(),
                size: 2,
                modified: 1,
                is_dir: false,
                source: "offline:schema4:retained".into(),
            })
            .unwrap();
        merge_catalog(
            &store,
            replacement.finish(3).unwrap(),
            "offline:schema4:",
            true,
        )
        .unwrap();
        let index = store.load().unwrap();
        assert_eq!(index.len(), 2);
        assert!(!index
            .records()
            .any(|r| r.source == "offline:schema4:deleted"));
        let other = index
            .records()
            .enumerate()
            .find(|(_, r)| r.source == "offline:other")
            .unwrap()
            .0;
        assert_eq!(
            index.record_metadata(other as u32).unwrap().unwrap().fields["Tag"],
            "offline:other"
        );
    }
}
