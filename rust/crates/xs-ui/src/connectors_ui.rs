//! 本地应用启动器与只读连接器（Chrome/Edge 书签、本地 Git 仓库）统一面板。
use eframe::egui::{self, RichText};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};
use xs_content::connectors::{self, ConnectorItem};
use xs_platform::apps::{self, AppEntry};

use crate::{ACCENT, ERROR, MUTED};

#[derive(Default)]
pub(crate) struct ConnectorsPane {
    pub(crate) open: bool,
    filter: String,
    apps_loaded: bool,
    apps: Vec<AppEntry>,
    bookmarks_path: String,
    bookmarks: Vec<ConnectorItem>,
    git_dir: String,
    git_repos: Vec<ConnectorItem>,
    source_manifest_path: String,
    source_data_path: String,
    static_source_name: String,
    remember_source: bool,
    persisted_grant: Option<(PathBuf, xs_platform::FileRevision)>,
    static_items: Vec<ConnectorItem>,
    notice: Option<String>,
    error: Option<String>,
    match_cache: RefCell<Option<ConnectorMatchCache>>,
}

struct ConnectorMatchCache {
    query: String,
    bookmarks: bool,
    git: bool,
    static_sources: bool,
    limit: usize,
    hits: Vec<ConnectorItem>,
}

impl ConnectorsPane {
    /// 主搜索与快捷搜索只使用用户在连接器面板明确载入的记录。
    /// 支持限定来源，最大返回数量统一受限。
    pub(crate) fn matching_items(
        &self,
        query: &str,
        bookmarks: bool,
        git: bool,
        static_sources: bool,
        limit: usize,
    ) -> Vec<ConnectorItem> {
        let static_sources = static_sources && self.grant_current();
        let limit = limit.min(12);
        if let Some(cache) = self.match_cache.borrow().as_ref() {
            if cache.query == query
                && cache.bookmarks == bookmarks
                && cache.git == git
                && cache.static_sources == static_sources
                && cache.limit == limit
            {
                return cache.hits.clone();
            }
        }
        let hits: Vec<_> = connectors::match_connector_items(
            self.bookmarks
                .iter()
                .filter(move |_| bookmarks)
                .chain(self.git_repos.iter().filter(move |_| git))
                .chain(self.static_items.iter().filter(move |_| static_sources)),
            query,
            limit,
        )
        .into_iter()
        .cloned()
        .collect();
        *self.match_cache.borrow_mut() = Some(ConnectorMatchCache {
            query: query.to_owned(),
            bookmarks,
            git,
            static_sources,
            limit,
            hits: hits.clone(),
        });
        hits
    }

    pub(crate) fn grant_current(&self) -> bool {
        self.persisted_grant
            .as_ref()
            .is_none_or(|(profile, revision)| connectors::static_grant_current(profile, *revision))
    }
    pub(crate) fn open_item(item: &ConnectorItem, ctx: &egui::Context) -> anyhow::Result<()> {
        match item.source.as_str() {
            "connector:chrome_bookmarks" => {
                // 载入时已经过滤过危险 scheme，这里再检查一次，避免未来入口绕过。
                anyhow::ensure!(
                    connectors::allowed_web_uri(&item.uri),
                    "书签不是允许的 HTTP(S) 链接"
                );
                ctx.open_url(egui::OpenUrl::new_tab(&item.uri));
                Ok(())
            }
            "connector:git_repo" => crate::open_path(Path::new(&item.uri)),
            source if source.starts_with("plugin:") => {
                let mut parts = source.split(':');
                let _ = parts.next();
                let id = parts.next().unwrap_or_default();
                let kind = parts.next().unwrap_or_default();
                anyhow::ensure!(!id.is_empty() && parts.next().is_none(), "来源标识无效");
                match kind {
                    "web" => {
                        anyhow::ensure!(
                            connectors::allowed_web_uri(&item.uri),
                            "仅允许 HTTP(S) 页面"
                        );
                        ctx.open_url(egui::OpenUrl::new_tab(&item.uri));
                        Ok(())
                    }
                    "local" => {
                        xs_platform::validate_local_path(Path::new(&item.uri), false)?;
                        crate::open_path(Path::new(&item.uri))
                    }
                    _ => anyhow::bail!("不允许此来源类型执行操作"),
                }
            }
            _ => anyhow::bail!("未知的连接器类型"),
        }
    }
    pub(crate) fn toggle(&mut self) {
        self.open = !self.open;
        if self.open && !self.apps_loaded {
            self.apps = apps::discover_installed_apps();
            self.apps_loaded = true;
        }
    }

