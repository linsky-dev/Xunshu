//! 当前配置下的文件名搜索历史与查询书签。
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};

const MAX_HISTORY: usize = 24;
const MAX_BOOKMARKS: usize = 48;
const MAX_FRECENCY: usize = 512;
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const HALF_LIFE_SECS: f64 = 7.0 * 24.0 * 3600.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FrecencyEntry {
    pub path: String,
    pub count: u32,
    pub last_used_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Activity {
    pub history: Vec<String>,
    pub bookmarks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frecency: Vec<FrecencyEntry>,
}

#[allow(dead_code)]
pub enum Edit {
    Remember(String),
    Bookmark(String),
    ForgetBookmark(String),
    ClearHistory,
    RecordLaunch(String),
}

impl Activity {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("desktop-activity.json");
        let file = match xs_platform::open_local_read(&path) {
            Ok(file) => file,
            Err(xs_platform::PlatformError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(Self::default())
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_BYTES, "搜索历史文件过大");
        let activity: Self = serde_json::from_slice(&bytes).context("搜索历史文件损坏")?;
        ensure!(
            activity.history.len() <= MAX_HISTORY
                && activity.bookmarks.len() <= MAX_BOOKMARKS
                && activity.frecency.len() <= MAX_FRECENCY,
            "搜索历史文件条数无效"
        );
        ensure!(
            activity
                .history
                .iter()
                .chain(&activity.bookmarks)
                .all(|query| !query.is_empty() && query.len() <= crate::desktop::MAX_QUERY_BYTES),
            "搜索历史文件包含无效查询"
        );
        Ok(activity)
    }

    pub fn edit(dir: &Path, edit: Edit) -> Result<Self> {
        xs_platform::create_local_dir_all(dir)?;
        let lock = xs_platform::open_or_create_local_lock(&dir.join("desktop-activity.lock"))?;
        let mut locked = false;
        for _ in 0..10 {
            if FileExt::try_lock_exclusive(&lock).is_ok() {
                locked = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        ensure!(locked, "搜索历史正在更新，请稍后重试");
        let mut activity = Self::load(dir)?;
        activity.apply(edit);
        xs_platform::atomic_write_local(
            &dir.join("desktop-activity.json"),
            &serde_json::to_vec(&activity)?,
            true,
        )?;
        Ok(activity)
    }

    fn apply(&mut self, edit: Edit) {
        match edit {
            Edit::Remember(query) => put_first(&mut self.history, query, MAX_HISTORY),
            Edit::Bookmark(query) => put_first(&mut self.bookmarks, query, MAX_BOOKMARKS),
            Edit::ForgetBookmark(query) => self.bookmarks.retain(|saved| *saved != query),
            Edit::ClearHistory => self.history.clear(),
            Edit::RecordLaunch(path) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                self.record_launch_at(path, now);
            }
        }
    }

    pub fn record_launch_at(&mut self, path: String, now_unix: u64) {
        let trimmed = path.trim();
        if trimmed.is_empty() || trimmed.len() > 4096 {
            return;
        }
        if let Some(pos) = self.frecency.iter().position(|entry| {
            if cfg!(windows) {
                entry.path.eq_ignore_ascii_case(trimmed)
            } else {
                entry.path == trimmed
            }
        }) {
            let mut entry = self.frecency.remove(pos);
            entry.count = entry.count.saturating_add(1);
            entry.last_used_unix = now_unix;
            self.frecency.insert(0, entry);
        } else {
            self.frecency.insert(
                0,
                FrecencyEntry {
                    path: trimmed.to_owned(),
                    count: 1,
                    last_used_unix: now_unix,
                },
            );
            self.frecency.truncate(MAX_FRECENCY);
        }
    }

    /// 构造当前搜索使用的有限加权表。由内核在全局候选排序时使用。
    pub fn frecency_boosts(&self, now_unix: u64) -> std::collections::HashMap<String, i64> {
        self.frecency
            .iter()
            .filter_map(|entry| {
                let boost = self.frecency_boost(&entry.path, now_unix);
                (boost > 0).then(|| {
                    let key = if cfg!(windows) {
                        entry.path.to_lowercase()
                    } else {
                        entry.path.clone()
                    };
                    (key, boost)
                })
            })
            .collect()
    }

    /// 基于 7 天半衰期的 Frecency 自适应加权分（0..=2000）。
    #[allow(dead_code)]
    pub fn frecency_boost(&self, path: &str, now_unix: u64) -> i64 {
        let Some(entry) = self.frecency.iter().find(|entry| {
            if cfg!(windows) {
                entry.path.eq_ignore_ascii_case(path)
            } else {
                entry.path == path
            }
        }) else {
            return 0;
        };
        let age = now_unix.saturating_sub(entry.last_used_unix) as f64;
        let decay = 0.5_f64.powf(age / HALF_LIFE_SECS);
        let raw = (f64::from(entry.count).ln_1p() * 450.0 * decay).round() as i64;
        raw.clamp(0, 2000)
    }
}

fn put_first(list: &mut Vec<String>, query: String, limit: usize) {
    let query = query.trim();
    if !query.is_empty() {
        let mut query = query.to_owned();
        crate::desktop::limit_query(&mut query);
        list.retain(|saved| *saved != query);
        list.insert(0, query);
        list.truncate(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histories_are_bounded_and_query_bookmarks_have_independent_lifecycle() {
        let mut activity = Activity::default();
        for n in 0..60 {
            activity.apply(Edit::Remember(format!("文档 {n}")));
        }
        activity.apply(Edit::Remember("  文档 57  ".into()));
        assert_eq!(activity.history.len(), MAX_HISTORY);
        assert_eq!(activity.history.first().unwrap(), "文档 57");
        assert_eq!(
            activity
                .history
                .iter()
                .filter(|query| *query == "文档 57")
                .count(),
            1
        );
        activity.apply(Edit::Bookmark("文档 57".into()));
        activity.apply(Edit::ClearHistory);
        assert!(activity.history.is_empty());
        assert_eq!(activity.bookmarks, ["文档 57"]);
        activity.apply(Edit::ForgetBookmark("文档 57".into()));
        assert!(activity.bookmarks.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn saves_and_reloads_history_without_touching_settings_or_index() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("test-tmp");
        xs_platform::create_local_dir_all(&root).unwrap();
        let profile = tempfile::tempdir_in(root).unwrap();
        let dir = profile.path();
        Activity::edit(dir, Edit::Remember("项目报告".into())).unwrap();
        Activity::edit(dir, Edit::Bookmark("ext:pdf".into())).unwrap();
        let restored = Activity::load(dir).unwrap();
        assert_eq!(restored.history, ["项目报告"]);
        assert_eq!(restored.bookmarks, ["ext:pdf"]);
        assert!(!dir.join("settings.json").exists() && !dir.join("index").exists());
        Activity::edit(dir, Edit::ClearHistory).unwrap();
        assert!(Activity::load(dir).unwrap().history.is_empty());
    }

    #[test]
    fn frecency_boost_decays_with_time_and_rewards_frequency() {
        let mut activity = Activity::default();
        let now = 1_760_000_000;
        activity.record_launch_at("/docs/spec.pdf".into(), now);
        activity.record_launch_at("/docs/spec.pdf".into(), now);
        activity.record_launch_at("/docs/old.pdf".into(), now - 14 * 24 * 3600);
        let hot = activity.frecency_boost("/docs/spec.pdf", now);
        let cold = activity.frecency_boost("/docs/old.pdf", now);
        assert!(hot > cold && cold > 0);
        assert_eq!(activity.frecency_boost("/docs/missing.pdf", now), 0);
    }
}
