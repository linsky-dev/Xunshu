//! 增量目录、历史修复、M1 兼容性和属性投影回归。
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use xs_core::{
    index::{ColumnSort, SortColumn},
    persistence::Store,
    CatalogMetadata, FileRecord, Index, IndexBuilder, JournalCheckpoint, SearchOptions,
};

fn segment(generation: u64, scope: &str, rows: &[(&str, &str)]) -> Index {
    let mut builder = IndexBuilder::new(scope.into());
    for (path, source) in rows {
        builder
            .add(FileRecord {
                path: (*path).into(),
                source: (*source).into(),
                size: 0,
                modified: 0,
                is_dir: false,
            })
            .unwrap();
    }
    builder.finish(generation).unwrap()
}

#[test]
fn delta_uses_generation_bound_ids_and_preserves_ranking_across_merge() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .publish(&segment(
            1,
            "authorized",
            &[
                ("/a/same.txt", "local"),
                ("/a/same.txt", "offline:efu"),
                ("/b/same.txt", "local"),
            ],
        ))
        .unwrap();
    let old_reader = store.load().unwrap();
    let extra = segment(2, "authorized", &[("/c/same.txt", "local")]);
    assert!(store.publish_delta(0, &[0], &extra).is_err());
    assert!(store.publish_delta(1, &[0, 0], &extra).is_err());
    store.publish_delta(1, &[0], &extra).unwrap();
    assert_eq!(old_reader.record(0).unwrap().source, "local");
    let expected = ["/a/same.txt", "/b/same.txt", "/c/same.txt"];
    for generation in 2..=10 {
        if generation != 2 {
            store
                .publish_delta(generation - 1, &[], &segment(generation, "authorized", &[]))
                .unwrap();
        }
        let index = store.load().unwrap();
        let response = index
            .search("same", &SearchOptions::default(), None)
            .unwrap();
        assert_eq!(response.generation, generation);
        assert_eq!(response.total, 3);
        assert_eq!(response.results[0].key().source, "offline:efu");
        assert_eq!(
            response
                .results
                .iter()
                .map(|hit| hit.path.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            index.records().map(|row| row.path).collect::<Vec<_>>(),
            expected
        );
        let page = index
            .search(
                "same",
                &SearchOptions {
                    limit: 1,
                    offset: 2,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(page.results[0].path, expected[2]);
        let sorted_page = index
            .search_sorted(
                "same",
                &SearchOptions {
                    limit: 2,
                    offset: 1,
                    ..Default::default()
                },
                None,
                ColumnSort {
                    column: SortColumn::Path,
                    descending: true,
                },
            )
            .unwrap();
        assert_eq!(sorted_page.total, 3);
        assert_eq!(sorted_page.results[0].path, expected[1]);
        assert_eq!(sorted_page.results[1].path, expected[0]);
    }
}

#[test]
fn cursor_and_tombstones_commit_together() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let build = |generation, journal_id, cursor| {
        let mut builder = IndexBuilder::new("scope".into());
        builder.set_journal_checkpoint(Some(JournalCheckpoint {
            volume_id: "disk".into(),
            journal_id,
            cursor,
        }));
        builder
            .add(FileRecord {
                path: format!("/file-{generation}"),
                source: "local".into(),
                size: 0,
                modified: 0,
                is_dir: false,
            })
            .unwrap();
        builder.finish(generation).unwrap()
    };
    store.publish(&build(1, 7, 100)).unwrap();
    assert!(store.publish_delta(1, &[0], &build(2, 8, 100)).is_err());
    assert!(store.publish_delta(1, &[0], &build(2, 7, 99)).is_err());
    assert_eq!(
        store.load().unwrap().journal_checkpoint().unwrap().cursor,
        100
    );
    store.publish_delta(1, &[0], &build(2, 7, 120)).unwrap();
    let index = store.load().unwrap();
    assert_eq!(index.generation(), 2);
    assert_eq!(index.journal_checkpoint().unwrap().cursor, 120);
    assert_eq!(index.record(0).unwrap().path, "/file-2");
}

#[test]
fn recovery_ignores_corrupt_current_and_different_scope() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .publish(&segment(1, "scope-1", &[("/a", "local")]))
        .unwrap();
    store
        .publish(&segment(2, "scope-2", &[("/b", "local")]))
        .unwrap();
    fs::write(dir.path().join("CURRENT.json"), b"{\"version\":").unwrap();
    assert!(store.load().is_err());
    assert!(store.recover("scope-2").is_err());
    let publication = store.recover("scope-1").unwrap();
    assert_eq!(publication.generation, 3);
    assert_eq!(store.load().unwrap().record(0).unwrap().path, "/a");
}

