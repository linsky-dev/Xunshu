//! 正文常驻查询协议通过继承的本地管道工作；接收端可打断正在运行的查询。

use crate::{ContentReader, SearchMode, SearchOptions};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    thread,
};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
type Running = Mutex<Option<(u64, Arc<AtomicBool>)>>;

#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Status {
        id: u64,
    },
    Search {
        id: u64,
        query: String,
        #[serde(default)]
        mode: SearchMode,
        #[serde(default)]
        options: SearchOptions,
    },
    Cancel {
        id: u64,
        request_id: u64,
    },
    Release {
        id: u64,
    },
    Shutdown {
        id: u64,
    },
}

enum Work {
    Search {
        id: u64,
        query: String,
        mode: SearchMode,
        options: SearchOptions,
        cancel: Arc<AtomicBool>,
    },
    Release {
        id: u64,
    },
}

fn lock<T>(state: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    state.lock().map_err(|_| anyhow!("正文进程状态不可用"))
}

fn response(output: &Mutex<impl Write>, id: Option<u64>, result: Result<Value>) -> Result<()> {
    let value = match result {
        Ok(value) => json!({"id": id, "ok": true, "result": value}),
        Err(error) => json!({"id": id, "ok": false, "error": error.to_string()}),
    };
    let mut frame = serde_json::to_vec(&value)?;
    if frame.len() > MAX_RESPONSE {
        frame = serde_json::to_vec(&json!({
            "id": id,
            "ok": false,
            "error": "正文响应过大，请缩小分页大小"
        }))?;
    }
    let mut output = lock(output)?;
    output.write_all(&frame)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn read_frame(input: &mut impl BufRead) -> Result<Option<Result<Request>>> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let bytes = input.fill_buf()?;
        if bytes.is_empty() {
            if line.is_empty() && !oversized {
                return Ok(None);
            }
            break;
        }
        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let size = newline.map_or(bytes.len(), |offset| offset + 1);
        if !oversized && line.len() + size <= MAX_REQUEST {
            line.extend_from_slice(&bytes[..size]);
        } else {
            oversized = true;
        }
        input.consume(size);
        if newline.is_some() {
            break;
        }
    }
    if oversized {
        return Ok(Some(Err(anyhow!("正文请求超过 64 KiB"))));
    }
    Ok(Some(
        serde_json::from_slice(&line).map_err(|e| anyhow!("无效的正文请求：{e}")),
    ))
}

fn work(
    queue: &SyncSender<Work>,
    running: &Running,
    output: &Mutex<impl Write>,
    request: Work,
    id: u64,
    flag: Arc<AtomicBool>,
) -> Result<()> {
    let mut state = lock(running)?;
    if state.is_some() {
        return response(output, Some(id), Err(anyhow!("已有正文任务，等待或先取消")));
    }
    *state = Some((id, flag));
    if queue.try_send(request).is_err() {
        *state = None;
        return response(output, Some(id), Err(anyhow!("正文查询进程不可用")));
    }
    Ok(())
}

pub fn serve(dir: &Path, mut input: impl BufRead, output: impl Write + Send) -> Result<()> {
    let output = Mutex::new(output);
    let running = Running::default();
    thread::scope(|scope| -> Result<()> {
        let (queue, jobs) = mpsc::sync_channel(1);
        let worker = scope.spawn(|| {
            let jobs = jobs;
            let mut reader = ContentReader::new(dir);
            while let Ok(job) = jobs.recv() {
                let (id, result) = match job {
                    Work::Search {
                        id,
                        query,
                        mode,
                        options,
                        cancel,
                    } => {
                        let result = reader
                            .search_shared(&query, mode, &options, cancel)
                            .and_then(|result| Ok(serde_json::to_value(result)?));
                        (id, result)
                    }
                    Work::Release { id } => {
                        reader.release();
                        (id, Ok(json!({"released": true})))
                    }
                };
                // 统一锁序确保下一请求在先前响应发出后才可入队。
                let mut state = lock(&running)?;
                *state = None;
                response(&output, Some(id), result)?;
            }
            Ok::<_, anyhow::Error>(())
        });

        let dispatch = (|| -> Result<Option<u64>> {
            while let Some(frame) = read_frame(&mut input)? {
                let request = match frame {
                    Ok(request) => request,
                    Err(error) => {
                        response(&output, None, Err(error))?;
                        continue;
                    }
                };
                match request {
                    Request::Status { id } => response(
                        &output,
                        Some(id),
                        Ok(json!({
                            "protocol": 1,
                            "transport": "inherited_stdio",
                            "capabilities": ["lexical", "cancel", "release"]
                        })),
                    )?,
                    Request::Search {
                        id,
                        query,
                        mode,
                        options,
                    } => {
                        let flag = Arc::new(AtomicBool::new(false));
                        work(
                            &queue,
                            &running,
                            &output,
                            Work::Search {
                                id,
                                query,
                                mode,
                                options,
                                cancel: Arc::clone(&flag),
                            },
                            id,
                            flag,
                        )?;
                    }
                    Request::Cancel { id, request_id } => {
                        let state = lock(&running)?;
                        let cancelled = state.as_ref().is_some_and(|(active, flag)| {
                            if *active == request_id {
                                flag.store(true, Ordering::Release);
                                true
                            } else {
                                false
                            }
                        });
                        response(&output, Some(id), Ok(json!({"cancelled": cancelled})))?;
                    }
                    Request::Release { id } => work(
                        &queue,
                        &running,
                        &output,
                        Work::Release { id },
                        id,
                        Arc::new(AtomicBool::new(false)),
                    )?,
                    Request::Shutdown { id } => return Ok(Some(id)),
                }
            }
            Ok(None)
        })();
        if let Some((_, flag)) = lock(&running)?.as_ref() {
            flag.store(true, Ordering::Release);
        }
        drop(queue);
        worker
            .join()
            .map_err(|_| anyhow!("正文查询工作线程异常"))??;
        if let Some(id) = dispatch? {
            response(&output, Some(id), Ok(json!({"shutdown": true})))?;
        }
        Ok(())
    })
}
