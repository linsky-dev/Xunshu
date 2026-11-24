//! 跨平台本地应用程序发现（严格只读本地开始菜单与 .desktop 入口，不联网）。
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppEntry {
    pub name: String,
    pub launch_target: String,
    pub source: String,
}

/// 扫描当前系统的常用应用快捷方式（Windows 开始菜单 / Linux .desktop）。
pub fn discover_installed_apps() -> Vec<AppEntry> {
    let mut apps = Vec::new();
    #[cfg(windows)]
    {
        for env_key in ["PROGRAMDATA", "APPDATA"] {
            if let Ok(base) = std::env::var(env_key) {
                let dir = PathBuf::from(base)
                    .join("Microsoft")
                    .join("Windows")
                    .join("Start Menu")
                    .join("Programs");
                collect_windows_shortcuts(&dir, 0, &mut apps);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut dirs = vec![PathBuf::from("/usr/share/applications")];
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/applications"));
        }
        for dir in dirs {
            collect_linux_desktop_entries(&dir, &mut apps);
        }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    apps
}

#[cfg(windows)]
fn collect_windows_shortcuts(dir: &Path, depth: usize, out: &mut Vec<AppEntry>) {
    if depth > 4 || out.len() >= 2000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_windows_shortcuts(&path, depth + 1, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("lnk") || ext.eq_ignore_ascii_case("exe") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let lower = stem.to_ascii_lowercase();
                    if lower.contains("uninstall") || lower.contains("卸载") {
                        continue;
                    }
                    out.push(AppEntry {
                        name: stem.to_owned(),
                        launch_target: path.to_string_lossy().into_owned(),
                        source: "start_menu".into(),
                    });
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn collect_linux_desktop_entries(dir: &Path, out: &mut Vec<AppEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(app) = parse_desktop_entry(&text, &path) {
                out.push(app);
            }
        }
        if out.len() >= 2000 {
            break;
        }
    }
}

pub fn parse_desktop_entry(content: &str, path: &Path) -> Option<AppEntry> {
    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut no_display = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("NoDisplay=") {
            no_display = v.eq_ignore_ascii_case("true");
        } else if let Some(v) = line.strip_prefix("Name=") {
            if name.is_none() && !v.trim().is_empty() {
                name = Some(v.trim().to_owned());
            }
        } else if let Some(v) = line.strip_prefix("Exec=") {
            if exec.is_none() && !v.trim().is_empty() {
                exec = Some(v.trim().to_owned());
            }
        }
    }
    if no_display {
        return None;
    }
    Some(AppEntry {
        name: name?,
        launch_target: exec.unwrap_or_else(|| path.to_string_lossy().into_owned()),
        source: "desktop_entry".into(),
    })
}

/// 按 Desktop Entry Exec 的引号/转义规则解析参数，不经过 shell。
/// 未提供文件或 URL 时省略对应占位符；无法安全解释的字段码拒绝启动。
/// 不进行变量替换或命令替换，防止 Exec 中的 shell 元字符被执行。
pub fn parse_desktop_exec(raw: &str) -> Result<Vec<String>, String> {
    if raw.len() > 16 * 1024 || raw.contains('\0') {
        return Err("应用启动参数过长或包含 NUL".into());
    }
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut present = false;
    for ch in raw.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            present = true;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
            present = true;
        } else if ch.is_whitespace() && !quoted {
            if present {
                tokens.push(std::mem::take(&mut token));
                present = false;
            }
        } else {
            token.push(ch);
            present = true;
        }
        if tokens.len() > 128 {
            return Err("应用启动参数过多".into());
        }
    }
    if escaped || quoted {
        return Err(".desktop Exec 的转义或引号未闭合".into());
    }
    if present {
        tokens.push(token);
    }
    let mut args = Vec::new();
    for token in tokens {
        if matches!(
            token.as_str(),
            "%f" | "%F" | "%u" | "%U" | "%i" | "%c" | "%k"
        ) {
            // 不附带文件、URL 或图标参数；客户端应按 Desktop Entry 协议省略字段码。
            continue;
        }
        if token.contains('%') {
            let mut expanded = String::new();
            let mut chars = token.chars();
            while let Some(ch) = chars.next() {
                if ch != '%' {
                    expanded.push(ch);
                    continue;
                }
                if chars.next() == Some('%') {
                    expanded.push('%');
                } else {
                    return Err("Exec 中存在未知或不允许拼接的字段码".into());
                }
            }
            args.push(expanded);
        } else {
            args.push(token);
        }
    }
    if args.first().is_none_or(|bin| bin.is_empty()) {
        return Err("应用启动命令为空".into());
    }
    Ok(args)
}

/// 按前缀优先、子串其次匹配本地应用列表。
pub fn match_apps<'a>(apps: &'a [AppEntry], query: &str, limit: usize) -> Vec<&'a AppEntry> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return apps.iter().take(limit).collect();
    }
    let mut scored: Vec<(u8, &'a AppEntry)> = apps
        .iter()
        .filter_map(|app| {
            let lower = app.name.to_lowercase();
            if lower == needle {
                Some((0, app))
            } else if lower.starts_with(&needle) {
                Some((1, app))
            } else if lower.contains(&needle) {
                Some((2, app))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().take(limit).map(|(_, app)| app).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_desktop_entries_and_ranks_app_matches() {
        let sample = "[Desktop Entry]\nName=Visual Studio Code\nExec=/usr/bin/code %F\n";
        let app =
            parse_desktop_entry(sample, Path::new("/usr/share/applications/code.desktop")).unwrap();
        assert_eq!(app.name, "Visual Studio Code");
        let hidden = "[Desktop Entry]\nName=Hidden\nNoDisplay=true\n";
        assert!(parse_desktop_entry(hidden, Path::new("/tmp/h.desktop")).is_none());
        let apps = vec![
            app,
            AppEntry {
                name: "Codecs Tool".into(),
                launch_target: "/usr/bin/codecs".into(),
                source: "desktop_entry".into(),
            },
        ];
        let hits = match_apps(&apps, "code", 5);
        assert_eq!(hits[0].name, "Codecs Tool");
        assert_eq!(hits[1].name, "Visual Studio Code");
    }
    #[test]
    fn desktop_exec_preserves_quoted_spaces_and_disallows_unknown_codes() {
        let args = parse_desktop_exec(
            r#"/usr/bin/myapp --name "项目 文件" --label a\ b %F --literal=100%%"#,
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "/usr/bin/myapp",
                "--name",
                "项目 文件",
                "--label",
                "a b",
                "--literal=100%"
            ]
        );
        assert!(parse_desktop_exec("/usr/bin/app %Z").is_err());
        assert!(parse_desktop_exec("/usr/bin/app \"unclosed").is_err());
        assert!(parse_desktop_exec("/usr/bin/app %f.txt").is_err());
    }
}
