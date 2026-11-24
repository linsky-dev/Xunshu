//! 正文工作台通过独立 CLI 查询进程访问索引，抽取另由受限 worker 完成。
use anyhow::{ensure, Context as _, Result};
use eframe::egui::{self, Color32, FontId, RichText, TextFormat};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use xs_content::{BuildReport, ContentHit, Location, SearchMode, SearchResponse};
use xs_platform::{Scope, Settings};

use crate::{desktop, ACCENT, BACKGROUND, BORDER, ERROR, INK, MUTED};

const PAGE_SIZE: usize = 30;
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;
const VALIDATION_INTERVAL: Duration = Duration::from_millis(500);
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(2);

struct Validation {
    id: u64,
    deadline: Instant,
    receiver: Receiver<bool>,
}

impl Validation {
    fn poll(&self, now: Instant) -> Option<bool> {
        if now >= self.deadline {
            return Some(false);
        }
        match self.receiver.try_recv() {
            Ok(valid) => Some(valid),
            Err(TryRecvError::Disconnected) => Some(false),
            Err(TryRecvError::Empty) => None,
        }
    }
}

fn sibling(name: &str) -> Result<PathBuf> {
    let path =
        std::env::current_exe()?.with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    xs_platform::validate_local_path(&path, false).context("正文组件未找到，请修复安装")?;
    Ok(path)
}

enum Reply {
    Ready,
    Search(u64, Result<Box<SearchResponse>, String>),
    Failed(String),
}

struct Client {
    child: Child,
    commands: SyncSender<Value>,
    replies: Receiver<Reply>,
    ready: bool,
}

impl Client {
    fn spawn(profile: &Path, ctx: &egui::Context) -> Result<Self> {
        let mut command = Command::new(sibling("xunshu")?);
        command
            .arg("--data-dir")
            .arg(profile)
            .arg("content-serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().context("无法启动正文查询进程")?;
        let mut writer = child.stdin.take().context("正文控制管道未建立")?;
        let reader = child.stdout.take().context("正文结果管道未建立")?;
        let (commands, command_rx) = mpsc::sync_channel(8);
        let (replies_tx, replies) = mpsc::sync_channel(8);
        commands.try_send(json!({"id":1,"method":"status"}))?;
        thread::spawn(move || {
            while let Ok(command) = command_rx.recv() {
                if desktop::send(&mut writer, &command).is_err() {
                    break;
                }
            }
        });
        let ctx = ctx.clone();
        let profile = profile.to_path_buf();
        thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            while let Ok(Some(value)) = read_reply(&mut reader) {
                let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
                let ok = value.get("ok") == Some(&Value::Bool(true));
                let reply = match id {
                    0 => continue,
                    1 if ok
                        && value.pointer("/result/protocol").and_then(Value::as_u64) == Some(1) =>
                    {
                        Reply::Ready
                    }
                    1 => Reply::Failed("正文查询组件版本不兼容，请修复安装".into()),
                    _ if !ok => Reply::Search(
                        id,
                        Err(value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("正文查询失败")
                            .to_owned()),
                    ),
                    _ => Reply::Search(
                        id,
                        (|| -> Result<Box<SearchResponse>> {
                            let response: SearchResponse = serde_json::from_value(
                                value.get("result").cloned().unwrap_or(Value::Null),
                            )?;
                            xs_content::validate_response(&profile, &response)?;
                            Ok(Box::new(response))
                        })()
                        .map_err(|error| format!("{error:#}")),
                    ),
                };
                if replies_tx.send(reply).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            let _ = replies_tx.send(Reply::Failed("正文查询进程已断开，可重新搜索".into()));
            ctx.request_repaint();
        });
        Ok(Self {
            child,
            commands,
            replies,
            ready: false,
        })
    }

