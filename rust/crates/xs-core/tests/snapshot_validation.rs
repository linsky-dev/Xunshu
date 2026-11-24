//! Valid CRCs do not excuse structurally invalid snapshots. These regressions
//! mutate complete files, just as an interrupted editor or faulty writer could.
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path, sync::atomic::AtomicBool};
use xs_core::{CatalogMetadata, FileRecord, Index, IndexBuilder, SearchOptions};

fn sample() -> Index {
    let mut builder = IndexBuilder::default();
    builder
        .add(FileRecord {
            path: "/catalog/ABC.txt".into(),
            size: 12,
            modified: 123,
            is_dir: false,
            source: "offline:test".into(),
        })
        .unwrap();
    builder.finish(1).unwrap()
}

fn rewrite(path: &Path, change: impl FnOnce(&mut Value, &mut Vec<u8>)) {
    let original = fs::read(path).unwrap();
    let length = u32::from_le_bytes(original[12..16].try_into().unwrap()) as usize;
    let mut manifest: Value = serde_json::from_slice(&original[32..32 + length]).unwrap();
    let mut payload = original[32 + length..].to_vec();
    change(&mut manifest, &mut payload);
    let manifest = serde_json::to_vec(&manifest).unwrap();
    let mut header = original[..32].to_vec();
    header[12..16].copy_from_slice(&(manifest.len() as u32).to_le_bytes());
    header[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&manifest);
    hasher.update(&payload);
    header[24..28].copy_from_slice(&hasher.finalize().to_le_bytes());
    header.extend(manifest);
    header.extend(payload);
    fs::write(path, header).unwrap();
}

#[test]
fn valid_crc_cannot_hide_invalid_normalization_or_postings() {
    let dir = tempfile::tempdir().unwrap();
    for kind in 0..4 {
        let path = dir.path().join(format!("bad-{kind}.xsi"));
        sample().write_snapshot(&path).unwrap();
        rewrite(&path, |manifest, payload| match kind {
            0 => payload[32..36].copy_from_slice(&2u32.to_le_bytes()),
            1 => {
                let gram = manifest["grams"]["start"].as_u64().unwrap() as usize;
                payload[gram + 16..gram + 24].fill(0);
            }
            2 => manifest["parents"][0] = Value::String(format!("/{}", "p".repeat(262144))),
            _ => {
                let post = manifest["postings"]["start"].as_u64().unwrap() as usize;
                payload[post] = 128;
            }
        });
        assert!(
            Index::open_snapshot(&path).is_err(),
            "mutation {kind} was accepted"
        );
    }
}

#[test]
fn sparse_metadata_survives_snapshot_and_rebuild() {
    let fields = BTreeMap::from([
        ("Size".into(), "".into()),
        ("Date Modified".into(), "133444736000000001".into()),
        ("自定义".into(), "含引号\"与换行\n的原值".into()),
    ]);
    let mut builder = IndexBuilder::default();
    builder
        .add_with_metadata(
            sample().record(0).unwrap(),
            Some(CatalogMetadata {
                fields: fields.clone(),
            }),
        )
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.xsi");
    builder.finish(1).unwrap().write_snapshot(&path).unwrap();
    let mapped = Index::open_snapshot(&path).unwrap();
    assert_eq!(mapped.record_metadata(0).unwrap().unwrap().fields, fields);
    let mut rebuilt = IndexBuilder::default();
    rebuilt.add_from_index(&mapped, 0).unwrap();
    assert_eq!(
        rebuilt
            .finish(2)
            .unwrap()
            .record_metadata(0)
            .unwrap()
            .unwrap()
            .fields,
        fields
    );
    drop(mapped);
    rewrite(&path, |_, payload| {
        payload[36..40].copy_from_slice(&u32::MAX.to_le_bytes())
    });
    assert!(Index::open_snapshot(&path).is_err());
}

#[test]
fn pre_cancelled_wide_or_does_not_generate_candidates() {
    let index = sample();
    let query = std::iter::repeat_n("abc", 128)
        .collect::<Vec<_>>()
        .join(" | ");
    let response = index
        .search(
            &query,
            &SearchOptions::default(),
            Some(&AtomicBool::new(true)),
        )
        .unwrap();
    assert!(!response.complete);
    assert!(!response.candidate_count_exact);
    assert_eq!(response.examined, 0);
    assert_eq!(response.total_relation, "at_least");
}

#[test]
fn an_expired_deadline_never_claims_an_exact_empty_result() {
    let mut builder = IndexBuilder::default();
    for i in 0..25000 {
        builder
            .add(FileRecord {
                path: format!("/catalog/common-{i:08}.txt"),
                size: 1,
                modified: 0,
                is_dir: false,
                source: "offline:test".into(),
            })
            .unwrap();
    }
    let response = builder
        .finish(1)
        .unwrap()
        .search(
            "common",
            &SearchOptions {
                time_budget_ms: 1,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    if !response.complete {
        assert_eq!(response.total_relation, "at_least");
    } else {
        assert_eq!(response.total, 25000);
    }
}
