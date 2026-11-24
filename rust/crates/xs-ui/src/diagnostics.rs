//! 索引状态面板。检查与导出均调用同一个本地脱敏诊断入口。

use crate::utility;
use eframe::egui;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, SystemTime},
};
use xs_core::persistence::Store;
use xs_platform::{FileRevision, ScanProgress, Settings};

enum Task {
    Read,
    Export(PathBuf),
}

struct Snapshot {
    report: Value,
    publication_age: Option<Duration>,
}

impl Snapshot {
    fn read(dir: &Path, report: Value) -> Result<Self, &'static str> {
        if report["schema"].as_u64() != Some(1) || report["network"].as_bool() != Some(false) {
            return Err("诊断格式与当前界面不兼容，请修复安装后重试。");
        }
        let publication_age = report["index"]["generation"]
            .as_u64()
            .and_then(|generation| publication_age(dir, generation));
        Ok(Self {
            report,
            publication_age,
        })
    }
}

fn publication_age(dir: &Path, generation: u64) -> Option<Duration> {
    let store = Store::open_read_only(dir.join("index")).ok()?;
    let pointer = store.directory().join("CURRENT.json");
    let before = FileRevision::at_path(&pointer).ok()?;
    if store.current_generation().ok().flatten() != Some(generation)
        || before != FileRevision::at_path(&pointer).ok()?
    {
        return None;
    }
    let seconds = u64::try_from(before.modified.seconds).ok()?;
    let modified =
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds, before.modified.nanoseconds))?;
    SystemTime::now().duration_since(modified).ok()
}

type Outcome = Result<(Snapshot, bool), &'static str>;

#[derive(Default)]
pub(crate) struct Pane {
    open: bool,
    destination: String,
    probe_path: String,
    probe_result: Option<Vec<String>>,
    pending: Option<Receiver<Outcome>>,
    snapshot: Option<Snapshot>,
    notice: Option<&'static str>,
    error: Option<&'static str>,
}

impl Pane {
    pub(crate) fn open(&mut self, ctx: &egui::Context, dir: &Path) {
        if !self.open && self.pending.is_none() {
            self.start(ctx, dir, Task::Read);
        }
        self.open = true;
    }

    fn start(&mut self, ctx: &egui::Context, dir: &Path, task: Task) {
        self.notice = None;
        self.error = None;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let dir = dir.to_path_buf();
        std::thread::spawn(move || {
            let export = matches!(&task, Task::Export(_));
            let mut args = vec!["doctor".into(), "--json".into()];
            if let Task::Export(path) = task {
                args.extend(["--export".into(), path.to_string_lossy().into_owned()]);
            }
            let result = utility::run(dir.clone(), &args)
                // 底层错误可能带配置路径。诊断界面只显示固定状态和建议。
                .map_err(|_| {
                    if export {
                        "导出失败，请确认目标是可写的本地新文件，且路径尚未被占用。"
                    } else {
                        "无法读取本机诊断，请重试或修复安装。"
                    }
                })
                .and_then(|report| Snapshot::read(&dir, report))
                .map(|snapshot| (snapshot, export));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.pending = Some(rx);
    }

    /// 返回用户显式触发的重建请求，调用方继续使用既有扫描授权入口。
    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        dir: &Path,
        settings: Option<&Settings>,
        current_generation: Option<u64>,
        progress: Option<&ScanProgress>,
        can_rebuild: bool,
    ) -> bool {
        if let Some(receiver) = &self.pending {
            match receiver.try_recv() {
                Ok(Ok((snapshot, exported))) => {
                    self.pending = None;
                    self.snapshot = Some(snapshot);
                    if exported {
                        self.notice = Some("脱敏报告已导出为新文件。");
                    }
                }
                Ok(Err(error)) => {
                    self.pending = None;
                    self.error = Some(error);
                }
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.error = Some("本机诊断已中断，请重试。");
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if !self.open {
            return false;
        }
        let mut open = true;
        let mut task = None;
        let mut rebuild = false;
        egui::Window::new("索引状态与诊断")
            .open(&mut open)
            .default_width(500.0)
            .vscroll(true)
            .show(ctx, |ui| {
                if let Some(snapshot) = &self.snapshot {
                    show_report(ui, snapshot, current_generation);
                    if self.pending.is_some() || self.error.is_some() {
                        ui.label("此处仍显示上次完成检查时的状态。");
                    }
                }
                if let Some(progress) = progress {
                    ui.label(format!(
                        "当前扫描已检查 {} 项，已收录 {} 项，错误 {} 项。",
                        progress.visited, progress.indexed, progress.errors
                    ));
                }
                if self.pending.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在读取本机诊断…");
                    });
                }
                if let Some(error) = self.error {
                    ui.colored_label(crate::ERROR, error);
                }
                if let Some(notice) = self.notice {
                    ui.label(notice);
                }
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(self.pending.is_none(), egui::Button::new("刷新诊断"))
                        .clicked()
                    {
                        task = Some(Task::Read);
                    }
                    rebuild = ui
                        .add_enabled(can_rebuild, egui::Button::new("重新扫描已授权范围"))
                        .clicked();
                });
                ui.separator();
                ui.label(
                    "导出会重新检查索引。报告仅含版本、状态和统计数字，省略路径、查询和正文。",
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.destination)
                        .desired_width(f32::INFINITY)
                        .hint_text("输入新诊断文件的本地绝对路径"),
                );
                if ui
                    .add_enabled(
                        self.pending.is_none() && !self.destination.trim().is_empty(),
                        egui::Button::new("导出脱敏报告"),
                    )
                    .clicked()
                {
                    let path = PathBuf::from(self.destination.trim());
                    if path.is_absolute() {
                        task = Some(Task::Export(path));
                    } else {
                        self.notice = None;
                        self.error = Some("请输入新诊断文件的本地绝对路径。");
                    }
                }
                ui.separator();
                ui.label(
                    "为什么搜不到？输入任意本地文件路径，即时排查范围、过滤策略与云盘占位符原因：",
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.probe_path)
                        .desired_width(f32::INFINITY)
                        .hint_text("输入需要排查的本地文件绝对路径"),
                );
                if ui
                    .add_enabled(
                        !self.probe_path.trim().is_empty(),
                        egui::Button::new("排查路径"),
                    )
                    .clicked()
                {
                    self.probe_result = Some(diagnose_missing_path(
                        Path::new(self.probe_path.trim()),
                        settings,
                    ));
                }
                if let Some(lines) = &self.probe_result {
                    for line in lines {
                        ui.label(line);
                    }
                }
            });
        self.open = open;
        if let Some(task) = task {
            self.start(ctx, dir, task);
        }
        rebuild
    }
}

