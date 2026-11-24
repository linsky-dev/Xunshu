use anyhow::Result;
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use xs_core::operations::{self, ReconcileOutcome};
use xs_platform::{open_local_read, FileRevision, ScopeWatcher, Settings, WatchEvent};

const POLL: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(5);
const RECONCILE: Duration = Duration::from_secs(600);

struct Authorization {
    settings: Settings,
    token: String,
    cancel: AtomicBool,
}

enum SettingsUpdate {
    Authorized(Arc<Authorization>),
    Revoked(String),
}

#[derive(PartialEq, Eq)]
enum SettingsRevision {
    Readable(FileRevision),
    Unavailable(String),
}

fn file_revision(path: &Path) -> xs_platform::Result<FileRevision> {
    FileRevision::from_file(&open_local_read(path)?)
}

fn read_settings_revision(
    path: &Path,
    last: &mut Option<SettingsRevision>,
) -> Option<Result<Settings>> {
    let mut opened = open_local_read(path)
        .and_then(|file| FileRevision::from_file(&file).map(|revision| (file, revision)));
    let observed = match &opened {
        Ok((_, revision)) => SettingsRevision::Readable(*revision),
        Err(error) => SettingsRevision::Unavailable(error.to_string()),
    };
    if last.as_ref() == Some(&observed) {
        return None;
    }
    let result = match &mut opened {
        Ok((file, revision)) => {
            let loaded = Settings::load_open_file(file);
            if FileRevision::from_file(file).ok() != Some(*revision)
                || file_revision(path).ok() != Some(*revision)
            {
                *last = None;
                return Some(Err(anyhow::anyhow!("设置在读取期间发生变化")));
            }
            loaded.map_err(Into::into)
        }
        Err(error) => Err(anyhow::anyhow!("{error}")),
    };
    *last = Some(observed);
    Some(result)
}

// 独立于索引扫描接收设置通知，撤权时可取消仍在执行的旧范围操作。
fn observe_settings(
    path: &Path,
    shutdown: &AtomicBool,
    stopped: &AtomicBool,
    pending: &Mutex<Option<SettingsUpdate>>,
) {
    let mut active: Option<Arc<Authorization>> = None;
    let mut last = None;
    let mut watcher = None;
    let mut check_at = Instant::now();
    let mut subscribe_at = check_at;
    while !shutdown.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
        if Instant::now() >= check_at {
            if watcher.is_none() && Instant::now() >= subscribe_at {
                watcher = ScopeWatcher::for_file(path).ok();
                subscribe_at = Instant::now() + RETRY;
            }
            if let Some(result) = read_settings_revision(path, &mut last) {
                let result = result.and_then(|settings| Ok((settings.scope_token()?, settings)));
                let update = match result {
                    Ok((token, _)) if active.as_ref().is_some_and(|old| old.token == token) => None,
                    Ok((token, settings)) => {
                        Some(SettingsUpdate::Authorized(Arc::new(Authorization {
                            settings,
                            token,
                            cancel: AtomicBool::new(false),
                        })))
                    }
                    Err(error) => Some(SettingsUpdate::Revoked(error.to_string())),
                };
                if let Some(update) = update {
                    if let Some(old) = active.take() {
                        old.cancel.store(true, Ordering::Release);
                    }
                    if let SettingsUpdate::Authorized(next) = &update {
                        active = Some(Arc::clone(next));
                    }
                    // 合并积压的状态通知，每个已撤销授权的令牌仍保持取消。
                    *pending.lock().unwrap() = Some(update);
                }
            }
            check_at = Instant::now() + RETRY;
        }
        if let Some(notifications) = &mut watcher {
            match notifications.poll(POLL) {
                Ok(WatchEvent::Idle) => (),
                Ok(WatchEvent::Changed) => check_at = Instant::now(),
                _ => {
                    // 丢失通知时先核对一次，下一次低频检查再尝试订阅。
                    watcher = None;
                    if let Some(old) = active.take() {
                        old.cancel.store(true, Ordering::Release);
                    }
                    *pending.lock().unwrap() = Some(SettingsUpdate::Revoked(
                        "设置监听已断开，等待重新核对".into(),
                    ));
                    last = None;
                    check_at = Instant::now();
                    subscribe_at = check_at + RETRY;
                }
            }
        } else {
            thread::sleep(POLL);
        }
    }
    if let Some(active) = active {
        active.cancel.store(true, Ordering::Release);
    }
}