    pub(crate) fn show(&mut self, ctx: &egui::Context, profile: &Path) {
        if !self.open {
            return;
        }
        let mut open = self.open;
        egui::Window::new("应用启动器与本地连接器")
            .open(&mut open)
            .default_size([560.0, 440.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("过滤：");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .desired_width(280.0)
                            .hint_text("输入应用名、书签标题或仓库名"),
                    );
                    if ui.button("刷新本机应用").clicked() {
                        self.apps = apps::discover_installed_apps();
                        self.apps_loaded = true;
                    }
                });
                if let Some(error) = &self.error {
                    ui.colored_label(ERROR, error);
                }
                if let Some(notice) = &self.notice {
                    ui.label(RichText::new(notice).small().color(ACCENT));
                }
                ui.separator();
                ui.label(
                    RichText::new(format!("本机应用（已发现 {} 个）", self.apps.len())).strong(),
                );
                let matched_apps = apps::match_apps(&self.apps, &self.filter, 12);
                egui::ScrollArea::vertical()
                    .id_salt("apps_list")
                    .max_height(120.0)
                    .show(ui, |ui| {
                        if matched_apps.is_empty() {
                            ui.label(RichText::new("未匹配到本机应用。").small().color(MUTED));
                        }
                        let mut launch_app: Option<String> = None;
                        for app in matched_apps {
                            ui.horizontal(|ui| {
                                if ui.small_button("启动").clicked() {
                                    launch_app = Some(app.launch_target.clone());
                                }
                                ui.strong(&app.name);
                                ui.label(RichText::new(&app.launch_target).small().color(MUTED));
                            });
                        }
                        if let Some(target) = launch_app {
                            if let Err(err) = launch_app_entry(&target) {
                                self.error = Some(format!("启动应用失败：{err:#}"));
                            } else {
                                self.notice = Some(format!("已启动：{target}"));
                            }
                        }
                    });
                ui.separator();
                ui.label(RichText::new("Chrome / Edge 本地书签连接器（只读离线）").strong());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.bookmarks_path)
                            .desired_width(340.0)
                            .hint_text("输入本地 Bookmarks JSON 文件绝对路径"),
                    );
                    if ui.button("载入书签").clicked() {
                        self.error = None;
                        self.notice = None;
                        match connectors::load_chrome_bookmarks(Path::new(
                            self.bookmarks_path.trim(),
                        )) {
                            Ok(items) => {
                                self.notice =
                                    Some(format!("已离线载入 {} 条浏览器书签。", items.len()));
                                self.bookmarks = items;
                                self.match_cache.borrow_mut().take();
                            }
                            Err(err) => self.error = Some(format!("书签载入失败：{err:#}")),
                        }
                    }
                });
                let needle = self.filter.trim().to_lowercase();
                egui::ScrollArea::vertical()
                    .id_salt("bookmarks_list")
                    .max_height(95.0)
                    .show(ui, |ui| {
                        for item in self
                            .bookmarks
                            .iter()
                            .filter(|item| {
                                needle.is_empty()
                                    || item.title.to_lowercase().contains(&needle)
                                    || item.uri.to_lowercase().contains(&needle)
                            })
                            .take(20)
                        {
                            ui.horizontal_wrapped(|ui| {
                                if ui.small_button("打开").clicked() {
                                    ctx.open_url(egui::OpenUrl::new_tab(&item.uri));
                                }
                                ui.strong(&item.title);
                                ui.label(RichText::new(&item.snippet).small().color(MUTED));
                            });
                        }
                    });
                ui.separator();
                ui.label(RichText::new("本地 Git 仓库连接器（只读离线）").strong());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.git_dir)
                            .desired_width(340.0)
                            .hint_text("输入本地 Git 仓库目录绝对路径"),
                    );
                    if ui.button("探测仓库").clicked() {
                        self.error = None;
                        self.notice = None;
                        match connectors::inspect_local_git_repo(Path::new(self.git_dir.trim())) {
                            Ok(Some(repo)) => {
                                self.notice = Some(format!("已发现仓库：{}", repo.title));
                                self.git_repos.retain(|r| r.uri != repo.uri);
                                self.git_repos.insert(0, repo);
                                self.match_cache.borrow_mut().take();
                            }
                            Ok(None) => self.error = Some("该目录未包含 .git 仓库。".into()),
                            Err(err) => self.error = Some(format!("探测失败：{err:#}")),
                        }
                    }
                });
                let mut open_repo: Option<String> = None;
                for repo in &self.git_repos {
                    if needle.is_empty() || repo.title.to_lowercase().contains(&needle) {
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("打开目录").clicked() {
                                open_repo = Some(repo.uri.clone());
                            }
                            ui.strong(&repo.title);
                            ui.label(RichText::new(&repo.snippet).small().color(MUTED));
                        });
                    }
                }
                if let Some(uri) = open_repo {
                    if let Err(err) = crate::open_path(Path::new(&uri)) {
                        self.error = Some(format!("打开仓库目录失败：{err:#}"));
                    }
                }
                ui.separator();
                ui.label(RichText::new("声明式本地来源（不执行插件代码）").strong());
                ui.label(
                    RichText::new(
                        "手动授权导入 JSON 清单及数据；可一键撤销；仅支持已校验的链接和本地路径。",
                    )
                    .small()
                    .color(MUTED),
                );
                ui.horizontal(|ui| {
                    ui.label("清单");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.source_manifest_path)
                            .desired_width(390.0)
                            .hint_text("source-manifest.json 绝对路径"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("数据");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.source_data_path)
                            .desired_width(390.0)
                            .hint_text("source-items.json 绝对路径"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.remember_source, "记住授权（内容变化须重新授权）");
                });
                ui.horizontal(|ui| {
                    if ui.button("授权载入来源").clicked() {
                        let manifest = Path::new(self.source_manifest_path.trim());
                        let data = Path::new(self.source_data_path.trim());
                        let loaded = if self.remember_source {
                            connectors::remember_static_source(profile, manifest, data).map(
                                |grant| {
                                    (
                                        grant.manifest,
                                        grant.items,
                                        Some((profile.to_path_buf(), grant.grant_revision)),
                                    )
                                },
                            )
                        } else {
                            connectors::load_static_source(manifest, data).and_then(
                                |(manifest, items)| {
                                    connectors::revoke_static_source(profile)?;
                                    Ok((manifest, items, None))
                                },
                            )
                        };
                        match loaded {
                            Ok((manifest, items, grant)) => {
                                self.static_source_name = manifest.name;
                                self.static_items = items;
                                self.persisted_grant = grant;
                                self.match_cache.borrow_mut().take();
                                self.error = None;
                                self.notice = Some(format!(
                                    "已授权载入 {} 条：{}",
                                    self.static_items.len(),
                                    self.static_source_name
                                ));
                            }
                            Err(err) => self.error = Some(format!("来源载入失败：{err:#}")),
                        }
                    }
                    if ui.button("恢复已记住来源").clicked() {
                        match connectors::restore_static_source(profile) {
                            Ok(Some(grant)) => {
                                self.static_source_name = grant.manifest.name;
                                self.static_items = grant.items;
                                self.persisted_grant =
                                    Some((profile.to_path_buf(), grant.grant_revision));
                                self.match_cache.borrow_mut().take();
                                self.error = None;
                                self.notice = Some(format!(
                                    "已恢复 {} 条已授权来源",
                                    self.static_items.len()
                                ));
                            }
                            Ok(None) => self.error = Some("没有已记住的来源授权".into()),
                            Err(err) => self.error = Some(format!("恢复授权失败：{err:#}")),
                        }
                    }
                    if ui.button("撤销此来源授权").clicked() {
                        self.static_items.clear();
                        self.static_source_name.clear();
                        self.persisted_grant = None;
                        self.match_cache.borrow_mut().take();
                        match connectors::revoke_static_source(profile) {
                            Ok(()) => {
                                self.notice = Some("已撤销来源授权并移除本次载入数据".into());
                                self.error = None;
                            }
                            Err(err) => {
                                self.error =
                                    Some(format!("本次数据已移除，但持久撤销未确认：{err:#}"))
                            }
                        }
                    }
                    if !self.static_source_name.is_empty() {
                        ui.label(format!(
                            "{} · {} 项",
                            self.static_source_name,
                            self.static_items.len()
                        ));
                    }
                });
            });
        self.open = open;
    }
}

/// 直接启动本机应用：Linux 解析 .desktop Exec 并直接 spawn，Windows 通过外壳打开 .lnk/.exe。
pub(crate) fn launch_app_entry(target: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let args = apps::parse_desktop_exec(target)
            .map_err(|e| anyhow::anyhow!("无法解析本机应用启动命令：{e}"))?;
        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        cmd.spawn()?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        crate::open_path(Path::new(target))
    }
}