fn status(value: &Value) -> &'static str {
    match value.as_str() {
        Some("readable" | "local_accessible") => "可读",
        Some("missing") => "未创建",
        Some("scope_mismatch") => "索引范围已变化，请重新扫描",
        Some("scope_unverified") => "索引范围未验证",
        Some("invalid_configuration") => "设置无效",
        Some("invalid_local_path") => "本地路径无效",
        Some("invalid_snapshot") => "索引无法校验",
        Some("access_denied") => "访问被拒绝",
        Some("runtime_unverified") => "需通过桌面操作验证",
        Some("unsupported") => "当前不可用",
        _ => "无法访问",
    }
}

fn show_report(ui: &mut egui::Ui, snapshot: &Snapshot, current_generation: Option<u64>) {
    let report = &snapshot.report;
    let index = &report["index"];
    let settings = &report["settings"];
    ui.label(format!("寻枢 {} · 离线", env!("CARGO_PKG_VERSION")));
    for (name, value) in [
        ("设置", &settings["state"]),
        ("数据目录", &report["disk"]["data_directory"]),
        ("索引", &index["state"]),
        ("快捷键", &report["hotkey"]["state"]),
        ("提权服务", &report["privileged_service"]["state"]),
    ] {
        ui.label(format!("{name}　{}", status(value)));
    }
    if let Some(mode) = settings["requested_mode"].as_str() {
        ui.label(format!(
            "配置采集模式　{}",
            match mode {
                "standard" => "标准扫描",
                "on_demand_elevated" => "按需提权采集",
                "fast_service" => "提权服务采集",
                _ => "未知模式",
            }
        ));
    }
    let roots = &settings["configured_roots"];
    if let (Some(accessible), Some(denied), Some(unavailable)) = (
        roots["accessible"].as_u64(),
        roots["denied"].as_u64(),
        roots["unavailable"].as_u64(),
    ) {
        ui.label(format!(
            "已配置范围　{accessible} 个可访问，{denied} 个权限不足，{unavailable} 个不可用"
        ));
    }
    if let (Some(generation), Some(entries), Some(bytes), Some(complete)) = (
        index["generation"].as_u64(),
        index["entries"].as_u64(),
        index["snapshot_bytes"].as_u64(),
        index["coverage_complete"].as_bool(),
    ) {
        let coverage = if complete {
            "扫描覆盖完整"
        } else {
            "扫描覆盖未完成"
        };
        ui.label(format!("代次 {generation} · {entries} 项 · {coverage}"));
        ui.label(format!("索引快照占用磁盘 {}", crate::size_label(bytes)));
        if current_generation.is_some_and(|current| current != generation) {
            ui.label("当前索引已换代，请刷新诊断。");
        } else if let Some(age) = snapshot.publication_age {
            ui.label(format!("检查时距离索引发布 {} 秒", age.as_secs()));
        }
        ui.label("索引发布时间与文件变更可搜时延分别测量。进程树内存由性能验收测量。");
    }
}

