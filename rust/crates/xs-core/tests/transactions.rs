use std::{fs, path::Path, sync::atomic::AtomicBool};
use xs_core::{operations, persistence::Store, FileRecord, IndexBuilder, SearchOptions};
use xs_platform::{Scope, Settings, SetupState};
fn settings(path: &Path, ext: &str) -> Settings {
    let mut s = Settings::default();
    let mut scope = Scope::new(path.to_str().unwrap());
    scope.extensions = Some(vec![ext.into()]);
    s.roots = vec![scope];
    s.setup_state = SetupState::Completed;
    s
}
fn sample() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("files");
    let data = root.join("app-private");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("报告.pdf"), b"synthetic").unwrap();
    fs::write(root.join("计划.docx"), b"synthetic").unwrap();
    fs::write(root.join("notes.txt"), b"synthetic").unwrap();
    (d, root, data)
}
#[test]
fn scope_change_cancellation_and_self_exclusion_are_transactional() {
    let (_d, root, data) = sample();
    operations::save_settings(&data, &settings(&root, "pdf")).unwrap();
    let first = operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(first.published);
    let i = operations::load_index(&data).unwrap();
    assert_eq!(
        i.search("ext:pdf", &SearchOptions::default(), None)
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        i.search("settings", &SearchOptions::default(), None)
            .unwrap()
            .total,
        0
    );
    let before = fs::read(data.join("index/CURRENT.json")).unwrap();
    operations::save_settings(&data, &settings(&root, "docx")).unwrap();
    assert!(operations::load_index(&data).is_err());
    let cancelled = operations::rebuild(&data, &AtomicBool::new(true), |_| {}).unwrap();
    assert!(!cancelled.published);
    assert_eq!(before, fs::read(data.join("index/CURRENT.json")).unwrap());
    let second = operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(second.published);
    let i = operations::load_index(&data).unwrap();
    assert_eq!(
        i.search("ext:pdf", &SearchOptions::default(), None)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        i.search("ext:docx", &SearchOptions::default(), None)
            .unwrap()
            .total,
        1
    );
    assert!(i.records().all(|r| r.source == "local"));
}
#[test]
fn unavailable_root_never_deletes_published_records() {
    let (_d, root, data) = sample();
    operations::save_settings(&data, &settings(&root, "pdf")).unwrap();
    operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    let before = fs::read(data.join("index/CURRENT.json")).unwrap();
    let missing = root.join("missing");
    operations::save_settings(&data, &settings(&missing, "pdf")).unwrap();
    let outcome = operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(!outcome.published);
    assert!(!outcome.report.errors.is_empty());
    assert_eq!(before, fs::read(data.join("index/CURRENT.json")).unwrap());
}
#[test]
fn scope_edited_mid_scan_cannot_publish_under_new_settings() {
    let (_d, root, data) = sample();
    operations::save_settings(&data, &settings(&root, "pdf")).unwrap();
    operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    let before = fs::read(data.join("index/CURRENT.json")).unwrap();
    let mut edited = false;
    let result = operations::rebuild(&data, &AtomicBool::new(false), |_| {
        if !edited {
            operations::save_settings(&data, &settings(&root, "docx")).unwrap();
            edited = true;
        }
    });
    assert!(result.is_err());
    assert_eq!(before, fs::read(data.join("index/CURRENT.json")).unwrap());
}
#[test]
fn rebuilding_local_scopes_keeps_offline_catalogs_without_opening_them() {
    let (_d, root, data) = sample();
    operations::save_settings(&data, &settings(&root, "pdf")).unwrap();
    let store = Store::open(data.join("index")).unwrap();
    let mut b = IndexBuilder::default();
    b.add(FileRecord {
        path: "\\\\nonexistent-server\\share\\secret.pdf".into(),
        size: 0,
        modified: 0,
        is_dir: false,
        source: "offline:archive".into(),
    })
    .unwrap();
    store.publish(&b.finish(1).unwrap()).unwrap();
    operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    let i = operations::load_index(&data).unwrap();
    let row = i
        .records()
        .find(|r| r.source.starts_with("offline:"))
        .unwrap();
    assert!(operations::authorize_open(&settings(&root, "pdf"), &row).is_err());
    assert_eq!(
        i.search("secret", &SearchOptions::default(), None)
            .unwrap()
            .total,
        1
    );
}
#[test]
fn corrupt_settings_are_not_replaced_by_defaults() {
    let (_d, _root, data) = sample();
    fs::create_dir_all(&data).unwrap();
    let p = data.join("settings.json");
    fs::write(&p, b"{corrupt").unwrap();
    assert!(operations::load_settings(&data).is_err());
    assert_eq!(fs::read(p).unwrap(), b"{corrupt");
}
#[test]
fn local_open_rechecks_current_extension_policy() {
    let (_d, root, _data) = sample();
    let record = FileRecord {
        path: root.join("报告.pdf").to_str().unwrap().into(),
        size: 9,
        modified: 0,
        is_dir: false,
        source: "local".into(),
    };
    assert!(operations::authorize_open(&settings(&root, "pdf"), &record).is_ok());
    assert!(operations::authorize_open(&settings(&root, "docx"), &record).is_err());
}

