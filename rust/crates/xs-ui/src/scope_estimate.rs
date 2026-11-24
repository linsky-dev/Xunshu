//! 仅在用户确认探测后测量所选目录的标准元数据扫描。

use std::path::{Component, Path};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, RecvTimeoutError},
};
use std::time::{Duration, Instant};
use xs_platform::{PlatformError, Result, Settings, SpeedMode};

const VISIT_LIMIT: u64 = 512;
const TIME_LIMIT: Duration = Duration::from_millis(350);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Estimate {
    /// 遍历完整。耗时只涵盖本次扫描，不含构建与提交索引。
    Measured {
        visited: u64,
        indexed: u64,
        elapsed: Duration,
    },
    /// 已达到预算，仅能报告已经观测到的条目数。
    Limited {
        visited: u64,
        indexed: u64,
    },
    /// 扫描遇到访问错误，仅能报告已经观测到的条目数。
    Incomplete {
        visited: u64,
        indexed: u64,
    },
    Cancelled,
}

/// 调用者须先取得用户确认，并为每次估算提供一个独立的取消标记。
/// 估算只保留计数，不保存设置、发布索引或读取正文。
pub(crate) fn estimate(
    settings: &Settings,
    data_dir: &Path,
    cancel: &AtomicBool,
) -> Result<Estimate> {
    estimate_with_limits(settings, data_dir, cancel, VISIT_LIMIT, TIME_LIMIT)
}

fn estimate_with_limits(
    settings: &Settings,
    data_dir: &Path,
    cancel: &AtomicBool,
    visit_limit: u64,
    time_limit: Duration,
) -> Result<Estimate> {
    if cancel.load(Ordering::Acquire) {
        return Ok(Estimate::Cancelled);
    }
    if settings.mode != SpeedMode::Standard || settings.roots.is_empty() {
        return Err(PlatformError::UnsupportedMode(
            "仅能估算已选的标准扫描目录".into(),
        ));
    }
    settings.validate()?;
    #[cfg(target_os = "linux")]
    let volumes = xs_platform::volumes()?;
    for scope in &settings.roots {
        let path = Path::new(&scope.path);
        let root = !path
            .components()
            .any(|part| matches!(part, Component::Normal(_)));
        #[cfg(target_os = "linux")]
        let root = root || volumes.iter().any(|volume| path == Path::new(&volume.root));
        if root {
            return Err(PlatformError::UnsupportedMode(
                "整卷范围的条目数和扫描时间未知，请选择一个子目录后估算".into(),
            ));
        }
    }

    let limited = AtomicBool::new(false);
    let stop_for_budget = || {
        if cancel
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            limited.store(true, Ordering::Release);
        }
    };
    let mut visited = 0;
    let mut indexed = 0;
    let started = Instant::now();
    let report = std::thread::scope(|threads| {
        let (done, wait) = mpsc::channel::<()>();
        let stop_for_budget = &stop_for_budget;
        std::thread::Builder::new()
            .name("scope-estimate-deadline".into())
            .spawn_scoped(threads, move || {
                let remaining = time_limit.saturating_sub(started.elapsed());
                if wait.recv_timeout(remaining) == Err(RecvTimeoutError::Timeout) {
                    stop_for_budget();
                }
            })
            .map_err(|source| PlatformError::Io {
                path: "扫描估算计时器".into(),
                source,
            })?;
        // 标准扫描流式遍历。跳过项未必触发进度回调，计时器会独立取消。
        let report = xs_platform::scan_with_batches(
            settings,
            cancel,
            |progress| {
                visited = progress.visited;
                indexed = progress.indexed;
                if visited >= visit_limit {
                    stop_for_budget();
                }
            },
            &[data_dir.to_path_buf()],
            64,
            |_| Ok(()),
        );
        drop(done);
        report
    })?;
    let elapsed = started.elapsed();
    if limited.load(Ordering::Acquire) {
        Ok(Estimate::Limited { visited, indexed })
    } else if report.cancelled || cancel.load(Ordering::Acquire) {
        Ok(Estimate::Cancelled)
    } else if !report.complete {
        Ok(Estimate::Incomplete { visited, indexed })
    } else if elapsed >= time_limit {
        Ok(Estimate::Limited { visited, indexed })
    } else {
        Ok(Estimate::Measured {
            visited,
            indexed,
            elapsed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use xs_platform::Scope;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "xs-estimate-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            xs_platform::create_local_dir_new(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = xs_platform::remove_local_file(&self.0.join("included.pdf"));
            let _ = xs_platform::remove_local_file(&self.0.join("excluded.txt"));
            let _ = std::fs::remove_dir(self.0.join("app-data"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    fn settings(root: &Path) -> Settings {
        let mut scope = Scope::new(root.to_str().unwrap());
        scope.extensions = Some(vec!["pdf".into()]);
        Settings {
            roots: vec![scope],
            ..Settings::default()
        }
    }

    #[test]
    fn measures_selected_types_and_excludes_app_data() {
        let fixture = Fixture::new();
        let data = fixture.0.join("app-data");
        xs_platform::create_local_dir_new(&data).unwrap();
        for name in ["included.pdf", "excluded.txt"] {
            xs_platform::create_local_new(&fixture.0.join(name)).unwrap();
        }
        let cancel = AtomicBool::new(false);
        let estimate = estimate_with_limits(
            &settings(&fixture.0),
            &data,
            &cancel,
            20,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(matches!(
            estimate,
            Estimate::Measured {
                visited: 3,
                indexed: 2,
                ..
            }
        ));
        assert!(!cancel.load(Ordering::Acquire));
    }

    #[test]
    fn stops_at_visit_limit_without_claiming_total() {
        let fixture = Fixture::new();
        let cancel = AtomicBool::new(false);
        xs_platform::create_local_new(&fixture.0.join("included.pdf")).unwrap();
        assert_eq!(
            estimate_with_limits(
                &settings(&fixture.0),
                &fixture.0.join("app-data"),
                &cancel,
                1,
                Duration::from_secs(10),
            )
            .unwrap(),
            Estimate::Limited {
                visited: 1,
                indexed: 1
            },
        );
        assert!(cancel.load(Ordering::Acquire));
    }

    #[test]
    fn reports_cancelled_incomplete_and_volume_refusal() {
        let fixture = Fixture::new();
        let missing = fixture.0.join("missing");
        let data = fixture.0.join("app-data");
        let cancel = AtomicBool::new(true);
        assert_eq!(
            estimate(&settings(&missing), &data, &cancel).unwrap(),
            Estimate::Cancelled
        );
        cancel.store(false, Ordering::Release);
        assert_eq!(
            estimate(&settings(&missing), &data, &cancel).unwrap(),
            Estimate::Incomplete {
                visited: 0,
                indexed: 0
            }
        );
        let root = fixture.0.ancestors().last().unwrap();
        assert!(matches!(
            estimate(&settings(root), &data, &cancel),
            Err(PlatformError::UnsupportedMode(_))
        ));
    }
}