#[test]
fn bounded_history_reclaims_unneeded_generation_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut paths = Vec::new();
    for generation in 1..=8 {
        let p = store.publish(&segment(generation, "scope", &[])).unwrap();
        assert!(p.durable);
        paths.push(p.path);
    }
    assert!(paths[..4].iter().all(|path| !path.exists()));
    assert!(paths[4..].iter().all(|path| path.exists()));
}

fn write_m1_fixture(dir: &Path, index: &Index) -> (PathBuf, Vec<u8>) {
    let filename = "generation-1-99-123.xsi";
    let path = dir.join(filename);
    // 小列表在 M1 和 v2 的倒排编码相同。去掉 v2 字段并重建完整校验头。
    assert!(index.len() <= 4);
    index.write_snapshot(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let mut manifest: serde_json::Value = serde_json::from_slice(&bytes[32..32 + size]).unwrap();
    manifest.as_object_mut().unwrap().remove("rle_postings");
    let manifest = serde_json::to_vec(&manifest).unwrap();
    let mut legacy = bytes[..32].to_vec();
    legacy[8..12].copy_from_slice(&1u32.to_le_bytes());
    legacy[12..16].copy_from_slice(&(manifest.len() as u32).to_le_bytes());
    legacy.extend(manifest);
    legacy.extend(&bytes[32 + size..]);
    let crc = crc32fast::hash(&legacy[32..]);
    legacy[24..28].copy_from_slice(&crc.to_le_bytes());
    fs::write(&path, &legacy).unwrap();
    fs::write(
        dir.join("CURRENT.json"),
        format!("{{\"version\":1,\"file\":\"{filename}\",\"generation\":1}}"),
    )
    .unwrap();
    (path, legacy)
}

#[test]
fn loads_an_m1_manifest_and_pointer_without_rewriting_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (path, legacy) =
        write_m1_fixture(dir.path(), &segment(1, "scope", &[("/abc.txt", "local")]));
    let index = store.load().unwrap();
    assert!(index.coverage_complete());
    assert_eq!(
        index
            .search("abc", &SearchOptions::default(), None)
            .unwrap()
            .total,
        1
    );
    assert!(store.publish(&segment(2, "scope", &[])).is_err());
    assert!(store
        .publish_delta(1, &[0], &segment(2, "scope", &[]))
        .is_err());
    assert!(store.recover("scope").is_err());
    assert!(!dir.path().join("writer.lock").exists());
    assert_eq!(fs::read(path).unwrap(), legacy);
}

#[test]
fn migrate_m1_keeps_source_files_and_validates_scope_in_new_store() {
    use xs_core::operations;
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("m1");
    let destination = temporary.path().join("v2");
    let files = temporary.path().join("synthetic");
    fs::create_dir_all(source.join("index")).unwrap();
    fs::create_dir(&files).unwrap();
    let settings = xs_platform::Settings {
        roots: vec![xs_platform::Scope::new(files.to_str().unwrap())],
        ..Default::default()
    };
    let config_path = source.join("settings.json");
    settings.save(&config_path).unwrap();
    let token = settings.scope_token().unwrap();
    let local = files.join("本地.txt");
    let index = segment(
        1,
        &token,
        &[
            (local.to_str().unwrap(), "local"),
            ("\\\\fixture\\share\\离线.txt", "offline:efu"),
        ],
    );
    let (legacy_path, legacy) = write_m1_fixture(&source.join("index"), &index);
    let old_settings = fs::read(&config_path).unwrap();
    let old_pointer = fs::read(source.join("index/CURRENT.json")).unwrap();
    assert!(operations::save_settings(&source, &xs_platform::Settings::default()).is_err());
    assert!(operations::migrate_m1(&source, &source.join("embedded")).is_err());
    let outcome = operations::migrate_m1(&source, &destination).unwrap();
    assert_eq!(
        (outcome.generation, outcome.indexed, outcome.offline_entries),
        (1, 2, 1)
    );
    let restored = operations::load_index(&destination).unwrap();
    assert_eq!(restored.scope_token(), token);
    assert_eq!(
        restored.records().collect::<Vec<_>>(),
        index.records().collect::<Vec<_>>()
    );
    assert!(operations::migrate_m1(&source, &destination).is_err());
    assert_eq!(fs::read(&config_path).unwrap(), old_settings);
    assert_eq!(fs::read(legacy_path).unwrap(), legacy);
    assert_eq!(
        fs::read(source.join("index/CURRENT.json")).unwrap(),
        old_pointer
    );
    assert!(!source.join("index/writer.lock").exists());
}

