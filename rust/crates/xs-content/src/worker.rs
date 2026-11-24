//! 抽取子进程仅接收已授权字节。父进程同时读写有界管道并负责终止进程组。

use crate::extract::{self, Segment, MAX_INPUT, MAX_WORKER_OUTPUT};
use crate::transport::{Framing, Transport};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::Path,
    process::Command,
    sync::atomic::AtomicBool,
};
use xs_inference::Ocr;

const MAX_EXTENSION: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerOptions {
    pub memory_mib: u32,
    pub timeout_ms: u64,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            memory_mib: 384,
            timeout_ms: 10_000,
        }
    }
}

impl WorkerOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            (64..=8192).contains(&self.memory_mib) && (10..=300_000).contains(&self.timeout_ms),
            "正文抽取内存预算应为 64–8192 MiB，超时应为 10–300000 毫秒"
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Response {
    Complete { segments: Vec<Segment> },
    Error { message: String },
}

#[derive(Debug)]
pub(crate) struct WorkerFault(anyhow::Error);

impl std::fmt::Display for WorkerFault {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{:#}", self.0)
    }
}

impl std::error::Error for WorkerFault {}

pub fn extract_worker(extension: &str, input: impl Read, mut output: impl Write) -> Result<()> {
    extract_worker_with_ocr(extension, input, &mut output, false)
}

pub fn extract_worker_with_ocr(
    extension: &str,
    input: impl Read,
    mut output: impl Write,
    ocr: bool,
) -> Result<()> {
    ensure!(extract::supports(extension, ocr), "不支持的正文格式");
    let mut bytes = Vec::new();
    input.take(MAX_INPUT as u64 + 1).read_to_end(&mut bytes)?;
    let frame = encode_document(extension, &bytes, ocr, &mut None)?;
    output.write_all(&frame)?;
    output.flush()?;
    Ok(())
}

fn encode_document(
    extension: &str,
    bytes: &[u8],
    ocr: bool,
    recognizer: &mut Option<Ocr>,
) -> Result<Vec<u8>> {
    let response = match extract::extract(extension, bytes, ocr, recognizer) {
        Ok(segments) => Response::Complete { segments },
        Err(error) => Response::Error {
            message: error.to_string().chars().take(512).collect(),
        },
    };
    let frame = serde_json::to_vec(&response)?;
    ensure!(frame.len() <= MAX_WORKER_OUTPUT, "正文抽取结果超出传输预算");
    Ok(frame)
}

pub fn extract_worker_serve(mut input: impl Read, mut output: impl Write) -> Result<()> {
    fn write_frame(out: &mut impl Write, frame: &[u8]) -> Result<()> {
        out.write_all(&(frame.len() as u32).to_le_bytes())?;
        out.write_all(frame)?;
        out.flush()?;
        Ok(())
    }
    write_frame(&mut output, b"XS-CONTENT/1")?;
    let mut recognizer = None;
    loop {
        let mut header = [0_u8; 6];
        if input.read(&mut header[..1])? == 0 {
            return Ok(());
        }
        input.read_exact(&mut header[1..])?;
        let flags = header[0];
        let extension_len = usize::from(header[1]);
        let size = u32::from_le_bytes(header[2..].try_into()?) as usize;
        ensure!(
            flags <= 1 && (1..=MAX_EXTENSION).contains(&extension_len) && size <= MAX_INPUT,
            "正文请求帧无效或超过大小上限"
        );
        let mut extension = [0_u8; MAX_EXTENSION];
        input.read_exact(&mut extension[..extension_len])?;
        let extension = std::str::from_utf8(&extension[..extension_len])?;
        ensure!(extract::supports(extension, flags != 0), "不支持的正文格式");
        let mut bytes = vec![0; size];
        input.read_exact(&mut bytes)?;
        let frame = encode_document(extension, &bytes, flags != 0, &mut recognizer)?;
        write_frame(&mut output, &frame)?;
    }
}

pub(crate) struct ExtractClient(Transport);

