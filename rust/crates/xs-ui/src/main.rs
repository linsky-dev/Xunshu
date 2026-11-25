#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! 寻枢离线桌面。查询和扫描在后台运行，拖放在原生窗口线程交给系统处理。
use anyhow::{bail, ensure, Context as _, Result};
use desktop::{HostMessage, RendererMessage, View};
use eframe::egui::{self, Color32, FontId, RichText, Stroke, Vec2};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    time::{Duration, Instant},
};
use xs_core::index::{ColumnSort, SortColumn};
use xs_core::{operations, Index, SearchOptions, SearchResponse};
use xs_platform::{ScanProgress, Scope, Settings, SetupState, SpeedMode};

mod activity;
mod connectors_ui;
mod content;
mod desktop;
mod diagnostics;
mod scope_estimate;
mod utility;

const INK: Color32 = Color32::from_rgb(23, 34, 32);
const MUTED: Color32 = Color32::from_rgb(76, 91, 87);
const ACCENT: Color32 = Color32::from_rgb(8, 103, 89);
const SOFT: Color32 = Color32::from_rgb(223, 241, 234);
const BORDER: Color32 = Color32::from_rgb(215, 225, 221);
const BACKGROUND: Color32 = Color32::from_rgb(246, 248, 246);
const ERROR: Color32 = Color32::from_rgb(164, 48, 48);
const PAGE_SIZE: usize = 200;
const QUICK_PAGE_SIZE: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DesktopMode {
    Default,
    Background,
    Standalone,
    Renderer,
}

struct Args {
    data_dir: PathBuf,
    smoke_test: bool,
    query: String,
    font: Option<PathBuf>,
    view: View,
    mode: DesktopMode,
    content: bool,
}
impl Args {
    fn parse() -> Result<Self> {
        let mut data_dir = None;
        let mut smoke_test = false;
        let mut query = String::new();
        let mut font = None;
        let mut view = View::Main;
        let mut mode = DesktopMode::Default;
        let mut content = false;
        let mut args = std::env::args_os().skip(1);
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--data-dir") => {
                    data_dir = Some(PathBuf::from(
                        args.next().context("--data-dir 需要本地目录路径")?,
                    ))
                }
                Some("--query") => {
                    query = args
                        .next()
                        .context("--query 需要查询文本")?
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("查询文本必须为 UTF-8"))?
                }
                Some("--font") => {
                    font = Some(PathBuf::from(
                        args.next().context("--font 需要本地字体路径")?,
                    ))
                }
                Some("--smoke-test") => smoke_test = true,
                Some("--quick") => view = View::Quick,
                Some("--content") => content = true,
                Some("--background") => mode = DesktopMode::Background,
                Some("--standalone") => mode = DesktopMode::Standalone,
                Some("--desktop-render") => mode = DesktopMode::Renderer,
                Some("--help" | "-h") => {
                    println!("寻枢 Rust 原生界面\n\nxs-ui [--data-dir 绝对路径] [--query 查询文本] [--font 本地字体] [--quick | --content] [--standalone | --background | --smoke-test]\n\n--quick 打开快捷搜索框。\n--content 直接打开正文页。\n--standalone 单独显示窗口，不启动托盘。\n--background 在 Windows 启动常驻托盘，不显示窗口。\n--smoke-test 验证原生窗口，不启动扫描。");
                    std::process::exit(0);
                }
                _ => bail!("无法识别的参数：{}", arg.to_string_lossy()),
            }
        }
        let data_dir = match data_dir {
            Some(p) => p,
            None => operations::default_data_dir()?,
        };
        ensure!(data_dir.is_absolute(), "--data-dir 必须为本地绝对路径");
        if content {
            ensure!(
                view == View::Main
                    && matches!(mode, DesktopMode::Default | DesktopMode::Standalone),
                "正文页只能在独立主窗口中直接打开"
            );
            mode = DesktopMode::Standalone;
        }
        desktop::limit_query(&mut query);
        Ok(Self {
            data_dir,
            smoke_test,
            query,
            font,
            view,
            mode,
            content,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Filter {
    All,
    Files,
    Folders,
    Documents,
    Pictures,
    Audio,
    Video,
    Archives,
    Other,
}
impl Filter {
    const OPTIONS: [Self; 9] = [
        Self::All,
        Self::Files,
        Self::Folders,
        Self::Documents,
        Self::Pictures,
        Self::Audio,
        Self::Video,
        Self::Archives,
        Self::Other,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Files => "文件",
            Self::Folders => "文件夹",
            Self::Documents => "文档",
            Self::Pictures => "图片",
            Self::Audio => "音频",
            Self::Video => "视频",
            Self::Archives => "压缩包",
            Self::Other => "其他",
        }
    }
    fn query(self, query: &str) -> String {
        // 复用索引查询的分类规则，在计数和分页之前执行筛选。
        let prefix = match self {
            Self::All => return query.to_owned(),
            Self::Files => "file:",
            Self::Folders => "folder:",
            Self::Documents => "doc:",
            Self::Pictures => "pic:",
            Self::Audio => "audio:",
            Self::Video => "video:",
            Self::Archives => "archive:",
            Self::Other => "file: !<doc: | pic: | audio: | video: | archive:>",
        };
        if query.trim().is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix} <{query}>")
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TypePolicy {
    All,
    Documents,
    PdfAndWord,
    Media,
    Code,
    Archives,
    Custom,
    Directories,
}
impl TypePolicy {
    const ALL: [Self; 8] = [
        Self::All,
        Self::Documents,
        Self::PdfAndWord,
        Self::Media,
        Self::Code,
        Self::Archives,
        Self::Custom,
        Self::Directories,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::All => "所有文件类型",
            Self::Documents => "常用文档",
            Self::PdfAndWord => "PDF 和 Word",
            Self::Media => "图片、音频与视频",
            Self::Code => "源代码与文本",
            Self::Archives => "压缩归档",
            Self::Custom => "自定义后缀",
            Self::Directories => "仅文件夹",
        }
    }
    fn extensions(self, custom: &str) -> Result<Option<Vec<String>>> {
        let list = match self {
            Self::All => return Ok(None),
            Self::Directories => return Ok(Some(Vec::new())),
            Self::Documents => "pdf doc docx xls xlsx ppt pptx txt md csv odt ods odp rtf",
            Self::PdfAndWord => "pdf doc docx docm dot dotx dotm",
            Self::Media => "jpg jpeg png gif webp svg bmp tif tiff mp3 wav flac mp4 mkv mov avi",
            Self::Code => {
                "rs py js ts tsx jsx html css json toml yaml yml xml sql sh c cpp h java go txt md"
            }
            Self::Archives => "zip rar 7z tar gz tgz bz2 xz zst cab iso",
            Self::Custom => custom,
        };
        let values: Vec<String> = list
            .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '，' || c == '；')
            .filter(|x| !x.is_empty())
            .map(|x| x.trim_start_matches('.').to_ascii_lowercase())
            .collect();
        ensure!(
            !values.is_empty(),
            "自定义后缀不能为空；如只收录目录，请选择“仅文件夹”。"
        );
        Ok(Some(values))
    }
}

