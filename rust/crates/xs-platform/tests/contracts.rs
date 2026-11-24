use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use xs_platform::{
    atomic_write_local, create_local_dir_all, create_local_dir_new, create_local_new,
    inspect_scoped_entry, open_local_read, open_or_create_local_lock, publish_local_dir_new,
    remove_local_file, scan, scan_with_batches, scan_with_exclusions, validate_local_path,
    write_local_new, write_local_replace, FileEntry, FileRevision, PlatformError, Scope,
    ScopeWatcher, Settings, SetupState, SpeedMode, WatchEvent,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

/// Fixtures contain only data created by this test. In the managed cloud run,
/// TMPDIR points to /tmp/xunshu-rust-validation-036c199d3807 (verified tmpfs).
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "xs-platform-{}-{}-{nonce}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        create_local_dir_all(&path).expect("test fixtures require an approved local filesystem");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn settings(&self) -> Settings {
        Settings {
            roots: vec![Scope::new(self.0.to_str().unwrap())],
            ..Settings::default()
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove owned synthetic fixture");
    }
}
fn cancelled() -> AtomicBool {
    AtomicBool::new(false)
}
fn file_names(entries: &[FileEntry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| {
            Path::new(&e.path)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[test]
fn default_settings_never_start_a_disk_scan() {
    let settings = Settings::default();
    assert!(settings.roots.is_empty() && settings.content_roots.is_empty());
    assert!(!settings.content_enabled && !settings.ocr_enabled && !settings.semantic_enabled);
    let mut updates = Vec::new();
    let report = scan(&settings, &cancelled(), |p| updates.push(p)).unwrap();
    assert!(report.complete && !report.cancelled && report.entries.is_empty());
    assert_eq!(updates.last().unwrap().visited, 0);
}

#[cfg(windows)]
#[test]
fn existing_drive_root_is_validated_without_a_create_attempt() {
    let f = Fixture::new();
    let root: PathBuf = f.path().components().take(2).collect();
    assert!(root.is_absolute());
    create_local_dir_all(&root).unwrap();
    create_local_dir_all(f.path()).unwrap();
    let ordinary = f.file("ordinary", "数据".as_bytes());
    assert!(create_local_dir_all(&ordinary).is_err());
}

#[test]
fn revision_tracks_hard_links_equal_length_rewrites_and_replacement() {
    let f = Fixture::new();
    let file = f.file("source.txt", b"aaaa");
    let link = f.path().join("alias.txt");
    fs::hard_link(&file, &link).unwrap();
    let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let set_modified = || {
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(fixed_time)
            .unwrap();
    };
    set_modified();
    let before = FileRevision::at_path(&file).unwrap();
    assert_eq!(
        before.identity,
        FileRevision::at_path(&link).unwrap().identity
    );
    assert_eq!(
        before,
        FileRevision::from_file(&open_local_read(&file).unwrap()).unwrap()
    );
    assert_eq!(FileRevision::at_path(f.path()).unwrap().size, 0);
    std::thread::sleep(std::time::Duration::from_millis(15));
    fs::write(&file, b"bbbb").unwrap();
    set_modified();
    let edited = FileRevision::at_path(&file).unwrap();
    assert_eq!(edited.identity, before.identity);
    assert_eq!(edited.size, before.size);
    assert_eq!(edited.modified, before.modified);
    assert_ne!(edited.changed, before.changed);
    fs::rename(&file, f.path().join("old.txt")).unwrap();
    fs::write(&file, b"cccc").unwrap();
    set_modified();
    assert_ne!(
        FileRevision::at_path(&file).unwrap().identity,
        before.identity
    );
    let report = scan(&f.settings(), &cancelled(), |_| {}).unwrap();
    let record = report
        .entries
        .iter()
        .find(|entry| Path::new(&entry.path) == file)
        .unwrap();
    assert_eq!(record.revision, Some(FileRevision::at_path(&file).unwrap()));
}

#[test]
fn bounded_scan_delivers_cancelled_prefix_without_retaining_entries() {
    let f = Fixture::new();
    for index in 0..8 {
        f.file(&format!("{index}.txt"), b"sample");
    }
    let mut sizes = Vec::new();
    let mut final_progress = 0;
    let report = scan_with_batches(
        &f.settings(),
        &cancelled(),
        |p| final_progress = p.indexed,
        &[],
        4,
        |batch| {
            sizes.push(batch.len());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(sizes, [4, 4, 1]);
    assert_eq!(final_progress, 9);
    assert!(report.complete && report.entries.is_empty());
    let stop = cancelled();
    sizes.clear();
    let report = scan_with_batches(
        &f.settings(),
        &stop,
        |_| {},
        &[],
        2,
        |batch| {
            sizes.push(batch.len());
            stop.store(true, Ordering::Release);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(sizes, [2]);
    assert!(report.cancelled && !report.complete && report.entries.is_empty());
}

#[test]
fn native_watch_ignores_hard_exclusions_and_reports_selected_changes() {
    let f = Fixture::new();
    let excluded = f.dir("data");
    let selected = f.dir("selected");
    let mut watcher = ScopeWatcher::new(&f.settings(), &[excluded]).unwrap();
    f.file("data/cache", b"ignored");
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
    while std::time::Instant::now() < deadline {
        assert_eq!(
            watcher.poll(std::time::Duration::from_millis(20)).unwrap(),
            WatchEvent::Idle
        );
    }
    let changed = selected.join("changed.txt");
    fs::write(&changed, b"visible").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        assert_eq!(await_watch_event(&mut watcher), WatchEvent::Changed);
        if watcher.take_changed_paths().unwrap().contains(&changed) {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
    }
    drop(watcher);
}

#[test]
fn native_file_watch_detects_atomic_settings_replacement_only() {
    let f = Fixture::new();
    let path = f.path().join("settings.json");
    Settings::default().save(&path).unwrap();
    let mut watcher = ScopeWatcher::for_file(&path).unwrap();
    f.file("unrelated/cache", b"ignored");
    assert_eq!(
        watcher.poll(std::time::Duration::from_millis(100)).unwrap(),
        WatchEvent::Idle
    );

    f.settings().save(&path).unwrap();
    assert_eq!(await_watch_event(&mut watcher), WatchEvent::Changed);
    assert_eq!(watcher.take_changed_paths().unwrap(), vec![path]);
    drop(watcher);
}

#[test]
fn native_watch_detects_root_replacement() {
    let f = Fixture::new();
    let root = f.dir("selected");
    let settings = Settings {
        roots: vec![Scope::new(root.to_str().unwrap())],
        ..Settings::default()
    };
    let mut watcher = ScopeWatcher::new(&settings, &[]).unwrap();
    fs::rename(&root, f.path().join("prior")).unwrap();
    fs::create_dir(&root).unwrap();
    assert_eq!(await_watch_event(&mut watcher), WatchEvent::RescanRequired);
    assert_eq!(
        watcher.poll(std::time::Duration::ZERO).unwrap(),
        WatchEvent::RescanRequired
    );
    drop(watcher);
}

fn await_watch_event(watcher: &mut ScopeWatcher) -> WatchEvent {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let event = watcher.poll(std::time::Duration::from_millis(50)).unwrap();
        if event != WatchEvent::Idle || std::time::Instant::now() >= deadline {
            return event;
        }
    }
}

#[test]
fn cleanup_only_removes_an_owned_file_and_stream_replacement_keeps_old_on_error() {
    let f = Fixture::new();
    let dir = f.dir("directory");
    let file = f.file("snapshot", b"old");
    assert!(remove_local_file(&dir).is_err());
    assert!(write_local_replace(&file, |temp| {
        temp.write_all(b"partial")?;
        Err(std::io::Error::other("injected write failure"))
    })
    .is_err());
    assert_eq!(fs::read(&file).unwrap(), b"old");
    write_local_replace(&file, |temp| temp.write_all(b"new")).unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"new");
    remove_local_file(&file).unwrap();
    assert!(!file.exists());
    assert!(dir.is_dir());
}

#[test]
fn staged_directory_publish_is_create_only_and_same_parent() {
    let f = Fixture::new();
    let stage = f.path().join("stage");
    let destination = f.path().join("destination");
    create_local_dir_new(&stage).unwrap();
    assert!(create_local_dir_new(&stage).is_err());
    let payload = f.file("stage/payload", b"prepared");
    let occupied = f.dir("occupied");
    let decoy = f.file("occupied/payload", b"previous");
    assert!(publish_local_dir_new(&stage, &occupied).is_err());
    assert!(publish_local_dir_new(&stage, &occupied.join("nested")).is_err());
    assert_eq!(fs::read(&payload).unwrap(), b"prepared");
    assert_eq!(fs::read(decoy).unwrap(), b"previous");
    publish_local_dir_new(&stage, &destination).unwrap();
    assert!(!stage.exists());
    assert_eq!(fs::read(destination.join("payload")).unwrap(), b"prepared");
}

#[test]
fn scoped_probe_applies_authorization_before_access_and_detects_missing_root() {
    let f = Fixture::new();
    let root = f.dir("selected");
    let allowed = f.file("selected/a.txt", b"allowed");
    let excluded = f.file("selected/a.bin", b"wrong extension");
    let outside = f.file("outside.txt", b"outside");
    let mut scope = Scope::new(root.to_str().unwrap());
    scope.extensions = Some(vec!["txt".into()]);
    let settings = Settings {
        roots: vec![scope],
        ..Settings::default()
    };
    assert!(inspect_scoped_entry(&settings, &outside, &[])
        .unwrap()
        .is_none());
    assert!(inspect_scoped_entry(&settings, &excluded, &[])
        .unwrap()
        .is_none());
    let record = inspect_scoped_entry(&settings, &allowed, &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        record.revision,
        Some(FileRevision::at_path(&allowed).unwrap())
    );
    assert!(
        inspect_scoped_entry(&settings, &allowed, std::slice::from_ref(&root))
            .unwrap()
            .is_none()
    );
    fs::remove_file(&allowed).unwrap();
    assert!(inspect_scoped_entry(&settings, &allowed, &[])
        .unwrap()
        .is_none());
    fs::rename(&root, f.path().join("old")).unwrap();
    assert!(inspect_scoped_entry(&settings, &allowed, &[]).is_err());
}

#[test]
fn settings_round_trip_creates_private_parent_and_normalizes_whitelist() {
    let f = Fixture::new();
    let mut settings = f.settings();
    settings.roots[0].extensions = Some(vec![".TXT".into(), "pdf".into(), "txt".into()]);
    settings.defer_setup();
    settings.record_reminder();
    let path = f.path().join("state/new/settings.json");
    settings.save(&path).unwrap();
    let loaded = Settings::load(&path).unwrap();
    assert_eq!(
        loaded.roots[0].extensions,
        Some(vec!["pdf".into(), "txt".into()])
    );
    assert_eq!(loaded.setup_state, SetupState::Deferred);
    assert_eq!(loaded.reminder_count, 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn strict_settings_reject_unknown_duplicate_invalid_types_and_versions() {
    let f = Fixture::new();
    let bad = [
        r#"{"version":1,"net_enabled":true}"#,
        r#"{"version":1,"version":1}"#,
        r#"{"version":1,"roots":[{"path":"/synthetic","allow_network":true}]}"#,
        r#"{"version":1,"reminder_count":4}"#,
        r#"{"version":1,"reminder_count":-1}"#,
        r#"{"version":1,"content_enabled":"true"}"#,
        r#"{"version":2}"#,
        r#"{"version":1,"setup_state":"finished"}"#,
        r#"{"version":1,"mode":"fast"}"#,
        r#"{"setup_state":"not_started"}"#,
        r#"{"setup_wizard_completed":true,"typo":1}"#,
        r#"{"setup_wizard_completed":true,"setup_wizard_completed":false}"#,
        r#"{"setup_wizard_completed":"false"}"#,
    ];
    for (index, json) in bad.iter().enumerate() {
        let path = f.file(&format!("bad-{index}.json"), json.as_bytes());
        assert!(
            Settings::load(path).is_err(),
            "accepted invalid settings: {json}"
        );
    }
}

#[test]
fn settings_size_limit_is_enforced_before_deserialization() {
    let f = Fixture::new();
    let path = f.file("oversized.json", &vec![b' '; 8 * 1024 * 1024 + 1]);
    assert!(Settings::load(path)
        .unwrap_err()
        .to_string()
        .contains("limit"));
}

#[test]
fn migration_is_create_only_preserves_old_file_and_does_not_infer_roots() {
    let f = Fixture::new();
    let original =
        br#"{"setup_wizard_completed":true,"history":["unchanged"],"semantic_enabled":false}"#;
    // Unknown legacy keys remain errors, even if the new schema knows the name.
    let rejected = f.file("rejected.json", original);
    assert!(Settings::load(rejected).is_err());
    let original = br#"{"setup_wizard_completed":true,"history":["unchanged"],"auto_index":true}"#;
    let old = f.file("legacy.json", original);
    let target = f.path().join("new/settings.json");
    let (settings, outcome) = Settings::migrate_legacy(&old, &target).unwrap();
    assert!(outcome.is_durable());
    assert_eq!(settings.setup_state, SetupState::Completed);
    assert!(settings.roots.is_empty() && settings.content_roots.is_empty());
    assert!(!settings.content_enabled);
    assert_eq!(fs::read(&old).unwrap(), original);
    let saved = fs::read(&target).unwrap();
    assert!(Settings::migrate_legacy(&old, &target).is_err());
    assert!(Settings::migrate_legacy(&old, &old).is_err());
    assert_eq!(fs::read(&old).unwrap(), original);
    assert_eq!(fs::read(&target).unwrap(), saved);
    let incomplete = f.file("incomplete.json", br#"{"setup_wizard_completed":false}"#);
    assert_eq!(
        Settings::load(incomplete).unwrap().setup_state,
        SetupState::NotStarted
    );
}

#[test]
fn setup_reminders_stop_at_three_and_manual_reopening_is_available() {
    let f = Fixture::new();
    let path = f.path().join("settings.json");
    let mut settings = Settings::default();
    assert!(settings.should_remind());
    settings.defer_setup();
    assert_eq!(settings.setup_state, SetupState::Deferred);
    for n in 1..=3 {
        assert!(settings.record_reminder());
        assert_eq!(settings.reminder_count, n);
    }
    assert!(!settings.record_reminder());
    settings.save(&path).unwrap();
    let mut settings = Settings::load(&path).unwrap();
    assert!(!settings.should_remind());
    settings.complete_setup();
    assert_eq!(settings.setup_state, SetupState::Completed);
    settings.reopen_setup();
    assert_eq!(settings.setup_state, SetupState::NotStarted);
    assert_eq!(settings.reminder_count, 3);
    assert!(!settings.should_remind());
}

#[test]
fn extensions_are_strict_and_empty_list_means_directories_only() {
    let f = Fixture::new();
    f.file("plain", b"a");
    f.file("allowed.TXT", b"bc");
    f.file("other.pdf", b"d");
    f.dir("empty");
    let mut settings = f.settings();
    assert_eq!(
        file_names(&scan(&settings, &cancelled(), |_| {}).unwrap().entries).len(),
        3
    );
    settings.roots[0].extensions = Some(vec![]);
    let report = scan(&settings, &cancelled(), |_| {}).unwrap();
    assert!(report.complete && file_names(&report.entries).is_empty());
    assert_eq!(report.entries.len(), 2);
    settings.roots[0].extensions = Some(vec![".txt".into()]);
    assert_eq!(
        file_names(&scan(&settings, &cancelled(), |_| {}).unwrap().entries),
        BTreeSet::from(["allowed.TXT".into()])
    );
    for invalid in [
        "", ".", "*.txt", "tar.gz", " txt", "txt ", "../x", "t/xt", "文本", "a;b",
    ] {
        settings.roots[0].extensions = Some(vec![invalid.into()]);
        assert!(settings.validate().is_err(), "accepted {invalid:?}");
    }
    settings.roots[0].extensions = Some(vec!["a".repeat(33)]);
    assert!(settings.validate().is_err());
}

#[test]
fn longest_root_owns_its_subtree_without_unioning_broader_whitelists() {
    let f = Fixture::new();
    f.file("top.txt", b"1");
    f.file("top.pdf", b"2");
    f.file("nested/no.txt", b"3");
    f.file("nested/yes.pdf", b"4");
    f.file("nested-two/sibling.txt", b"5");
    let mut settings = f.settings();
    settings.roots[0].extensions = Some(vec!["txt".into()]);
    settings.roots.push(Scope {
        path: f.path().join("nested").to_str().unwrap().into(),
        extensions: Some(vec!["pdf".into()]),
        smart_excludes: true,
    });
    let report = scan(&settings, &cancelled(), |_| {}).unwrap();
    assert!(report.complete);
    assert_eq!(
        file_names(&report.entries),
        BTreeSet::from(["top.txt".into(), "yes.pdf".into(), "sibling.txt".into()])
    );
    let paths: BTreeSet<_> = report.entries.iter().map(|e| &e.path).collect();
    assert_eq!(
        paths.len(),
        report.entries.len(),
        "overlap must not produce duplicate paths"
    );
    for entry in &report.entries {
        assert_eq!(entry.source, "local");
        assert!(settings
            .allows_filename(Path::new(&entry.path), entry.is_dir)
            .unwrap());
    }
    assert!(!settings
        .allows_filename(&f.path().join("nested/no.txt"), false)
        .unwrap());
    assert!(!settings
        .allows_filename(&f.path().with_extension("sibling"), false)
        .unwrap());
}

#[test]
fn name_content_scopes_and_tokens_are_independent() {
    let f = Fixture::new();
    let mut settings = f.settings();
    settings.roots[0].extensions = Some(vec![".TXT".into(), "txt".into()]);
    let name_token = settings.scope_token().unwrap();
    settings.content_roots = vec![Scope {
        path: f.path().join("content").to_str().unwrap().into(),
        extensions: Some(vec!["pdf".into()]),
        smart_excludes: false,
    }];
    settings.content_enabled = true;
    settings.ocr_enabled = true;
    settings.semantic_enabled = true;
    settings.defer_setup();
    settings.record_reminder();
    assert_eq!(settings.scope_token().unwrap(), name_token);
    assert!(settings
        .allows_filename(&f.path().join("top.txt"), false)
        .unwrap());
    assert!(!settings
        .allows_content(&f.path().join("top.txt"), false)
        .unwrap());
    assert!(settings
        .allows_content(&f.path().join("content/report.pdf"), false)
        .unwrap());
    assert!(!settings
        .allows_filename(&f.path().join("content/report.pdf"), false)
        .unwrap());
    settings.content_enabled = false;
    assert!(!settings
        .allows_content(&f.path().join("content/report.pdf"), false)
        .unwrap());
    settings.roots[0].smart_excludes = false;
    assert_ne!(settings.scope_token().unwrap(), name_token);
}

#[test]
fn scope_tokens_are_order_independent_and_duplicate_roots_are_rejected() {
    let f = Fixture::new();
    let mut settings = f.settings();
    settings
        .roots
        .push(Scope::new(f.path().join("nested").to_str().unwrap()));
    let token = settings.scope_token().unwrap();
    settings.roots.reverse();
    assert_eq!(settings.scope_token().unwrap(), token);
    settings.roots.push(settings.roots[0].clone());
    assert!(settings.validate().is_err());
}

#[test]
fn smart_excludes_can_be_overridden_only_by_an_explicit_deeper_scope() {
    let f = Fixture::new();
    f.file(".git/internal", b"a");
    f.file("node_modules/skipped.txt", b"b");
    f.file("node_modules/explicit/keep.txt", b"c");
    let mut settings = f.settings();
    assert!(file_names(&scan(&settings, &cancelled(), |_| {}).unwrap().entries).is_empty());
    settings.roots.push(Scope::new(
        f.path().join("node_modules/explicit").to_str().unwrap(),
    ));
    let report = scan(&settings, &cancelled(), |_| {}).unwrap();
    assert!(report.complete);
    assert_eq!(
        file_names(&report.entries),
        BTreeSet::from(["keep.txt".into()])
    );
    assert!(settings
        .allows_filename(&f.path().join("node_modules/explicit/keep.txt"), false)
        .unwrap());
}

#[test]
fn application_data_is_hard_excluded_even_with_smart_excludes_disabled() {
    let f = Fixture::new();
    f.file("normal.txt", b"1");
    f.file("state/index/file", b"2");
    f.file("state-sibling/keep", b"3");
    let mut settings = f.settings();
    settings.roots[0].smart_excludes = false;
    settings
        .roots
        .push(Scope::new(f.path().join("state/index").to_str().unwrap()));
    let report =
        scan_with_exclusions(&settings, &cancelled(), |_| {}, &[f.path().join("state")]).unwrap();
    assert!(report.complete);
    assert_eq!(
        file_names(&report.entries),
        BTreeSet::from(["normal.txt".into(), "keep".into()])
    );
    assert!(report
        .entries
        .iter()
        .all(|e| !Path::new(&e.path).starts_with(f.path().join("state"))));
    assert!(report.skipped_excluded >= 2);
}

#[test]
fn cancellation_before_and_during_scan_is_always_incomplete() {
    let f = Fixture::new();
    f.file("one.txt", b"one");
    let report = scan(&f.settings(), &AtomicBool::new(true), |_| {}).unwrap();
    assert!(report.cancelled && !report.complete && report.entries.is_empty());
    let cancel = cancelled();
    let mut observed = Vec::new();
    let report = scan(&f.settings(), &cancel, |p| {
        if p.visited == 1 {
            cancel.store(true, Ordering::Release);
        }
        observed.push(p);
    })
    .unwrap();
    assert!(report.cancelled && !report.complete);
    assert_eq!(report.entries.len(), 1);
    assert_eq!(observed.last().unwrap().indexed, 1);
}

#[test]
fn missing_root_and_regular_file_root_are_errors_not_empty_successes() {
    let f = Fixture::new();
    f.file("file.txt", b"x");
    let mut settings = f.settings();
    settings.roots = vec![
        Scope::new(f.path().join("missing").to_str().unwrap()),
        Scope::new(f.path().join("file.txt").to_str().unwrap()),
    ];
    let report = scan(&settings, &cancelled(), |_| {}).unwrap();
    assert!(!report.complete && !report.cancelled);
    assert_eq!(report.errors.len(), 2);
    assert!(report.entries.is_empty());
}

#[test]
fn unsupported_acceleration_modes_fail_explicitly() {
    for mode in [SpeedMode::FastService, SpeedMode::OnDemandElevated] {
        let settings = Settings {
            mode,
            ..Settings::default()
        };
        let error = scan(&settings, &cancelled(), |_| {}).unwrap_err();
        assert!(
            matches!(error, PlatformError::UnsupportedMode(_)),
            "{error}"
        );
        assert!(error.to_string().contains("无法使用此扫描方式"), "{error}");
    }
}

#[test]
fn local_boundary_rejects_relative_parent_url_and_unc_paths() {
    for path in [
        "relative",
        "https://example.invalid/file",
        "//server/share",
        r"\\server\share",
    ] {
        assert!(
            validate_local_path(Path::new(path), true).is_err(),
            "accepted {path}"
        );
    }
    let f = Fixture::new();
    assert!(validate_local_path(&f.path().join("../escape"), true).is_err());
    assert!(validate_local_path(&f.path().join("missing/deep"), true).is_ok());
    assert!(validate_local_path(&f.path().join("missing/deep"), false).is_err());
}

#[test]
fn atomic_create_only_and_replace_preserve_identity_contract() {
    let f = Fixture::new();
    let path = f.path().join("state.bin");
    assert!(atomic_write_local(&path, b"first", false)
        .unwrap()
        .is_durable());
    assert!(atomic_write_local(&path, b"forbidden", false).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"first");
    assert!(atomic_write_local(&path, b"second", true)
        .unwrap()
        .is_durable());
    let mut contents = String::new();
    open_local_read(&path)
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();
    assert_eq!(contents, "second");
    assert_eq!(
        fs::read_dir(f.path()).unwrap().count(),
        1,
        "atomic writes leave no temporary files"
    );
}

#[test]
fn create_new_and_lock_handles_preserve_existing_contents() {
    let f = Fixture::new();
    let path = f.path().join("new.bin");
    let mut file = create_local_new(&path).unwrap();
    file.write_all(b"immutable").unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert!(create_local_new(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"immutable");

    let lock_path = f.path().join("writer.lock");
    let mut lock = open_or_create_local_lock(&lock_path).unwrap();
    lock.write_all(b"preserved-lock-content").unwrap();
    lock.sync_all().unwrap();
    drop(lock);
    let mut lock = open_or_create_local_lock(&lock_path).unwrap();
    let mut text = String::new();
    lock.read_to_string(&mut text).unwrap();
    assert_eq!(
        text, "preserved-lock-content",
        "opening a lock must not truncate it"
    );
    assert!(open_or_create_local_lock(f.path()).is_err());
    assert!(create_local_new(&f.path().join("missing/child")).is_err());
}

#[test]
fn streaming_create_publishes_only_complete_files_and_cleans_callback_failures() {
    let f = Fixture::new();
    let target = f.path().join("snapshot.xsi");
    let failure = write_local_new(&target, |file| {
        file.write_all(b"partial")?;
        Err(std::io::Error::other("simulated interrupted stream"))
    });
    assert!(failure.is_err());
    assert!(!target.exists());
    assert_eq!(fs::read_dir(f.path()).unwrap().count(), 0);
    let outcome = write_local_new(&target, |file| {
        file.write_all(b"header")?;
        file.write_all(b"complete-body")
    })
    .unwrap();
    assert!(outcome.is_durable());
    assert_eq!(outcome.warning(), None);
    assert_eq!(serde_json::to_value(&outcome).unwrap()["state"], "durable");
    let mut called = false;
    assert!(write_local_new(&target, |_| {
        called = true;
        Ok(())
    })
    .is_err());
    assert!(
        !called,
        "an existing immutable destination is refused before streaming"
    );
    assert_eq!(fs::read(&target).unwrap(), b"headercomplete-body");
    assert_eq!(fs::read_dir(f.path()).unwrap().count(), 1);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::ffi::{CString, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn stream_commit_uses_pinned_parent_after_directory_becomes_an_external_link() {
        let f = Fixture::new();
        let chosen = f.dir("chosen");
        let moved = f.path().join("pinned-directory");
        let outside = f.dir("outside");
        let target = chosen.join("snapshot.xsi");
        fs::write(outside.join("snapshot.xsi"), b"outside untouched").unwrap();
        let outcome = write_local_new(&target, |file| {
            fs::rename(&chosen, &moved)?;
            symlink(&outside, &chosen)?;
            file.write_all(b"authorized snapshot")
        })
        .unwrap();
        assert!(outcome.is_durable());
        assert_eq!(
            fs::read(moved.join("snapshot.xsi")).unwrap(),
            b"authorized snapshot"
        );
        assert_eq!(
            fs::read(outside.join("snapshot.xsi")).unwrap(),
            b"outside untouched"
        );
        assert_eq!(fs::read_dir(&moved).unwrap().count(), 1);
    }

    #[test]
    fn failed_stream_cleanup_cannot_follow_a_swapped_parent_to_an_outside_file() {
        let f = Fixture::new();
        let chosen = f.dir("chosen");
        let moved = f.path().join("pinned-directory");
        let outside = f.dir("outside");
        let mut decoy = None;
        let outcome = write_local_new(&chosen.join("snapshot.xsi"), |file| {
            let temporary = fs::read_dir(&chosen)?.next().unwrap()?.file_name();
            let outside_temp = outside.join(&temporary);
            fs::write(&outside_temp, b"outside temporary-name decoy")?;
            decoy = Some(outside_temp);
            fs::rename(&chosen, &moved)?;
            symlink(&outside, &chosen)?;
            file.write_all(b"partial")?;
            Err(std::io::Error::other("interrupted after parent swap"))
        });
        assert!(outcome.is_err());
        assert_eq!(fs::read_dir(&moved).unwrap().count(), 0);
        assert_eq!(
            fs::read(decoy.unwrap()).unwrap(),
            b"outside temporary-name decoy"
        );
    }

    #[test]
    fn new_and_lock_opens_reject_links_and_special_files_before_data_open() {
        let f = Fixture::new();
        let outside = f.file("untouched.bin", b"original");
        let link = f.path().join("writer.lock");
        symlink(&outside, &link).unwrap();
        assert!(create_local_new(&link).is_err());
        assert!(open_or_create_local_lock(&link).is_err());
        let fifo = f.path().join("fifo");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(open_or_create_local_lock(&fifo).is_err());
        assert!(create_local_new(&fifo).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"original");
    }

    #[test]
    fn symlinks_and_ancestor_links_are_never_followed() {
        let f = Fixture::new();
        let root = f.dir("root");
        let outside = f.dir("outside");
        fs::write(outside.join("secret.txt"), b"do not scan").unwrap();
        symlink(&outside, root.join("linked")).unwrap();
        symlink(outside.join("secret.txt"), root.join("file-link")).unwrap();
        symlink("missing", root.join("dangling")).unwrap();
        let settings = Settings {
            roots: vec![Scope::new(root.to_str().unwrap())],
            ..Settings::default()
        };
        let report = scan(&settings, &cancelled(), |_| {}).unwrap();
        assert!(report.complete && report.errors.is_empty());
        assert_eq!(report.skipped_links, 3);
        assert!(file_names(&report.entries).is_empty());
        assert!(validate_local_path(&root.join("linked/secret.txt"), false).is_err());
        assert!(validate_local_path(&root.join("linked/new"), true).is_err());
        assert!(open_local_read(&root.join("file-link")).is_err());
        assert!(atomic_write_local(&root.join("file-link"), b"bad", true).is_err());
        assert!(create_local_dir_all(&root.join("linked/new")).is_err());
        assert_eq!(
            fs::read(outside.join("secret.txt")).unwrap(),
            b"do not scan"
        );
    }

    #[test]
    fn permission_error_marks_scan_incomplete_but_unreadable_file_metadata_is_indexed() {
        // SAFETY: geteuid 无参数，也不访问调用方内存。
        if unsafe { libc::geteuid() } == 0 {
            assert!(
                std::env::var("XS_REQUIRE_UNPRIVILEGED_TESTS")
                    .ok()
                    .as_deref()
                    != Some("1"),
                "权限回归门禁必须以普通用户运行"
            );
            eprintln!("跳过权限回归：root 可绕过普通目录权限，请以普通用户补跑");
            return;
        }
        let f = Fixture::new();
        let hidden = f.dir("unreadable");
        fs::write(hidden.join("not-deleted.txt"), b"kept").unwrap();
        let file = f.file("metadata-only.txt", b"five!");
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o0)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o0)).unwrap();
        let result = scan(&f.settings(), &cancelled(), |_| {});
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let report = result.unwrap();
        assert!(!report.complete);
        assert!(report
            .errors
            .iter()
            .any(|e| e.kind == "PermissionDenied" && Path::new(&e.path) == hidden));
        let indexed = report
            .entries
            .iter()
            .find(|e| Path::new(&e.path) == file)
            .unwrap();
        assert_eq!(indexed.size, 5);
    }

    #[test]
    fn invalid_utf8_is_explicitly_counted_without_lossy_index_entries() {
        let f = Fixture::new();
        let invalid = OsString::from_vec(vec![b'f', 0xff, b'.', b't', b'x', b't']);
        fs::write(f.path().join(invalid), b"one").unwrap();
        f.file("正常.txt", b"two");
        let report = scan(&f.settings(), &cancelled(), |_| {}).unwrap();
        assert!(!report.complete);
        assert_eq!(report.skipped_non_utf8, 1);
        assert_eq!(report.errors[0].kind, "non_utf8");
        assert!(report.errors[0].path.contains("66ff2e747874"));
        assert_eq!(
            file_names(&report.entries),
            BTreeSet::from(["正常.txt".into()])
        );
    }

    #[test]
    fn special_fifo_is_skipped_without_opening_its_data() {
        let f = Fixture::new();
        let fifo = f.path().join("pipe");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let report = scan(&f.settings(), &cancelled(), |_| {}).unwrap();
        assert!(report.complete && report.errors.is_empty());
        assert_eq!(report.skipped_special, 1);
        assert!(file_names(&report.entries).is_empty());
        assert!(open_local_read(&fifo).is_err());
    }

    #[test]
    fn opened_file_stays_pinned_when_the_user_path_is_replaced() {
        let f = Fixture::new();
        let path = f.file("original", b"verified inode");
        let mut file = open_local_read(&path).unwrap();
        fs::rename(&path, f.path().join("moved")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "verified inode");
    }

    #[test]
    fn overlay_and_virtual_filesystems_are_not_authorized_as_data_targets() {
        // This reads only kernel mount information; it never scans a discovered mount.
        let all = xs_platform::volumes().unwrap();
        for volume in all
            .iter()
            .filter(|v| matches!(v.filesystem.as_str(), "overlay" | "proc" | "sysfs"))
        {
            assert!(!volume.local && !volume.standard_supported && !volume.mft_supported);
        }
        let proc = Path::new("/proc/self/status");
        assert!(open_local_read(proc).is_err());
        assert!(validate_local_path(proc, false).is_err());
        // In the managed run this is overlay. Verify policy on the crate path
        // without listing it, reading file contents, or trying to create anything.
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mount = all
            .iter()
            .filter(|v| workspace.starts_with(&v.root))
            .max_by_key(|v| v.root.len());
        if mount.is_some_and(|v| v.filesystem == "overlay") {
            assert!(validate_local_path(workspace, false).is_err());
            assert!(validate_local_path(&workspace.join("does-not-exist"), true).is_err());
        }
    }
}