#[test]
fn an_empty_scope_token_cannot_admit_local_records() {
    let (d, root, data) = sample();
    operations::save_settings(&data, &settings(&root, "pdf")).unwrap();
    let mut builder = IndexBuilder::default();
    builder
        .add(FileRecord {
            path: d.path().join("outside.pdf").to_str().unwrap().into(),
            size: 1,
            modified: 0,
            is_dir: false,
            source: "local".into(),
        })
        .unwrap();
    let index = builder.finish(1).unwrap();
    assert!(operations::validate_index_scope(&index, &settings(&root, "pdf")).is_err());
    Store::open(data.join("index"))
        .unwrap()
        .publish(&index)
        .unwrap();
    let error = operations::load_index(&data).err().unwrap().to_string();
    assert!(error.contains("本地来源"), "{error}");
}

#[test]
fn denied_scope_is_rejected_before_probing_a_missing_path() {
    let (d, root, _data) = sample();
    let record = FileRecord {
        path: d
            .path()
            .join("outside-missing.pdf")
            .to_str()
            .unwrap()
            .into(),
        size: 1,
        modified: 0,
        is_dir: false,
        source: "local".into(),
    };
    let error = operations::authorize_open(&settings(&root, "pdf"), &record).unwrap_err();
    assert!(error.to_string().contains("授权目录"), "{error:#}");
    assert!(
        error.downcast_ref::<xs_platform::PlatformError>().is_none(),
        "filesystem was probed before scope rejection: {error:#}"
    );
}

#[test]
fn rebuild_keeps_catalog_precision_and_unknown_fields() {
    use xs_core::index::CatalogMetadata;
    let (_d, root, data) = sample();
    operations::save_settings(&data, &settings(&root, "pdf")).unwrap();
    let metadata = CatalogMetadata {
        fields: [
            ("Size".to_owned(), String::new()),
            ("Date Modified".to_owned(), "133444736000000001".to_owned()),
            ("Date Created".to_owned(), "133444736123456789".to_owned()),
            ("Attributes".to_owned(), "34".to_owned()),
            ("Project".to_owned(), "项目甲".to_owned()),
        ]
        .into_iter()
        .collect(),
    };
    let mut builder = IndexBuilder::default();
    builder
        .add_with_metadata(
            FileRecord {
                path: "\\\\synthetic-catalog\\share\\precise.pdf".into(),
                size: 0,
                modified: 1700000000,
                is_dir: false,
                source: "offline:precision".into(),
            },
            Some(metadata.clone()),
        )
        .unwrap();
    let store = Store::open(data.join("index")).unwrap();
    store.publish(&builder.finish(1).unwrap()).unwrap();
    let outcome = operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(outcome.published);
    assert!(outcome.durable);
    assert!(outcome.durability_warning.is_none());
    let index = operations::load_index(&data).unwrap();
    let id = (0..index.len() as u32)
        .find(|id| index.record(*id).unwrap().source == "offline:precision")
        .unwrap();
    assert_eq!(index.record_metadata(id).unwrap(), Some(metadata));
    assert!(index.is_mapped());
    assert!(index.records().any(|r| r.source == "local"));
}

#[test]
fn resident_service_revokes_a_cached_index_after_scope_change() {
    use std::io::{self, Write};
    struct UpdatingOutput {
        bytes: Vec<u8>,
        dir: std::path::PathBuf,
        replacement: Option<Settings>,
    }
    impl Write for UpdatingOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if let Some(replacement) = self.replacement.take() {
                operations::save_settings(&self.dir, &replacement).map_err(io::Error::other)?;
            }
            Ok(())
        }
    }
    let (_d, root, data) = sample();
    operations::save_settings(&data, &settings(&root, "pdf")).unwrap();
    operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    let input=b"{\"method\":\"search\",\"id\":1,\"query\":\"ext:pdf\"}\n{\"method\":\"search\",\"id\":2,\"query\":\"ext:pdf\"}\n{\"method\":\"shutdown\",\"id\":3}\n";
    let mut output = UpdatingOutput {
        bytes: Vec::new(),
        dir: data,
        replacement: Some(settings(&root, "docx")),
    };
    xs_core::service::serve(
        &output.dir.clone(),
        std::io::Cursor::new(input),
        &mut output,
    )
    .unwrap();
    let rows = String::from_utf8(output.bytes)
        .unwrap()
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[0]["result"]["total"], 1);
    assert_eq!(rows[1]["ok"], false);
    assert_eq!(rows[2]["ok"], true);
}