    fn send(&self, command: Value) -> Result<()> {
        self.commands
            .try_send(command)
            .context("正文查询进程暂不可用")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_reply(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    if reader
        .take((MAX_REPLY_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)?
        == 0
    {
        return Ok(None);
    }
    ensure!(
        bytes.len() <= MAX_REPLY_BYTES && bytes.last() == Some(&b'\n'),
        "正文结果超出大小限制或未结束"
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}

struct Form {
    enabled: bool,
    roots: String,
    ocr: bool,
    semantic: bool,
}

impl Form {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            enabled: settings.content_enabled,
            roots: settings
                .content_roots
                .iter()
                .map(|root| root.path.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            ocr: settings.ocr_enabled,
            semantic: settings.semantic_enabled,
        }
    }

    fn settings(&self, base: &Settings) -> Result<Settings> {
        let mut settings = base.clone();
        settings.content_enabled = self.enabled;
        settings.ocr_enabled = self.enabled && self.ocr;
        settings.semantic_enabled = self.enabled && self.semantic;
        if self.enabled {
            settings.content_roots = self
                .roots
                .lines()
                .map(str::trim)
                .filter(|root| !root.is_empty())
                .map(|path| {
                    ensure!(
                        Path::new(path).is_absolute(),
                        "正文收录目录必须使用本地绝对路径"
                    );
                    let previous = base.content_roots.iter().find(|root| root.path == path);
                    Ok(previous
                        .cloned()
                        .unwrap_or_else(|| Scope::new(path.to_owned())))
                })
                .collect::<Result<Vec<_>>>()?;
            ensure!(
                !settings.content_roots.is_empty(),
                "请先添加至少一个正文目录"
            );
        }
        settings.validate()?;
        Ok(settings)
    }
}

struct SearchRequest {
    id: u64,
    query: String,
    offset: usize,
    mode: SearchMode,
}

type BuildResult = Result<BuildReport, String>;

#[derive(Default)]
pub struct Pane {
    client: Option<Client>,
    form: Option<Form>,
    query: String,
    last_query: String,
    offset: usize,
    id: u64,
    active: Option<u64>,
    pending: Option<SearchRequest>,
    results: Option<Arc<SearchResponse>>,
    selected: usize,
    notice: Option<String>,
    error: Option<String>,
    build: Option<(Arc<AtomicBool>, Receiver<BuildResult>)>,
    watch_status: Option<Receiver<(bool, String)>>,
    next_validation: Option<Instant>,
    validation: Option<Validation>,
    mode: SearchMode,
    last_mode: SearchMode,
    model_check: Option<Receiver<bool>>,
    models_ready: Option<bool>,
}

impl Pane {
    pub fn start(&mut self, query: String, profile: &Path, ctx: &egui::Context) {
        self.query = query;
        desktop::limit_text(&mut self.query, 1024);
        if let Err(error) = self.search(profile, ctx, 0) {
            self.error = Some(format!("无法查询正文：{error:#}"));
        }
    }

    pub fn first_preview(&self) -> Option<&str> {
        self.results
            .as_ref()?
            .hits
            .first()
            .map(|hit| hit.snippet.as_str())
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn building(&self) -> bool {
        self.build.is_some()
    }

    pub fn release(&mut self) {
        self.client = None;
        self.active = None;
        self.pending = None;
        self.results = None;
        self.validation = None;
        self.next_validation = None;
    }

    pub fn cancel_build(&self) {
        if let Some((cancel, _)) = &self.build {
            cancel.store(true, Ordering::Release);
        }
    }

    fn launch(&mut self) -> Result<()> {
        let Some(client) = &self.client else {
            return Ok(());
        };
        if !client.ready || self.active.is_some() {
            return Ok(());
        }
        if let Some(request) = self.pending.take() {
            client.send(
                json!({"id":request.id,"method":"search","query":request.query,"mode":request.mode,
                "options":{"offset":request.offset,"limit":PAGE_SIZE}}),
            )?;
            self.active = Some(request.id);
            self.offset = request.offset;
            self.last_query = request.query;
            self.last_mode = request.mode;
        }
        Ok(())
    }

    fn search(&mut self, profile: &Path, ctx: &egui::Context, offset: usize) -> Result<()> {
        let query = self.query.trim();
        ensure!(!query.is_empty(), "请输入正文关键词");
        if self.client.is_none() {
            self.client = Some(Client::spawn(profile, ctx)?);
        }
        self.id = self.id.wrapping_add(1).max(2);
        self.results = None;
        self.validation = None;
        self.next_validation = None;
        self.error = None;
        self.selected = 0;
        self.pending = Some(SearchRequest {
            id: self.id,
            query: query.into(),
            offset,
            mode: self.mode,
        });
        if let Some(active) = self.active {
            self.client
                .as_ref()
                .unwrap()
                .send(json!({"id":0,"method":"cancel","request_id":active}))?;
        }
        self.launch()
    }

    fn rebuild(&mut self, profile: &Path, ctx: &egui::Context) -> Result<()> {
        let worker = sibling("xs-content-worker")?;
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let (tx, rx) = mpsc::channel();
        let profile = profile.to_owned();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let report =
                xs_content::rebuild(&profile, &worker, &stop).map_err(|error| format!("{error:#}"));
            let _ = tx.send(report);
            ctx.request_repaint();
        });
        self.release();
        self.build = Some((cancel, rx));
        self.notice = Some("正在收录已授权目录中的正文…".into());
        self.error = None;
        Ok(())
    }

    fn watch(&mut self, profile: &Path, ctx: &egui::Context) -> Result<()> {
        ensure!(self.build.is_none(), "正文任务已在运行");
        let worker = sibling("xs-content-worker")?;
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let (tx, rx) = mpsc::channel();
        let (status_tx, status_rx) = mpsc::sync_channel(4);
        let profile = profile.to_owned();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let outcome = xs_content::watch_changes(&profile, &worker, &stop, |report| {
                if report.cancelled {
                    return;
                }
                let ok = report.published
                    && report.durable
                    && report.complete
                    && report.request_warning.is_none();
                let status = if ok {
                    format!(
                        "实时监控中 · {} · 更新 {} · 删除 {} · 代次 {}",
                        if report.scope_reconciled {
                            "全范围校准"
                        } else {
                            "局部更新"
                        },
                        report.updated_documents,
                        report.removed_documents,
                        report.generation.unwrap_or_default()
                    )
                } else {
                    format!(
                        "实时收录暂未完成（{} 项失败），将重试。{}",
                        report.failed,
                        report
                            .failures
                            .first()
                            .map(|f| f.error.as_str())
                            .unwrap_or("请检查持久化及访问权限")
                    )
                };
                let _ = status_tx.try_send((ok, status));
                ctx.request_repaint();
            })
            .map_err(|error| format!("实时收录已停止：{error:#}"));
            let _ = tx.send(outcome.map(|()| BuildReport {
                cancelled: true,
                ..BuildReport::default()
            }));
            ctx.request_repaint();
        });
        self.build = Some((cancel, rx));
        self.watch_status = Some(status_rx);
        self.notice = Some("正在启动正文实时监控和完整校准…".into());
        self.error = None;
        Ok(())
    }

