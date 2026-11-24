//! 本地只读外部数据连接器（严格离线，仅读取用户明确授权的本地文件）。
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use xs_platform::FileRevision;

const MAX_BOOKMARK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BOOKMARK_DEPTH: usize = 32;
const MAX_BOOKMARK_ITEMS: usize = 20_000;
const MAX_FIELD_BYTES: usize = 8 * 1024;
const MAX_GIT_METADATA_BYTES: u64 = 256 * 1024;
const MAX_PLUGIN_MANIFEST_BYTES: u64 = 16 * 1024;
const MAX_PLUGIN_DATA_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PLUGIN_ITEMS: usize = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectorItem {
    pub source: String,
    pub title: String,
    pub uri: String,
    pub snippet: String,
}

/// 有界、确定性的本地连接器检索。不会主动扫描用户磁盘或访问网络。
/// 精确标题 > 标题前缀 > 标题包含 > URL/说明包含；同分保持原始次序。
pub fn match_connector_items<'a>(
    items: impl IntoIterator<Item = &'a ConnectorItem>,
    needle: &str,
    max_results: usize,
) -> Vec<&'a ConnectorItem> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() || max_results == 0 {
        return Vec::new();
    }
    // 只保留最高分的 K 条：常驻内存 O(K)，避免上万书签同时分配/排序。
    let mut ranked: Vec<(u8, usize, &'a ConnectorItem)> = Vec::with_capacity(max_results.min(32));
    for (order, item) in items.into_iter().enumerate() {
        let title = item.title.to_lowercase();
        let grade = if title == needle {
            0
        } else if title.starts_with(&needle) {
            1
        } else if title.contains(&needle) {
            2
        } else if item.snippet.to_lowercase().contains(&needle) {
            3
        } else {
            continue;
        };
        let position = ranked.partition_point(|(other_grade, other_order, _)| {
            (*other_grade, *other_order) < (grade, order)
        });
        if position < max_results {
            ranked.insert(position, (grade, order, item));
            if ranked.len() > max_results {
                ranked.pop();
            }
        }
    }
    ranked.into_iter().map(|(_, _, item)| item).collect()
}

/// 浏览器页面链接的统一校验规则：禁止控制字符、空 host、反斜杠和 userinfo。
/// 只能由明确的用户打开动作触发；此函数本身不访问网络。
/// 同时在导入和点击时验证，防止来源被其他入口绕过。
pub fn allowed_web_uri(uri: &str) -> bool {
    let Some(rest) = uri
        .strip_prefix("https://")
        .or_else(|| uri.strip_prefix("http://"))
    else {
        return false;
    };
    if uri.len() > MAX_FIELD_BYTES
        || uri
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains(['@', '%'])
        && !authority.starts_with(':')
        && !authority.ends_with(':')
}

/// 展示书签时只显示站点名，不暴露 URL 中的用户名、密码、路径及查询令牌。
fn safe_bookmark_site(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or_default();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority.rsplit('@').next().unwrap_or_default()
}

/// 有界读取本地书签文件，拒绝网络路径/超大文件并限制读取量。
/// UI 和其他消费者均应经此入口，不得直接 fs::read 到内存。
pub fn load_chrome_bookmarks(path: &Path) -> Result<Vec<ConnectorItem>> {
    let file = xs_platform::open_local_read(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_BOOKMARK_BYTES,
        "书签文件超出 16 MiB 限制"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BOOKMARK_BYTES + 1).read_to_end(&mut bytes)?;
    parse_chrome_bookmarks_json(&bytes)
}

/// 解析 Chromium / Chrome / Edge 本地 `Bookmarks` JSON，提取可搜索的书签条目。
pub fn parse_chrome_bookmarks_json(bytes: &[u8]) -> Result<Vec<ConnectorItem>> {
    ensure!(
        bytes.len() as u64 <= MAX_BOOKMARK_BYTES,
        "书签文件超出 16 MiB 限制"
    );
    let root: Value = serde_json::from_slice(bytes).context("无法解析浏览器书签 JSON")?;
    let mut out = Vec::new();
    if let Some(roots) = root.get("roots").and_then(Value::as_object) {
        let mut visited = 0;
        for (folder_name, node) in roots {
            collect_bookmark_node(node, folder_name, 0, &mut visited, &mut out)?;
        }
    }
    Ok(out)
}