struct StopObserver<'a>(&'a AtomicBool);

impl Drop for StopObserver<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// 先订阅再校准，避免重订阅与扫描之间出现监听空窗。
pub(crate) fn run(dir: &Path, cancel: &AtomicBool, status: impl FnMut(Value)) -> Result<()> {
    let pending = Mutex::new(None);
    let settings = operations::settings_path(dir);
    let stopped = AtomicBool::new(false);
    thread::scope(|threads| {
        let _observer = StopObserver(&stopped);
        threads.spawn(|| observe_settings(&settings, cancel, &stopped, &pending));
        run_watcher(dir, cancel, &pending, status);
    });
    Ok(())
}

fn run_watcher(
    dir: &Path,
    cancel: &AtomicBool,
    pending: &Mutex<Option<SettingsUpdate>>,
    mut status: impl FnMut(Value),
) {
    let mut authorized = None;
    let mut state = None;
    let mut retry_at = Instant::now();
    let mut reconcile_at = Instant::now();
    while !cancel.load(Ordering::Acquire) {
        let update = pending.lock().unwrap().take();
        if let Some(update) = update {
            state = None;
            retry_at = Instant::now();
            authorized = match update {
                SettingsUpdate::Authorized(value) => Some(value),
                SettingsUpdate::Revoked(message) => {
                    status(json!({"event":"settings_unavailable","message":message}));
                    None
                }
            };
        }
        let Some(active) = authorized
            .as_ref()
            .filter(|active| !active.cancel.load(Ordering::Acquire))
        else {
            state = None;
            thread::sleep(POLL);
            continue;
        };
        if state.is_none() && Instant::now() >= retry_at {
            match ScopeWatcher::new(&active.settings, &[dir.to_owned()]) {
                Ok(watcher) => {
                    state = Some(watcher);
                    reconcile_at = Instant::now();
                    status(json!({"event":"subscribed"}));
                }
                Err(error) => {
                    status(json!({"event":"watch_error","message":error.to_string()}));
                    retry_at = Instant::now() + RETRY;
                }
            }
        }
        let Some(watcher) = state.as_mut() else {
            thread::sleep(POLL);
            continue;
        };
        let event = watcher.poll(POLL);
        if active.cancel.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
            continue;
        }
        match event {
            Ok(WatchEvent::Changed) => {
                if Instant::now() < reconcile_at {
                    if let Some(paths) = watcher.take_changed_paths() {
                        if !paths.is_empty() {
                            match incremental(
                                dir,
                                &active.settings,
                                &active.token,
                                &paths,
                                &active.cancel,
                            ) {
                                Ok(ReconcileOutcome::NoChange) => (),
                                Ok(ReconcileOutcome::Published(publication)) => {
                                    status(
                                        json!({"event":"incremental","paths":paths.len(),"publication":publication}),
                                    );
                                }
                                Ok(ReconcileOutcome::FullScanRequired) => {
                                    reconcile_at = Instant::now()
                                }
                                Err(error) => {
                                    status(
                                        json!({"event":"incremental_error","message":error.to_string()}),
                                    );
                                    reconcile_at = (Instant::now() + RETRY).min(reconcile_at);
                                }
                            }
                        }
                    } else {
                        reconcile_at = Instant::now();
                    }
                }
            }
            Ok(WatchEvent::Idle) => (),
            other => {
                if let Err(error) = other {
                    status(json!({"event":"watch_error","message":error.to_string()}));
                }
                state = None;
                retry_at = Instant::now() + POLL;
                continue;
            }
        }
        if !active.settings.roots.is_empty() && Instant::now() >= reconcile_at {
            match operations::rebuild_if_scope(dir, &active.token, &active.cancel, |_| ()) {
                Ok(outcome) => {
                    let delay = if outcome.published { RECONCILE } else { RETRY };
                    reconcile_at = Instant::now() + delay;
                    status(json!({"event":"reconcile","outcome":outcome}));
                }
                Err(error) => {
                    reconcile_at = Instant::now() + RETRY;
                    status(json!({"event":"reconcile_error","message":error.to_string()}));
                }
            }
        }
    }
}