    pub fn update(&mut self, profile: &Path, ctx: &egui::Context) {
        if let Some(check) = &self.model_check {
            match check.try_recv() {
                Ok(ready) => {
                    self.models_ready = Some(ready);
                    self.model_check = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.models_ready = Some(false);
                    self.model_check = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let mut failure = None;
        if let Some(client) = &mut self.client {
            for reply in client.replies.try_iter() {
                match reply {
                    Reply::Ready => client.ready = true,
                    Reply::Failed(error) => failure = Some(error),
                    Reply::Search(id, result) if self.active == Some(id) => {
                        self.active = None;
                        if id == self.id {
                            match result {
                                Ok(response) => self.results = Some(Arc::from(response)),
                                Err(error) => self.error = Some(error),
                            }
                        }
                    }
                    Reply::Search(..) => {}
                }
            }
        }
        if let Err(error) = self.launch() {
            failure = Some(error.to_string());
        }
        if let Some(error) = failure {
            self.release();
            self.error = Some(error);
        }
        if let Some(status) = &self.watch_status {
            for (ok, message) in status.try_iter() {
                if ok {
                    self.notice = Some(message);
                    self.error = None;
                } else {
                    self.notice = None;
                    self.error = Some(message);
                }
            }
        }
        if let Some((_, build)) = &self.build {
            let result = match build.try_recv() {
                Ok(report) => Some(report),
                Err(TryRecvError::Disconnected) => Some(Err("正文收录任务异常退出".into())),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.build = None;
                let was_watching = self.watch_status.take().is_some();
                match result {
                    Ok(report) if report.cancelled => {
                        self.notice = Some(
                            if was_watching {
                                "正文实时监控已停止，已提交的索引保留"
                            } else {
                                "已取消正文收录，原索引保持不变"
                            }
                            .into(),
                        )
                    }
                    Ok(report) if report.published => {
                        self.notice = Some(if report.unchanged_snapshot {
                            format!(
                                "正文索引已是最新 · {} 份文档 · 代次 {} 保持不变",
                                report.documents,
                                report.generation.unwrap_or_default()
                            )
                        } else {
                            format!(
                                "正文差分索引已发布 · {} 份文档 · 重新抽取 {} · 删除 {} · 复用 {}{}",
                                report.documents,
                                report.updated_documents,
                                report.removed_documents,
                                report.reused_documents,
                                if report.resumed_checkpoint { " · 已从检查点恢复" } else { "" }
                            )
                        })
                    }
                    Ok(report) => {
                        let detail = report.failures.first().map_or_else(
                            || "请检查授权范围、扫描错误或文件占用后重试".to_owned(),
                            |first| format!("首个失败文件：{} · {}", first.path, first.error),
                        );
                        self.notice = None;
                        self.error = Some(format!(
                            "正文索引未发布（{} 项失败），原索引保持不变。{}{}",
                            report.failed,
                            detail,
                            if report.checkpoints_written > 0 {
                                " · 已保存成功文档的检查点，可修复后重试"
                            } else {
                                ""
                            }
                        ));
                    }
                    Err(error) => {
                        self.notice = None;
                        self.error = Some(error);
                    }
                }
            }
        }
        if let Some(validation) = &self.validation {
            if let Some(valid) = validation.poll(Instant::now()) {
                let id = validation.id;
                self.validation = None;
                if id == self.id && !valid {
                    self.results = None;
                    self.error = Some("正文授权、文件或索引复核失败或超时，请重新搜索".into());
                }
            }
        }
        if let Some(response) = self.results.as_ref().filter(|_| {
            self.validation.is_none()
                && self
                    .next_validation
                    .is_none_or(|next| Instant::now() >= next)
        }) {
            let response = response.clone();
            let profile = profile.to_owned();
            let ctx = ctx.clone();
            let id = self.id;
            let (tx, rx) = mpsc::channel();
            let now = Instant::now();
            self.validation = Some(Validation {
                id,
                deadline: now + VALIDATION_TIMEOUT,
                receiver: rx,
            });
            self.next_validation = Some(now + VALIDATION_INTERVAL);
            thread::spawn(move || {
                let _ = tx.send(xs_content::validate_response(&profile, &response).is_ok());
                ctx.request_repaint();
            });
        }
        if self.results.is_some() {
            ctx.request_repaint_after(VALIDATION_INTERVAL);
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        profile: &Path,
        settings: &Settings,
        saving: bool,
    ) -> Option<Settings> {
        let mut change = None;
        if !settings.semantic_enabled
            && matches!(self.mode, SearchMode::Semantic | SearchMode::Hybrid)
        {
            self.mode = SearchMode::Lexical;
            self.release();
        }
        ui.label(RichText::new("正文搜索").size(26.0).strong().color(INK));
        ui.label(
            RichText::new("只搜索单独授权的正文目录。结果包含可核对的段落、页码或表格位置。")
                .color(MUTED),
        );
        ui.add_space(14.0);
        let mut start_build = false;
        let mut start_watch = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !saving && self.build.is_none(),
                    egui::Button::new("正文收录范围"),
                )
                .clicked()
            {
                self.form = Some(Form::from_settings(settings));
                let ctx = ui.ctx().clone();
                let (tx, rx) = mpsc::channel();
                self.model_check = Some(rx);
                self.models_ready = None;
                thread::spawn(move || {
                    let _ = tx.send(xs_inference::Assets::installed().is_ok());
                    ctx.request_repaint();
                });
            }
            if self.build.is_some() {
                if ui
                    .button(if self.watch_status.is_some() {
                        "停止实时监控"
                    } else {
                        "取消收录"
                    })
                    .clicked()
                {
                    self.cancel_build();
                }
                ui.spinner();
            } else if ui
                .add_enabled(
                    !saving && settings.content_enabled,
                    egui::Button::new("更新正文索引"),
                )
                .clicked()
            {
                start_build = true;
            }
            if ui
                .add_enabled(
                    !saving && settings.content_enabled && self.build.is_none(),
                    egui::Button::new("启动实时收录"),
                )
                .on_hover_text("仅在当前应用运行时监听已授权范围；停止后下次启动先完整校准")
                .clicked()
            {
                start_watch = true;
            }
        });
        if !saving
            && self.build.is_none()
            && ui
                .button("清除未发布任务")
                .on_hover_text("只清理未完成的正文收录进度和变更队列，正式索引和用户文件不受影响")
                .clicked()
        {
            match xs_content::discard_pending_work(profile) {
                Ok(()) => {
                    self.error = None;
                    self.notice = Some("未发布任务已清除".into());
                }
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        if start_watch {
            if let Err(error) = self.watch(profile, ui.ctx()) {
                self.error = Some(error.to_string());
            }
        }
        if start_build {
            if let Err(error) = self.rebuild(profile, ui.ctx()) {
                self.error = Some(error.to_string());
            }
        }
        if let Some(form) = &mut self.form {
            let mut dismiss = false;
            egui::Frame::new()
                .fill(BACKGROUND)
                .stroke(egui::Stroke::new(1.0_f32, BORDER))
                .inner_margin(14)
                .show(ui, |ui| {
                    ui.checkbox(&mut form.enabled, "启用正文收录");
                    if form.enabled {
                        ui.label("输入本地绝对路径，每行一个目录。");
                        ui.add(
                            egui::TextEdit::multiline(&mut form.roots)
                                .desired_width(f32::INFINITY)
                                .desired_rows(3),
                        );
                        let models_ready = self.models_ready == Some(true);
                        ui.horizontal(|ui| {
                            ui.add_enabled(
                                models_ready || form.ocr,
                                egui::Checkbox::new(&mut form.ocr, "识别图片和扫描 PDF"),
                            );
                            ui.add_enabled(
                                models_ready || form.semantic,
                                egui::Checkbox::new(&mut form.semantic, "启用语义检索"),
                            );
                        });
                        let status = match self.models_ready {
                            Some(true) => "内置模型资源已校验",
                            Some(false) => "内置模型资源不可用，请修复安装",
                            None => "正在校验内置模型资源…",
                        };
                        ui.label(RichText::new(status).small().color(MUTED));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("保存正文范围").clicked() {
                            match form.settings(settings) {
                                Ok(candidate) => {
                                    change = Some(candidate);
                                    dismiss = true;
                                }
                                Err(error) => self.error = Some(error.to_string()),
                            }
                        }
                        if ui.button("取消").clicked() {
                            dismiss = true;
                        }
                    });
                });
            if dismiss {
                self.form = None;
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(ERROR, error);
        }
        if let Some(notice) = &self.notice {
            ui.label(RichText::new(notice).small().color(MUTED));
        }
        if !settings.content_enabled || saving {
            return change;
        }
        ui.add_space(12.0);
        if settings.semantic_enabled {
            ui.horizontal(|ui| {
                let previous = self.mode;
                ui.selectable_value(&mut self.mode, SearchMode::Lexical, "关键词");
                ui.selectable_value(&mut self.mode, SearchMode::Semantic, "语义");
                ui.selectable_value(&mut self.mode, SearchMode::Hybrid, "混合 RRF");
                if previous != self.mode {
                    self.release();
                }
            });
        }
        let mut search = false;
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 94.0).max(100.0);
            let edit = ui.add_sized(
                [width, 32.0],
                egui::TextEdit::singleline(&mut self.query).hint_text("输入正文关键词"),
            );
            desktop::limit_text(&mut self.query, 1024);
            search = ui.button("搜索正文").clicked()
                || (edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)));
        });
        if self.active.is_some() || self.pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在查询正文…");
                if ui.button("取消查询").clicked() {
                    self.release();
                }
            });
        }
        let page = self
            .results
            .clone()
            .and_then(|response| self.show_results(ui, &response));
        if search || page.is_some() {
            if let Err(error) = self.search(profile, ui.ctx(), page.unwrap_or(0)) {
                self.release();
                self.error = Some(format!("无法查询正文：{error:#}"));
            }
        }
        change
    }

    fn show_results(&mut self, ui: &mut egui::Ui, response: &SearchResponse) -> Option<usize> {
        ui.add_space(8.0);
        ui.label(format!("索引中匹配 {} 份文档", response.indexed_total));
        if response.omitted_stale > 0 {
            ui.label(
                RichText::new(format!("已省略 {} 条失效结果", response.omitted_stale))
                    .small()
                    .color(MUTED),
            );
        }
        let mut next = None;
        ui.horizontal(|ui| {
            let unchanged_query =
                self.query.trim() == self.last_query && self.mode == self.last_mode;
            if ui
                .add_enabled(
                    unchanged_query && self.offset != 0,
                    egui::Button::new("上一页"),
                )
                .clicked()
            {
                next = Some(self.offset.saturating_sub(PAGE_SIZE));
            }
            if ui
                .add_enabled(
                    unchanged_query && self.offset + PAGE_SIZE < response.indexed_total,
                    egui::Button::new("下一页"),
                )
                .clicked()
            {
                next = Some(self.offset + PAGE_SIZE);
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("content_hits")
            .max_height(180.0)
            .show(ui, |ui| {
                for (row, hit) in response.hits.iter().enumerate() {
                    let name = Path::new(&hit.path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(&hit.path);
                    let button = egui::Button::selectable(
                        self.selected == row,
                        format!("{name} · {}", location(&hit.location)),
                    )
                    .truncate();
                    if ui
                        .add_sized([ui.available_width(), 32.0], button)
                        .on_hover_text(&hit.path)
                        .clicked()
                    {
                        self.selected = row;
                    }
                }
            });
        if let Some(hit) = response.hits.get(self.selected) {
            ui.add_space(8.0);
            egui::Frame::new()
                .fill(BACKGROUND)
                .inner_margin(14)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("命中预览 · {}", location(&hit.location))).strong(),
                    );
                    ui.label(
                        RichText::new(format!(
                            "命中依据：{} · 文件身份和授权已校验",
                            hit.evidence.label()
                        ))
                        .small()
                        .color(MUTED),
                    );
                    egui::ScrollArea::vertical()
                        .id_salt("content_preview")
                        .show(ui, |ui| {
                            ui.label(preview(hit));
                        });
                });
        }
        next
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.cancel_build();
    }
}

