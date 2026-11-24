use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
};
use xs_core::{operations, persistence::Store, Index, SearchOptions, SearchResponse};

const MAX_INPUT: usize = 64 * 1024;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct Request {
    id: u64,
    #[serde(flatten)]
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Hello {
        protocol: u32,
    },
    Search {
        query: String,
        #[serde(default)]
        options: SearchOptions,
        #[serde(default)]
        preview: bool,
    },
    Cancel {
        target: u64,
    },
}

struct Job {
    id: u64,
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<Result<()>>,
}

struct Resident {
    dir: PathBuf,
    store: Store,
    loaded: Option<Index>,
}

impl Resident {
    fn search(
        &mut self,
        query: &str,
        options: &SearchOptions,
        preview: bool,
        cancel: &AtomicBool,
        mut emit: impl FnMut(&str, SearchResponse) -> Result<()>,
    ) -> Result<()> {
        let generation = self
            .store
            .current_generation()?
            .context("还没有文件名索引")?;
        if self
            .loaded
            .as_ref()
            .is_none_or(|index| index.generation() != generation)
        {
            self.loaded = Some(self.store.load()?);
        }
        let index = self.loaded.as_ref().context("文件名索引未加载")?;
        let current = || -> Result<()> {
            ensure!(!cancel.load(Ordering::Relaxed), "查询已取消");
            operations::validate_index_scope(index, &operations::load_settings(&self.dir)?)?;
            ensure!(
                self.store.current_generation()? == Some(index.generation()),
                "索引已更新，请重试查询"
            );
            Ok(())
        };
        current()?;
        if preview && options.offset == 0 {
            let preview_options = SearchOptions {
                time_budget_ms: 8,
                ..options.clone()
            };
            let first = index.search_preview(query, &preview_options, Some(cancel))?;
            current()?;
            if first.complete {
                return emit("final", first);
            }
            emit("preview", first)?;
        }
        let response = index.search(query, options, Some(cancel))?;
        current()?;
        emit("final", response)
    }
}

fn send(output: &Mutex<impl Write>, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_OUTPUT, "服务响应超出长度限制");
    let mut output = output
        .lock()
        .map_err(|_| anyhow::anyhow!("服务输出锁已失效"))?;
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

pub(crate) fn serve(
    dir: &Path,
    mut input: impl BufRead,
    output: impl Write + Send + 'static,
) -> Result<()> {
    let output = Arc::new(Mutex::new(output));
    let resident = Arc::new(Mutex::new(Resident {
        dir: dir.to_owned(),
        store: Store::open(dir.join("index"))?,
        loaded: None,
    }));
    let mut negotiated = false;
    let mut job: Option<Job> = None;
    let mut line = Vec::new();
    let result = (|| -> Result<()> {
        loop {
            line.clear();
            if Read::take(&mut input, MAX_INPUT as u64 + 1).read_until(b'\n', &mut line)? == 0 {
                return Ok(());
            }
            ensure!(line.len() <= MAX_INPUT, "服务请求超出长度限制");
            if job.as_ref().is_some_and(|job| job.handle.is_finished()) {
                finish(job.take())?;
            }
            let request: Request = match serde_json::from_slice(&line) {
                Ok(request) => request,
                Err(error) => {
                    send(
                        &output,
                        &json!({"id":null,"ok":false,"error":format!("服务请求无效：{error}")}),
                    )?;
                    continue;
                }
            };
            let id = request.id;
            match request.operation {
                Operation::Hello { protocol: 2 } => {
                    negotiated = true;
                    send(
                        &output,
                        &json!({"id":id,"ok":true,"protocol":2,"preview":true,"cancel":true,"auto_reload":true}),
                    )?;
                }
                Operation::Hello { .. } => send(
                    &output,
                    &json!({"id":id,"ok":false,"error":"不兼容的协议版本"}),
                )?,
                _ if !negotiated => {
                    send(&output, &json!({"id":id,"ok":false,"error":"请先协商协议"}))?
                }
                Operation::Cancel { target } => {
                    let cancelled =
                        job.as_ref()
                            .filter(|job| job.id == target)
                            .is_some_and(|job| {
                                job.cancel.store(true, Ordering::Relaxed);
                                true
                            });
                    send(&output, &json!({"id":id,"ok":true,"cancelled":cancelled}))?;
                }
                Operation::Search { .. } if job.is_some() => send(
                    &output,
                    &json!({"id":id,"ok":false,"error":"上一次查询尚未结束"}),
                )?,
                Operation::Search {
                    query,
                    options,
                    preview,
                } => {
                    let cancel = Arc::new(AtomicBool::new(false));
                    let task_cancel = cancel.clone();
                    let task_output = output.clone();
                    let task_resident = resident.clone();
                    let handle = thread::Builder::new().name("xunshu-search".into()).spawn(move || {
                        let mut resident = task_resident.lock().map_err(|_| anyhow::anyhow!("服务状态已失效"))?;
                        let result = resident.search(&query, &options, preview, &task_cancel, |phase, response| {
                            send(&task_output, &json!({"id":id,"ok":true,"phase":phase,"result":response}))
                        });
                        if let Err(error) = result {
                            send(&task_output, &json!({"id":id,"ok":false,"phase":"final","error":error.to_string()}))?;
                        }
                        Ok(())
                    })?;
                    job = Some(Job { id, cancel, handle });
                }
            }
        }
    })();
    if let Some(job) = &job {
        job.cancel.store(true, Ordering::Relaxed);
    }
    let joined = finish(job);
    result.and(joined)
}

fn finish(job: Option<Job>) -> Result<()> {
    if let Some(job) = job {
        job.handle
            .join()
            .map_err(|_| anyhow::anyhow!("搜索线程异常退出"))??;
    }
    Ok(())
}