fn collect_bookmark_node(
    node: &Value,
    folder_path: &str,
    depth: usize,
    visited: &mut usize,
    out: &mut Vec<ConnectorItem>,
) -> Result<()> {
    *visited += 1;
    ensure!(
        *visited <= MAX_BOOKMARK_ITEMS * 2,
        "书签 JSON 节点超出 40000 项限制"
    );
    ensure!(depth <= MAX_BOOKMARK_DEPTH, "书签目录嵌套层级过深");
    ensure!(out.len() <= MAX_BOOKMARK_ITEMS, "书签项目超出 20000 条限制");
    let Some(obj) = node.as_object() else {
        return Ok(());
    };
    let name = obj.get("name").and_then(Value::as_str).unwrap_or("").trim();
    ensure!(name.len() <= MAX_FIELD_BYTES, "书签名称过长");
    match obj.get("type").and_then(Value::as_str) {
        Some("url") => {
            if let Some(url) = obj.get("url").and_then(Value::as_str) {
                ensure!(url.len() <= MAX_FIELD_BYTES, "书签 URL 过长");
                // 无 UI 自动执行协议：仅允许普通浏览器页面；拒绝 javascript:/data:/file:。
                if allowed_web_uri(url) {
                    ensure!(out.len() < MAX_BOOKMARK_ITEMS, "书签项目超出 20000 条限制");
                    out.push(ConnectorItem {
                        source: "connector:chrome_bookmarks".into(),
                        title: if name.is_empty() {
                            let site = safe_bookmark_site(url);
                            if site.is_empty() {
                                "未命名书签".to_owned()
                            } else {
                                site.to_owned()
                            }
                        } else {
                            name.to_owned()
                        },
                        uri: url.to_owned(),
                        snippet: format!("{folder_path} · {}", safe_bookmark_site(url)),
                    });
                }
            }
        }
        _ => {
            let next_folder = if name.is_empty() {
                folder_path.to_owned()
            } else {
                format!("{folder_path}/{name}")
            };
            ensure!(next_folder.len() <= MAX_FIELD_BYTES * 2, "书签目录路径过长");
            if let Some(children) = obj.get("children").and_then(Value::as_array) {
                for child in children {
                    collect_bookmark_node(child, &next_folder, depth + 1, visited, out)?;
                }
            }
        }
    }
    Ok(())
}

fn read_git_text(path: &Path) -> Result<String> {
    let file = xs_platform::open_local_read(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_GIT_METADATA_BYTES,
        "Git 元数据超出大小限制"
    );
    let mut bytes = Vec::new();
    file.take(MAX_GIT_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_GIT_METADATA_BYTES,
        "Git 元数据超出大小限制"
    );
    String::from_utf8(bytes).context("Git 元数据不是 UTF-8")
}

/// 本地只读探测 Git 仓库元数据（读取 `.git/HEAD` 与 `.git/config`，不执行外部 git 命令）。
pub fn inspect_local_git_repo(repo_root: &Path) -> Result<Option<ConnectorItem>> {
    let git_dir = repo_root.join(".git");
    if !git_dir.is_dir() {
        return Ok(None);
    }
    let head = read_git_text(&git_dir.join("HEAD"))?.trim().to_owned();
    let branch = head
        .strip_prefix("ref: refs/heads/")
        .unwrap_or(if head.is_empty() { "detached" } else { &head });
    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("git-repo")
        .to_owned();
    // 远程 URL 可能嵌入 userinfo、token、查询参数，绝不将原始地址传给 UI。
    let config_text = read_git_text(&git_dir.join("config"))?;
    let has_remote = config_text
        .lines()
        .any(|line| line.trim_start().starts_with("url ="));
    let remote = if has_remote {
        "已配置（地址隐藏）"
    } else {
        "未配置"
    };
    Ok(Some(ConnectorItem {
        source: "connector:git_repo".into(),
        title: repo_name,
        uri: repo_root.to_string_lossy().into_owned(),
        snippet: format!("分支: {branch} · 远端: {remote}"),
    }))
}

/// 最小权限、无进程执行能力的插件来源清单。仅支持用户主动选择的本地 JSON 文件。
/// 不接受脚本、网络拉取、自动发现或插件权限提升。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaticSourceManifest {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub kind: String,
    pub offline_only: bool,
}