impl ExtractClient {
    pub(crate) fn start(
        executable: &Path,
        options: &WorkerOptions,
        ocr: bool,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command.arg("--serve");
        let memory = if ocr {
            options.memory_mib.max(768)
        } else {
            options.memory_mib
        };
        let process = Transport::start(
            command,
            memory,
            options.timeout_ms,
            Framing::Binary,
            MAX_WORKER_OUTPUT,
        )?;
        ensure!(
            process.receive(cancel)? == b"XS-CONTENT/1",
            "正文工作进程协议不匹配"
        );
        Ok(Self(process))
    }

    pub(crate) fn extract(
        &mut self,
        extension: &str,
        bytes: Vec<u8>,
        ocr: bool,
        cancel: &AtomicBool,
    ) -> Result<Vec<Segment>> {
        ensure!(
            extension.len() <= MAX_EXTENSION && bytes.len() <= MAX_INPUT,
            "正文请求超过大小上限"
        );
        let mut header = Vec::with_capacity(6 + extension.len());
        header.push(u8::from(ocr));
        header.push(extension.len() as u8);
        header.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        header.extend_from_slice(extension.as_bytes());
        let frame = self.0.request(header, bytes, cancel).map_err(WorkerFault)?;
        let reply: Response = serde_json::from_slice(&frame)
            .context("正文工作进程返回无效结果")
            .map_err(WorkerFault)?;
        match reply {
            Response::Complete { segments } => Ok(segments),
            Response::Error { message } => bail!("{message}"),
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod platform {
    use anyhow::Result;
    use std::{
        io,
        os::unix::process::CommandExt,
        process::{Child, Command},
    };

    pub(crate) struct Containment(libc::pid_t);

    pub(crate) fn spawn(mut command: Command, budget: u64) -> Result<(Child, Containment)> {
        let parent = std::process::id() as libc::pid_t;
        // 只在 fork 后、exec 前执行不分配内存的系统调用。
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: budget as libc::rlim_t,
                    rlim_max: budget as libc::rlim_t,
                };
                if libc::setpgid(0, 0) != 0
                    || libc::setrlimit(libc::RLIMIT_AS, &limit) != 0
                    || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let containment = Containment(child.id() as libc::pid_t);
        Ok((child, containment))
    }

    impl Containment {
        pub(crate) fn kill_all(&self) {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
}

#[cfg(windows)]
pub(crate) mod platform {
    use anyhow::Result;
    use std::{
        io,
        os::windows::{io::AsRawHandle, process::CommandExt},
        process::{Child, Command},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            },
            Threading::{
                ProcessPowerThrottling, SetProcessInformation, BELOW_NORMAL_PRIORITY_CLASS,
                CREATE_NO_WINDOW, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
            },
        },
    };

    pub(crate) struct Containment(HANDLE);

    impl Drop for Containment {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    pub(crate) fn spawn(mut command: Command, budget: u64) -> Result<(Child, Containment)> {
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let containment = Containment(handle);
        let mut limit = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limit.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limit.BasicLimitInformation.ActiveProcessLimit = 1;
        limit.ProcessMemoryLimit = usize::try_from(budget)?;
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limit).cast(),
                std::mem::size_of_val(&limit) as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut child = command
            .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
            .spawn()?;
        // 尽量开启 Windows EcoQoS 低功耗限流（旧版系统不支持时静默忽略，不影响功能）。
        let throttling = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        unsafe {
            let _ = SetProcessInformation(
                child.as_raw_handle(),
                ProcessPowerThrottling,
                ptr::from_ref(&throttling).cast(),
                std::mem::size_of_val(&throttling) as u32,
            );
        }
        // 子进程在读到管道字节前不能解析任何文档，此时完成 Job 绑定。
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle()) } == 0 {
            let error = io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        Ok((child, containment))
    }

    impl Containment {
        pub(crate) fn kill_all(&self) {
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) mod platform {
    use anyhow::{bail, Result};
    use std::process::{Child, Command};

    pub(crate) struct Containment;

    pub(crate) fn spawn(_: Command, _: u64) -> Result<(Child, Containment)> {
        bail!("此平台没有正文抽取进程资源限制")
    }

    impl Containment {
        pub(crate) fn kill_all(&self) {}
    }
}