#[test]
fn file_notifications_publish_delta_without_deleting_offline_or_unauthorized_records() {
    use operations::ReconcileOutcome;
    let (_d, root, data) = sample();
    let policy = settings(&root, "pdf");
    let token = policy.scope_token().unwrap();
    operations::save_settings(&data, &policy).unwrap();
    let store = Store::open(data.join("index")).unwrap();
    let old_path = root.join("报告.pdf");
    let mut offline = IndexBuilder::default();
    offline
        .add(FileRecord {
            path: old_path.to_str().unwrap().into(),
            source: "offline:fixture".into(),
            size: 1,
            modified: 0,
            is_dir: false,
        })
        .unwrap();
    store.publish(&offline.finish(1).unwrap()).unwrap();
    operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    let new_path = root.join("改名报告.pdf");
    fs::rename(&old_path, &new_path).unwrap();
    let cancel = AtomicBool::new(false);
    let outcome =
        operations::reconcile_changed_paths(&data, &token, &[old_path, new_path.clone()], &cancel)
            .unwrap();
    let ReconcileOutcome::Published(publication) = outcome else {
        panic!("expected a file delta");
    };
    assert_eq!(publication.path.extension().unwrap(), "xsc");
    let rows = operations::load_index(&data)
        .unwrap()
        .records()
        .filter(|r| !r.is_dir)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .any(|row| row.source == "offline:fixture" && row.path.ends_with("报告.pdf")));
    assert!(rows
        .iter()
        .any(|row| row.source == "local" && row.path.ends_with("改名报告.pdf")));
    assert!(matches!(
        operations::reconcile_changed_paths(&data, &token, &[new_path], &cancel).unwrap(),
        ReconcileOutcome::NoChange
    ));
    assert!(matches!(
        operations::reconcile_changed_paths(&data, &token, std::slice::from_ref(&root), &cancel)
            .unwrap(),
        ReconcileOutcome::FullScanRequired
    ));
    operations::save_settings(&data, &settings(&root, "txt")).unwrap();
    assert!(matches!(
        operations::reconcile_changed_paths(&data, &token, &[root], &cancel).unwrap(),
        ReconcileOutcome::FullScanRequired
    ));
}

#[test]
fn repeated_equal_length_writes_to_one_path_remain_one_search_result() {
    use operations::ReconcileOutcome;

    let (_d, root, data) = sample();
    let policy = settings(&root, "pdf");
    let token = policy.scope_token().unwrap();
    operations::save_settings(&data, &policy).unwrap();
    operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();

    let path = root.join("报告.pdf");
    let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for content in [b"first revision", b"other revision"] {
        std::thread::sleep(std::time::Duration::from_millis(15));
        fs::write(&path, content).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(fixed_time)
            .unwrap();
        assert!(matches!(
            operations::reconcile_changed_paths(
                &data,
                &token,
                std::slice::from_ref(&path),
                &AtomicBool::new(false),
            )
            .unwrap(),
            ReconcileOutcome::Published(_)
        ));

        let index = operations::load_index(&data).unwrap();
        let matches = index
            .search("报告", &SearchOptions::default(), None)
            .unwrap();
        assert_eq!(matches.total, 1, "repeated writes must not append rows");
        let rows = index
            .records()
            .filter(|record| record.path == path.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].size, content.len() as u64);
    }
}

#[test]
fn hard_link_paths_share_file_identity_and_old_revisions_refuse_open() {
    let (_d, root, data) = sample();
    let first = root.join("报告.pdf");
    let alias = root.join("别名报告.pdf");
    fs::hard_link(&first, &alias).unwrap();
    operations::save_settings(&data, &settings(&root, "pdf")).unwrap();
    operations::rebuild(&data, &AtomicBool::new(false), |_| {}).unwrap();
    let index = operations::load_index(&data).unwrap();
    let first_id = (0..index.len() as u32)
        .find(|&id| index.record(id).unwrap().path == first.to_str().unwrap())
        .unwrap();
    let alias_id = (0..index.len() as u32)
        .find(|&id| index.record(id).unwrap().path == alias.to_str().unwrap())
        .unwrap();
    assert_eq!(
        index.record_revision(first_id).unwrap().unwrap().identity,
        index.record_revision(alias_id).unwrap().unwrap().identity
    );
    assert_eq!(
        operations::authorize_indexed_open(&data, &index, alias_id).unwrap(),
        alias
    );
    fs::remove_file(&alias).unwrap();
    fs::write(&alias, b"synthetic").unwrap();
    assert!(operations::authorize_indexed_open(&data, &index, alias_id).is_err());
}
