//! 由原生界面调用现有本地 CLI，复用 EFU、比较和 Agent 审核的授权边界。
use anyhow::{ensure, Context as _, Result};
use eframe::egui::{self, RichText};
use serde_json::Value;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Edit,
    EfuCompare,
    FolderCompare,
    Agent,
}

#[derive(Clone, Copy)]
enum Task {
    Review,
    Save,
    Compare,
    InspectApproval,
    Approve,
}

struct Review {
    input: String,
    row: usize,
    revision: String,
}

type ToolResult = (Task, Result<Value, String>);

#[derive(Default)]
pub struct Pane {
    pub open: bool,
    mode: Mode,
    input: String,
    other: String,
    row: usize,
    fields: String,
    delete: bool,
    approval: String,
    inspected: Option<String>,
    review: Option<Review>,
    pending: Option<Receiver<ToolResult>>,
    result: Option<(Task, Value)>,
    error: Option<String>,
}

fn absolute(value: &str) -> Result<String> {
    let path = value.trim();
    ensure!(Path::new(path).is_absolute(), "请输入本地绝对路径");
    Ok(path.to_owned())
}

impl Pane {
    fn execute(&mut self, task: Task, dir: &Path, ctx: &egui::Context) -> Result<()> {
        let args = match task {
            Task::Review => vec![
                "efu-inspect".into(),
                absolute(&self.input)?,
                "--row".into(),
                self.row.to_string(),
            ],
            Task::Save => {
                let input = absolute(&self.input)?;
                let output = absolute(&self.other)?;
                let review = self.review.as_ref().context("请先检查当前 EFU 行")?;
                ensure!(
                    review.input == input && review.row == self.row,
                    "输入或行号已变更，请重新检查"
                );
                let mut args = vec![
                    "efu-edit".into(),
                    input,
                    output,
                    "--row".into(),
                    self.row.to_string(),
                    "--revision".into(),
                    review.revision.clone(),
                ];
                if self.delete {
                    args.push("--delete".into());
                } else {
                    for field in self.fields.lines().filter(|line| !line.trim().is_empty()) {
                        ensure!(field.contains('='), "每一行修改须为 列名=值");
                        args.push(format!("--set={field}"));
                    }
                    ensure!(args.len() > 7, "请输入至少一项字段修改");
                }
                args
            }
            Task::Compare => vec![
                match self.mode {
                    Mode::EfuCompare => "efu-compare",
                    _ => "folder-compare",
                }
                .into(),
                absolute(&self.input)?,
                absolute(&self.other)?,
            ],
            Task::InspectApproval | Task::Approve => {
                let id = self.approval.trim();
                ensure!(
                    id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "请输入本地 Agent 提供的 32 位请求编号"
                );
                if matches!(task, Task::Approve) {
                    ensure!(
                        self.inspected.as_deref() == Some(id),
                        "请先核对该授权请求的查询内容"
                    );
                }
                vec![
                    if matches!(task, Task::Approve) {
                        "agent-approve"
                    } else {
                        "agent-inspect"
                    }
                    .into(),
                    id.into(),
                ]
            }
        };
        let (tx, rx) = mpsc::channel();
        let dir = dir.to_path_buf();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = run(dir, &args).map_err(|error| format!("{error:#}"));
            let _ = tx.send((task, result));
            ctx.request_repaint();
        });
        self.pending = Some(rx);
        self.error = None;
        Ok(())
    }

    pub fn show(&mut self, ctx: &egui::Context, dir: &Path) {
        if let Some(receiver) = &self.pending {
            match receiver.try_recv() {
                Ok((task, result)) => {
                    self.pending = None;
                    match result {
                        Ok(result) => {
                            if matches!(task, Task::Review) {
                                self.review = result.get("revision").and_then(Value::as_str).map(
                                    |revision| Review {
                                        input: self.input.trim().into(),
                                        row: self.row,
                                        revision: revision.into(),
                                    },
                                );
                            }
                            if matches!(task, Task::InspectApproval) {
                                self.inspected = result
                                    .get("request_id")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                            }
                            if matches!(task, Task::Approve) {
                                self.inspected = None;
                            }
                            self.result = Some((task, result));
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.error = Some("本地工具进程已中断".into());
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if !self.open {
            return;
        }
        let mut open = true;
        let mut action = None;
        egui::Window::new("本地工具").open(&mut open).default_width(560.0).show(ctx, |ui| {
            ui.add_enabled_ui(self.pending.is_none(), |ui| {
                let previous = self.mode;
                ui.horizontal_wrapped(|ui| {
                    for (mode, title) in [(Mode::Edit,"编辑 EFU"),(Mode::EfuCompare,"比较清单"),(Mode::FolderCompare,"比较目录"),(Mode::Agent,"Agent 授权")] {
                        ui.selectable_value(&mut self.mode, mode, title);
                    }
                });
                if previous != self.mode { self.result = None; self.error = None; }
                match self.mode {
                    Mode::Edit => {
                        ui.label("输入清单的本地绝对路径，并检查要修改的行。另存时须使用新文件路径。");
                        path_field(ui, &mut self.input, "原始清单");
                        self.row = self.row.max(1);
                        ui.horizontal(|ui| {
                            ui.label("数据行号");
                            ui.add(egui::DragValue::new(&mut self.row).range(1..=usize::MAX));
                            if ui.button("检查该行").clicked() { action = Some(Task::Review); }
                        });
                        if self.review.as_ref().is_some_and(|review| review.input == self.input.trim() && review.row == self.row) {
                            path_field(ui, &mut self.other, "新清单路径");
                            ui.checkbox(&mut self.delete, "从新清单删除该行");
                            if !self.delete {
                                ui.add(egui::TextEdit::multiline(&mut self.fields).desired_width(f32::INFINITY).hint_text("每行一项修改，例如 Size=1024"));
                            }
                            if ui.button("另存新清单").clicked() { action = Some(Task::Save); }
                        }
                    }
                    Mode::FolderCompare | Mode::EfuCompare => {
                        ui.label(if self.mode == Mode::FolderCompare { "比较两个已收录目录在当前完整索引中的元数据。" } else { "比较两份本地 EFU 清单。清单内的路径只作为数据。" });
                        path_field(ui, &mut self.input, "对照基线");
                        path_field(ui, &mut self.other, "比较目标");
                        if ui.button("比较").clicked() { action = Some(Task::Compare); }
                    }
                    Mode::Agent => {
                        ui.label("查看本地 Agent 发起的正文请求。核对查询词和条数后，可允许一次。");
                        ui.add(egui::TextEdit::singleline(&mut self.approval).hint_text("本地授权请求编号").desired_width(f32::INFINITY));
                        ui.horizontal(|ui| {
                            if ui.button("核对请求").clicked() { action = Some(Task::InspectApproval); }
                            if ui.add_enabled(self.inspected.as_deref() == Some(self.approval.trim()), egui::Button::new("允许一次")).clicked() { action = Some(Task::Approve); }
                        });
                    }
                }
            });
            if self.pending.is_some() { ui.spinner(); }
            if let Some(error) = &self.error { ui.colored_label(crate::ERROR, format!("操作未完成：{error}")); }
            if let Some((task, result)) = &self.result { render_result(ui, *task, result); }
        });
        self.open = open;
        if let Some(action) = action {
            if let Err(error) = self.execute(action, dir, ctx) {
                self.error = Some(error.to_string());
            }
        }
    }
}

fn path_field(ui: &mut egui::Ui, value: &mut String, label: &str) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
}

fn render_result(ui: &mut egui::Ui, task: Task, result: &Value) {
    ui.separator();
    match task {
        Task::Review => {
            if let Some(fields) = result
                .pointer("/metadata/fields")
                .and_then(Value::as_object)
            {
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for (column, field) in fields {
                            ui.label(format!("{column}：{}", field.as_str().unwrap_or_default()));
                        }
                    });
            }
        }
        Task::Compare => {
            let count = |key| result.get(key).and_then(Value::as_u64).unwrap_or(0);
            ui.label(format!(
                "新增 {} · 删除 {} · 变更 {}",
                count("added"),
                count("removed"),
                count("changed")
            ));
            if result.get("complete") != Some(&Value::Bool(true)) {
                ui.label(RichText::new("列表仅显示首批差异。").small());
            }
            if let Some(changes) = result.get("changes").and_then(Value::as_array) {
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .show(ui, |ui| {
                        for entry in changes {
                            let kind = match entry.get("kind").and_then(Value::as_str) {
                                Some("added") => "新增",
                                Some("removed") => "删除",
                                _ => "变更",
                            };
                            ui.label(format!(
                                "{kind} · {}",
                                entry
                                    .get("path")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                            ));
                        }
                    });
            }
        }
        Task::Save => {
            ui.label("清单已另存为新文件。");
        }
        Task::InspectApproval => {
            ui.label(format!(
                "正文查询：{}",
                result
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ));
            ui.label(format!(
                "最多返回 {} 条。",
                result.get("limit").and_then(Value::as_u64).unwrap_or(0)
            ));
        }
        Task::Approve => {
            ui.label("已允许一次。Agent 需用相同参数继续原请求。");
        }
    }
}

pub(crate) fn run(dir: PathBuf, args: &[String]) -> Result<Value> {
    let cli =
        std::env::current_exe()?.with_file_name(format!("xunshu{}", std::env::consts::EXE_SUFFIX));
    xs_platform::validate_local_path(&cli, false).context("缺少本地命令行组件，请修复安装")?;
    let mut command = Command::new(cli);
    command
        .arg("--data-dir")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("本地工具输出管道未建立")?;
    let stderr = child.stderr.take().context("本地工具诊断管道未建立")?;
    let output = thread::spawn(move || read_limited(stdout, 4 * 1024 * 1024));
    let error = thread::spawn(move || read_limited(stderr, 64 * 1024));
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("本地工具超时，请缩小处理范围后重试");
        }
        thread::sleep(Duration::from_millis(25));
    };
    let output = output
        .join()
        .map_err(|_| anyhow::anyhow!("本地工具读取失败"))??;
    let error = error
        .join()
        .map_err(|_| anyhow::anyhow!("本地工具诊断读取失败"))??;
    ensure!(
        status.success(),
        "{}",
        String::from_utf8_lossy(&error).trim()
    );
    Ok(serde_json::from_slice(&output)?)
}

fn read_limited(input: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    input.take(limit + 1).read_to_end(&mut output)?;
    ensure!(output.len() as u64 <= limit, "本地工具输出超出大小限制");
    Ok(output)
}