fn incremental(
    dir: &Path,
    settings: &Settings,
    scope: &str,
    paths: &[std::path::PathBuf],
    cancel: &AtomicBool,
) -> Result<ReconcileOutcome> {
    #[cfg(windows)]
    if settings.mode == xs_platform::SpeedMode::OnDemandElevated {
        return operations::reconcile_native_journal(dir, scope, cancel);
    }
    #[cfg(not(windows))]
    let _ = settings;
    operations::reconcile_changed_paths(dir, scope, paths, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xs_platform::{atomic_write_local, remove_local_file, Scope};

    #[test]
    fn settings_revision_cache_detects_replacement_invalid_data_and_removal() {
        let fixture = tempfile::tempdir().unwrap();
        let path = operations::settings_path(fixture.path());
        let mut last = None;
        assert!(read_settings_revision(&path, &mut last).unwrap().is_err());
        assert!(read_settings_revision(&path, &mut last).is_none());

        Settings::default().save(&path).unwrap();
        assert!(read_settings_revision(&path, &mut last).unwrap().is_ok());
        assert!(read_settings_revision(&path, &mut last).is_none());

        atomic_write_local(&path, b"broken", true).unwrap();
        assert!(read_settings_revision(&path, &mut last).unwrap().is_err());
        assert!(read_settings_revision(&path, &mut last).is_none());

        remove_local_file(&path).unwrap();
        assert!(read_settings_revision(&path, &mut last).unwrap().is_err());
        assert!(read_settings_revision(&path, &mut last).is_none());
    }

    fn await_settings(pending: &Mutex<Option<SettingsUpdate>>) -> SettingsUpdate {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(update) = pending.lock().unwrap().take() {
                return update;
            }
            assert!(Instant::now() < deadline, "未及时收到设置通知");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn scope_revocation_cancels_prior_work_even_when_the_same_settings_return() {
        let fixture = tempfile::tempdir().unwrap();
        let path = operations::settings_path(fixture.path());
        let settings = Settings {
            roots: vec![Scope::new(fixture.path().to_str().unwrap())],
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        let stopped = AtomicBool::new(false);
        let pending = Mutex::new(None);
        thread::scope(|threads| {
            let _observer = StopObserver(&stopped);
            threads.spawn(|| observe_settings(&path, &stopped, &stopped, &pending));
            let SettingsUpdate::Authorized(old) = await_settings(&pending) else {
                panic!("初始设置应获得授权");
            };
            assert!(!old.cancel.load(Ordering::Acquire));

            atomic_write_local(&path, b"invalid", true).unwrap();
            assert!(matches!(
                await_settings(&pending),
                SettingsUpdate::Revoked(_)
            ));
            assert!(old.cancel.load(Ordering::Acquire));

            settings.save(&path).unwrap();
            let SettingsUpdate::Authorized(new) = await_settings(&pending) else {
                panic!("恢复设置后应获得新的授权");
            };
            assert_eq!(new.token, old.token);
            assert!(!new.cancel.load(Ordering::Acquire));
            assert!(old.cancel.load(Ordering::Acquire));
        });
    }
}