impl StaticSourceManifest {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.kind == "static_json" && self.offline_only,
            "来源清单要求 schema=1、static_json 和 offline_only=true"
        );
        ensure!(
            self.id.len() >= 3
                && self.id.len() <= 48
                && self
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                && self
                    .id
                    .bytes()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase()),
            "来源 ID 必须是 3–48 字节的小写字母、数字和连字符"
        );
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(char::is_control),
            "来源名称无效"
        );
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticSourceRow {
    title: String,
    uri: String,
    #[serde(default)]
    snippet: String,
}

fn bounded_local_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = xs_platform::open_local_read(path)?;
    ensure!(file.metadata()?.len() <= limit, "来源文件超过最大允许大小");
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "来源文件超过最大允许大小");
    Ok(bytes)
}

/// 导入用户明确选择的静态来源：读取清单和 JSON 数据，不运行任何第三方代码。
/// 打开每条结果前仍需 UI 二次校验 URI，并且必须由用户操作触发。
pub fn load_static_source(
    manifest_path: &Path,
    data_path: &Path,
) -> Result<(StaticSourceManifest, Vec<ConnectorItem>)> {
    let manifest = bounded_local_read(manifest_path, MAX_PLUGIN_MANIFEST_BYTES)?;
    let data = bounded_local_read(data_path, MAX_PLUGIN_DATA_BYTES)?;
    parse_static_source(&manifest, &data)
}

fn parse_static_source(
    manifest_bytes: &[u8],
    data_bytes: &[u8],
) -> Result<(StaticSourceManifest, Vec<ConnectorItem>)> {
    let manifest: StaticSourceManifest =
        serde_json::from_slice(manifest_bytes).context("来源清单 JSON 无效")?;
    manifest.validate()?;
    let entries: Vec<StaticSourceRow> =
        serde_json::from_slice(data_bytes).context("来源数据 JSON 无效")?;
    ensure!(
        entries.len() <= MAX_PLUGIN_ITEMS,
        "声明式来源最多接受 5000 条记录"
    );
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        ensure!(
            !entry.title.trim().is_empty()
                && entry.title.len() <= 512
                && !entry.title.contains("://")
                && !entry.title.chars().any(char::is_control),
            "来源记录标题不合法"
        );
        ensure!(
            entry.uri.len() <= MAX_FIELD_BYTES
                && entry.snippet.len() <= 1024
                && !entry.snippet.contains("://")
                && !entry.snippet.chars().any(char::is_control),
            "来源记录字段过长或包含控制字符"
        );
        let kind = if allowed_web_uri(&entry.uri) {
            "web"
        } else {
            ensure!(
                !entry.uri.starts_with("https://") && !entry.uri.starts_with("http://"),
                "来源包含不安全的 HTTP(S) 链接"
            );
            let path = Path::new(&entry.uri);
            xs_platform::validate_local_path(path, false)
                .context("来源记录包含不安全或不存在的本地路径")?;
            "local"
        };
        items.push(ConnectorItem {
            source: format!("plugin:{}:{kind}", manifest.id),
            title: entry.title,
            uri: entry.uri,
            snippet: entry.snippet,
        });
    }
    Ok((manifest, items))
}

const GRANT_FILE: &str = "static-source-grant.json";
const GRANT_LIMIT: u64 = 256 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceGrant {
    manifest: PathBuf,
    data: PathBuf,
    manifest_sha256: String,
    data_sha256: String,
}

