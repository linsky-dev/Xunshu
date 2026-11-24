//! 抽取和模型共用的单请求受限管道。取消或超时后丢弃整个会话。

use crate::worker::platform;
use anyhow::{bail, ensure, Context, Result};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(crate) enum Framing {
    Line,
    Binary,
}

struct Request {
    header: Vec<u8>,
    body: Vec<u8>,
}

pub(crate) struct Transport {
    child: Child,
    containment: platform::Containment,
    requests: Option<SyncSender<Request>>,
    responses: Option<Receiver<Result<Vec<u8>>>>,
    tasks: Vec<JoinHandle<()>>,
    timeout: Duration,
}

impl Transport {
    pub(crate) fn start(
        mut command: Command,
        memory_mib: u32,
        timeout_ms: u64,
        framing: Framing,
        max_response: usize,
    ) -> Result<Self> {
        command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let budget = u64::from(memory_mib) * 1024 * 1024;
        let (child, containment) = platform::spawn(command, budget)?;
        let mut process = Self {
            child,
            containment,
            requests: None,
            responses: None,
            tasks: Vec::new(),
            timeout: Duration::from_millis(timeout_ms),
        };
        let mut input = process.child.stdin.take().context("受限进程缺少输入管道")?;
        let output = process
            .child
            .stdout
            .take()
            .context("受限进程缺少输出管道")?;
        let (requests, pipe) = mpsc::sync_channel::<Request>(1);
        let (read, responses) = mpsc::sync_channel(1);
        process.requests = Some(requests);
        process.responses = Some(responses);
        process.tasks.push(
            thread::Builder::new()
                .name("xunshu-worker-input".into())
                .spawn(move || {
                    while let Ok(request) = pipe.recv() {
                        let sent = input
                            .write_all(&request.header)
                            .and_then(|_| input.write_all(&request.body))
                            .and_then(|_| input.flush());
                        if sent.is_err() {
                            break;
                        }
                    }
                })?,
        );
        process.tasks.push(
            thread::Builder::new()
                .name("xunshu-worker-output".into())
                .spawn(move || {
                    let mut output = BufReader::new(output);
                    loop {
                        let frame = read_frame(&mut output, framing, max_response);
                        let bad = frame.is_err();
                        if read.send(frame).is_err() || bad {
                            break;
                        }
                    }
                })?,
        );
        Ok(process)
    }

    pub(crate) fn request(
        &self,
        header: Vec<u8>,
        body: Vec<u8>,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        ensure!(!cancel.load(Ordering::Acquire), "受限任务已取消");
        self.requests
            .as_ref()
            .context("受限进程管道已退出")?
            .try_send(Request { header, body })
            .context("受限进程不可用")?;
        self.receive(cancel)
    }

    pub(crate) fn receive(&self, cancel: &AtomicBool) -> Result<Vec<u8>> {
        let response = self.responses.as_ref().context("受限进程管道已退出")?;
        let start = Instant::now();
        loop {
            ensure!(!cancel.load(Ordering::Acquire), "受限任务已取消");
            ensure!(start.elapsed() < self.timeout, "受限进程超过时限");
            match response.recv_timeout(Duration::from_millis(2)) {
                Ok(frame) => return frame,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("受限进程已退出"),
            }
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // 先断开队列并终止尚未回收的进程组，再等待读写线程退出。
        self.requests = None;
        self.responses = None;
        self.containment.kill_all();
        let _ = self.child.kill();
        let _ = self.child.wait();
        for task in self.tasks.drain(..) {
            let _ = task.join();
        }
    }
}

fn read_frame(input: &mut impl BufRead, framing: Framing, limit: usize) -> Result<Vec<u8>> {
    if let Framing::Binary = framing {
        let mut prefix = [0_u8; 4];
        input.read_exact(&mut prefix)?;
        let size = u32::from_le_bytes(prefix) as usize;
        ensure!(size <= limit, "受限进程返回了过大响应");
        let mut frame = vec![0; size];
        input.read_exact(&mut frame)?;
        return Ok(frame);
    }
    let mut frame = Vec::new();
    loop {
        let bytes = input.fill_buf()?;
        ensure!(!bytes.is_empty(), "受限进程已关闭响应管道");
        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(bytes.len(), |position| position + 1);
        ensure!(frame.len() + count <= limit, "受限进程返回了过大响应");
        frame.extend_from_slice(&bytes[..count]);
        input.consume(count);
        if newline.is_some() {
            return Ok(frame);
        }
    }
}