fn location(location: &Location) -> String {
    match location {
        Location::Text { line } => format!("第 {line} 行"),
        Location::Paragraph { number } => format!("第 {number} 段"),
        Location::Slide { number } => format!("第 {number} 张幻灯片"),
        Location::Page { number } => format!("第 {number} 页"),
        Location::PageRegion {
            number,
            image,
            x,
            y,
            ..
        } => format!("第 {number} 页 · 图片 {image} · 坐标 {x}, {y}"),
        Location::ImageRegion { x, y, .. } => format!("图片文字 · 坐标 {x}, {y}"),
        Location::Cell { sheet, address } => format!("工作表 {sheet} · {address}"),
    }
}

fn preview(hit: &ContentHit) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let normal = TextFormat {
        font_id: FontId::proportional(15.0),
        color: INK,
        ..Default::default()
    };
    let highlighted = TextFormat {
        color: ACCENT,
        background: Color32::from_rgb(223, 244, 239),
        ..normal.clone()
    };
    let mut end = 0;
    for range in &hit.highlights {
        let (Some(before), Some(found)) = (
            hit.snippet.get(end..range.start),
            hit.snippet.get(range.start..range.end),
        ) else {
            continue;
        };
        job.append(before, 0.0, normal.clone());
        job.append(found, 0.0, highlighted.clone());
        end = range.end;
    }
    job.append(&hit.snippet[end..], 0.0, normal);
    job
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn preview_validation_fails_closed_on_timeout_or_disconnect() {
        let now = Instant::now();
        let (tx, rx) = mpsc::channel();
        let validation = Validation {
            id: 7,
            deadline: now + VALIDATION_TIMEOUT,
            receiver: rx,
        };
        assert_eq!(validation.poll(now), None);
        assert_eq!(validation.poll(now + VALIDATION_TIMEOUT), Some(false));
        tx.send(true).unwrap();
        // Even a late success cannot extend the authorization deadline.
        assert_eq!(validation.poll(now + VALIDATION_TIMEOUT), Some(false));
        assert_eq!(validation.poll(now), Some(true));
        drop(tx);
        assert_eq!(validation.poll(now), Some(false));
        let (tx, rx) = mpsc::channel();
        let denied = Validation {
            id: 8,
            deadline: now + VALIDATION_TIMEOUT,
            receiver: rx,
        };
        tx.send(false).unwrap();
        assert_eq!(denied.poll(now), Some(false));
    }
}
