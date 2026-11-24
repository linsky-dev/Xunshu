//! 用独立合成文件测量正文重建及常驻进程往返，运行后自动清理本次样本。

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::Instant,
};
use xs_platform::{Scope, Settings};

struct Resident {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Resident {
    fn start(cli: &Path, data: &Path) -> Result<Self> {
        let mut child = Command::new(cli)
            .arg("--data-dir")
            .arg(data)
            .arg("content-serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().context("缺少测试进程输入管道")?;
        let output = BufReader::new(child.stdout.take().context("缺少测试进程输出管道")?);
        Ok(Self {
            child,
            input,
            output,
        })
    }

    fn request(&mut self, value: &Value) -> Result<Value> {
        serde_json::to_writer(&mut self.input, value)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let mut line = String::new();
        ensure!(
            self.output.read_line(&mut line)? > 0,
            "常驻正文进程提前退出"
        );
        let reply: Value = serde_json::from_str(&line)?;
        ensure!(reply["id"] == value["id"], "基准返回了错位响应");
        ensure!(reply["ok"] == true, "正文查询失败：{}", reply["error"]);
        Ok(reply["result"].clone())
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const TEXT: [&str; 4] = [
    "项目预算草案。季度成本、预算计划和采购记录。",
    "项目进展与会议纪要。确认收录和后续安排。",
    "Budget review and quarterly procurement tracking.",
    "Budget meeting notes and annual vendor evaluation.",
];

#[cfg(target_os = "linux")]
fn reaped_child_cpu_seconds() -> Option<f64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
        return None;
    }
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0;
    Some(seconds(usage.ru_utime) + seconds(usage.ru_stime))
}

#[cfg(not(target_os = "linux"))]
fn reaped_child_cpu_seconds() -> Option<f64> {
    None
}

fn run(parent: &Path, cli: &Path, documents: usize) -> Result<Value> {
    ensure!(
        (4..=100_000).contains(&documents) && documents.is_multiple_of(4),
        "文档数须为 4–100000 的 4 的倍数"
    );
    xs_platform::validate_local_path(parent, false)?;
    xs_platform::validate_local_path(cli, false)?;
    let fixture = tempfile::Builder::new()
        .prefix("xunshu-benchmark-content-")
        .tempdir_in(parent)?;
    let data = fixture.path().join("data");
    let corpus = fixture.path().join("documents");
    fs::create_dir(&corpus)?;
    for index in 0..documents {
        let shard = corpus.join(format!("{:03}", index / 1000));
        if index.is_multiple_of(1000) {
            fs::create_dir(&shard)?;
        }
        let mut body = TEXT[index % 4].to_owned();
        if index + 1 == documents {
            body.push_str("\n唯一测试标识");
        }
        fs::write(shard.join(format!("test-{index:06}.txt")), body)?;
    }
    Settings {
        content_enabled: true,
        content_roots: vec![Scope::new(corpus.to_str().context("测试目录编码无效")?)],
        ..Settings::default()
    }
    .save(data.join("settings.json"))?;

    let before = reaped_child_cpu_seconds();
    let started = Instant::now();
    let rebuild = Command::new(cli)
        .arg("--data-dir")
        .arg(&data)
        .arg("content-rebuild")
        .output()?;
    let rebuild_ms = started.elapsed().as_secs_f64() * 1000.0;
    let cpu = reaped_child_cpu_seconds()
        .zip(before)
        .map(|(end, start)| end - start);
    if !rebuild.status.success() {
        bail!(
            "合成正文重建失败：{}",
            String::from_utf8_lossy(&rebuild.stderr)
        );
    }
    let report: Value = serde_json::from_slice(&rebuild.stdout)?;
    ensure!(
        report["complete"] == true && report["documents"].as_u64() == Some(documents as u64),
        "合成正文重建缺少文件：{report}"
    );
    let mut ipc = Resident::start(cli, &data)?;
    let _ = ipc.request(&json!({"id": 1, "method": "status"}))?;
    let cases = [
        ("唯一测试标识", 1),
        ("项目", documents / 2),
        ("budget", documents / 2),
    ];
    let query = |id: u64, term: &str| {
        json!({
            "id": id, "method": "search", "query": term, "options": {"limit": 40, "offset": 0}
        })
    };
    let started = Instant::now();
    let first = ipc.request(&query(2, cases[0].0))?;
    let first_ms = started.elapsed().as_secs_f64() * 1000.0;
    ensure!(first["indexed_total"] == cases[0].1, "首次查询命中数量错误");
    let mut results = Vec::new();
    let mut id = 3;
    for (term, expected) in cases {
        let mut samples = Vec::new();
        for _ in 0..30 {
            let started = Instant::now();
            let result = ipc.request(&query(id, term))?;
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            ensure!(
                result["indexed_total"] == expected && result["omitted_stale"] == 0,
                "查询命中数量错误"
            );
            id += 1;
        }
        samples.sort_unstable_by(f64::total_cmp);
        results.push(json!({
            "query": term, "hits": expected, "samples": samples.len(), "p95_ms": samples[28],
            "max_ms": samples[29], "passes_100ms_p95": samples[28] <= 100.0,
        }));
    }
    let _ = ipc.request(&json!({"id": id, "method": "shutdown"}))?;
    let storage_bytes = || -> Result<u64> {
        let mut total = 0;
        for name in ["content.idx", "content.base-a.idx", "content.base-b.idx"] {
            match fs::metadata(data.join(name)) {
                Ok(m) => total += m.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(total)
    };
    let initial_catalog_bytes = storage_bytes()?;
    let command = |args: &[&str], paths: &[PathBuf]| -> Result<Value> {
        let result = Command::new(cli)
            .arg("--data-dir")
            .arg(&data)
            .args(args)
            .args(paths)
            .output()?;
        ensure!(
            result.status.success(),
            "基准命令失败：{}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(serde_json::from_slice(&result.stdout)?)
    };
    let mut increments = Vec::new();
    for (batch, count) in [1, 100, 1000]
        .into_iter()
        .filter(|n| *n < documents)
        .enumerate()
    {
        let marker = format!("incrementmarker{batch}");
        let mut paths = Vec::new();
        for index in 0..count {
            let path = corpus
                .join(format!("{:03}", index / 1000))
                .join(format!("test-{index:06}.txt"));
            fs::write(&path, format!("{}\n{marker}", TEXT[index % 4]))?;
            paths.push(path);
        }
        let started = Instant::now();
        command(&["content-enqueue"], &paths)?;
        let report = command(&["content-apply"], &[])?;
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        ensure!(
            report["complete"] == true
                && report["targeted_update"] == true
                && report["updated_documents"] == count,
            "增量更新失败：{report}"
        );
        let mut verify = Resident::start(cli, &data)?;
        let found = verify.request(&query(1, &marker))?;
        ensure!(
            found["indexed_total"] == count && found["omitted_stale"] == 0,
            "增量命中错误"
        );
        if batch > 0 {
            let stale = verify.request(&query(2, &format!("incrementmarker{}", batch - 1)))?;
            ensure!(stale["indexed_total"] == 0, "旧正文命中未清理");
        }
        ensure!(
            verify.request(&query(3, cases[0].0))?["indexed_total"] == 1,
            "未变化文档丢失"
        );
        let _ = verify.request(&json!({"id": 4, "method": "shutdown"}))?;
        increments.push(json!({"changed_documents": count, "elapsed_ms": elapsed, "physical_catalog_bytes": storage_bytes()?, "publication_bytes_written": report["publication_bytes_written"], "delta_blocks": report["delta_blocks"], "base_rewritten": report["base_rewritten"], "verified": true}));
    }
    Ok(json!({
        "documents": documents,
        "platform": std::env::consts::OS,
        "catalog_bytes": initial_catalog_bytes,
        "incremental_updates": increments,
        "foreground_rebuild_ms": rebuild_ms,
        "reaped_process_tree_cpu_seconds": cpu,
        "foreground_rebuild_one_core_cpu_percent": cpu.map(|seconds| 100_000.0 * seconds / rebuild_ms),
        "first_ipc_search_ms": first_ms,
        "first_load_os_page_cache_flushed": false,
        "warm_ipc": results,
    }))
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let parent = PathBuf::from(
        args.next()
            .context("用法：benchmark <合成样本父目录> <xunshu 程序> [文档数]")?,
    );
    let cli = PathBuf::from(args.next().context("需要 xunshu 程序绝对路径")?);
    let count = args
        .next()
        .map(|value| value.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(100_000);
    ensure!(args.next().is_none(), "基准参数过多");
    println!(
        "{}",
        serde_json::to_string_pretty(&run(&parent, &cli, count)?)?
    );
    Ok(())
}