/// An explicit grant is tied to exact manifest/data bytes. It never stores
/// imported item text or URLs. No third-party code is executed on restore.
#[derive(Debug)]
pub struct GrantedSource {
    pub manifest: StaticSourceManifest,
    pub items: Vec<ConnectorItem>,
    pub grant_revision: FileRevision,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn grant_lock(profile: &Path) -> Result<std::fs::File> {
    xs_platform::create_local_dir_all(profile)?;
    let lock = xs_platform::open_or_create_local_lock(&profile.join("static-source-grant.lock"))?;
    FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

/// Persist only after the user explicitly selects remember. Changed content
/// requires a new grant instead of silently importing a replacement.
pub fn remember_static_source(
    profile: &Path,
    manifest: &Path,
    data: &Path,
) -> Result<GrantedSource> {
    let _lock = grant_lock(profile)?;
    let manifest_bytes = bounded_local_read(manifest, MAX_PLUGIN_MANIFEST_BYTES)?;
    let data_bytes = bounded_local_read(data, MAX_PLUGIN_DATA_BYTES)?;
    let (parsed, items) = parse_static_source(&manifest_bytes, &data_bytes)?;
    let grant = SourceGrant {
        manifest: manifest.into(),
        data: data.into(),
        manifest_sha256: hash(&manifest_bytes),
        data_sha256: hash(&data_bytes),
    };
    let bytes = serde_json::to_vec(&Some(grant))?;
    ensure!(bytes.len() as u64 <= GRANT_LIMIT, "来源授权记录超出限制");
    let path = profile.join(GRANT_FILE);
    ensure!(
        xs_platform::atomic_write_local(&path, &bytes, true)?.is_durable(),
        "来源授权可能已提交，但持久化未确认"
    );
    Ok(GrantedSource {
        manifest: parsed,
        items,
        grant_revision: FileRevision::at_path(&path)?,
    })
}

/// Restore only a remembered grant. Missing or revoked grants return None;
/// malformed, replaced or altered input is a visible error, never an import.
pub fn restore_static_source(profile: &Path) -> Result<Option<GrantedSource>> {
    let path = profile.join(GRANT_FILE);
    let revision = match FileRevision::at_path(&path) {
        Ok(revision) => revision,
        Err(xs_platform::PlatformError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    };
    let _lock = grant_lock(profile)?;
    let Some(grant) =
        serde_json::from_slice::<Option<SourceGrant>>(&bounded_local_read(&path, GRANT_LIMIT)?)?
    else {
        return Ok(None);
    };
    let manifest_bytes = bounded_local_read(&grant.manifest, MAX_PLUGIN_MANIFEST_BYTES)?;
    let data_bytes = bounded_local_read(&grant.data, MAX_PLUGIN_DATA_BYTES)?;
    ensure!(
        hash(&manifest_bytes) == grant.manifest_sha256 && hash(&data_bytes) == grant.data_sha256,
        "来源内容已变化，请重新核对并授权"
    );
    let (manifest, items) = parse_static_source(&manifest_bytes, &data_bytes)?;
    ensure!(
        FileRevision::at_path(&path)? == revision,
        "来源授权记录已变更，请重试"
    );
    Ok(Some(GrantedSource {
        manifest,
        items,
        grant_revision: revision,
    }))
}

pub fn static_grant_current(profile: &Path, revision: FileRevision) -> bool {
    FileRevision::at_path(&profile.join(GRANT_FILE)).ok() == Some(revision)
}

/// Atomically replace with a tombstone: a successful revocation is durable,
/// and another process can invalidate previously loaded results by revision.
pub fn revoke_static_source(profile: &Path) -> Result<()> {
    let _lock = grant_lock(profile)?;
    ensure!(
        xs_platform::atomic_write_local(&profile.join(GRANT_FILE), b"null", true)?.is_durable(),
        "来源撤销可能已提交，但持久化未确认"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_uri_validator_rejects_credentials_and_ambiguous_urls() {
        assert!(allowed_web_uri("https://example.com/docs?q=one"));
        assert!(allowed_web_uri("http://127.0.0.1:8080/help"));
        assert!(allowed_web_uri("https://[::1]:8443/a"));
        for unsafe_url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://",
            "https://@example.com",
            "https://user:password@example.com/path",
            "https://%65xample.com/",
            "https://example.com\nmalicious",
            "https://example.com\\evil",
            "https://example.com/a b",
            "https://:443/",
            "https://example.com:/",
        ] {
            assert!(
                !allowed_web_uri(unsafe_url),
                "unexpectedly accepted {unsafe_url:?}"
            );
        }
    }

    #[test]
    fn browser_bookmarks_skip_invalid_url_and_never_display_userinfo() {
        let input = br#"{"roots":{"bookmark_bar":{"children":[
            {"type":"url","url":"https://user:pass@example.com/secret?q=token","name":""},
            {"type":"url","url":"https://valid.test/secret?q=token","name":""},
            {"type":"url","url":"javascript:alert(1)","name":"unsafe"}
        ]}}}"#;
        let items = parse_chrome_bookmarks_json(input).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "valid.test");
        assert!(!items[0].snippet.contains("secret"));
        assert!(!items[0].snippet.contains("token"));
    }

    #[test]
    fn declarative_source_import_is_bounded_and_revocable_at_caller() {
        let root = std::env::var_os("XS_CONTENT_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let tmp = tempfile::Builder::new()
            .prefix("xunshu-plugin-")
            .tempdir_in(root)
            .unwrap();
        let manifest = tmp.path().join("manifest.json");
        let data = tmp.path().join("items.json");
        std::fs::write(&manifest, r#"{"schema":1,"id":"my-notes","name":"我的笔记","kind":"static_json","offline_only":true}"#).unwrap();
        std::fs::write(&data, r#"[{"title":"设计说明","uri":"https://example.org/page?token=never-display","snippet":"本地已授权来源"}]"#).unwrap();
        let (m, items) = load_static_source(&manifest, &data).unwrap();
        assert_eq!(m.id, "my-notes");
        assert_eq!(items[0].source, "plugin:my-notes:web");
        assert!(!items[0].title.contains("token"));
        std::fs::write(
            &data,
            r#"[{"title":"恶意来源","uri":"javascript:alert(1)"}]"#,
        )
        .unwrap();
        assert!(load_static_source(&manifest, &data).is_err());
        std::fs::write(
            &data,
            r#"[{"title":"越权路径","uri":"/nonexistent/xunshu-file.txt"}]"#,
        )
        .unwrap();
        assert!(load_static_source(&manifest, &data).is_err());
    }

    #[test]
    fn remembered_source_is_bound_to_bytes_and_revocable_across_processes() {
        let root = std::env::var_os("XS_CONTENT_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let tmp = tempfile::Builder::new().tempdir_in(root).unwrap();
        let profile = tmp.path().join("profile");
        let manifest = tmp.path().join("manifest.json");
        let data = tmp.path().join("items.json");
        std::fs::write(
            &manifest,
            r#"{"schema":1,"id":"notes","name":"笔记","kind":"static_json","offline_only":true}"#,
        )
        .unwrap();
        let original = r#"[{"title":"安全条目","uri":"https://example.org/?token=sensitive"}]"#;
        std::fs::write(&data, original).unwrap();
        assert!(restore_static_source(&profile).unwrap().is_none());
        let granted = remember_static_source(&profile, &manifest, &data).unwrap();
        assert!(static_grant_current(&profile, granted.grant_revision));
        assert_eq!(
            restore_static_source(&profile).unwrap().unwrap().items,
            granted.items
        );
        assert!(!std::fs::read_to_string(profile.join(GRANT_FILE))
            .unwrap()
            .contains("sensitive"));
        std::fs::write(
            &data,
            r#"[{"title":"替换条目","uri":"https://example.org/new"}]"#,
        )
        .unwrap();
        assert!(restore_static_source(&profile).is_err());
        std::fs::write(&data, original).unwrap();
        revoke_static_source(&profile).unwrap();
        assert!(!static_grant_current(&profile, granted.grant_revision));
        assert!(restore_static_source(&profile).unwrap().is_none());
    }

    #[test]
    fn bookmark_without_name_does_not_display_url_credentials() {
        // Credential-bearing links are now rejected at import, not merely redacted in the UI.
        let forbidden = br#"{"roots":{"bar":{"type":"folder","children":[{"type":"url","url":"https://alice:secret@example.com/api?token=sensitive"}]}}}"#;
        assert!(parse_chrome_bookmarks_json(forbidden).unwrap().is_empty());
        let safe = br#"{"roots":{"bar":{"type":"folder","children":[{"type":"url","url":"https://example.com/api?token=sensitive"}]}}}"#;
        let items = parse_chrome_bookmarks_json(safe).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "example.com");
        assert!(!format!("{}{}", items[0].title, items[0].snippet).contains("sensitive"));
    }

    #[test]
    fn static_source_manifest_rejects_executable_or_network_permissions() {
        let mut manifest = StaticSourceManifest {
            schema: 1,
            id: "local-docs".into(),
            name: "我的资料".into(),
            kind: "static_json".into(),
            offline_only: true,
        };
        assert!(manifest.validate().is_ok());
        manifest.offline_only = false;
        assert!(manifest.validate().is_err());
        manifest.offline_only = true;
        manifest.kind = "script".into();
        assert!(manifest.validate().is_err());
        manifest.kind = "static_json".into();
        manifest.id = "../danger".into();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn parses_chrome_bookmarks_tree_offline() {
        let sample = br#"{
            "roots": {
                "bookmark_bar": {
                    "type": "folder",
                    "name": "Bookmarks Bar",
                    "children": [
                        { "type": "url", "name": "Rust Docs", "url": "https://doc.rust-lang.org/" },
                        {
                            "type": "folder",
                            "name": "Search",
                            "children": [
                                { "type": "url", "name": "Xunshu", "url": "https://example.invalid/xunshu" }
                            ]
                        }
                    ]
                }
            }
        }"#;
        let items = parse_chrome_bookmarks_json(sample).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Rust Docs");
        assert!(items[1].snippet.contains("Search"));
    }

    #[test]
    fn inspects_local_git_repository_without_spawning_git() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git.join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/example/xunshu.git\n",
        )
        .unwrap();
        let item = inspect_local_git_repo(dir.path()).unwrap().unwrap();
        assert!(item.snippet.contains("分支: main"));
        assert!(item.snippet.contains("地址隐藏"));
    }
    #[test]
    fn redacts_git_remote_with_embedded_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git.join("config"),
            "[remote \"origin\"]\n url = https://alice:SECRET_TOKEN@example.com/project.git?token=ALSO_SECRET\n").unwrap();
        let item = inspect_local_git_repo(dir.path()).unwrap().unwrap();
        assert!(!item.snippet.contains("SECRET"));
        assert!(!item.snippet.contains("alice"));
        assert!(!item.snippet.contains("example.com"));
    }

    #[test]
    fn bounds_bookmark_bytes_depth_and_entry_count() {
        assert!(parse_chrome_bookmarks_json(&vec![b'x'; MAX_BOOKMARK_BYTES as usize + 1]).is_err());
        let mut nested = serde_json::json!({"type": "url", "url": "https://example.test"});
        for _ in 0..(MAX_BOOKMARK_DEPTH + 2) {
            nested = serde_json::json!({"type": "folder", "children": [nested]});
        }
        let input = serde_json::json!({"roots": {"bookmark_bar": nested}});
        assert!(parse_chrome_bookmarks_json(&serde_json::to_vec(&input).unwrap()).is_err());
    }

    #[test]
    fn bookmarks_never_display_url_secrets() {
        let sample = br#"{"roots":{"bookmark_bar":{"type":"folder","children":[{"type":"url","name":"Private Portal","url":"https://company.example/hidden?token=CONFIDENTIAL#frag"}]}}}"#;
        let items = parse_chrome_bookmarks_json(sample).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].snippet.contains("company.example"));
        assert!(!items[0].snippet.contains("CONFIDENTIAL"));
        assert!(!items[0].snippet.contains("hidden"));
        let with_credentials = br#"{"roots":{"bookmark_bar":{"type":"folder","children":[{"type":"url","name":"Private Portal","url":"https://alice:SECRET@company.example/hidden"}]}}}"#;
        assert!(parse_chrome_bookmarks_json(with_credentials)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn connector_topk_equals_full_rank_on_large_input() {
        let items: Vec<ConnectorItem> = (0..20_000)
            .map(|i| ConnectorItem {
                source: "connector:chrome_bookmarks".into(),
                title: format!("Some notes {i}"),
                uri: "https://example.invalid".into(),
                snippet: "misc".into(),
            })
            .collect();
        let found = match_connector_items(items.iter(), "notes", 7);
        assert_eq!(found.len(), 7);
        assert_eq!(found[0].title, "Some notes 0");
        assert_eq!(found[6].title, "Some notes 6");
    }

    #[test]
    fn connector_rank_is_stable_and_bounded() {
        let sample = |title: &str, snippet: &str| ConnectorItem {
            source: "connector:chrome_bookmarks".into(),
            title: title.into(),
            uri: "https://example.invalid".into(),
            snippet: snippet.into(),
        };
        let items = [
            sample("Xunshu notes", ""),
            sample("notes", ""),
            sample("Notes archive", ""),
            sample("misc", "project notes"),
        ];
        let found = match_connector_items(items.iter(), " NOTES ", 3);
        assert_eq!(
            found
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            ["notes", "Notes archive", "Xunshu notes"]
        );
        assert!(match_connector_items(items.iter(), "", 4).is_empty());
        assert_eq!(match_connector_items(items.iter(), "notes", 1).len(), 1);
    }
}