#[test]
fn imported_nulls_remain_unknown_inside_negated_boolean_queries() {
    let mut builder = IndexBuilder::default();
    builder
        .add_with_metadata(
            FileRecord {
                path: "/unknown.txt".into(),
                source: "offline:efu".into(),
                size: 0,
                modified: 0,
                is_dir: false,
            },
            Some(CatalogMetadata {
                fields: BTreeMap::from([
                    ("Size".into(), "".into()),
                    ("Date Modified".into(), "".into()),
                ]),
            }),
        )
        .unwrap();
    builder
        .add(FileRecord {
            path: "/known.txt".into(),
            source: "offline:efu".into(),
            size: 0,
            modified: 0,
            is_dir: false,
        })
        .unwrap();
    let index = builder.finish(1).unwrap();
    let size = index
        .search("size:0", &SearchOptions::default(), None)
        .unwrap();
    assert_eq!(size.results.len(), 1);
    assert_eq!(size.results[0].path, "/known.txt");
    assert_eq!(
        index
            .search("!size:0", &SearchOptions::default(), None)
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        index
            .search("!<size:0 | unknown>", &SearchOptions::default(), None)
            .unwrap()
            .total,
        0
    );
}

#[test]
fn damaged_delta_catalog_never_looks_like_an_empty_success() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .publish(&segment(1, "scope", &[("/original.txt", "local")]))
        .unwrap();
    let delta = store
        .publish_delta(
            1,
            &[0],
            &segment(2, "scope", &[("/replacement.txt", "local")]),
        )
        .unwrap();
    let mut bytes = fs::read(&delta.path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&delta.path, bytes).unwrap();
    assert!(store.load().is_err());
    assert_eq!(store.recover("scope").unwrap().generation, 3);
    assert_eq!(
        store.load().unwrap().record(0).unwrap().path,
        "/original.txt"
    );
}

#[test]
fn packed_rows_keep_extreme_utc_times_and_attributes() {
    use xs_core::FileProperties;
    let mut builder = IndexBuilder::new("scope".into());
    let mut expected = Vec::new();
    for i in 0..2048 {
        let row = FileRecord {
            path: format!("/folder/合成文件批次-{i:04}.txt"),
            source: "local".into(),
            size: i,
            modified: if i % 2 == 0 { i64::MAX } else { i64::MIN },
            is_dir: false,
        };
        let properties = FileProperties {
            created: Some(xs_platform::FileTimestamp {
                seconds: -100,
                nanoseconds: 17,
            }),
            attributes: Some(32),
        };
        builder
            .add_record_with_properties(row.clone(), None, None, properties)
            .unwrap();
        expected.push((row, properties));
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packed.xsi");
    builder.finish(1).unwrap().write_snapshot(&path).unwrap();
    let index = Index::open_snapshot(&path).unwrap();
    for (id, (row, properties)) in expected.into_iter().enumerate() {
        assert_eq!(index.record(id as u32).unwrap(), row);
        assert_eq!(index.record_properties(id as u32).unwrap(), properties);
    }
    assert_eq!(
        index
            .search("attrib:a", &SearchOptions::default(), None)
            .unwrap()
            .total,
        2048
    );
}