#[derive(Clone)]
struct EditableScope {
    path: String,
    policy: TypePolicy,
    custom: String,
    smart_excludes: bool,
}
impl From<&Scope> for EditableScope {
    fn from(scope: &Scope) -> Self {
        Self {
            path: scope.path.clone(),
            policy: match &scope.extensions {
                None => TypePolicy::All,
                Some(v) if v.is_empty() => TypePolicy::Directories,
                Some(_) => TypePolicy::Custom,
            },
            custom: scope
                .extensions
                .as_ref()
                .map(|v| v.join(", "))
                .unwrap_or_default(),
            smart_excludes: scope.smart_excludes,
        }
    }
}
impl EditableScope {
    fn into_scope(self) -> Result<Scope> {
        Ok(Scope {
            path: self.path.trim().to_owned(),
            extensions: self.policy.extensions(&self.custom)?,
            smart_excludes: self.smart_excludes,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WizardStage {
    Welcome,
    Scope,
    Review,
}
struct Wizard {
    stage: WizardStage,
    roots: Vec<EditableScope>,
    draft_path: String,
    global_policy: TypePolicy,
    global_custom: String,
    global_smart_excludes: bool,
    error: Option<String>,
    volumes: Option<Result<Vec<VolumeCard>, String>>,
    estimate: Option<WizardEstimate>,
}

enum WizardEstimate {
    Running(EstimateTask),
    Ready(Result<scope_estimate::Estimate, String>),
}

struct EstimateTask {
    cancel: Arc<AtomicBool>,
    result: Receiver<Result<scope_estimate::Estimate, String>>,
}

impl Drop for EstimateTask {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
impl Wizard {
    fn new(settings: &Settings, welcome: bool) -> Self {
        let roots: Vec<EditableScope> = settings.roots.iter().map(EditableScope::from).collect();
        let (global_policy, global_custom, global_smart_excludes) = roots
            .first()
            .map(|first| (first.policy, first.custom.clone(), first.smart_excludes))
            .unwrap_or((TypePolicy::All, String::new(), true));
        Self {
            stage: if welcome {
                WizardStage::Welcome
            } else {
                WizardStage::Scope
            },
            roots,
            draft_path: String::new(),
            global_policy,
            global_custom,
            global_smart_excludes,
            error: None,
            volumes: None,
            estimate: None,
        }
    }
    fn has_root(&self, path: &str) -> bool {
        self.roots.iter().any(|root| {
            if cfg!(windows) {
                root.path.eq_ignore_ascii_case(path)
            } else {
                root.path == path
            }
        })
    }
    fn toggle_volume(&mut self, path: &str, selected: bool) {
        if selected {
            if !self.has_root(path) {
                self.roots.push(EditableScope {
                    path: path.to_owned(),
                    policy: self.global_policy,
                    custom: self.global_custom.clone(),
                    smart_excludes: self.global_smart_excludes,
                });
            }
        } else {
            self.roots.retain(|root| {
                if cfg!(windows) {
                    !root.path.eq_ignore_ascii_case(path)
                } else {
                    root.path != path
                }
            });
        }
    }
    fn apply_global_policy(&mut self) {
        for root in &mut self.roots {
            root.policy = self.global_policy;
            root.custom = self.global_custom.clone();
            root.smart_excludes = self.global_smart_excludes;
        }
    }
    fn candidate(&self, previous: &Settings) -> Result<Settings> {
        ensure!(!self.roots.is_empty(), "请至少添加一个本地目录。");
        let mut settings = previous.clone();
        settings.roots = self
            .roots
            .iter()
            .cloned()
            .map(EditableScope::into_scope)
            .collect::<Result<_>>()?;
        settings.mode = SpeedMode::Standard;
        settings.complete_setup();
        settings.validate()?;
        Ok(settings)
    }

    fn read_estimate(&mut self) {
        if let Some(WizardEstimate::Running(task)) = &self.estimate {
            match task.result.try_recv() {
                Ok(result) => self.estimate = Some(WizardEstimate::Ready(result)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.estimate = Some(WizardEstimate::Ready(Err("估算进程异常结束".into())));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn start_estimate(&mut self, settings: Settings, dir: PathBuf, ctx: egui::Context) {
        let (sender, result) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        self.estimate = Some(
            match std::thread::Builder::new()
                .name("xunshu-scope-estimate".into())
                .spawn(move || {
                    let result = scope_estimate::estimate(&settings, &dir, &worker_cancel)
                        .map_err(|error| error.to_string());
                    let _ = sender.send(result);
                    ctx.request_repaint();
                }) {
                Ok(_) => WizardEstimate::Running(EstimateTask { cancel, result }),
                Err(error) => WizardEstimate::Ready(Err(format!("无法启动估算：{error}"))),
            },
        );
    }
}

#[derive(Clone, Copy)]
enum SaveAction {
    Defer,
    Apply,
    Content,
}
struct Loaded {
    settings: Settings,
    persisted: bool,
    index: Option<Arc<Index>>,
    notice: Option<String>,
    offer_setup: bool,
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ScanSummary {
    published: bool,
    coverage_complete: bool,
    durability_warning: Option<String>,
    indexed: usize,
    cancelled: bool,
    elapsed_ms: f64,
    errors: Vec<String>,
    skipped: u64,
}
struct SettingsSaved {
    settings: Settings,
    warning: Option<String>,
}
enum Message {
    Loaded {
        request: u64,
        result: std::result::Result<Loaded, String>,
    },
    Search {
        request: u64,
        epoch: u64,
        result: std::result::Result<SearchResponse, String>,
    },
    SearchPreview {
        request: u64,
        epoch: u64,
        response: SearchResponse,
    },
    Activity(std::result::Result<activity::Activity, String>),
    ExternalChange,
    Progress {
        request: u64,
        progress: ScanProgress,
    },
    Scan {
        request: u64,
        result: std::result::Result<ScanSummary, String>,
    },
    Saved {
        action: SaveAction,
        result: std::result::Result<SettingsSaved, String>,
    },
    ResultAction {
        request: u64,
        epoch: u64,
        result: Result<ActionOutcome, String>,
    },
    VolumeCards(Result<Vec<VolumeCard>, String>),
    PickedFolder(Option<String>),
}

struct VolumeCard {
    volume: xs_platform::VolumeCapability,
    capacity: Option<u64>,
}

#[derive(Clone, Copy)]
enum ResultAction {
    Open,
    OpenParent,
    CopyPath,
    Preview,
    QuickSwitch,
}

enum ActionOutcome {
    Opened,
    CopyPath(String),
    Preview(Preview<egui::ColorImage>),
}

struct Preview<I> {
    name: String,
    content: PreviewContent<I>,
    find_query: String,
    find_index: usize,
}

enum PreviewContent<I> {
    Text { text: String, truncated: bool },
    Image(I),
}

fn discover_volume_cards() -> Result<Vec<VolumeCard>, String> {
    let volumes = xs_platform::volumes().map_err(|error| error.to_string())?;
    Ok(volumes
        .into_iter()
        .filter(|volume| volume.local && volume.fixed)
        .map(|volume| {
            let capacity = (volume.standard_supported
                && xs_platform::validate_local_path(Path::new(&volume.root), false).is_ok())
            .then(|| fs2::total_space(&volume.root).ok())
            .flatten();
            VolumeCard { volume, capacity }
        })
        .collect())
}

/// Request IDs, independent of persisted generation numbers, protect the UI
/// against late worker results after a scope edit, reload, or fast new query.
#[derive(Default)]
struct SearchGate {
    request: u64,
    epoch: u64,
}
impl SearchGate {
    fn next(&mut self) -> u64 {
        self.request = self.request.wrapping_add(1);
        self.request
    }
    fn invalidate_index(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.next();
    }
    fn accepts(&self, request: u64, epoch: u64) -> bool {
        request == self.request && epoch == self.epoch
    }
}

struct App {
    data_dir: PathBuf,
    brand_icon: egui::TextureHandle,
    tx: Sender<Message>,
    rx: Receiver<Message>,
    settings: Option<Settings>,
    persisted: bool,
    index: Option<Arc<Index>>,
    loading: bool,
    load_request: u64,
    query: String,
    filter: Filter,
    smart_mode: bool,
    offset: usize,
    sort: Option<ColumnSort>,
    budget_ms: u64,
    search_due: Option<Instant>,
    gate: SearchGate,
    search_cancel: Option<Arc<AtomicBool>>,
    searching: bool,
    results: Option<SearchResponse>,
    selected: Option<usize>,
    selected_app: Option<usize>,
    selected_connector: Option<usize>,
    search_error: Option<String>,
    scan_request: u64,
    scan_cancel: Option<Arc<AtomicBool>>,
    scan_progress: Option<ScanProgress>,
    scan_started: Option<Instant>,
    saving: bool,
    opening: bool,
    picking_folder: bool,
    preview: Option<Preview<egui::TextureHandle>>,
    high_contrast: bool,
    wizard: Option<Wizard>,
    notice: Option<String>,
    error: Option<String>,
    show_help: bool,
    started: Instant,
    smoke_test: bool,
    smoke_exit_requested: bool,
    close_after_save: bool,
    cjk_font: bool,
    view: View,
    view_queries: [String; 2],
    view_filters: [Filter; 2],
    view_positions: [Option<egui::Pos2>; 2],
    desktop_commands: Option<Receiver<HostMessage>>,
    close_notified: bool,
    #[cfg(windows)]
    tray_hidden: bool,
    focus_query: bool,
    ime_composing: bool,
    monitor_stop: Arc<AtomicBool>,
    activity: activity::Activity,
    activity_busy: bool,
    show_activity: bool,
    utility: utility::Pane,
    diagnostics: diagnostics::Pane,
    connectors: connectors_ui::ConnectorsPane,
    installed_apps: Vec<xs_platform::apps::AppEntry>,
    content_tab: bool,
    content: content::Pane,
    close_after_content: bool,
    initial_content_query: Option<String>,
    content_smoke_since: Option<Instant>,
    #[cfg(windows)]
    native_window: Option<windows_sys::Win32::Foundation::HWND>,
    #[cfg(windows)]
    focus_native_until: Option<Instant>,
}

fn send(tx: &Sender<Message>, ctx: &egui::Context, message: Message) {
    let _ = tx.send(message);
    ctx.request_repaint();
}

fn monitor_profile(dir: PathBuf, stop: Arc<AtomicBool>, tx: Sender<Message>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let read = || {
            let scope = operations::load_settings(&dir)
                .and_then(|settings| Ok((settings.scope_token()?, settings.content_scope_token()?)))
                .ok();
            let generation = xs_core::persistence::Store::open(dir.join("index"))
                .and_then(|store| store.current_generation())
                .ok()
                .flatten();
            (scope, generation)
        };
        let mut previous = read();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(400));
            let current = read();
            if current != previous {
                previous = current;
                send(&tx, &ctx, Message::ExternalChange);
            }
        }
    });
}

fn run_scan(
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ScanProgress),
) -> std::result::Result<ScanSummary, String> {
    let mut last = Instant::now() - Duration::from_secs(1);
    operations::rebuild(dir, cancel, |mut update| {
        if last.elapsed() >= Duration::from_millis(100) {
            if let Some(path) = &mut update.current_path {
                desktop::limit_query(path);
            }
            progress(update);
            last = Instant::now();
        }
    })
    .map(|outcome| ScanSummary {
        published: outcome.published,
        coverage_complete: outcome.report.complete && !outcome.report.cancelled,
        durability_warning: outcome.durability_warning,
        indexed: outcome.indexed,
        cancelled: outcome.report.cancelled,
        elapsed_ms: outcome.elapsed_ms,
        errors: outcome
            .report
            .errors
            .iter()
            .take(8)
            .map(|error| {
                let mut text = error.to_string();
                desktop::limit_text(&mut text, 800);
                text
            })
            .collect(),
        skipped: outcome.report.skipped_links
            + outcome.report.skipped_mounts
            + outcome.report.skipped_excluded
            + outcome.report.skipped_non_utf8
            + outcome.report.skipped_special,
    })
    .map_err(|error| format!("{error:#}"))
}

fn load_state(dir: &Path, offer_setup: bool) -> Result<Loaded> {
    xs_platform::validate_local_path(dir, true)?;
    let path = operations::settings_path(dir);
    xs_platform::validate_local_path(&path, true)?;
    let persisted = match std::fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    let mut settings = if persisted {
        operations::load_settings(dir)?
    } else {
        Settings::default()
    };
    let offer_setup = offer_setup && settings.should_remind();
    let mut notice = None;
    let mut persisted = persisted;
    if offer_setup {
        let mut offered = settings.clone();
        offered.record_reminder();
        match operations::save_settings(dir, &offered) {
            Ok(commit) => {
                settings = operations::load_settings(dir)?;
                persisted = true;
                notice = commit
                    .warning()
                    .map(|warning| format!("提醒次数已保存；落盘持久性未确认：{warning}"));
            }
            Err(e) => notice = Some(format!("无法保存向导提醒次数：{e}")),
        }
    }
    let store = xs_core::persistence::Store::open(dir.join("index"))?;
    let index = if store.has_snapshot()? {
        match operations::load_index(dir) {
            Ok(index) => Some(Arc::new(index)),
            Err(e) => {
                notice = Some(format!("索引未加载：{e:#}"));
                None
            }
        }
    } else {
        None
    };
    Ok(Loaded {
        settings,
        persisted,
        index,
        notice,
        offer_setup,
    })
}

fn save_settings(
    dir: &Path,
    expected: Option<&Settings>,
    candidate: &Settings,
    action: SaveAction,
) -> Result<SettingsSaved> {
    let path = operations::settings_path(dir);
    xs_platform::validate_local_path(&path, true)?;
    match expected {
        Some(expected) => ensure!(
            &operations::load_settings(dir)? == expected,
            "配置已被其他窗口或命令行修改。请关闭向导，点击“重新加载”后再设置。"
        ),
        None => ensure!(
            !path.try_exists()?,
            "配置已由其他进程创建。请重新加载后再设置。"
        ),
    }
    candidate.validate()?;
    let validate_roots = match action {
        SaveAction::Apply => candidate.roots.as_slice(),
        SaveAction::Content if candidate.content_enabled => candidate.content_roots.as_slice(),
        _ => &[],
    };
    if !validate_roots.is_empty() {
        for scope in validate_roots {
            let root = Path::new(&scope.path);
            xs_platform::validate_local_path(root, false)?;
            ensure!(
                std::fs::symlink_metadata(root)?.is_dir(),
                "范围必须是本地文件夹：{}",
                scope.path
            );
        }
    }
    let commit = operations::save_settings(dir, candidate)?;
    let mut warning = commit.warning().map(str::to_owned);
    if matches!(action, SaveAction::Content)
        && expected.and_then(|old| old.content_scope_token().ok())
            != candidate.content_scope_token().ok()
    {
        if let Err(error) = xs_content::discard_pending_work(dir) {
            warning = Some(format!(
                "正文授权已保存，但待处理任务未清理：{error:#}{}",
                warning.map(|s| format!("；{s}")).unwrap_or_default()
            ));
        }
    }
    // A readback error after commit cannot turn a completed save into failure.
    let settings = match operations::load_settings(dir) {
        Ok(settings) => settings,
        Err(error) => {
            warning = Some(format!(
                "配置已保存，但重新读取失败，请重新加载：{error:#}{}",
                warning.map(|s| format!("；{s}")).unwrap_or_default()
            ));
            candidate.clone()
        }
    };
    Ok(SettingsSaved { settings, warning })
}

/// 搜索范围路由：普通词可同时匹配文件/应用/已授权连接器，
/// 显式前缀只查询对应数据源，绝不交给文件搜索语法解析器。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchSource {
    All,
    Applications,
    Bookmarks,
    Repositories,
    Plugins,
}

fn routed_query(raw: &str) -> (SearchSource, &str) {
    let text = raw.trim();
    for (prefix, source) in [
        ("app:", SearchSource::Applications),
        ("bookmark:", SearchSource::Bookmarks),
        ("bm:", SearchSource::Bookmarks),
        ("git:", SearchSource::Repositories),
        ("src:", SearchSource::Plugins),
        (">", SearchSource::Applications),
    ] {
        if let Some(value) = text.strip_prefix(prefix) {
            return (source, value.trim());
        }
    }
    (SearchSource::All, text)
}

fn source_suggestions_allowed(raw: &str) -> bool {
    let (source, query) = routed_query(raw);
    source != SearchSource::All || (!query.is_empty() && !query.contains([':', '|', '!', '<', '>']))
}

/// `app:` / `>` 是显式仅应用模式；普通关键词同时提供本机应用建议。
/// 不解释文件搜索运算符，避免将 `ext:pdf` 误当应用名称。
fn app_query(raw: &str) -> (bool, &str) {
    let (source, query) = routed_query(raw);
    (source == SearchSource::Applications, query)
}

fn app_matches_allowed(raw: &str) -> bool {
    let (source, _) = routed_query(raw);
    (source == SearchSource::Applications || source == SearchSource::All)
        && source_suggestions_allowed(raw)
}

impl App {
    fn external_only(&self) -> bool {
        routed_query(&self.query).0 != SearchSource::All
    }

    fn allows_external_suggestions(&self) -> bool {
        self.filter == Filter::All || self.external_only()
    }

    fn matching_connectors(&self) -> Vec<xs_content::connectors::ConnectorItem> {
        if !self.allows_external_suggestions() || !source_suggestions_allowed(&self.query) {
            return Vec::new();
        }
        let (source, query) = routed_query(&self.query);
        let (bookmarks, git, plugins) = match source {
            SearchSource::All => (true, true, true),
            SearchSource::Bookmarks => (true, false, false),
            SearchSource::Repositories => (false, true, false),
            SearchSource::Plugins => (false, false, true),
            SearchSource::Applications => (false, false, false),
        };
        self.connectors
            .matching_items(query, bookmarks, git, plugins, 8)
    }

    fn open_connector(&mut self, row: usize, ctx: &egui::Context) {
        if !self.connectors.grant_current() {
            self.error = Some("来源授权已变更，请重新载入来源".into());
            self.selected_connector = None;
            return;
        }
        let Some(item) = self.matching_connectors().get(row).cloned() else {
            self.error = Some("连接器结果已更新，请重新搜索".into());
            return;
        };
        match connectors_ui::ConnectorsPane::open_item(&item, ctx) {
            Ok(()) => {
                self.error = None;
                if self.view == View::Quick && item.source == "connector:git_repo" {
                    self.request_close(ctx);
                }
            }
            Err(error) => self.error = Some(format!("无法打开 {}：{error:#}", item.title)),
        }
    }

    fn matching_apps(&self) -> Vec<xs_platform::apps::AppEntry> {
        if !self.allows_external_suggestions() || !app_matches_allowed(&self.query) {
            return Vec::new();
        }
        let (_, query) = app_query(&self.query);
        xs_platform::apps::match_apps(&self.installed_apps, query, 5)
            .into_iter()
            .cloned()
            .collect()
    }

    fn launch_app(&mut self, row: usize, ctx: &egui::Context) {
        let Some(app) = self.matching_apps().get(row).cloned() else {
            self.error = Some("应用结果已更新，请重新搜索".into());
            return;
        };
        match connectors_ui::launch_app_entry(&app.launch_target) {
            Ok(()) => {
                self.error = None;
                if self.view == View::Quick {
                    self.request_close(ctx);
                } else {
                    self.notice = Some(format!("已启动本机应用：{}", app.name));
                }
            }
            Err(error) => self.error = Some(format!("无法启动应用 {}：{error:#}", app.name)),
        }
    }

    fn new(
        cc: &eframe::CreationContext<'_>,
        args: Args,
        view_queries: [String; 2],
        font: Option<Vec<u8>>,
        icon: egui::ColorImage,
    ) -> Self {
        let cjk_font = configure_ui(&cc.egui_ctx, font);
        let brand_icon =
            cc.egui_ctx
                .load_texture("xunshu_brand", icon, egui::TextureOptions::LINEAR);
        let initial_content_query = (args.content
            && !view_queries[View::Main.index()].trim().is_empty())
        .then(|| view_queries[View::Main.index()].clone());
        let (tx, rx) = mpsc::channel();
        let monitor_stop = Arc::new(AtomicBool::new(false));
        if !args.smoke_test {
            monitor_profile(
                args.data_dir.clone(),
                monitor_stop.clone(),
                tx.clone(),
                cc.egui_ctx.clone(),
            );
        }
        let desktop_commands = (args.mode == DesktopMode::Renderer).then(|| {
            let (tx, rx) = mpsc::sync_channel(8);
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                let mut input = std::io::stdin().lock();
                while let Ok(Some(command)) = desktop::receive(&mut input) {
                    if tx.send(command).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                drop(tx);
                ctx.request_repaint();
            });
            rx
        });
        let mut app = Self {
            data_dir: args.data_dir,
            brand_icon,
            tx,
            rx,
            settings: None,
            persisted: false,
            index: None,
            loading: true,
            load_request: 0,
            query: view_queries[args.view.index()].clone(),
            filter: Filter::All,
            smart_mode: true,
            offset: 0,
            sort: None,
            budget_ms: 2500,
            search_due: None,
            gate: SearchGate::default(),
            search_cancel: None,
            searching: false,
            results: None,
            selected: None,
            selected_app: None,
            selected_connector: None,
            search_error: None,
            scan_request: 0,
            scan_cancel: None,
            scan_progress: None,
            scan_started: None,
            saving: false,
            opening: false,
            picking_folder: false,
            preview: None,
            high_contrast: false,
            wizard: None,
            notice: None,
            error: None,
            show_help: false,
            started: Instant::now(),
            smoke_test: args.smoke_test,
            smoke_exit_requested: false,
            close_after_save: false,
            cjk_font,
            view: args.view,
            view_queries,
            view_filters: [Filter::All; 2],
            view_positions: [None; 2],
            desktop_commands,
            close_notified: false,
            #[cfg(windows)]
            tray_hidden: false,
            focus_query: true,
            ime_composing: false,
            monitor_stop,
            activity: activity::Activity::default(),
            activity_busy: false,
            show_activity: false,
            utility: utility::Pane::default(),
            diagnostics: diagnostics::Pane::default(),
            connectors: connectors_ui::ConnectorsPane::default(),
            installed_apps: xs_platform::apps::discover_installed_apps(),
            content_tab: args.content,
            content: content::Pane::default(),
            close_after_content: false,
            initial_content_query,
            content_smoke_since: None,
            #[cfg(windows)]
            native_window: desktop::windows::native_window(cc),
            #[cfg(windows)]
            focus_native_until: (args.mode == DesktopMode::Renderer && args.view == View::Quick)
                .then(|| Instant::now() + Duration::from_millis(600)),
        };
        app.emit_desktop(&RendererMessage::Ready);
        app.reload(&cc.egui_ctx, app.view == View::Main);
        app
    }

    fn emit_desktop(&self, event: &RendererMessage) {
        if self.desktop_commands.is_some() {
            let _ = desktop::send(&mut std::io::stdout().lock(), event);
        }
    }

    fn query_edited(&mut self) {
        desktop::limit_query(&mut self.query);
        self.emit_desktop(&RendererMessage::Query {
            view: self.view,
            query: self.query.clone(),
        });
    }

    fn switch_view(&mut self, ctx: &egui::Context, view: View, query: Option<String>) {
        use egui::ViewportCommand as Cmd;
        if view != self.view {
            let (title, size, minimum) = view_geometry(view);
            ctx.send_viewport_cmd(Cmd::Maximized(false));
            ctx.send_viewport_cmd(Cmd::Title(title.into()));
            ctx.send_viewport_cmd(Cmd::Decorations(view == View::Main));
            ctx.send_viewport_cmd(Cmd::WindowLevel(if view == View::Quick {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            }));
            // 分别记住两个窗口的外边框原点，避免装饰变化造成累积偏移。
            let position = ctx.input(|input| input.viewport().outer_rect.map(|rect| rect.min));
            self.view_positions[self.view.index()] = position;
            if let Some(position) = self.view_positions[view.index()].or(position) {
                ctx.send_viewport_cmd(Cmd::OuterPosition(position));
            }
            ctx.send_viewport_cmd(Cmd::MinInnerSize(minimum));
            ctx.send_viewport_cmd(Cmd::InnerSize(size));
            self.view_queries[self.view.index()] = std::mem::take(&mut self.query);
            self.view_filters[self.view.index()] = self.filter;
            self.view = view;
            if view == View::Quick {
                self.content.release();
            }
            self.query.clone_from(&self.view_queries[view.index()]);
            self.filter = self.view_filters[view.index()];
            self.emit_desktop(&RendererMessage::View(view));
            self.schedule_search(true);
        }
        if let Some(query) = query.filter(|query| *query != self.query) {
            self.query = query;
            self.query_edited();
            self.schedule_search(true);
        }
        self.focus_query = true;
        #[cfg(windows)]
        {
            if std::mem::take(&mut self.tray_hidden) {
                ctx.send_viewport_cmd(Cmd::Visible(true));
            }
            self.focus_native_until = self
                .desktop_commands
                .is_some()
                .then(|| Instant::now() + Duration::from_millis(600));
        }
        ctx.send_viewport_cmd(Cmd::Minimized(false));
        ctx.send_viewport_cmd(Cmd::Focus);
    }

    fn desktop_messages(&mut self, ctx: &egui::Context) {
        loop {
            let Some(receiver) = &self.desktop_commands else {
                return;
            };
            match receiver.try_recv() {
                Ok(HostMessage::Show { view, query }) => self.switch_view(ctx, view, query),
                Ok(HostMessage::FileDialog(hwnd)) => {
                    #[cfg(windows)]
                    desktop::windows::set_cached_file_dialog(hwnd);
                    #[cfg(not(windows))]
                    let _ = hwnd;
                }
                Ok(HostMessage::Hide) => self.request_close(ctx),
                Ok(HostMessage::ScanStarted) => self.mark_scan(),
                Ok(HostMessage::ScanProgress(progress)) => {
                    self.mark_scan();
                    self.scan_progress = Some(progress);
                }
                Ok(HostMessage::ScanFinished(result)) => {
                    let _ = self.tx.send(Message::Scan {
                        request: self.scan_request,
                        result,
                    });
                    ctx.request_repaint();
                }
                Ok(HostMessage::Boot { .. }) => {}
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
            }
        }
    }

    fn ime_active_this_frame(&mut self, ctx: &egui::Context) -> bool {
        let mut active = self.ime_composing;
        ctx.input(|input| {
            for event in &input.events {
                match event {
                    egui::Event::Ime(egui::ImeEvent::Preedit(text)) => {
                        active = true;
                        self.ime_composing = !text.is_empty();
                    }
                    egui::Event::Ime(egui::ImeEvent::Commit(_)) => {
                        active = true;
                        self.ime_composing = false;
                    }
                    egui::Event::Ime(egui::ImeEvent::Disabled)
                    | egui::Event::WindowFocused(false) => {
                        self.ime_composing = false;
                    }
                    _ => {}
                }
            }
        });
        active
    }

    fn navigate(&mut self, down: bool) {
        if self.view == View::Quick {
            let apps = self.matching_apps().len();
            let connectors = self.matching_connectors().len();
            let files = if self.external_only() {
                0
            } else {
                self.results.as_ref().map_or(0, |r| r.results.len())
            };
            let count = apps + connectors + files;
            if count == 0 {
                return;
            }
            let current = self
                .selected_app
                .or_else(|| self.selected_connector.map(|n| n + apps))
                .or_else(|| self.selected.map(|n| n + apps + connectors));
            let next = if down {
                current.map_or(0, |n| (n + 1).min(count - 1))
            } else {
                current.unwrap_or(0).saturating_sub(1)
            };
            self.selected_app = (next < apps).then_some(next);
            self.selected_connector =
                (next >= apps && next < apps + connectors).then_some(next.saturating_sub(apps));
            self.selected =
                (next >= apps + connectors).then_some(next.saturating_sub(apps + connectors));
            return;
        }
        if let Some(count) = self
            .results
            .as_ref()
            .map(|result| result.results.len())
            .filter(|count| *count > 0)
        {
            self.selected = Some(if down {
                self.selected.map_or(0, |row| (row + 1).min(count - 1))
            } else {
                self.selected.unwrap_or(0).saturating_sub(1)
            });
        }
    }

    fn request_close(&mut self, ctx: &egui::Context) {
        if self.content.building() {
            self.content.cancel_build();
            self.close_after_content = true;
        } else if needs_defer_on_close(self.settings.as_ref(), self.wizard.is_some()) {
            self.close_after_save = true;
            if !self.saving {
                self.defer(ctx);
            }
        } else {
            if !self.close_notified {
                self.close_notified = true;
                self.emit_desktop(&RendererMessage::Closing);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn cancel_query(&mut self) {
        if let Some(cancel) = self.search_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.searching = false;
    }

    fn activity_job(&mut self, ctx: &egui::Context, edit: Option<activity::Edit>) {
        if self.activity_busy {
            return;
        }
        self.activity_busy = true;
        let dir = self.data_dir.clone();
        let ctx = ctx.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = match edit {
                Some(edit) => activity::Activity::edit(&dir, edit),
                None => activity::Activity::load(&dir),
            }
            .map_err(|error| format!("{error:#}"));
            send(&tx, &ctx, Message::Activity(result));
        });
    }

    fn activity_panel(&mut self, ctx: &egui::Context) {
        if !self.show_activity {
            return;
        }
        let mut open = true;
        let mut next = None;
        let mut edit = None;
        egui::Window::new("搜索历史与书签")
            .open(&mut open)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let query = self.query.trim();
                    if ui
                        .add_enabled(
                            !self.activity_busy && !query.is_empty(),
                            egui::Button::new("收藏当前查询"),
                        )
                        .clicked()
                    {
                        edit = Some(activity::Edit::Bookmark(query.to_owned()));
                    }
                    if ui
                        .add_enabled(
                            !self.activity_busy && !self.activity.history.is_empty(),
                            egui::Button::new("清空搜索历史"),
                        )
                        .clicked()
                    {
                        edit = Some(activity::Edit::ClearHistory);
                    }
                });
                if self.activity_busy {
                    ui.spinner();
                }
                egui::ScrollArea::vertical()
                    .max_height(380.0)
                    .show(ui, |ui| {
                        ui.label(RichText::new("查询书签").strong());
                        for saved in &self.activity.bookmarks {
                            ui.horizontal(|ui| {
                                if ui.small_button("移除").clicked() {
                                    edit = Some(activity::Edit::ForgetBookmark(saved.clone()));
                                }
                                if ui.add(egui::Button::new(saved).truncate()).clicked() {
                                    next = Some(saved.clone());
                                }
                            });
                        }
                        ui.add_space(10.0);
                        ui.label(RichText::new("最近显式搜索").strong());
                        for saved in &self.activity.history {
                            if ui.add(egui::Button::new(saved).truncate()).clicked() {
                                next = Some(saved.clone());
                            }
                        }
                    });
            });
        self.show_activity = open;
        if let Some(edit) = edit {
            self.activity_job(ctx, Some(edit));
        }
        if let Some(query) = next {
            self.query = query;
            self.query_edited();
            self.show_activity = false;
            self.focus_query = true;
            self.schedule_search(true);
        }
    }
    fn schedule_search(&mut self, immediate: bool) {
        self.cancel_query();
        self.gate.next();
        self.offset = 0;
        self.budget_ms = 2500;
        self.results = None;
        self.selected = None;
        self.selected_app = None;
        self.selected_connector = None;
        self.preview = None;
        self.search_error = None;
        self.search_due = (!self.external_only()
            && (self.view == View::Main || !self.query.trim().is_empty()))
        .then(|| {
            Instant::now()
                + if immediate {
                    Duration::ZERO
                } else {
                    Duration::from_millis(3)
                }
        });
    }
    fn reload(&mut self, ctx: &egui::Context, offer_setup: bool) {
        self.cancel_query();
        self.gate.invalidate_index();
        self.results = None;
        self.selected = None;
        self.preview = None;
        self.search_due = None;
        self.loading = true;
        self.load_request = self.load_request.wrapping_add(1);
        let request = self.load_request;
        let dir = self.data_dir.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            send(
                &tx,
                &ctx,
                Message::Loaded {
                    request,
                    result: load_state(&dir, offer_setup).map_err(|e| format!("{e:#}")),
                },
            )
        });
    }
    fn run_search(&mut self, ctx: &egui::Context) {
        self.search_due = None;
        if self.external_only() {
            self.cancel_query();
            self.search_error = None;
            self.results = None;
            return;
        }
        let Some(index) = self.index.clone() else {
            return;
        };
        self.cancel_query();
        let request = self.gate.next();
        let epoch = self.gate.epoch;
        let query = self
            .filter
            .query(&apply_smart_mode(&self.query, self.smart_mode));
        let sort = (self.view == View::Main).then_some(self.sort).flatten();
        let options = SearchOptions {
            limit: if self.view == View::Quick {
                QUICK_PAGE_SIZE
            } else {
                PAGE_SIZE
            },
            offset: self.offset,
            time_budget_ms: self.budget_ms,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.search_cancel = Some(cancel.clone());
        self.searching = true;
        self.search_error = None;
        self.results = None;
        self.selected = None;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let dir = self.data_dir.clone();
        let expected_token = self.settings.as_ref().and_then(|s| s.scope_token().ok());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let boosts = self.activity.frecency_boosts(now);
        std::thread::spawn(move || {
            let result = (|| -> Result<SearchResponse> {
                let current_settings = operations::load_settings(&dir)?;
                let current_token = current_settings.scope_token()?;
                ensure!(
                    expected_token.as_ref() == Some(&current_token),
                    "搜索范围已被其他窗口或命令行修改，请重新加载后再搜索。"
                );
                operations::validate_index_scope(&index, &current_settings)?;
                if sort.is_none() && options.offset == 0 && !query.trim().is_empty() {
                    let preview_options = SearchOptions {
                        limit: options.limit.min(16),
                        offset: 0,
                        time_budget_ms: 8,
                    };
                    let preview = if boosts.is_empty() {
                        index.search_preview(&query, &preview_options, Some(&cancel))
                    } else {
                        index.search_preview_personalized(
                            &query,
                            &preview_options,
                            Some(&cancel),
                            &boosts,
                        )
                    };
                    if let Ok(response) = preview {
                        if !response.results.is_empty() && !cancel.load(Ordering::Relaxed) {
                            ensure!(
                                operations::load_settings(&dir)?.scope_token()? == current_token,
                                "查询期间搜索范围已更改，本次结果已丢弃。请重新加载。"
                            );
                            send(
                                &tx,
                                &ctx,
                                Message::SearchPreview {
                                    request,
                                    epoch,
                                    response,
                                },
                            );
                        }
                    }
                }
                let response = match sort {
                    Some(sort) => index.search_sorted(&query, &options, Some(&cancel), sort)?,
                    None if !boosts.is_empty() => {
                        index.search_personalized(&query, &options, Some(&cancel), &boosts)?
                    }
                    None => index.search(&query, &options, Some(&cancel))?,
                };
                ensure!(
                    operations::load_settings(&dir)?.scope_token()? == current_token,
                    "查询期间搜索范围已更改，本次结果已丢弃。请重新加载。"
                );
                Ok(response)
            })()
            .map_err(|e| format!("{e:#}"));
            if !cancel.load(Ordering::Relaxed) {
                send(
                    &tx,
                    &ctx,
                    Message::Search {
                        request,
                        epoch,
                        result,
                    },
                );
            }
        });
    }
    fn scan(&mut self, ctx: &egui::Context) {
        if self.scan_cancel.is_some() || self.settings.as_ref().is_none_or(|s| s.roots.is_empty()) {
            return;
        }
        self.mark_scan();
        if self.desktop_commands.is_some() {
            self.emit_desktop(&RendererMessage::StartScan);
            return;
        }
        let request = self.scan_request;
        let cancel = self.scan_cancel.as_ref().unwrap().clone();
        let dir = self.data_dir.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = run_scan(&dir, &cancel, |progress| {
                send(&tx, &ctx, Message::Progress { request, progress });
            });
            send(&tx, &ctx, Message::Scan { request, result });
        });
    }
    fn mark_scan(&mut self) {
        if self.scan_cancel.is_none() {
            self.scan_request = self.scan_request.wrapping_add(1);
            self.scan_cancel = Some(Arc::new(AtomicBool::new(false)));
            self.scan_started = Some(Instant::now());
            self.scan_progress = None;
            self.error = None;
            self.notice = Some("正在扫描已配置的本地范围；可继续搜索当前索引。".into());
        }
    }
    fn save(&mut self, ctx: &egui::Context, candidate: Settings, action: SaveAction) {
        if self.saving {
            return;
        }
        self.saving = true;
        let expected = if self.persisted {
            self.settings.clone()
        } else {
            None
        };
        let dir = self.data_dir.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = save_settings(&dir, expected.as_ref(), &candidate, action)
                .map_err(|e| format!("{e:#}"));
            send(&tx, &ctx, Message::Saved { action, result });
        });
    }
    fn defer(&mut self, ctx: &egui::Context) {
        if self.saving {
            return;
        }
        let Some(mut candidate) = self.settings.clone() else {
            self.wizard = None;
            return;
        };
        // Closing an already configured settings editor keeps its last committed
        // setup state. An unfinished onboarding is explicitly Deferred.
        if candidate.setup_state == SetupState::Completed {
            self.wizard = None;
            return;
        }
        candidate.defer_setup();
        self.save(ctx, candidate, SaveAction::Defer);
    }
    fn open_selected(&mut self, ctx: &egui::Context) {
        if self.view == View::Quick {
            if let Some(row) = self.selected_app {
                self.launch_app(row, ctx);
                return;
            }
            if let Some(row) = self.selected_connector {
                self.open_connector(row, ctx);
                return;
            }
        }
        self.result_action(ctx, ResultAction::Open);
    }
    #[cfg(windows)]
    fn drag_result(&mut self, row: usize) {
        let result = (|| -> Result<()> {
            let window = self.native_window.context("当前窗口无法启动文件拖放")?;
            let index = self.index.as_ref().context("索引正在重新加载")?;
            let response = self.results.as_ref().context("结果已更新，请重新拖动")?;
            let hit = response
                .results
                .get(row)
                .context("结果已更新，请重新拖动")?;
            ensure!(
                response.generation == index.generation(),
                "结果代次已更新，请重新拖动"
            );
            let path = authorize_drag(&self.data_dir, index, hit)?;
            desktop::windows::drag_local_file(window, &path)
        })();
        if let Err(error) = result {
            self.error = Some(format!("拖放失败：{error:#}"));
        }
    }
    fn result_action(&mut self, ctx: &egui::Context, action: ResultAction) {
        if self.opening {
            return;
        }
        let Some(hit) = self
            .selected
            .and_then(|n| self.results.as_ref()?.results.get(n))
            .cloned()
        else {
            return;
        };
        if matches!(action, ResultAction::Open) && hit.source == "local" {
            self.activity_job(ctx, Some(activity::Edit::RecordLaunch(hit.path.clone())));
        }
        self.opening = true;
        let Some(index) = self.index.clone() else {
            self.opening = false;
            return;
        };
        let dir = self.data_dir.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let (request, epoch) = (self.gate.request, self.gate.epoch);
        std::thread::spawn(move || {
            let result =
                execute_result_action(&dir, &index, &hit, action).map_err(|e| format!("{e:#}"));
            send(
                &tx,
                &ctx,
                Message::ResultAction {
                    request,
                    epoch,
                    result,
                },
            );
        });
    }
    fn messages(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Message::Activity(result) => {
                    self.activity_busy = false;
                    match result {
                        Ok(activity) => self.activity = activity,
                        Err(error) => self.error = Some(format!("无法更新搜索历史或书签：{error}")),
                    }
                }
                Message::ExternalChange => self.reload(ctx, false),
                Message::VolumeCards(volumes) => {
                    if let Some(wizard) = &mut self.wizard {
                        wizard.volumes = Some(volumes);
                    }
                }
                Message::PickedFolder(picked) => {
                    self.picking_folder = false;
                    if let (Some(path), Some(wizard)) = (picked, &mut self.wizard) {
                        wizard.draft_path = path;
                        wizard.error = None;
                    }
                }
                Message::Loaded { request, result } if request == self.load_request => {
                    self.loading = false;
                    match result {
                        Ok(loaded) => {
                            let content_changed = self
                                .settings
                                .as_ref()
                                .and_then(|settings| settings.content_scope_token().ok())
                                != loaded.settings.content_scope_token().ok();
                            if content_changed {
                                self.content.release();
                            }
                            self.settings = Some(loaded.settings);
                            self.persisted = loaded.persisted;
                            self.index = loaded.index;
                            if loaded.notice.is_some() {
                                self.notice = loaded.notice;
                            }
                            self.error = None;
                            if loaded.offer_setup {
                                self.start_wizard(ctx, true);
                            }
                            if self.content_tab {
                                if let Some(query) = self.initial_content_query.take() {
                                    self.content.start(query, &self.data_dir, ctx);
                                }
                            } else {
                                self.schedule_search(true);
                            }
                        }
                        Err(e) => {
                            self.index = None;
                            self.settings = None;
                            self.error = Some(format!("配置加载失败：{e}"));
                        }
                    }
                }
                Message::Search {
                    request,
                    epoch,
                    result,
                } if self.gate.accepts(request, epoch) => {
                    self.searching = false;
                    self.search_cancel = None;
                    match result {
                        Ok(response) => {
                            let key = self
                                .selected
                                .and_then(|row| self.results.as_ref()?.results.get(row))
                                .map(|hit| hit.key());
                            self.selected = key.and_then(|key| {
                                response.results.iter().position(|hit| {
                                    hit.path == key.path && hit.source == key.source
                                })
                            });
                            // Frecency 已由索引内核在全局 Top-K 前计算，无须分页后二次重排。
                            self.results = Some(response);
                        }
                        Err(e) => {
                            self.results = None;
                            self.search_error = Some(e);
                        }
                    }
                }
                Message::SearchPreview {
                    request,
                    epoch,
                    response,
                } if self.gate.accepts(request, epoch) => {
                    self.results = Some(response);
                    self.selected = None;
                }
                Message::Progress { request, progress }
                    if request == self.scan_request && self.scan_cancel.is_some() =>
                {
                    self.scan_progress = Some(progress)
                }
                Message::Scan { request, result } if request == self.scan_request => {
                    self.scan_cancel = None;
                    self.scan_progress = None;
                    self.scan_started = None;
                    match result {
                        Ok(summary) if summary.published && summary.coverage_complete => {
                            self.reload(ctx, false);
                            let message = format!(
                                "扫描完成：{} 项，耗时 {:.1} 秒，按策略跳过 {} 项。",
                                summary.indexed,
                                summary.elapsed_ms / 1000.0,
                                summary.skipped
                            );
                            self.notice = Some(match summary.durability_warning {
                                Some(warning) => {
                                    format!("{message} 新索引已发布，落盘持久性未确认：{warning}")
                                }
                                None => message,
                            });
                        }
                        Ok(summary) if summary.published => {
                            self.reload(ctx, false);
                            self.notice = Some(format!(
                                "{}，已发布的 {} 项仍可搜索，收录范围尚未覆盖完整。",
                                if summary.cancelled {
                                    "扫描已取消"
                                } else {
                                    "扫描未完整完成"
                                },
                                summary.indexed,
                            ));
                            if !summary.errors.is_empty() {
                                self.error = Some(summary.errors.join("\n"));
                            }
                        }
                        Ok(summary) => {
                            self.notice = Some(if summary.cancelled {
                                "扫描已取消，未替换已保存的索引。若范围已更改，请重新扫描。".into()
                            } else {
                                "扫描未完整完成，未替换原索引。".into()
                            });
                            if !summary.errors.is_empty() {
                                self.error = Some(summary.errors.join("\n"));
                            }
                        }
                        Err(e) => self.error = Some(format!("扫描失败，原索引未替换：{e}")),
                    }
                }
                Message::Saved { action, result } => {
                    self.saving = false;
                    match result {
                        Ok(saved) => {
                            let settings = saved.settings;
                            let changed = self.settings.as_ref().and_then(|s| s.scope_token().ok())
                                != settings.scope_token().ok();
                            let content_changed = self
                                .settings
                                .as_ref()
                                .and_then(|s| s.content_scope_token().ok())
                                != settings.content_scope_token().ok();
                            if changed || content_changed {
                                self.preview = None;
                                self.cancel_query();
                                self.gate.invalidate_index();
                            }
                            if content_changed {
                                self.content.release();
                            }
                            self.settings = Some(settings);
                            self.persisted = true;
                            if !matches!(action, SaveAction::Content) {
                                self.wizard = None;
                            } else {
                                self.notice =
                                    Some("正文范围已保存，更新正文索引后即可搜索。".into());
                            }
                            if matches!(action, SaveAction::Apply) && !self.close_after_save {
                                if changed {
                                    self.index = None;
                                    self.results = None;
                                    self.selected = None;
                                }
                                self.scan(ctx);
                            } else if content_changed && !self.content_tab {
                                self.schedule_search(true);
                            }
                            if let Some(warning) = saved.warning {
                                self.notice = Some(format!("配置已保存：{warning}"));
                            }
                        }
                        Err(e) => {
                            self.close_after_save = false;
                            if let Some(wizard) = self.wizard.as_mut() {
                                wizard.error = Some(e);
                            } else {
                                self.error = Some(e);
                            }
                        }
                    }
                }
                Message::ResultAction {
                    request,
                    epoch,
                    result,
                } => {
                    self.opening = false;
                    if self.gate.accepts(request, epoch) {
                        match result {
                            Err(error) => self.error = Some(format!("操作未完成：{error}")),
                            Ok(ActionOutcome::Opened) if self.view == View::Quick => {
                                self.request_close(ctx)
                            }
                            Ok(ActionOutcome::Opened) => {}
                            Ok(ActionOutcome::CopyPath(path)) => ctx.copy_text(path),
                            Ok(ActionOutcome::Preview(Preview { name, content, .. })) => {
                                let content = match content {
                                    PreviewContent::Text { text, truncated } => {
                                        PreviewContent::Text { text, truncated }
                                    }
                                    PreviewContent::Image(image) => {
                                        PreviewContent::Image(ctx.load_texture(
                                            "本地图片预览",
                                            image,
                                            egui::TextureOptions::LINEAR,
                                        ))
                                    }
                                };
                                self.preview = Some(Preview {
                                    name,
                                    content,
                                    find_query: String::new(),
                                    find_index: 0,
                                });
                            }
                        }
                    }
                }
                _ => {} // An obsolete request must never overwrite newer UI state.
            }
        }
    }
    fn setup(&mut self, ctx: &egui::Context) {
        if self.saving || self.scan_cancel.is_some() {
            return;
        }
        self.start_wizard(ctx, false);
    }
    fn start_wizard(&mut self, ctx: &egui::Context, welcome: bool) {
        if let Some(settings) = &self.settings {
            self.wizard = Some(Wizard::new(settings, welcome));
            if !self.smoke_test {
                let tx = self.tx.clone();
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    send(&tx, &ctx, Message::VolumeCards(discover_volume_cards()));
                });
            }
        }
    }
    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("app_header")
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(18, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.image((self.brand_icon.id(), Vec2::splat(28.0)));
                    ui.add_space(8.0);
                    ui.label(RichText::new("寻枢").size(19.0).strong().color(INK));
                    ui.separator();
                    ui.label(RichText::new("本地文件搜索").color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                self.settings.is_some()
                                    && self.scan_cancel.is_none()
                                    && !self.saving,
                                egui::Button::new("搜索范围与设置")
                                    .min_size(Vec2::new(116.0, 28.0)),
                            )
                            .clicked()
                        {
                            self.setup(ctx);
                        }
                        ui.add_space(10.0);
                        if ui.button("快捷搜索").clicked() {
                            self.switch_view(ctx, View::Quick, None);
                        }
                        if ui.button("本地工具").clicked() {
                            self.utility.open = true;
                        }
                        if ui
                            .toggle_value(&mut self.high_contrast, "高对比度")
                            .changed()
                        {
                            configure_visuals(ctx, self.high_contrast);
                        }
                        let was_content = self.content_tab;
                        ui.selectable_value(&mut self.content_tab, true, "正文");
                        ui.selectable_value(&mut self.content_tab, false, "文件名");
                        if was_content != self.content_tab {
                            if self.content_tab {
                                self.cancel_query();
                                self.search_due = None;
                            } else {
                                self.content.release();
                                self.focus_query = true;
                                self.schedule_search(true);
                            }
                        }
                    });
                });
            });
    }
    fn footer(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(24, 9)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("本地索引").small().color(MUTED));
                    if let Some(index) = &self.index {
                        ui.separator();
                        ui.label(
                            RichText::new(format!(
                                "{} 项 · 索引 {} · {}{}",
                                index.len(),
                                index.generation(),
                                size_label(index.storage_bytes() as u64),
                                if index.coverage_complete() {
                                    ""
                                } else {
                                    " · 覆盖未完成"
                                },
                            ))
                            .small()
                            .color(MUTED),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !self.loading && !self.saving && self.scan_cancel.is_none(),
                                egui::Button::new("重新加载").small(),
                            )
                            .on_hover_text("加载其他窗口或命令行已保存的配置与索引")
                            .clicked()
                        {
                            self.reload(ctx, false);
                        }
                        ui.label(RichText::new("数据目录").small().color(MUTED))
                            .on_hover_text(self.data_dir.display().to_string());
                        if ui.small_button("索引状态").clicked() {
                            self.diagnostics.open(ctx, &self.data_dir);
                        }
                        if ui.button("应用与连接器").clicked() {
                            self.connectors.toggle();
                        }
                    });
                });
            });
    }
    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(152.0)
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(13, 16)),
            )
            .show(ctx, |ui| {
                ui.label(RichText::new("工作空间").size(12.0).color(MUTED));
                ui.add_space(8.0);
                egui::Frame::new()
                    .fill(SOFT)
                    .corner_radius(8)
                    .inner_margin(9)
                    .show(ui, |ui| {
                        ui.set_min_width(108.0);
                        ui.horizontal(|ui| {
                            magnifier(ui, 18.0, ACCENT);
                            ui.label(RichText::new("搜索文件").color(ACCENT).strong());
                        });
                    });
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("已收录范围").small().color(MUTED));
                    if let Some(s) = &self.settings {
                        ui.label(
                            RichText::new(s.roots.len().to_string())
                                .small()
                                .color(ACCENT),
                        );
                    }
                });
                ui.add_space(10.0);
                let roots = self
                    .settings
                    .as_ref()
                    .map(|s| s.roots.clone())
                    .unwrap_or_default();
                if roots.is_empty() {
                    ui.label(RichText::new("还没有添加目录").size(13.0).color(MUTED));
                }
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_roots")
                    .max_height(250.0)
                    .show(ui, |ui| {
                        for scope in &roots {
                            let name = Path::new(&scope.path)
                                .file_name()
                                .and_then(|x| x.to_str())
                                .unwrap_or(&scope.path);
                            ui.label(RichText::new(shorten(name, 18)).size(13.0).color(INK))
                                .on_hover_text(&scope.path);
                            ui.add_space(8.0);
                        }
                    });
                ui.add_space(16.0);
                if ui
                    .add_enabled(
                        !roots.is_empty()
                            && self.scan_cancel.is_none()
                            && !self.loading
                            && !self.saving,
                        egui::Button::new("重新扫描范围").min_size(Vec2::new(114.0, 30.0)),
                    )
                    .clicked()
                {
                    self.scan(ctx);
                }
                ui.add_space(15.0);
                ui.separator();
                ui.add_space(10.0);
                if ui.small_button("搜索语法").clicked() {
                    self.show_help = !self.show_help;
                }
                if ui.small_button("搜索历史与书签").clicked() {
                    self.show_activity = true;
                    self.activity_job(ctx, None);
                }
            });
    }
    fn content_panel(&mut self, ctx: &egui::Context) {
        let candidate = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(28, 24)),
            )
            .show(ctx, |ui| {
                self.settings
                    .as_ref()
                    .and_then(|settings| self.content.ui(ui, &self.data_dir, settings, self.saving))
            })
            .inner;
        if let Some(candidate) = candidate {
            self.content.release();
            self.save(ctx, candidate, SaveAction::Content);
        }
    }
    fn quick_panel(&mut self, ctx: &egui::Context, ime_active: bool) {
        let (down, up, enter, escape) = if ime_active {
            (false, false, false, false)
        } else {
            ctx.input_mut(|input| {
                let mut key = |key| input.consume_key(egui::Modifiers::NONE, key);
                (
                    key(egui::Key::ArrowDown),
                    key(egui::Key::ArrowUp),
                    key(egui::Key::Enter),
                    key(egui::Key::Escape),
                )
            })
        };
        if down {
            self.navigate(true);
        } else if up {
            self.navigate(false);
        }
        if escape {
            self.request_close(ctx);
        }
        let mut open = false;
        let mut show_main = false;
        let mut changed = false;
        let mut launch_app_row = None;
        let mut open_connector_row = None;
        let external_only = self.external_only();
        let app_hits = self.matching_apps();
        let connector_hits = self.matching_connectors();
        egui::TopBottomPanel::bottom("quick_footer")
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(16, 9)),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(if self.searching {
                        "正在搜索…"
                    } else {
                        "方向键选择 · 回车打开 · app:/bm:/git:/src:限定来源 · Esc 隐藏"
                    })
                    .small()
                    .color(MUTED),
                );
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(18, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    magnifier(ui, 18.0, ACCENT);
                    ui.label(RichText::new("快捷搜索").strong().size(16.0).color(INK));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("隐藏").clicked() {
                            self.request_close(ctx);
                        }
                        if ui.small_button("主窗口").clicked() {
                            show_main = true;
                        }
                    });
                });
                ui.add_space(8.0);
                let edit = ui.add_sized(
                    [ui.available_width(), 38.0],
                    egui::TextEdit::singleline(&mut self.query)
                        .id_salt("quick_query")
                        .font(FontId::proportional(18.0))
                        .hint_text("文件/应用/书签/仓库/来源；app:/bm:/git:/src:"),
                );
                if self.focus_query {
                    self.focus_query = false;
                    edit.request_focus();
                }
                changed = edit.changed();
                ui.add_space(8.0);
                ui.separator();
                if !app_hits.is_empty() {
                    ui.label(RichText::new("本机应用").small().color(ACCENT));
                    for (row, app) in app_hits.iter().enumerate() {
                        let response = ui.add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::selectable(
                                self.selected_app == Some(row),
                                format!("{}  ·  应用", app.name),
                            )
                            .truncate(),
                        );
                        if response.clicked() {
                            self.selected_app = Some(row);
                            self.selected_connector = None;
                            self.selected = None;
                            edit.request_focus();
                        }
                        if response.double_clicked() {
                            launch_app_row = Some(row);
                        }
                    }
                    ui.separator();
                }
                if !connector_hits.is_empty() {
                    ui.label(RichText::new("已授权本地来源").small().color(ACCENT));
                    for (row, item) in connector_hits.iter().enumerate() {
                        let kind = if item.source == "connector:git_repo" {
                            "仓库"
                        } else if item.source.starts_with("plugin:") {
                            "本地来源"
                        } else {
                            "书签"
                        };
                        let response = ui.add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::selectable(
                                self.selected_connector == Some(row),
                                format!("{}  ·  {kind}", item.title),
                            )
                            .truncate(),
                        );
                        if response.clicked() {
                            self.selected_connector = Some(row);
                            self.selected_app = None;
                            self.selected = None;
                            edit.request_focus();
                        }
                        if response.double_clicked() {
                            open_connector_row = Some(row);
                        }
                    }
                    ui.separator();
                }
                if let Some(error) = self.search_error.as_ref().or(self.error.as_ref()) {
                    ui.colored_label(ERROR, format!("操作未完成：{error}"));
                } else if external_only {
                    if app_hits.is_empty() && connector_hits.is_empty() {
                        empty_state(
                            ui,
                            "没有匹配的已载入条目",
                            "可在应用启动器与连接器面板刷新应用或明确载入书签、仓库。",
                            false,
                        );
                    }
                } else if self.loading {
                    empty_state(ui, "正在加载索引", "稍候即可搜索已收录的文件。", true);
                } else if self.index.is_none() {
                    empty_state(ui, "还没有可用索引", "在主窗口设置范围并扫描文件。", false);
                } else if self.query.trim().is_empty() {
                    empty_state(
                        ui,
                        "搜索本地文件",
                        "例如：项目计划、ext:pdf、pinyin:baogao",
                        false,
                    );
                } else if let Some(results) = &self.results {
                    if results.results.is_empty() {
                        let title = if results.complete {
                            "没有匹配的文件"
                        } else {
                            "查询尚未完成"
                        };
                        empty_state(ui, title, "可以缩短关键词或在主窗口继续查看。", false);
                    } else {
                        let mut selected = self.selected;
                        egui::ScrollArea::vertical()
                            .id_salt("quick_results")
                            .show(ui, |ui| {
                                for (row, hit) in results.results.iter().enumerate() {
                                    let title = format!(
                                        "{}  ·  {}",
                                        hit.name,
                                        if hit.is_dir { "文件夹" } else { "文件" }
                                    );
                                    let response = ui
                                        .add_sized(
                                            [ui.available_width(), 34.0],
                                            egui::Button::selectable(selected == Some(row), title)
                                                .truncate(),
                                        )
                                        .on_hover_text(&hit.path);
                                    if response.clicked() {
                                        selected = Some(row);
                                        self.selected_app = None;
                                        self.selected_connector = None;
                                        edit.request_focus();
                                    }
                                    if response.double_clicked() {
                                        selected = Some(row);
                                        self.selected_app = None;
                                        self.selected_connector = None;
                                        open = true;
                                    }
                                    if selected == Some(row) && (down || up) {
                                        response.scroll_to_me(None);
                                    }
                                }
                            });
                        self.selected = selected;
                        if !results.coverage_complete || !results.complete {
                            let status = if !results.coverage_complete {
                                "索引尚未覆盖全部收录范围"
                            } else if self.searching {
                                "正在补全结果"
                            } else {
                                "仅显示已核实的部分结果"
                            };
                            ui.label(RichText::new(status).small().color(MUTED));
                        }
                    }
                }
            });
        if changed {
            self.query_edited();
            self.schedule_search(false);
        } else if enter && !self.query.trim().is_empty() {
            if self.selected_app.is_none()
                && self.selected_connector.is_none()
                && self.selected.is_none()
                && external_only
                && !app_hits.is_empty()
            {
                self.selected_app = Some(0);
            }
            if self.selected_app.is_none()
                && self.selected_connector.is_none()
                && self.selected.is_none()
                && external_only
                && !connector_hits.is_empty()
            {
                self.selected_connector = Some(0);
            }
            if self.selected_app.is_none()
                && self.selected_connector.is_none()
                && self.selected.is_none()
                && self
                    .results
                    .as_ref()
                    .is_some_and(|result| !result.results.is_empty())
            {
                self.selected = Some(0);
            }
            open |= self.selected.is_some()
                || self.selected_app.is_some()
                || self.selected_connector.is_some();
            if !open && !self.searching {
                self.schedule_search(true);
            }
        }
        if show_main {
            self.switch_view(ctx, View::Main, Some(self.query.clone()));
        } else if let Some(row) = launch_app_row {
            if !changed {
                self.launch_app(row, ctx);
            }
        } else if let Some(row) = open_connector_row {
            if !changed {
                self.open_connector(row, ctx);
            }
        } else if open && !changed {
            self.open_selected(ctx);
        }
    }

    fn search_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("search_bar")
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(18, 14)),
            )
            .show(ctx, |ui| {
                let mut changed = false;
                let mut immediate = false;
                ui.horizontal(|ui| {
                    let width = ui.available_width();
                    egui::Frame::new()
                        .fill(BACKGROUND)
                        .stroke(ui.visuals().widgets.inactive.bg_stroke)
                        .corner_radius(8)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            magnifier(ui, 20.0, ACCENT);
                            let edit = ui.add_sized(
                                [(width - 182.0).max(160.0), 26.0],
                                egui::TextEdit::singleline(&mut self.query)
                                    .id_salt("main_query")
                                    .font(FontId::proportional(19.0))
                                    .hint_text("搜索文件名、路径或扩展名")
                                    .frame(false),
                            );
                            changed = edit.changed();
                            if self.focus_query
                                || ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::K))
                            {
                                self.focus_query = false;
                                edit.request_focus();
                            }
                            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                immediate = true;
                                edit.request_focus();
                            }
                            if ui
                                .add_enabled(!self.query.is_empty(), egui::Button::new("×"))
                                .on_hover_text("清空搜索")
                                .clicked()
                            {
                                self.query.clear();
                                changed = true;
                                edit.request_focus();
                            }
                        });
                    if ui.add_sized([88.0, 44.0], primary("搜索")).clicked() {
                        immediate = true;
                    }
                });
                if changed {
                    self.query_edited();
                }
                if immediate && !self.query.trim().is_empty() {
                    self.activity_job(ctx, Some(activity::Edit::Remember(self.query.clone())));
                }
                if changed || immediate {
                    self.schedule_search(immediate);
                }
            });
    }

    fn main_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::WHITE).inner_margin(egui::Margin::symmetric(18, 14))).show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("类型").small().color(MUTED));
                let enabled = !self.external_only();
                for filter in Filter::OPTIONS {
                    let mut button = ui.add_enabled(
                        enabled,
                        egui::Button::selectable(self.filter == filter, filter.label()),
                    );
                    if filter == Filter::Other {
                        button = button.on_hover_text("其他扩展名与无扩展名的文件");
                    }
                    if button.clicked() && self.filter != filter {
                        self.filter = filter;
                        self.schedule_search(true);
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("全局排序").small().color(MUTED));
                let previous = self.sort;
                if ui.selectable_label(self.sort.is_none(), "相关度").clicked() { self.sort = None; }
                for (column, label) in [(SortColumn::Name, "名称"), (SortColumn::Path, "路径"), (SortColumn::Size, "大小"), (SortColumn::Modified, "修改时间")] {
                    let current = self.sort.filter(|sort| sort.column == column);
                    let label = match current {
                        Some(sort) if sort.descending => format!("{label} ↓"),
                        Some(_) => format!("{label} ↑"),
                        None => label.to_owned(),
                    };
                    if ui.selectable_label(current.is_some(), label).clicked() {
                        self.sort = Some(ColumnSort { column, descending: current.is_some_and(|sort| !sort.descending) });
                    }
                }
                if self.sort != previous { self.schedule_search(true); }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.searching { ui.spinner(); ui.label(RichText::new("搜索中").small().color(MUTED)); }
                    else if let Some(r) = &self.results { ui.label(RichText::new(format!("{}{} 项 · {:.1} ms", if r.complete { "" } else { "至少 " }, r.total, r.elapsed_ms)).small().color(MUTED)); }
                });
            });
            if self.show_help {
                ui.add_space(12.0);
                egui::Frame::new().fill(BACKGROUND).corner_radius(8).inner_margin(12).show(ui, |ui| {
                    ui.label(RichText::new("例：报告　ext:pdf　path:项目　pinyin:baogao　app:记事本　bm:Rust　git:项目").size(12.0));
                    ui.label(RichText::new("空格组合条件，| 表示或，! 排除，< > 分组。").small().color(MUTED));
                });
            }
            if !self.cjk_font { ui.colored_label(ERROR, "缺少中文字体。请修复安装，或通过 --font 指定本地字体。"); }
            if let Some(error) = self.search_error.clone() { ui.add_space(10.0); ui.colored_label(ERROR, format!("搜索未完成：{error}")); }
            if let Some(error) = self.error.clone() {
                ui.add_space(12.0);
                egui::Frame::new().fill(Color32::from_rgb(255, 244, 243)).corner_radius(8).inner_margin(10).show(ui, |ui| {
                    ui.horizontal(|ui| { ui.colored_label(ERROR, "操作未完成"); if ui.small_button("收起").clicked() { self.error = None; } });
                    egui::ScrollArea::vertical().id_salt("operation_error").max_height(90.0).show(ui, |ui| { ui.label(RichText::new(error).size(12.0).color(ERROR)); });
                });
            }
            if let Some(notice) = &self.notice { ui.add_space(10.0); ui.label(RichText::new(notice).size(12.0).color(MUTED)); }
            self.scan_panel(ui);
            let app_hits = self.matching_apps();
            let connector_hits = self.matching_connectors();
            if !app_hits.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new("本机应用 · 点击启动").strong().color(ACCENT));
                let mut launch = None;
                for (row, app) in app_hits.iter().enumerate() {
                    if ui.button(format!("{}  ·  应用", app.name)).clicked() {
                        launch = Some(row);
                    }
                }
                if let Some(row) = launch {
                    self.launch_app(row, ctx);
                }
            }
            if !connector_hits.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new("已授权本地来源 · 点击打开").strong().color(ACCENT));
                let mut open_item = None;
                for (row, item) in connector_hits.iter().enumerate() {
                    let kind = if item.source == "connector:git_repo" { "仓库" }
                        else if item.source.starts_with("plugin:") { "本地来源" }
                        else { "书签" };
                    if ui.button(format!("{}  ·  {kind}", item.title)).clicked() {
                        open_item = Some(row);
                    }
                }
                if let Some(row) = open_item { self.open_connector(row, ctx); }
            }
            ui.add_space(12.0); ui.separator();
            if self.external_only() {
                if app_hits.is_empty() && connector_hits.is_empty() {
                    empty_state(ui, "未找到已载入条目", "可在应用启动器与连接器面板刷新应用或明确载入书签、仓库。", false);
                }
            }
            else if self.loading { empty_state(ui, "正在加载索引", "索引加载在后台进行。", true); }
            else if self.index.is_none() {
                let configured = self.settings.as_ref().is_some_and(|s|!s.roots.is_empty());
                empty_state(ui, if configured { "准备建立你的搜索索引" } else { "先选择你想搜索的目录" }, if configured { "首批收录提交后即可搜索，后续范围会继续扫描。" } else { "明确选择本地范围，文件信息保存在本机。" }, self.scan_cancel.is_some());
                ui.vertical_centered(|ui| { if ui.add_enabled(self.settings.is_some() && self.scan_cancel.is_none(), primary(if configured { "开始扫描" } else { "设置搜索范围" })).clicked() { if configured { self.scan(ctx) } else { self.setup(ctx) } } });
            } else if let Some(result) = &self.results {
                if result.results.is_empty() {
                    let complete = result.complete;
                    empty_state(ui, if complete { "没有匹配的文件" } else { "查询尚未完成" }, if complete { "试试更短的关键词，或检查搜索范围与类型。" } else { "已到达本次查询时间预算，尚未找到匹配项。可继续查询或缩小条件。" }, false);
                    ui.vertical_centered(|ui| {
                        if !complete && self.budget_ms < 60_000 && ui.button("继续完整查询").clicked() { self.budget_ms = (self.budget_ms * 4).min(60_000); self.run_search(ctx); }
                        if self.offset > 0 && ui.button("返回第一页").clicked() { self.offset = 0; self.run_search(ctx); }
                    });
                } else { self.results_workspace(ui, ctx); }
            } else if self.searching { empty_state(ui, "正在搜索", "输入新关键词可立即取消旧查询。", true); }
        });
    }
    fn scan_panel(&mut self, ui: &mut egui::Ui) {
        if let Some(cancel) = self.scan_cancel.clone() {
            ui.add_space(12.0);
            egui::Frame::new()
                .fill(SOFT)
                .corner_radius(8)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let elapsed = self.scan_started.map_or(0.0, |t| t.elapsed().as_secs_f32());
                        ui.label(
                            RichText::new(if cancel.load(Ordering::Relaxed) {
                                "正在取消扫描…".to_owned()
                            } else {
                                format!("扫描中 · {elapsed:.1} 秒")
                            })
                            .color(ACCENT)
                            .strong(),
                        );
                        if let Some(progress) = &self.scan_progress {
                            ui.label(
                                RichText::new(format!(
                                    "已访问 {} · 已收录 {} · 错误 {}",
                                    progress.visited, progress.indexed, progress.errors
                                ))
                                .size(12.0),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(
                                    !cancel.load(Ordering::Relaxed),
                                    egui::Button::new("取消"),
                                )
                                .clicked()
                            {
                                cancel.store(true, Ordering::Relaxed);
                                self.emit_desktop(&RendererMessage::CancelScan);
                            }
                        });
                    });
                    if let Some(path) = self
                        .scan_progress
                        .as_ref()
                        .and_then(|p| p.current_path.as_ref())
                    {
                        ui.label(RichText::new(shorten(path, 110)).size(12.0).color(MUTED))
                            .on_hover_text(path);
                    }
                });
        }
    }
    fn results_workspace(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let width = ui.available_width();
        if width < 800.0 {
            self.result_list(ui, ctx, false);
            return;
        }

        // 宽窗口同时显示结果和所选文件的详情，窄窗口保留行尾操作。
        let details_width = (width * 0.31).clamp(266.0, 370.0);
        let height = ui.available_height();
        ui.horizontal_top(|ui| {
            let list_width = width - details_width - 13.0;
            ui.allocate_ui_with_layout(
                Vec2::new(list_width, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(list_width);
                    self.result_list(ui, ctx, true);
                },
            );
            ui.separator();
            ui.allocate_ui_with_layout(
                Vec2::new(details_width, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(details_width);
                    self.file_details(ui, ctx);
                },
            );
        });
    }

    fn file_details(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(hit) = self
            .selected
            .and_then(|row| self.results.as_ref()?.results.get(row))
            .cloned()
        else {
            ui.add_space(12.0);
            ui.label(RichText::new("文件详情").strong().color(INK));
            ui.label(RichText::new("选择一条结果查看详情和操作。").color(MUTED));
            return;
        };
        let local = hit.source == "local";
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("file_details")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(12.0);
                ui.label(
                    RichText::new(file_kind(&hit))
                        .small()
                        .strong()
                        .color(ACCENT),
                );
                ui.label(RichText::new(&hit.name).size(20.0).strong().color(INK))
                    .on_hover_text(&hit.name);
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if hit.is_dir {
                        modified_label(hit.modified)
                    } else {
                        format!(
                            "{} · {}",
                            size_label(hit.size),
                            modified_label(hit.modified)
                        )
                    })
                    .color(MUTED),
                );
                ui.add_space(10.0);
                ui.add_enabled_ui(!self.opening, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(local, primary("打开")).clicked() {
                            action = Some(ResultAction::Open);
                        }
                        if ui
                            .add_enabled(local, egui::Button::new("所在文件夹"))
                            .clicked()
                        {
                            action = Some(ResultAction::OpenParent);
                        }
                        if ui
                            .add_enabled(
                                local
                                    && !hit.is_dir
                                    && preview_format(Path::new(&hit.path)).is_some(),
                                egui::Button::new("预览"),
                            )
                            .clicked()
                        {
                            action = Some(ResultAction::Preview);
                        }
                    });
                });
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(RichText::new("完整路径").small().strong().color(MUTED));
                ui.add(egui::Label::new(&hit.path).wrap())
                    .on_hover_text(&hit.path);
                if ui
                    .add_enabled(!self.opening, egui::Button::new("复制路径"))
                    .clicked()
                {
                    action = Some(ResultAction::CopyPath);
                }
                ui.add_space(12.0);
                ui.label(
                    RichText::new(if local {
                        "本地文件系统"
                    } else {
                        "离线目录"
                    })
                    .small()
                    .color(MUTED),
                );
            });
        if let Some(action) = action {
            self.result_action(ctx, action);
        }
    }

    fn result_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, inspector: bool) {
        let response = self.results.as_ref().unwrap();
        let total = response.total;
        let complete = response.complete;
        let coverage_complete = response.coverage_complete;
        let count = response.results.len();
        let items = &response.results;
        ui.add_space(5.0);
        let table = ui.available_width() >= 485.0;
        let (header, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 27.0), egui::Sense::hover());
        let labels = if table {
            ["名称", "所在位置", "类型", "大小", "修改时间"].as_slice()
        } else {
            ["名称 / 所在位置"].as_slice()
        };
        for (i, label) in labels.iter().enumerate() {
            ui.painter().text(
                egui::pos2(result_columns(header)[i], header.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                FontId::proportional(12.0),
                MUTED,
            );
        }
        let mut action = None;
        let mut selected = self.selected;
        #[cfg(windows)]
        let mut dragged = None;
        let list_height = (ui.available_height() - if inspector { 70.0 } else { 108.0 }).max(100.0);
        let row_height = if table { 42.0 } else { 51.0 };
        egui::ScrollArea::vertical()
            .id_salt("results")
            .max_height(list_height)
            .auto_shrink([false, false])
            .show_rows(ui, row_height, count, |ui, rows| {
                for row in rows {
                    let hit = &items[row];
                    #[cfg(windows)]
                    let can_drag = hit.source == "local" && !hit.is_dir && !self.opening;
                    #[cfg(not(windows))]
                    let can_drag = false;
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), row_height),
                        if can_drag {
                            egui::Sense::click_and_drag()
                        } else {
                            egui::Sense::click()
                        },
                    );
                    #[cfg(windows)]
                    if can_drag && response.drag_started_by(egui::PointerButton::Primary) {
                        dragged = Some(row);
                        selected = Some(row);
                    }
                    if response.clicked() {
                        selected = Some(row);
                    }
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::SelectableLabel,
                            true,
                            selected == Some(row),
                            format!("{}，{}", hit.name, hit.path),
                        )
                    });
                    if response.double_clicked() {
                        selected = Some(row);
                        if hit.source == "local" {
                            action = Some(ResultAction::Open);
                        }
                    }
                    response.context_menu(|ui| {
                        ui.add_enabled_ui(!self.opening, |ui| {
                            if let Some(next) = result_actions(ui, hit, true) {
                                selected = Some(row);
                                action = Some(next);
                                ui.close();
                            }
                        });
                    });
                    let chosen = selected == Some(row);
                    ui.painter().rect_filled(
                        rect.shrink2(Vec2::new(0.0, 2.0)),
                        6.0,
                        if chosen {
                            SOFT
                        } else if response.hovered() {
                            BACKGROUND
                        } else {
                            Color32::WHITE
                        },
                    );
                    if chosen {
                        let stripe = egui::Rect::from_min_size(
                            rect.min + Vec2::new(0.0, 3.0),
                            Vec2::new(3.0, rect.height() - 6.0),
                        );
                        ui.painter().rect_filled(stripe, 2.0, ACCENT);
                    }
                    if response.has_focus() {
                        ui.painter().rect_stroke(
                            rect.shrink(1.0),
                            6.0,
                            Stroke::new(2.0_f32, ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    }
                    ui.painter()
                        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0_f32, BORDER));

                    let columns = result_columns(rect);
                    let name_end = if table {
                        columns[1] - 7.0
                    } else {
                        rect.right() - 65.0
                    };
                    let name_y = if table {
                        rect.top() + 10.0
                    } else {
                        rect.top() + 5.0
                    };
                    let name_cell =
                        egui::Rect::from_x_y_ranges(columns[0]..=name_end, rect.y_range());
                    let name = ui
                        .painter()
                        .with_clip_rect(name_cell.intersect(ui.clip_rect()));
                    let mut label = name_highlight(&hit.name, &self.query);
                    label.wrap.max_width = name_cell.width();
                    label.wrap.max_rows = 1;
                    label.wrap.break_anywhere = true;
                    name.galley(egui::pos2(columns[0], name_y), name.layout_job(label), INK);
                    let folder = Path::new(&hit.path)
                        .parent()
                        .filter(|path| !path.as_os_str().is_empty());
                    let kind = if hit.source == "local" {
                        file_kind(hit)
                    } else {
                        "离线目录".into()
                    };
                    let size = if hit.is_dir {
                        "—".into()
                    } else {
                        size_label(hit.size)
                    };
                    if table {
                        let location = folder.map_or("—".into(), |dir| {
                            dir.file_name().unwrap_or(dir.as_os_str()).to_string_lossy()
                        });
                        let modified = modified_label(hit.modified);
                        for (i, value) in [location.as_ref(), &kind, &size, &modified]
                            .iter()
                            .enumerate()
                        {
                            paint_result_cell(
                                ui,
                                rect,
                                columns[i + 1],
                                columns[i + 2],
                                rect.center().y,
                                value,
                            );
                        }
                    } else {
                        let location = folder.map_or("—".into(), Path::to_string_lossy);
                        paint_result_cell(
                            ui,
                            rect,
                            columns[0],
                            name_end,
                            rect.top() + 36.0,
                            &location,
                        );
                        paint_result_cell(ui, rect, name_end, rect.right(), rect.center().y, &size);
                    }
                    response.on_hover_text(&hit.path);
                }
            });
        self.selected = selected;
        if self.wizard.is_none()
            && self.selected.is_some()
            && ui
                .ctx()
                .input(|i| i.modifiers.command && i.key_pressed(egui::Key::G))
        {
            self.result_action(ctx, ResultAction::QuickSwitch);
        }
        if self.wizard.is_none() && !ui.ctx().wants_keyboard_input() {
            if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                self.selected = Some(self.selected.map_or(0, |n| (n + 1).min(count - 1)));
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                self.selected = Some(self.selected.unwrap_or(0).saturating_sub(1));
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                action = Some(ResultAction::Open);
            }
        }
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.offset > 0 && !self.searching,
                    egui::Button::new("上一页").small(),
                )
                .clicked()
            {
                self.offset = self.offset.saturating_sub(PAGE_SIZE);
                self.run_search(ctx);
            }
            ui.label(
                RichText::new(format!("第 {}–{} 项", self.offset + 1, self.offset + count))
                    .small()
                    .color(MUTED),
            );
            if ui
                .add_enabled(
                    self.offset + count < total
                        && self.offset + PAGE_SIZE <= 10_000
                        && !self.searching,
                    egui::Button::new("下一页").small(),
                )
                .clicked()
            {
                self.offset += PAGE_SIZE;
                self.run_search(ctx);
            }
            if !complete
                && self.budget_ms < 60_000
                && ui
                    .small_button("继续完整查询")
                    .on_hover_text("扩大本次查询的时间预算，重新检索当前索引")
                    .clicked()
            {
                self.budget_ms = (self.budget_ms * 4).min(60_000);
                self.run_search(ctx);
            }
        });
        if !complete || !coverage_complete {
            let status = if !coverage_complete {
                "索引尚未覆盖全部收录范围。"
            } else if self.searching {
                "正在补全排序和计数，当前数量为下限。"
            } else {
                "查询已达时间预算，当前数量为下限。"
            };
            ui.label(RichText::new(status).small().color(MUTED));
        }
        if !inspector {
            ui.add_space(7.0);
            let selected_hit = self
                .selected
                .and_then(|n| self.results.as_ref()?.results.get(n))
                .cloned();
            if let Some(hit) = selected_hit {
                ui.horizontal_wrapped(|ui| {
                    ui.add_enabled_ui(!self.opening, |ui| {
                        if let Some(next) = result_actions(ui, &hit, false) {
                            action = Some(next);
                        }
                    });
                    ui.label(
                        RichText::new(if hit.source == "local" {
                            "双击打开 · 打开前重新校验范围"
                        } else {
                            "离线目录仅显示文件信息"
                        })
                        .small()
                        .color(MUTED),
                    );
                });
            } else {
                ui.label(
                    RichText::new("选择一项查看操作，双击打开本地文件。")
                        .small()
                        .color(MUTED),
                );
            }
        }
        #[cfg(windows)]
        if let Some(row) = dragged {
            self.drag_result(row);
            return;
        }
        if let Some(action) = action {
            self.result_action(ctx, action);
        }
    }

    fn preview_dialog(&mut self, ctx: &egui::Context) {
        let Some(preview) = &mut self.preview else {
            return;
        };
        let mut open = true;
        egui::Window::new("已授权的文件预览")
            .open(&mut open)
            .default_size([520.0, 360.0])
            .show(ctx, |ui| {
                ui.strong(&preview.name);
                match &preview.content {
                    PreviewContent::Text { text, truncated } => {
                        if *truncated {
                            ui.label("仅显示文件开头。");
                        }
                        let find_id = egui::Id::new("preview_find_box");
                        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
                            ctx.memory_mut(|m| m.request_focus(find_id));
                        }
                        let needle = preview.find_query.trim().to_lowercase();
                        let matched_lines: Vec<usize> = if needle.is_empty() {
                            Vec::new()
                        } else {
                            text.lines()
                                .enumerate()
                                .filter(|(_, line)| line.to_lowercase().contains(&needle))
                                .map(|(idx, _)| idx)
                                .collect()
                        };
                        if !matched_lines.is_empty() {
                            preview.find_index %= matched_lines.len();
                        } else {
                            preview.find_index = 0;
                        }
                        ui.horizontal(|ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut preview.find_query)
                                    .id(find_id)
                                    .desired_width(210.0)
                                    .hint_text("页内查找 (Ctrl+F)"),
                            );
                            if resp.changed() {
                                preview.find_index = 0;
                            }
                            if !needle.is_empty() {
                                let summary = if matched_lines.is_empty() {
                                    "0 处匹配".to_owned()
                                } else {
                                    format!(
                                        "第 {} / {} 行",
                                        preview.find_index + 1,
                                        matched_lines.len()
                                    )
                                };
                                ui.label(RichText::new(summary).small().color(MUTED));
                                if !matched_lines.is_empty() {
                                    if ui.small_button("上一处").clicked() {
                                        preview.find_index = if preview.find_index == 0 {
                                            matched_lines.len() - 1
                                        } else {
                                            preview.find_index - 1
                                        };
                                    }
                                    if ui.small_button("下一处").clicked() {
                                        preview.find_index =
                                            (preview.find_index + 1) % matched_lines.len();
                                    }
                                }
                            }
                        });
                        ui.separator();
                        let active_line = matched_lines.get(preview.find_index).copied();
                        egui::ScrollArea::both().show(ui, |ui| {
                            if needle.is_empty() {
                                ui.monospace(text);
                            } else {
                                for (idx, line) in text.lines().enumerate() {
                                    let is_active = active_line == Some(idx);
                                    let is_hit = line.to_lowercase().contains(&needle);
                                    let rich = if is_active {
                                        RichText::new(line).monospace().strong().color(ACCENT)
                                    } else if is_hit {
                                        RichText::new(line).monospace().color(INK)
                                    } else {
                                        RichText::new(line).monospace().color(MUTED)
                                    };
                                    let label = ui.label(rich);
                                    if is_active {
                                        label.scroll_to_me(Some(egui::Align::Center));
                                    }
                                }
                            }
                        });
                    }
                    PreviewContent::Image(image) => {
                        ui.add(
                            egui::Image::new(image)
                                .max_size(egui::vec2(620.0, 440.0))
                                .alt_text(&preview.name),
                        );
                    }
                }
            });
        if !open {
            self.preview = None;
        }
    }
    fn wizard(&mut self, ctx: &egui::Context) {
        let Some(mut wizard) = self.wizard.take() else {
            return;
        };
        wizard.read_estimate();
        let mut defer = false;
        let mut apply = false;
        let mut estimate = false;
        let mut cancel_estimate = false;
        let width = (ctx.content_rect().width() - 70.0).clamp(350.0, 680.0);
        let modal = egui::Modal::new(egui::Id::new("scope_setup"))
            .backdrop_color(Color32::from_black_alpha(105))
            .frame(egui::Frame::new().fill(Color32::WHITE).corner_radius(14).inner_margin(26).stroke(Stroke::new(1.0_f32, BORDER)))
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(match wizard.stage { WizardStage::Welcome => "欢迎使用寻枢", WizardStage::Scope => "选择搜索范围", WizardStage::Review => "确认并建立索引" }).size(24.0).strong().color(INK));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| { if ui.add_enabled(!self.saving, egui::Button::new("关闭").small()).clicked() { defer = true; } });
                });
                ui.add_space(8.0);
                ui.label(RichText::new("1  开始使用     /     2  范围与类型     /     3  确认扫描").small().color(MUTED));
                ui.add_space(18.0);
                ui.add_enabled_ui(!self.saving, |ui| match wizard.stage {
                    WizardStage::Welcome => {
                        ui.label("选择需要检索的本地磁盘与文件类型。确认后仅建立文件名与元数据索引，正文与 AI 检索默认关闭。");
                        ui.add_space(16.0);
                        egui::Frame::new().fill(SOFT).corner_radius(10).inner_margin(16).show(ui, |ui| {
                            ui.label(RichText::new("选择磁盘与文件类型（推荐）").strong().color(ACCENT));
                            ui.add_space(6.0); ui.label(RichText::new("自动预填桌面、文档、下载目录，并在下一步直接勾选需要收录的本地磁盘与文件类型预设。").small().color(MUTED));
                            ui.add_space(10.0);
                            ui.horizontal_wrapped(|ui| {
                                if ui.add(primary("选择磁盘与文件类型")).clicked() {
                                    if wizard.roots.is_empty() {
                                        wizard.roots = xs_platform::suggested_roots().iter()
                                            .filter(|scope| {
                                                let path = Path::new(&scope.path);
                                                xs_platform::validate_local_path(path, false).is_ok()
                                                    && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
                                            }).map(EditableScope::from).collect();
                                    }
                                    wizard.stage = WizardStage::Scope;
                                    wizard.error = None;
                                }
                                if ui.button("一键使用常用目录直达确认").clicked() {
                                    wizard.roots = xs_platform::suggested_roots().iter()
                                        .filter(|scope| {
                                            let path = Path::new(&scope.path);
                                            xs_platform::validate_local_path(path, false).is_ok()
                                                && std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
                                        }).map(EditableScope::from).collect();
                                    if wizard.roots.is_empty() {
                                        wizard.stage = WizardStage::Scope;
                                        wizard.error = Some("没有可用的推荐目录，请在下方勾选本地磁盘或添加目录。".into());
                                    } else {
                                        wizard.stage = WizardStage::Review;
                                        wizard.error = None;
                                    }
                                }
                            });
                        });
                        ui.add_space(14.0);
                        ui.label(RichText::new("所有处理均在本地完成，不上传任何文件数据。之后随时可在主界面重新调整磁盘与类型。").small().color(MUTED));
                    }
                    WizardStage::Scope => {
                        ui.label(RichText::new("1. 勾选需要检索的本地磁盘").strong());
                        ui.add_space(6.0);
                        let mut volume_toggles: Vec<(String, bool)> = Vec::new();
                        egui::Frame::new().fill(SOFT).corner_radius(9).inner_margin(10).show(ui, |ui| match &wizard.volumes {
                            Some(Ok(volumes)) if volumes.is_empty() => { ui.label("未发现固定本地卷，可在下方手动添加目录。"); }
                            Some(Ok(volumes)) => {
                                egui::ScrollArea::vertical().id_salt("volumes").max_height(115.0).show(ui, |ui| {
                                    for VolumeCard { volume, capacity } in volumes {
                                        let mut checked = wizard.has_root(&volume.root);
                                        ui.horizontal_wrapped(|ui| {
                                            let checkbox = ui.add_enabled(
                                                volume.standard_supported,
                                                egui::Checkbox::new(&mut checked, RichText::new(&volume.root).strong()),
                                            );
                                            if checkbox.changed() {
                                                volume_toggles.push((volume.root.clone(), checked));
                                            }
                                            ui.label(RichText::new(if volume.filesystem.is_empty() { "文件系统未知" } else { &volume.filesystem }).small().color(MUTED));
                                            ui.label(RichText::new(capacity.map_or_else(|| "容量未知".into(), size_label)).small().color(MUTED));
                                            let speed = if volume.mft_supported { "NTFS 支持极速 MFT（CLI --fast-volume）" }
                                                else if volume.standard_supported { "标准目录扫描" } else { "暂不可用" };
                                            ui.label(RichText::new(speed).small().color(if volume.mft_supported { ACCENT } else { MUTED }));
                                        });
                                    }
                                });
                            }
                            Some(Err(error)) => { ui.colored_label(ERROR, format!("无法读取本地磁盘能力：{error}")); }
                            None => { ui.label("正在探测本地磁盘…"); }
                        });
                        for (path, selected) in volume_toggles {
                            wizard.toggle_volume(&path, selected);
                            wizard.error = None;
                        }
                        ui.add_space(10.0);
                        ui.label(RichText::new("2. 选择收录文件类型（一键应用到全部已选范围）").strong());
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            for policy in TypePolicy::ALL {
                                if ui.selectable_label(wizard.global_policy == policy, policy.label()).clicked() {
                                    wizard.global_policy = policy;
                                    wizard.apply_global_policy();
                                }
                            }
                        });
                        if wizard.global_policy == TypePolicy::Custom {
                            ui.add_space(4.0);
                            if ui.add(egui::TextEdit::singleline(&mut wizard.global_custom).hint_text("全局自定义后缀，逗号分隔，例如 pdf, docx, rs, zip").desired_width(f32::INFINITY)).changed() {
                                wizard.apply_global_policy();
                            }
                        }
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut wizard.global_smart_excludes, "全局跳过缓存与工具目录（推荐）").changed() {
                                wizard.apply_global_policy();
                            }
                            if !wizard.roots.is_empty() && ui.small_button("同步到下方全部目录").clicked() {
                                wizard.apply_global_policy();
                            }
                        });
                        ui.add_space(10.0);
                        ui.label(RichText::new("3. 添加自定义目录或单独微调每项策略").strong());
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.add_sized([(width-165.0).max(180.0), 31.0], egui::TextEdit::singleline(&mut wizard.draft_path).hint_text(if cfg!(windows) { r"例如 C:\Users\你的用户名\Documents" } else { "例如 /home/你的用户名/Documents" }));
                            if ui.add_enabled(!self.picking_folder, egui::Button::new(if self.picking_folder { "选择中…" } else { "浏览…" })).clicked() {
                                self.picking_folder = true;
                                let tx = self.tx.clone();
                                let ctx = ctx.clone();
                                std::thread::spawn(move || {
                                    send(&tx, &ctx, Message::PickedFolder(pick_local_folder()));
                                });
                            }
                            if ui.button("添加目录").clicked() && !wizard.draft_path.trim().is_empty() {
                                let path = wizard.draft_path.trim().to_owned();
                                if !wizard.has_root(&path) {
                                    wizard.roots.push(EditableScope {
                                        path,
                                        policy: wizard.global_policy,
                                        custom: wizard.global_custom.clone(),
                                        smart_excludes: wizard.global_smart_excludes,
                                    });
                                }
                                wizard.draft_path.clear(); wizard.error = None;
                            }
                        });
                        ui.add_space(12.0);
                        let mut remove = None;
                        egui::ScrollArea::vertical().id_salt("scope_editor").max_height((ctx.content_rect().height()-390.0).clamp(140.0, 330.0)).show(ui, |ui| {
                            if wizard.roots.is_empty() { ui.label(RichText::new("尚未添加目录。可从上方输入完整路径。").color(MUTED)); }
                            for (n, root) in wizard.roots.iter_mut().enumerate() {
                                ui.push_id(n, |ui| {
                                    egui::Frame::new().fill(BACKGROUND).corner_radius(9).inner_margin(12).show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.add_sized([(width-110.0).max(230.0), 26.0], egui::TextEdit::singleline(&mut root.path));
                                            if ui.small_button("移除").clicked() { remove = Some(n); }
                                        });
                                        ui.add_space(7.0);
                                        ui.horizontal(|ui| {
                                            egui::ComboBox::from_id_salt("type_policy").selected_text(root.policy.label()).width(160.0).show_ui(ui, |ui| {
                                                for policy in TypePolicy::ALL { ui.selectable_value(&mut root.policy, policy, policy.label()); }
                                            });
                                            ui.checkbox(&mut root.smart_excludes, "跳过缓存与工具目录");
                                        });
                                        if root.policy == TypePolicy::Custom { ui.add_space(6.0); ui.add(egui::TextEdit::singleline(&mut root.custom).hint_text("后缀用逗号分隔，例如 pdf, docx, rs").desired_width(f32::INFINITY)); }
                                        if root.policy == TypePolicy::Directories { ui.label(RichText::new("只收录目录，不收录普通文件。").small().color(ACCENT)); }
                                    }); ui.add_space(10.0);
                                });
                            }
                        });
                        if let Some(n) = remove { wizard.roots.remove(n); }
                        ui.add_space(8.0); ui.label(RichText::new("标准扫描 · 显式目录遍历").strong());
                        ui.label(RichText::new("此处只设置文件名收录范围。正文范围、图片识别和语义检索可在主窗口的正文页单独设置。").small().color(MUTED));
                    }
                    WizardStage::Review => {
                        ui.label(RichText::new(format!("将收录 {} 个明确选择的目录", wizard.roots.len())).strong()); ui.add_space(12.0);
                        egui::ScrollArea::vertical().id_salt("scope_review").max_height(260.0).show(ui, |ui| {
                            for root in &wizard.roots {
                                ui.label(RichText::new(&root.path).color(INK));
                                ui.label(RichText::new(format!("{} · {}", root.policy.label(), if root.smart_excludes { "跳过缓存目录" } else { "包含工具与缓存目录" })).small().color(MUTED));
                                if root.policy == TypePolicy::Custom { ui.label(RichText::new(&root.custom).small().color(MUTED)); }
                                ui.add_space(10.0);
                            }
                        });
                        ui.add_space(12.0);
                        ui.label("确认后开始标准扫描。空库首扫会分批发布结果，并标明未完成的覆盖范围。取消已有完整索引的重扫会保留上次完整代次。改变收录范围后，旧授权立即失效。");
                        ui.horizontal(|ui| {
                            if matches!(wizard.estimate, Some(WizardEstimate::Running(_))) {
                                ui.spinner();
                                ui.label("正在估算所选目录…");
                                cancel_estimate = ui.button("取消估算").clicked();
                            } else {
                                estimate = ui.button("试扫并估算").clicked();
                                ui.label("只检查所选目录的文件信息，不保存范围或索引。");
                            }
                        });
                        if let Some(WizardEstimate::Ready(result)) = &wizard.estimate {
                            use scope_estimate::Estimate;
                            let label = match result {
                                Ok(Estimate::Measured { visited, indexed, elapsed }) => format!("试扫检查 {visited} 项，预计收录 {indexed} 项。遍历耗时 {} ms，另需构建和写入索引。", elapsed.as_millis()),
                                Ok(Estimate::Limited { visited, indexed }) => format!("达到试扫上限，已检查 {visited} 项、符合范围 {indexed} 项。总量和耗时未知。"),
                                Ok(Estimate::Incomplete { visited, indexed }) => format!("部分位置不可读，已检查 {visited} 项、符合范围 {indexed} 项。总量和耗时未知。"),
                                Ok(Estimate::Cancelled) => "估算已取消。".into(),
                                Err(error) => format!("无法估算：{error}"),
                            };
                            ui.label(RichText::new(label).small().color(MUTED));
                        }
                        ui.add_space(10.0);
                        ui.label(RichText::new(format!("配置与索引保存到：{}", self.data_dir.display())).small().color(MUTED));
                    }
                });
                if let Some(error) = &wizard.error { ui.add_space(10.0); egui::ScrollArea::vertical().max_height(95.0).id_salt("wizard_error").show(ui, |ui| { ui.colored_label(ERROR, error); }); }
                ui.add_space(20.0); ui.separator(); ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.saving, egui::Button::new(if self.settings.as_ref().is_some_and(|s| s.setup_state == SetupState::Completed) { "取消修改" } else { "稍后设置" })).clicked() { defer = true; }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.saving { ui.spinner(); ui.label("正在校验并保存…"); }
                        else { match wizard.stage {
                            WizardStage::Scope => {
                                if ui.add(primary("下一步")).clicked() {
                                    if let Some(previous) = &self.settings { match wizard.candidate(previous) { Ok(_) => { wizard.stage = WizardStage::Review; wizard.error = None; }, Err(e) => wizard.error = Some(format!("{e:#}")) } }
                                }
                                if ui.button("上一步").clicked() { wizard.stage = WizardStage::Welcome; }
                            }
                            WizardStage::Review => { if ui.add(primary("保存并开始扫描")).clicked() { apply = true; }
 if ui.button("返回修改").clicked() { wizard.stage = WizardStage::Scope; } }
                            WizardStage::Welcome => {}
                        } }
                    });
                });
            });
        if modal.should_close() && !self.saving {
            defer = true;
        }
        if wizard.stage != WizardStage::Review || apply || defer || cancel_estimate {
            wizard.estimate = None;
        } else if estimate {
            if let Some(result) = self
                .settings
                .as_ref()
                .map(|settings| wizard.candidate(settings))
            {
                match result {
                    Ok(settings) => {
                        wizard.start_estimate(settings, self.data_dir.clone(), ctx.clone())
                    }
                    Err(error) => wizard.error = Some(format!("{error:#}")),
                }
            }
        }
        let candidate = if apply {
            self.settings.as_ref().map(|s| wizard.candidate(s))
        } else {
            None
        };
        self.wizard = Some(wizard);
        if defer {
            self.defer(ctx);
        } else if let Some(candidate) = candidate {
            match candidate {
                Ok(settings) => self.save(ctx, settings, SaveAction::Apply),
                Err(e) => {
                    if let Some(w) = self.wizard.as_mut() {
                        w.error = Some(format!("{e:#}"));
                    }
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.messages(ctx);
        if self.search_due.is_some_and(|due| Instant::now() >= due) && !self.loading {
            self.run_search(ctx);
        }
        self.desktop_messages(ctx);
        self.content.update(&self.data_dir, ctx);
        // Windows 首帧有时早于桌面合成器接管新窗口，短暂重绘以覆盖首次呈现。
        #[cfg(windows)]
        if self.started.elapsed() < Duration::from_secs(2) {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        #[cfg(windows)]
        if let Some(deadline) = self.focus_native_until {
            if Instant::now() >= deadline
                || self
                    .native_window
                    .is_none_or(desktop::windows::focus_from_gesture)
            {
                self.focus_native_until = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(32));
            }
        }
        let ime_active = self.ime_active_this_frame(ctx);
        match self.view {
            View::Main => {
                self.header(ctx);
                self.footer(ctx);
                if self.content_tab {
                    self.content_panel(ctx);
                } else {
                    self.search_bar(ctx);
                    self.sidebar(ctx);
                    self.main_panel(ctx);
                    self.activity_panel(ctx);
                    self.preview_dialog(ctx);
                    self.connectors.show(ctx, &self.data_dir);
                }
                self.wizard(ctx);
                self.utility.show(ctx, &self.data_dir);
                let can_rebuild = !self.loading
                    && !self.saving
                    && self.scan_cancel.is_none()
                    && self.wizard.is_none()
                    && self.settings.as_ref().is_some_and(|s| !s.roots.is_empty());
                if self.diagnostics.show(
                    ctx,
                    &self.data_dir,
                    self.settings.as_ref(),
                    self.index.as_ref().map(|index| index.generation()),
                    self.scan_progress.as_ref(),
                    can_rebuild,
                ) {
                    self.scan(ctx);
                }
            }
            View::Quick => self.quick_panel(ctx, ime_active),
        }
        if self.loading
            || self.searching
            || self.scan_cancel.is_some()
            || self.saving
            || self.search_due.is_some()
            || self.smoke_test
        {
            let delay = self.search_due.map_or(Duration::from_millis(50), |due| {
                due.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50))
            });
            ctx.request_repaint_after(delay);
        }
        if self.smoke_test && !self.smoke_exit_requested {
            if self.content_tab && self.content.first_preview().is_some() {
                let first = self.content_smoke_since.get_or_insert_with(Instant::now);
                if first.elapsed() >= Duration::from_millis(120) {
                    self.smoke_exit_requested = true;
                    eprintln!("XS_UI_CONTENT_SMOKE_OK cjk_font={}", self.cjk_font);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else if self.started.elapsed()
                >= Duration::from_secs(if self.content_tab { 15 } else { 5 })
            {
                self.smoke_exit_requested = true;
                if self.content_tab {
                    eprintln!(
                        "XS_UI_CONTENT_SMOKE_FAILED: {}",
                        self.content.error().unwrap_or("没有已验证的正文预览")
                    );
                } else {
                    eprintln!("XS_UI_SMOKE_OK: native window rendered");
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            #[cfg(windows)]
            if desktop::windows::take_minimize_request()
                && (self.content.building() || self.saving || self.wizard.is_some())
            {
                // 正文任务或未完成的向导保留在后台，托盘可恢复同一窗口。
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if let Some(window) = self.native_window {
                    self.tray_hidden = true;
                    desktop::windows::hide_renderer_window(window);
                    self.emit_desktop(&RendererMessage::Hidden(window as usize));
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                return;
            }
            if self.content.building()
                || needs_defer_on_close(self.settings.as_ref(), self.wizard.is_some())
            {
                self.request_close(ctx);
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            } else if !self.close_notified {
                self.close_notified = true;
                self.emit_desktop(&RendererMessage::Closing);
            }
        }
        if self.close_after_save && !self.saving && self.wizard.is_none() {
            self.close_after_save = false;
            self.request_close(ctx);
        }
        if self.close_after_content && !self.content.building() {
            self.close_after_content = false;
            self.request_close(ctx);
        }
        #[cfg(windows)]
        if self.tray_hidden && !self.content.building() && !self.saving && self.wizard.is_none() {
            self.tray_hidden = false;
            self.request_close(ctx);
        }
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.monitor_stop.store(true, Ordering::Relaxed);
        self.cancel_query();
        if let Some(cancel) = self.scan_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn needs_defer_on_close(settings: Option<&Settings>, wizard_open: bool) -> bool {
    wizard_open && settings.is_some_and(|s| s.setup_state != SetupState::Completed)
}

fn read_font(mut file: std::fs::File) -> Result<Vec<u8>> {
    const LIMIT: u64 = 64 * 1024 * 1024;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= LIMIT,
        "字体必须是小于 64 MiB 的本地常规文件。"
    );
    let mut bytes = Vec::new();
    (&mut file).take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= LIMIT, "字体超过大小限制。");
    let face =
        ttf_parser::Face::parse(&bytes, 0).map_err(|_| anyhow::anyhow!("无法识别字体格式"))?;
    ensure!(
        face.glyph_index('寻').is_some() && face.glyph_index('枢').is_some(),
        "字体缺少简体中文字符"
    );
    Ok(bytes)
}
fn read_requested_font(path: &Path) -> Result<Vec<u8>> {
    let file = xs_platform::open_local_read(path)
        .context("--font 只允许安全的本机字体文件，禁止网络路径和符号链接")?;
    read_font(file)
}

fn configure_ui(ctx: &egui::Context, requested_font: Option<Vec<u8>>) -> bool {
    let candidates = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansSC-Regular.ttf",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "/System/Library/Fonts/PingFang.ttc",
    ];
    // These fixed OS font locations are trusted application resources. Any
    // user-provided --font has already passed the strict local descriptor guard.
    let font = requested_font.or_else(|| {
        candidates
            .iter()
            .find_map(|path| read_font(std::fs::File::open(path).ok()?).ok())
    });
    let cjk_font = font.is_some();
    if let Some(bytes) = font {
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("local-cjk".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .get_mut(&egui::FontFamily::Proportional)
            .unwrap()
            .insert(0, "local-cjk".into());
        fonts
            .families
            .get_mut(&egui::FontFamily::Monospace)
            .unwrap()
            .push("local-cjk".into());
        ctx.set_fonts(fonts);
    }
    ctx.set_theme(egui::Theme::Light);
    configure_visuals(ctx, false);
    let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(9.0, 5.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, FontId::proportional(12.0));
    ctx.set_style_of(egui::Theme::Light, style);
    cjk_font
}

fn configure_visuals(ctx: &egui::Context, high_contrast: bool) {
    let mut visuals = egui::Visuals::light();
    let edge = if high_contrast {
        Color32::BLACK
    } else {
        ACCENT
    };
    let border = if high_contrast {
        Color32::BLACK
    } else {
        BORDER
    };
    let stroke = if high_contrast { 2.0_f32 } else { 1.0_f32 };
    visuals.override_text_color = Some(if high_contrast { Color32::BLACK } else { INK });
    visuals.panel_fill = Color32::WHITE;
    visuals.window_fill = Color32::WHITE;
    visuals.selection.bg_fill = SOFT;
    visuals.selection.stroke = Stroke::new(stroke, edge);
    visuals.widgets.inactive.bg_fill = BACKGROUND;
    visuals.widgets.inactive.weak_bg_fill = BACKGROUND;
    visuals.widgets.inactive.bg_stroke = Stroke::new(stroke, border);
    visuals.widgets.hovered.bg_fill = SOFT;
    visuals.widgets.hovered.weak_bg_fill = SOFT;
    visuals.widgets.hovered.bg_stroke = Stroke::new(stroke, edge);
    if high_contrast {
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(stroke, border);
    }
    visuals.widgets.active.bg_fill = SOFT;
    visuals.widgets.active.weak_bg_fill = SOFT;
    visuals.widgets.active.bg_stroke = Stroke::new(stroke, edge);
    ctx.set_visuals_of(egui::Theme::Light, visuals);
}
fn primary(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE))
        .fill(ACCENT)
        .corner_radius(7)
}
fn magnifier(ui: &mut egui::Ui, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let center = rect.min + Vec2::splat(size * 0.4);
    let radius = size * 0.27;
    let stroke = Stroke::new((size * 0.055).max(1.4), color);
    ui.painter().circle_stroke(center, radius, stroke);
    ui.painter().line_segment(
        [
            center + Vec2::splat(radius * std::f32::consts::FRAC_1_SQRT_2),
            rect.min + Vec2::splat(size * 0.88),
        ],
        stroke,
    );
}
fn empty_state(ui: &mut egui::Ui, title: &str, detail: &str, spinner: bool) {
    ui.add_space(45.0);
    ui.vertical_centered(|ui| {
        if spinner {
            ui.spinner();
        } else {
            magnifier(ui, 48.0, Color32::from_rgb(155, 178, 185));
        }
        ui.add_space(13.0);
        ui.label(RichText::new(title).size(19.0).strong().color(INK));
        ui.add_space(9.0);
        ui.label(RichText::new(detail).size(13.0).color(MUTED));
        ui.add_space(18.0);
    });
}
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!(
            "{}…",
            text.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}
fn size_label(size: u64) -> String {
    if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else if size < 1024 * 1024 * 1024 {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", size as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
// 为列表和表头使用相同的列基准，避免放大或缩放时文字错位。
fn result_columns(rect: egui::Rect) -> [f32; 6] {
    let right = rect.right();
    let flexible = rect.width() - 227.0;
    [
        rect.left() + 9.0,
        rect.left() + flexible * 0.57,
        right - 227.0,
        right - 177.0,
        right - 111.0,
        right,
    ]
}

fn paint_result_cell(ui: &egui::Ui, rect: egui::Rect, left: f32, right: f32, y: f32, value: &str) {
    let clip = egui::Rect::from_x_y_ranges(left..=(right - 4.0).max(left), rect.y_range());
    let painter = ui.painter().with_clip_rect(clip.intersect(ui.clip_rect()));
    let mut job = egui::text::LayoutJob::simple_singleline(
        value.to_owned(),
        FontId::proportional(12.0),
        MUTED,
    );
    job.wrap.max_width = clip.width();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    painter.galley(egui::pos2(left, y - galley.size().y / 2.0), galley, MUTED);
}

fn file_kind(hit: &xs_core::SearchHit) -> String {
    if hit.is_dir {
        return "文件夹".into();
    }
    Path::new(&hit.name)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .map_or_else(|| "文件".into(), str::to_uppercase)
}

fn modified_label(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
        || "—".into(),
        |stamp| {
            stamp
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

fn result_actions(ui: &mut egui::Ui, hit: &xs_core::SearchHit, menu: bool) -> Option<ResultAction> {
    let local = hit.source == "local";
    let mut action = None;
    for (candidate, label, enabled) in [
        (ResultAction::Open, "打开", local),
        (ResultAction::OpenParent, "所在文件夹", local),
        (ResultAction::CopyPath, "复制路径", true),
        (
            ResultAction::Preview,
            "预览",
            local && !hit.is_dir && preview_format(Path::new(&hit.path)).is_some(),
        ),
        (ResultAction::QuickSwitch, "跳转对话框 (Ctrl+G)", local),
    ] {
        let button = if menu || !matches!(candidate, ResultAction::Open) {
            egui::Button::new(label)
        } else {
            primary(label)
        };
        if ui.add_enabled(enabled, button).clicked() {
            action = Some(candidate);
        }
    }
    action
}

enum PreviewFormat {
    Text,
    Image(image::ImageFormat),
}

fn preview_format(path: &Path) -> Option<PreviewFormat> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "txt" | "md" | "log" | "csv" | "rs" | "json" | "toml" | "yaml" | "yml" => {
            Some(PreviewFormat::Text)
        }
        "png" => Some(PreviewFormat::Image(image::ImageFormat::Png)),
        "jpg" | "jpeg" => Some(PreviewFormat::Image(image::ImageFormat::Jpeg)),
        _ => None,
    }
}

#[cfg(windows)]
fn authorize_drag(dir: &Path, index: &Index, hit: &xs_core::SearchHit) -> Result<PathBuf> {
    ensure!(hit.source == "local" && !hit.is_dir, "仅支持拖放本地文件");
    ensure!(
        index.record_revision(hit.id)?.is_some(),
        "请重新扫描后拖放此文件"
    );
    let store = xs_core::persistence::Store::open_read_only(dir.join("index"))?;
    ensure!(
        store.current_generation()? == Some(index.generation()),
        "索引代次已更新，请重新拖动"
    );
    operations::authorize_indexed_open(dir, index, hit.id)
}

fn execute_result_action(
    dir: &Path,
    index: &Index,
    hit: &xs_core::SearchHit,
    action: ResultAction,
) -> Result<ActionOutcome> {
    if matches!(action, ResultAction::CopyPath) && hit.source != "local" {
        operations::validate_index_scope(index, &operations::load_settings(dir)?)?;
        return Ok(ActionOutcome::CopyPath(hit.path.clone()));
    }
    let path = operations::authorize_indexed_open(dir, index, hit.id)?;
    match action {
        ResultAction::Open => {
            open_path(&path)?;
            Ok(ActionOutcome::Opened)
        }
        ResultAction::OpenParent => {
            let parent = path.parent().context("没有上一级文件夹")?;
            let settings = operations::load_settings(dir)?;
            operations::validate_index_scope(index, &settings)?;
            ensure!(
                settings.allows_filename(parent, true)?,
                "上一级文件夹超出当前授权范围"
            );
            xs_platform::validate_local_path(parent, false)?;
            open_path(parent)?;
            Ok(ActionOutcome::Opened)
        }
        ResultAction::CopyPath => Ok(ActionOutcome::CopyPath(path.to_string_lossy().into_owned())),
        ResultAction::Preview => read_preview(dir, index, hit, &path),
        ResultAction::QuickSwitch => {
            let target_dir = if hit.is_dir {
                path.as_path()
            } else {
                path.parent().context("该文件没有上级目录")?
            };
            #[cfg(windows)]
            {
                if desktop::windows::jump_active_file_dialog(target_dir)? {
                    return Ok(ActionOutcome::Opened);
                }
                anyhow::bail!("未检测到处于前台的 Windows 打开/保存文件对话框");
            }
            #[cfg(not(windows))]
            {
                let _ = target_dir;
                anyhow::bail!("对话框快速跳转仅支持 Windows 文件对话框");
            }
        }
    }
}

fn read_preview(
    dir: &Path,
    index: &Index,
    hit: &xs_core::SearchHit,
    path: &Path,
) -> Result<ActionOutcome> {
    ensure!(!hit.is_dir, "文件夹不支持预览");
    let format = preview_format(path).context("当前文件类型无法预览")?;
    let settings = operations::load_settings(dir)?;
    let content_token = settings.content_scope_token()?;
    ensure!(
        settings.allows_content(path, false)?,
        "请先在正文页授权预览此位置和文件类型"
    );
    let expected = index
        .record_revision(hit.id)?
        .context("此索引缺少文件版本，请重新扫描后预览")?;
    let file = xs_platform::open_local_read(path)?;
    ensure!(
        xs_platform::FileRevision::from_file(&file)? == expected,
        "文件在预览前已发生变化"
    );
    let content = decode_preview(&file, format)?;
    ensure!(
        xs_platform::FileRevision::from_file(&file)? == expected,
        "文件在预览过程中已发生变化"
    );
    operations::authorize_indexed_open(dir, index, hit.id)?;
    let settings = operations::load_settings(dir)?;
    ensure!(
        settings.content_scope_token()? == content_token,
        "正文授权在预览期间已更改"
    );
    Ok(ActionOutcome::Preview(Preview {
        name: hit.name.clone(),
        content,
        find_query: String::new(),
        find_index: 0,
    }))
}

fn decode_preview(
    reader: impl Read,
    format: PreviewFormat,
) -> Result<PreviewContent<egui::ColorImage>> {
    let limit = match format {
        PreviewFormat::Text => 16 * 1024,
        PreviewFormat::Image(_) => 8 * 1024 * 1024,
    };
    let mut bytes = Vec::new();
    reader.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    match format {
        PreviewFormat::Text => {
            bytes.truncate(limit);
            let text = match std::str::from_utf8(&bytes) {
                Ok(text) => text,
                Err(error) if truncated && error.error_len().is_none() => {
                    std::str::from_utf8(&bytes[..error.valid_up_to()])?
                }
                Err(_) => bail!("仅支持 UTF-8 文本预览"),
            };
            Ok(PreviewContent::Text {
                text: text.to_owned(),
                truncated,
            })
        }
        PreviewFormat::Image(format) => {
            ensure!(!truncated, "图片超过 8 MiB 预览限制");
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(2048);
            limits.max_image_height = Some(2048);
            limits.max_alloc = Some(32 * 1024 * 1024);
            let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
            reader.limits(limits);
            let rgba = reader.decode()?.into_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            Ok(PreviewContent::Image(
                egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
            ))
        }
    }
}

fn name_highlight(name: &str, query: &str) -> egui::text::LayoutJob {
    let query = query.trim();
    let match_span = (!query.is_empty()
        && !query.contains([':', '<', '>', '!', '|', '"', '*', '?']))
    .then(|| name.to_ascii_lowercase().find(&query.to_ascii_lowercase()))
    .flatten()
    .map(|start| (start, start + query.len()));
    let style = egui::TextFormat::simple(FontId::proportional(15.0), INK);
    let mut job = egui::text::LayoutJob::default();
    if let Some((start, end)) = match_span {
        job.append(&name[..start], 0.0, style.clone());
        job.append(
            &name[start..end],
            0.0,
            egui::TextFormat {
                background: SOFT,
                color: ACCENT,
                ..style.clone()
            },
        );
        job.append(&name[end..], 0.0, style);
    } else {
        job.append(name, 0.0, style);
    }
    job
}

fn pick_local_folder() -> Option<String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog; if ($d.ShowDialog() -eq 'OK') { [Console]::Write($d.SelectedPath) }";
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-STA", "-Command", script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!path.is_empty()).then_some(path)
    }
    #[cfg(target_os = "linux")]
    {
        for (cmd, args) in [
            ("zenity", &["--file-selection", "--directory"][..]),
            ("kdialog", &["--getexistingdirectory", "."][..]),
        ] {
            if let Ok(output) = std::process::Command::new(cmd).args(args).output() {
                if output.status.success() {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    if !path.is_empty() {
                        return Some(path);
                    }
                }
            }
        }
        None
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

fn apply_smart_mode(query: &str, smart_mode: bool) -> String {
    let trimmed = query.trim();
    if !smart_mode
        || trimmed.is_empty()
        || trimmed.chars().count() < 2
        || trimmed.contains([':', '|', '!', '<', '>', '*', '?', '"', '/', '\\', ' '])
    {
        return trimmed.to_owned();
    }
    if trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        format!("smart:{trimmed}")
    } else {
        trimmed.to_owned()
    }
}

fn open_path(path: &Path) -> Result<()> {
    // No shell interpreter or shell quoting. The validated absolute path is one
    // OS argument; offline catalog records never reach this function.
    #[cfg(target_os = "linux")]
    let child = std::process::Command::new("xdg-open").arg(path).spawn()?;
    #[cfg(windows)]
    return desktop::windows::open_local_path(path);
    #[cfg(target_os = "macos")]
    let child = std::process::Command::new("open").arg(path).spawn()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = path;
        bail!("当前系统无法从寻枢打开文件");
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let status = child.wait_with_output()?.status;
        ensure!(status.success(), "系统文件打开程序返回错误：{status}");
        Ok(())
    }
}
fn view_geometry(view: View) -> (&'static str, Vec2, Vec2) {
    match view {
        View::Main => (
            "寻枢 · 本地文件搜索",
            Vec2::new(1120.0, 780.0),
            Vec2::new(740.0, 580.0),
        ),
        View::Quick => (
            "寻枢 · 快捷搜索",
            Vec2::new(640.0, 490.0),
            Vec2::new(420.0, 290.0),
        ),
    }
}

fn run_renderer(mut args: Args) -> Result<()> {
    let queries = if args.mode == DesktopMode::Renderer {
        match desktop::receive(&mut std::io::stdin().lock())? {
            Some(HostMessage::Boot { view, mut queries }) => {
                args.view = view;
                for query in &mut queries {
                    desktop::limit_query(query);
                }
                queries
            }
            _ => bail!("寻枢界面没有收到宿主的启动消息"),
        }
    } else {
        let mut queries = [String::new(), String::new()];
        queries[args.view.index()] = std::mem::take(&mut args.query);
        queries
    };
    let font = args
        .font
        .as_deref()
        .map(read_requested_font)
        .transpose()?
        .or_else(|| {
            let executable = std::env::current_exe().ok()?;
            let bundled = executable
                .parent()?
                .join("resources/fonts/NotoSansSC-Regular.otf");
            read_requested_font(&bundled).ok()
        });
    let (title, size, minimum) = view_geometry(args.view);
    let quick = args.view == View::Quick;
    let icon = image::load_from_memory(include_bytes!("../../../assets/xunshu.png"))
        .context("无法读取内置寻枢图标")?
        .into_rgba8();
    let (width, height) = icon.dimensions();
    let color_icon =
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], icon.as_raw());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            })
            .with_inner_size(size)
            .with_min_inner_size(minimum)
            .with_decorations(!quick)
            .with_active(true)
            .with_window_level(if quick {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            }),
        centered: quick,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "寻枢",
        options,
        Box::new(move |cc| {
            #[cfg(windows)]
            if args.mode == DesktopMode::Renderer {
                let installed = desktop::windows::native_window(cc)
                    .is_some_and(desktop::windows::install_minimize_handler);
                if !installed {
                    eprintln!("未能启用最小化到通知区域，继续使用系统最小化");
                }
            }
            Ok(Box::new(App::new(cc, args, queries, font, color_icon)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("无法显示寻枢原生窗口：{e}"))
}

fn main() -> Result<()> {
    xs_runtime::enforce_offline().context("无法启用寻枢的离线运行约束")?;
    let args = Args::parse()?;
    let mode = args.mode;
    #[cfg(windows)]
    if !args.smoke_test && matches!(mode, DesktopMode::Default | DesktopMode::Background) {
        let result = desktop::windows::run(
            args.data_dir,
            args.font,
            args.query,
            args.view,
            mode == DesktopMode::Background,
        );
        if let Err(error) = &result {
            desktop::windows::show_error(&format!("{error:#}"));
        }
        return result;
    }
    #[cfg(not(windows))]
    ensure!(
        mode != DesktopMode::Background,
        "当前平台不支持寻枢托盘常驻模式，请直接打开窗口"
    );
    let result = run_renderer(args);
    if mode == DesktopMode::Renderer {
        if let Err(error) = &result {
            let mut message = format!("{error:#}");
            desktop::limit_query(&mut message);
            let _ = desktop::send(
                &mut std::io::stdout().lock(),
                &RendererMessage::Failure(message),
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn drag_refuses_outdated_generations_and_replaced_files() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("documents");
        let data = fixture.path().join("profile");
        std::fs::create_dir(&root).unwrap();
        let file = root.join("synthetic-drag.txt");
        std::fs::write(&file, b"first revision").unwrap();
        let settings = Settings {
            roots: vec![Scope::new(root.to_str().unwrap())],
            ..Default::default()
        };
        operations::save_settings(&data, &settings).unwrap();
        let cancel = AtomicBool::new(false);
        operations::rebuild(&data, &cancel, |_| {}).unwrap();
        let first = operations::load_index(&data).unwrap();
        let matches = first
            .search("synthetic-drag", &SearchOptions::default(), None)
            .unwrap();
        let hit = &matches.results[0];
        assert_eq!(authorize_drag(&data, &first, hit).unwrap(), file);

        operations::rebuild(&data, &cancel, |_| {}).unwrap();
        assert!(authorize_drag(&data, &first, hit).is_err());
        let current = operations::load_index(&data).unwrap();
        let matches = current
            .search("synthetic-drag", &SearchOptions::default(), None)
            .unwrap();
        let hit = &matches.results[0];
        std::fs::write(&file, b"a longer replacement").unwrap();
        assert!(authorize_drag(&data, &current, hit).is_err());
    }

    #[test]
    fn stale_queries_cannot_replace_new_query_or_scope() {
        let mut gate = SearchGate::default();
        let a = gate.next();
        let epoch = gate.epoch;
        let b = gate.next();
        assert!(!gate.accepts(a, epoch));
        assert!(gate.accepts(b, epoch));
        gate.invalidate_index();
        assert!(!gate.accepts(b, epoch));
        let c = gate.next();
        assert!(gate.accepts(c, gate.epoch));
    }
    #[test]
    fn explicit_empty_policy_never_means_all_files() {
        assert_eq!(TypePolicy::All.extensions("").unwrap(), None);
        assert_eq!(
            TypePolicy::Directories.extensions("").unwrap(),
            Some(Vec::new())
        );
        assert!(TypePolicy::Custom.extensions(" , ").is_err());
        assert_eq!(
            TypePolicy::Custom.extensions(".PDF, rs；").unwrap(),
            Some(vec!["pdf".into(), "rs".into()])
        );
        let original = Scope {
            path: "/synthetic/root".into(),
            extensions: Some(Vec::new()),
            smart_excludes: true,
        };
        assert_eq!(
            EditableScope::from(&original).into_scope().unwrap(),
            original
        );
    }
    #[test]
    fn closing_setup_defers_only_unfinished_onboarding() {
        let mut settings = Settings::default();
        assert!(needs_defer_on_close(Some(&settings), true));
        assert!(!needs_defer_on_close(Some(&settings), false));
        settings.defer_setup();
        assert!(needs_defer_on_close(Some(&settings), true));
        settings.complete_setup();
        assert!(!needs_defer_on_close(Some(&settings), true));
        assert!(!needs_defer_on_close(None, true));
    }
    #[test]
    fn custom_fonts_reject_remote_or_relative_paths() {
        for path in [
            r"\\server\share\font.ttf",
            "//server/share/font.ttf",
            "https://example.test/font.ttf",
            "relative-font.ttf",
        ] {
            assert!(
                read_requested_font(Path::new(path)).is_err(),
                "must reject {path}"
            );
        }
    }
    #[test]
    fn filters_group_user_or_expression() {
        assert_eq!(Filter::Files.query("cat | dog"), "file: <cat | dog>");
        assert_eq!(Filter::Folders.query(""), "folder:");
        assert_eq!(Filter::All.query("ext:pdf"), "ext:pdf");
    }

    #[test]
    fn filename_categories_cover_unknown_extensions_and_preserve_user_queries() {
        use xs_query::{EntryRef, Query};

        for (name, is_dir, expected) in [
            ("林地项目.PDF", false, Filter::Documents),
            ("施工项目.webp", false, Filter::Pictures),
            ("河岸项目.FLAC", false, Filter::Audio),
            ("展览项目.mp4", false, Filter::Video),
            ("项目备份.tar.gz", false, Filter::Archives),
            ("项目草稿.blend", false, Filter::Other),
            ("招标备忘", false, Filter::Other),
            ("项目资料.pdf", true, Filter::Folders),
        ] {
            let entry = EntryRef {
                name,
                path: name,
                size: 0,
                modified: 0,
                is_dir,
                source: "local",
            };
            for filter in Filter::OPTIONS {
                let matches = filter == Filter::All
                    || filter == expected
                    || (filter == Filter::Files && !is_dir);
                assert_eq!(
                    Query::parse(&filter.query("项目 | 招标"))
                        .unwrap()
                        .matches(&entry),
                    matches,
                    "{filter:?}: {name}",
                );
            }
            assert!(Query::parse(&expected.query(" ")).unwrap().matches(&entry));
        }
        let query = Filter::Pictures.query("项目 ext:pdf");
        assert!(!Query::parse(&query).unwrap().matches(&EntryRef {
            name: "项目.pdf",
            path: "项目.pdf",
            size: 0,
            modified: 0,
            is_dir: false,
            source: "local",
        }));
    }

    #[test]
    fn file_category_filters_the_whole_index_before_counting_and_paging() {
        use xs_core::index::{FileRecord, IndexBuilder};

        let mut builder = IndexBuilder::default();
        for row in 0..215 {
            for extension in ["pdf", "png", "bin"] {
                builder
                    .add(FileRecord {
                        path: format!("/synthetic/项目{row:03}.{extension}"),
                        size: 0,
                        modified: 0,
                        is_dir: false,
                        source: "local".into(),
                    })
                    .unwrap();
            }
        }
        let index = builder.finish(1).unwrap();
        for (offset, length) in [(0, 200), (200, 15)] {
            let result = index
                .search_sorted(
                    &Filter::Pictures.query("项目"),
                    &SearchOptions {
                        limit: 200,
                        offset,
                        time_budget_ms: 5000,
                    },
                    None,
                    ColumnSort {
                        column: SortColumn::Name,
                        descending: false,
                    },
                )
                .unwrap();
            assert!(result.complete && result.coverage_complete);
            assert_eq!(result.total, 215);
            assert_eq!(result.results.len(), length);
            assert_eq!(result.results[0].name, format!("项目{offset:03}.png"));
            assert!(result.results.iter().all(|hit| hit.name.ends_with(".png")));
        }
    }
    #[test]
    fn archives_policy_and_global_wizard_controls_apply_cleanly() {
        let archives = TypePolicy::Archives.extensions("").unwrap().unwrap();
        assert!(archives.contains(&"zip".to_owned()));
        assert!(archives.contains(&"7z".to_owned()));
        let mut wizard = Wizard::new(&Settings::default(), false);
        wizard.global_policy = TypePolicy::Documents;
        wizard.toggle_volume("/synthetic/disk_d", true);
        assert!(wizard.has_root("/synthetic/disk_d"));
        assert_eq!(wizard.roots[0].policy, TypePolicy::Documents);
        wizard.global_policy = TypePolicy::Code;
        wizard.global_smart_excludes = false;
        wizard.apply_global_policy();
        assert_eq!(wizard.roots[0].policy, TypePolicy::Code);
        assert!(!wizard.roots[0].smart_excludes);
        wizard.toggle_volume("/synthetic/disk_d", false);
        assert!(wizard.roots.is_empty());
    }
    #[test]
    fn smart_mode_wraps_plain_ascii_without_touching_explicit_syntax() {
        assert_eq!(apply_smart_mode("xssj", true), "smart:xssj");
        assert_eq!(apply_smart_mode("xssj", false), "xssj");
        assert_eq!(apply_smart_mode("ext:pdf", true), "ext:pdf");
        assert_eq!(
            apply_smart_mode("budget | meeting", true),
            "budget | meeting"
        );
        assert_eq!(apply_smart_mode("寻枢", true), "寻枢");
    }
}

#[cfg(test)]
mod unified_app_tests {
    use super::*;

    #[test]
    fn app_queries_do_not_consume_file_query_syntax() {
        assert_eq!(app_query("  app:记事本 "), (true, "记事本"));
        assert_eq!(app_query(" > Terminal "), (true, "Terminal"));
        assert_eq!(app_query("报告"), (false, "报告"));
        assert!(!app_matches_allowed("ext:pdf"));
        assert!(!app_matches_allowed("file: <报告>"));
        assert!(app_matches_allowed("app:"));
        assert!(app_matches_allowed("记事本"));
        assert!(!app_matches_allowed("git:项目"));
        assert!(!app_matches_allowed("bm:Rust"));
        assert_eq!(routed_query("bm: Rust"), (SearchSource::Bookmarks, "Rust"));
        assert_eq!(
            routed_query("bookmark:参考"),
            (SearchSource::Bookmarks, "参考")
        );
        assert_eq!(
            routed_query("git:repo"),
            (SearchSource::Repositories, "repo")
        );
        assert_eq!(routed_query("ext:pdf"), (SearchSource::All, "ext:pdf"));
        assert!(!source_suggestions_allowed("ext:pdf"));
        assert!(source_suggestions_allowed("bm:Rust"));
    }
}