/// 面向用户的“为什么搜不到？”路径诊断器，按范围、类型策略、智能排除与文件存在性逐项排查。
pub(crate) fn diagnose_missing_path(path: &Path, settings: Option<&Settings>) -> Vec<String> {
    let mut findings = Vec::new();
    if !path.is_absolute() {
        findings.push("路径不是绝对路径，请输入完整本地路径。".into());
        return findings;
    }
    if let Err(err) = xs_platform::validate_local_path(path, false) {
        findings.push(format!("本地路径安全检查提示：{err}"));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                findings.push("该条目是符号链接：为避免死循环，默认扫描会跳过符号链接。".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const CLOUD_MASK: u32 = 0x0000_1000 | 0x0004_0000 | 0x0040_0000;
                if meta.file_attributes() & CLOUD_MASK != 0 {
                    findings.push("该文件是未下载到本地的云盘占位符：正文索引会主动跳过以防止误触发自动下载。".into());
                }
            }
        }
        Err(err) => {
            findings.push(format!("文件系统无法访问该路径：{err}"));
        }
    }
    let Some(settings) = settings else {
        findings.push("尚未加载索引范围设置，请先完成首次范围向导。".into());
        return findings;
    };
    if settings.roots.is_empty() {
        findings.push("当前尚未配置任何扫描磁盘或目录。".into());
        return findings;
    }
    let matched_scope = settings.roots.iter().find(|scope| {
        let root = Path::new(&scope.path);
        path.starts_with(root)
    });
    match matched_scope {
        None => findings.push("该路径不在任何已勾选的扫描磁盘或目录范围内，请在“搜索范围与设置”中添加其所在磁盘或父目录。".into()),
        Some(scope) => {
            findings.push(format!("已命中扫描范围：{}", scope.path));
            if let Some(allowed) = &scope.extensions {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                if allowed.is_empty() {
                    findings.push("该范围当前设置为“仅文件夹”，不会收录普通文件。".into());
                } else if !ext.is_empty() && !allowed.iter().any(|item| item == &ext) {
                    findings.push(format!(
                        "扩展名 .{ext} 不在该范围的收录文件类型白名单中，请切换为“所有文件类型”或补充该后缀。"
                    ));
                }
            }
        }
    }
    if findings.len() == 1 {
        findings.push(
            "范围与策略检查均已通过：若仍搜不到，请点击“重新扫描已授权范围”刷新索引代次。".into(),
        );
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_other_protocols_and_never_echoes_unknown_state() {
        let dir = Path::new("unused");
        assert!(Snapshot::read(dir, json!({"schema":2,"network":false})).is_err());
        assert!(Snapshot::read(dir, json!({"schema":1,"network":true})).is_err());
        assert!(Snapshot::read(dir, json!({"schema":1,"network":false})).is_ok());
        assert_eq!(status(&json!("private-path-marker")), "无法访问");
        assert_eq!(status(&json!("access_denied")), "访问被拒绝");
    }

    #[test]
    fn missing_path_diagnoser_identifies_scope_and_extension_mismatch() {
        let base = std::env::temp_dir().join("xunshu-diagnostics-synthetic");
        assert!(base.is_absolute());
        let docs = base.join("docs");
        let mut settings = Settings::default();
        settings.roots.push(xs_platform::Scope {
            path: docs.to_string_lossy().into_owned(),
            extensions: Some(vec!["pdf".into()]),
            smart_excludes: true,
        });
        let outside = diagnose_missing_path(&base.join("other").join("a.pdf"), Some(&settings));
        assert!(outside.iter().any(|line| line.contains("不在任何已勾选")));
        let wrong_ext = diagnose_missing_path(&docs.join("note.txt"), Some(&settings));
        assert!(wrong_ext.iter().any(|line| line.contains(".txt")));
    }
}
