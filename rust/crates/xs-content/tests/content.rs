use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use xs_content::{rebuild, serve, validate_response, ContentReader, Location, SearchOptions};
use xs_platform::{Scope, Settings};

fn sandbox() -> tempfile::TempDir {
    let root = std::env::var_os("XS_CONTENT_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    tempfile::Builder::new()
        .prefix("xunshu-content-test-")
        .tempdir_in(root)
        .unwrap()
}

fn settings(dir: &Path, docs: &Path) -> Settings {
    let settings = Settings {
        content_enabled: true,
        content_roots: vec![Scope::new(docs.to_str().unwrap())],
        ..Settings::default()
    };
    settings.save(dir.join("settings.json")).unwrap();
    settings
}

fn worker() -> &'static Path {
    static WORKER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    WORKER
        .get_or_init(|| {
            std::env::var_os("XS_CONTENT_TEST_WORKER_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_xs-content-worker")))
        })
        .as_path()
}

fn zip(files: &[(&str, &str)]) -> Vec<u8> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, text) in files {
        archive
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(text.as_bytes()).unwrap();
    }
    archive.finish().unwrap().into_inner()
}

#[test]
fn real_worker_indexes_safe_formats_and_locations() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, tmp.path());
    fs::write(docs.join("plain.txt"), "预算和會議。\n预算和会议纪要。").unwrap();
    fs::write(
        docs.join("sample.html"),
        "<html><head><title>隐藏标题</title></head><body><script>恶意脚本</script><p>可见正文</p></body></html>",
    )
    .unwrap();
    fs::write(data.join("private.txt"), "数据目录秘密").unwrap();
    fs::write(
        docs.join("sample.docx"),
        zip(&[("word/document.xml",
            r#"<w:document xmlns:w="urn:word"><w:body><w:p><w:r><w:t>文档&amp;段落</w:t></w:r></w:p></w:body></w:document>"#,
        )]),
    )
    .unwrap();
    fs::write(
        docs.join("sample.odp"),
        zip(&[(
            "content.xml",
            r#"<office:document-content xmlns:office="urn:office" xmlns:draw="urn:draw" xmlns:text="urn:text"><office:body><office:presentation><draw:page/><draw:page><text:p>第二页演示</text:p></draw:page></office:presentation></office:body></office:document-content>"#,
        )]),
    )
    .unwrap();
    fs::write(
        docs.join("sample.xlsx"),
        zip(&[
            ("[Content_Types].xml", r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#),
            ("_rels/.rels", r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
            ("xl/workbook.xml", r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="表格" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
            ("xl/_rels/workbook.xml.rels", r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#),
            ("xl/worksheets/sheet1.xml", r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="3"><c r="AB3" t="inlineStr"><is><t>单元格位置</t></is></c><c r="AC3"><f>SUM(1,2)</f><v>3</v></c></row></sheetData></worksheet>"#),
        ]),
    )
    .unwrap();

    let report = rebuild(&data, worker(), &AtomicBool::new(false)).unwrap();
    assert!(report.published && report.complete, "{report:?}");
    assert_eq!(report.documents, 5);
    let mut reader = ContentReader::new(&data);
    let mut query = |text| {
        reader
            .search(text, &SearchOptions::default(), &AtomicBool::new(false))
            .unwrap()
    };
    assert_eq!(query("数据目录秘密").indexed_total, 0);
    assert_eq!(query("恶意脚本").indexed_total, 0);
    assert_eq!(query("隐藏标题").indexed_total, 0);
    assert_eq!(query("可见正文").indexed_total, 1);
    let hit = &query("文档").hits[0];
    assert_eq!(hit.location, Location::Paragraph { number: 1 });
    assert_eq!(hit.snippet, "文档&段落");
    assert_eq!(
        query("第二页演示").hits[0].location,
        Location::Slide { number: 2 }
    );
    let result = query("单元格位置");
    validate_response(&data, &result).unwrap();
    assert_eq!(
        result.hits[0].location,
        Location::Cell {
            sheet: "表格".into(),
            address: "AB3".into(),
        }
    );
    assert_eq!(
        query("SUM").hits[0].location,
        Location::Cell {
            sheet: "表格".into(),
            address: "AC3".into(),
        }
    );
    let hit = &query("会议").hits[0];
    let span = &hit.highlights[0];
    assert_eq!(&hit.snippet[span.start..span.end], "会议");
}

#[test]
fn unchanged_files_reuse_verified_segments_and_changes_are_reindexed() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    fs::write(docs.join("alpha.txt"), "阿尔法文档内容").unwrap();
    fs::write(docs.join("beta.txt"), "贝塔文档内容").unwrap();
    let cancel = AtomicBool::new(false);
    let first = rebuild(&data, worker(), &cancel).unwrap();
    assert!(first.published && first.complete && first.reused_documents == 0);
    let again = rebuild(&data, worker(), &cancel).unwrap();
    assert!(again.published && again.complete);
    assert_eq!(again.reused_documents, 2);
    assert_eq!(again.reused_segments, first.segments);
    fs::write(docs.join("beta.txt"), "新版贝塔文档内容更长").unwrap();
    let updated = rebuild(&data, worker(), &cancel).unwrap();
    assert!(updated.published && updated.complete);
    assert_eq!(updated.reused_documents, 1);
    let mut reader = ContentReader::new(&data);
    assert_eq!(
        reader
            .search("新版贝塔", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        1
    );
    assert_eq!(
        reader
            .search("阿尔法", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        1
    );
}

#[test]
fn failed_extraction_never_replaces_complete_index() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    fs::write(docs.join("good.txt"), "确认可用的历史正文").unwrap();
    let cancel = AtomicBool::new(false);
    let first = rebuild(&data, worker(), &cancel).unwrap();
    assert!(first.published && first.complete);
    // 畸形 DOCX 不能使原来完整可用的快照被缺文档的新快照覆盖。
    fs::write(docs.join("bad.docx"), b"this is not a ZIP package").unwrap();
    let attempted = rebuild(&data, worker(), &cancel).unwrap();
    assert!(!attempted.published);
    assert!(attempted.failed > 0);
    let mut reader = ContentReader::new(&data);
    let existing = reader
        .search("历史正文", &SearchOptions::default(), &cancel)
        .unwrap();
    assert_eq!(existing.generation, first.generation.unwrap());
    assert_eq!(existing.indexed_total, 1);
}

#[test]
fn replacement_scope_change_and_corruption_fail_closed() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    let mut settings = settings(&data, &docs);
    let doc = docs.join("cache.txt");
    fs::write(&doc, "原始正文").unwrap();
    let cancel = AtomicBool::new(false);
    let first = rebuild(&data, worker(), &cancel)
        .unwrap()
        .generation
        .unwrap();
    let mut reader = ContentReader::new(&data);
    let options = SearchOptions::default();
    let original = reader.search("原始", &options, &cancel).unwrap();
    validate_response(&data, &original).unwrap();

    fs::write(&doc, "更新正文").unwrap();
    assert!(validate_response(&data, &original).is_err());
    let stale = reader.search("原始", &options, &cancel).unwrap();
    assert!(stale.hits.is_empty());
    assert_eq!(stale.omitted_stale, 1);
    let next = rebuild(&data, worker(), &cancel)
        .unwrap()
        .generation
        .unwrap();
    assert_eq!(next, first + 1);
    assert_eq!(
        reader.search("更新", &options, &cancel).unwrap().hits.len(),
        1
    );
    assert!(validate_response(&data, &original).is_err());

    let cancelled = rebuild(&data, worker(), &AtomicBool::new(true)).unwrap();
    assert!(cancelled.cancelled && !cancelled.published);
    assert_eq!(
        reader.search("更新", &options, &cancel).unwrap().generation,
        next
    );
    assert!(reader
        .search("更新", &options, &AtomicBool::new(true))
        .is_err());

    settings.content_enabled = false;
    settings.save(data.join("settings.json")).unwrap();
    assert!(reader.search("更新", &options, &cancel).is_err());
    settings.content_enabled = true;
    settings.save(data.join("settings.json")).unwrap();
    let index = data.join("content.idx");
    let mut damaged = fs::read(&index).unwrap();
    let last = damaged.last_mut().unwrap();
    *last ^= 0x80;
    fs::write(index, damaged).unwrap();
    assert!(reader.search("更新", &options, &cancel).is_err());
    assert!(rebuild(&data, worker(), &cancel).unwrap().published);
    assert_eq!(
        reader.search("更新", &options, &cancel).unwrap().hits.len(),
        1
    );
}

#[test]
fn ipc_bounds_requests_and_resynchronizes() {
    let mut request = vec![b'!'; 96 * 1024];
    request.extend_from_slice(
        br#"
{"method":"status","id":1}
{"method":"shutdown","id":2}
"#,
    );
    let mut output = Vec::new();
    serve(Path::new("unused"), Cursor::new(request), &mut output).unwrap();
    let frames: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0]["ok"], false);
    assert_eq!(frames[1]["result"]["protocol"], 1);
    assert_eq!(frames[2]["result"]["shutdown"], true);
}

#[cfg(target_os = "linux")]
#[test]
fn process_timeout_terminates_group_without_replacing_good_catalog() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    use xs_content::{rebuild_with_worker_options, WorkerOptions};

    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    fs::write(docs.join("known.txt"), "正文保留").unwrap();
    let cancel = AtomicBool::new(false);
    let generation = rebuild(&data, worker(), &cancel)
        .unwrap()
        .generation
        .unwrap();
    let original_hits = ContentReader::new(&data)
        .search("正文保留", &SearchOptions::default(), &cancel)
        .unwrap();
    assert_eq!(
        original_hits.hits.len(),
        1,
        "original indexed query failed: {original_hits:?}"
    );
    // Force extraction rather than reusing the unchanged document from the
    // prior complete generation; otherwise the timeout worker is not invoked.
    fs::write(docs.join("known.txt"), "正文保留（本次已修改）").unwrap();
    let bad_worker = tmp.path().join("hang-worker");
    fs::write(
        &bad_worker,
        "#!/bin/sh\nprintf '\\014\\000\\000\\000XS-CONTENT/1'\n/bin/sleep 20\n",
    )
    .unwrap();
    fs::set_permissions(&bad_worker, fs::Permissions::from_mode(0o700)).unwrap();
    let start = Instant::now();
    let report = rebuild_with_worker_options(
        &data,
        &bad_worker,
        &WorkerOptions {
            memory_mib: 384,
            timeout_ms: 250,
        },
        &cancel,
    )
    .unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(!report.published && report.failed == 1);
    let result = ContentReader::new(&data)
        .search("正文保留", &SearchOptions::default(), &cancel)
        .unwrap();
    assert_eq!(result.generation, generation);
    // The complete catalog remains in place, but the modified on-disk file
    // must be excluded until a successful extraction validates its revision.
    assert!(result.hits.is_empty());
    assert_eq!(result.omitted_stale, 1);
}

#[test]
fn incremental_delta_add_update_delete_and_noop_preserve_results() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    let alpha = docs.join("alpha.txt");
    let beta = docs.join("beta.txt");
    let ignored = docs.join("ignored.md");
    fs::write(&alpha, "alpha-old unique-token").unwrap();
    fs::write(&beta, "beta-stable unique-token").unwrap();
    let cancel = AtomicBool::new(false);
    let first = rebuild(&data, worker(), &cancel).unwrap();
    assert!(first.published && first.complete);
    let generation = first.generation.unwrap();

    let noop = rebuild(&data, worker(), &cancel).unwrap();
    assert!(noop.published && noop.complete && noop.unchanged_snapshot);
    assert_eq!(noop.generation, Some(generation));
    assert_eq!(noop.updated_documents, 0);
    assert_eq!(noop.removed_documents, 0);
    assert_eq!(noop.reused_documents, 2);

    fs::remove_file(&alpha).unwrap();
    fs::write(&beta, "beta-updated unique-token").unwrap();
    fs::write(&ignored, "newly-added unique-token").unwrap();
    let changed = rebuild(&data, worker(), &cancel).unwrap();
    assert!(changed.published && changed.complete && !changed.unchanged_snapshot);
    assert_eq!(changed.removed_documents, 1);
    assert_eq!(changed.updated_documents, 2);
    assert_eq!(changed.generation, Some(generation + 1));
    let mut reader = ContentReader::new(&data);
    let mut search = |q| {
        reader
            .search(q, &SearchOptions::default(), &cancel)
            .unwrap()
    };
    assert_eq!(search("alpha-old").indexed_total, 0);
    assert_eq!(search("beta-stable").indexed_total, 0);
    assert_eq!(search("beta-updated").indexed_total, 1);
    assert_eq!(search("newly-added").indexed_total, 1);
}

#[test]
fn incremental_failed_update_keeps_previous_published_generation() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    fs::write(docs.join("good.txt"), "old-persisted-content").unwrap();
    let cancel = AtomicBool::new(false);
    let first = rebuild(&data, worker(), &cancel).unwrap();
    let generation = first.generation.unwrap();
    fs::write(docs.join("good.txt"), "new-different-content").unwrap();
    fs::write(docs.join("bad.docx"), b"not a valid archive").unwrap();
    let failed = rebuild(&data, worker(), &cancel).unwrap();
    assert!(!failed.published && failed.failed > 0);
    let mut reader = ContentReader::new(&data);
    let r = reader
        .search("old-persisted-content", &SearchOptions::default(), &cancel)
        .unwrap();
    assert_eq!(r.generation, generation);
    assert!(r.hits.is_empty()); // 搜索时仍禁止显示已变更文件的旧结果
    fs::remove_file(docs.join("bad.docx")).unwrap();
    let retry = rebuild(&data, worker(), &cancel).unwrap();
    assert!(retry.published && retry.complete);
    assert_eq!(retry.generation, Some(generation + 1));
    assert_eq!(
        reader
            .search("new-different-content", &SearchOptions::default(), &cancel)
            .unwrap()
            .hits
            .len(),
        1
    );
}

#[test]
fn empty_document_is_persisted_and_noop_does_not_publish_a_new_generation() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    let empty = docs.join("empty.txt");
    let filled = docs.join("filled.txt");
    fs::write(&empty, "").unwrap();
    fs::write(&filled, "searchable initial-content").unwrap();
    let cancel = AtomicBool::new(false);

    let first = rebuild(&data, worker(), &cancel).unwrap();
    assert!(first.published && first.complete, "{first:?}");
    assert_eq!(first.documents, 2); // zero-segment files still have a revision
    assert_eq!(first.updated_documents, 2);
    let generation = first.generation.unwrap();
    let unchanged = rebuild(&data, worker(), &cancel).unwrap();
    assert!(unchanged.unchanged_snapshot, "{unchanged:?}");
    assert_eq!(unchanged.generation, Some(generation));
    assert_eq!(unchanged.reused_documents, 2);
    assert_eq!(unchanged.updated_documents, 0);

    // A formerly searchable document can become empty without leaving stale FTS terms.
    fs::write(&filled, "").unwrap();
    let cleared = rebuild(&data, worker(), &cancel).unwrap();
    assert!(cleared.published && cleared.complete, "{cleared:?}");
    assert_eq!(cleared.updated_documents, 1);
    let mut reader = ContentReader::new(&data);
    assert_eq!(
        reader
            .search("initial-content", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        0
    );
    let nochange = rebuild(&data, worker(), &cancel).unwrap();
    assert!(nochange.unchanged_snapshot, "{nochange:?}");
    assert_eq!(nochange.reused_documents, 2);
    assert_eq!(nochange.updated_documents, 0);

    // Deleted empty documents must still be removed, not silently retained forever.
    fs::remove_file(&empty).unwrap();
    let deleted = rebuild(&data, worker(), &cancel).unwrap();
    assert!(deleted.published && deleted.complete, "{deleted:?}");
    assert_eq!(deleted.removed_documents, 1);
    assert_eq!(deleted.documents, 1);
    assert!(
        rebuild(&data, worker(), &cancel)
            .unwrap()
            .unchanged_snapshot
    );
}

#[test]
fn failed_scan_does_not_publish_pending_deletions_or_replacements() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    let keep = docs.join("keep.txt");
    let deleted = docs.join("deleted.txt");
    fs::write(&keep, "first stable contents").unwrap();
    fs::write(&deleted, "deleted stable contents").unwrap();
    let cancel = AtomicBool::new(false);
    let baseline = rebuild(&data, worker(), &cancel).unwrap();
    assert!(baseline.published && baseline.complete);
    fs::remove_file(&deleted).unwrap();
    fs::write(&keep, "new changed contents").unwrap();
    fs::write(docs.join("broken.docx"), b"invalid zip data").unwrap();
    let failure = rebuild(&data, worker(), &cancel).unwrap();
    assert!(!failure.published && failure.failed > 0, "{failure:?}");
    // The index header generation remains untouched by unsuccessful work.
    let original = ContentReader::new(&data)
        .search("deleted stable", &SearchOptions::default(), &cancel)
        .unwrap();
    assert_eq!(original.generation, baseline.generation.unwrap());
    assert!(original.hits.is_empty()); // file was deleted; no stale result exposed
    fs::remove_file(docs.join("broken.docx")).unwrap();
    let retry = rebuild(&data, worker(), &cancel).unwrap();
    assert!(retry.published && retry.complete, "{retry:?}");
    assert_eq!(retry.removed_documents, 1);
    let mut reader = ContentReader::new(&data);
    assert_eq!(
        reader
            .search("new changed", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        1
    );
    assert_eq!(
        reader
            .search("first stable", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        0
    );
}

#[test]
fn failed_build_resumes_candidate_without_exposing_unpublished_documents() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    let cancel = AtomicBool::new(false);
    fs::write(docs.join("old.txt"), "原有正文").unwrap();
    let first = rebuild(&data, worker(), &cancel).unwrap();
    fs::write(docs.join("new.txt"), "新增候选正文").unwrap();
    fs::write(docs.join("changing.txt"), "候选旧内容").unwrap();
    fs::write(docs.join("bad.docx"), b"invalid zip").unwrap();
    let failed = rebuild(&data, worker(), &cancel).unwrap();
    assert!(
        !failed.published && failed.checkpoints_written > 0,
        "{failed:?}"
    );
    let pending = data.join("content.pending.idx");
    assert!(pending.is_file());
    let mut reader = ContentReader::new(&data);
    let result = reader
        .search("新增候选", &SearchOptions::default(), &cancel)
        .unwrap();
    assert_eq!(result.generation, first.generation.unwrap());
    assert_eq!(result.indexed_total, 0);
    fs::remove_file(docs.join("bad.docx")).unwrap();
    fs::write(docs.join("changing.txt"), "已重新编辑候选正文").unwrap();
    fs::remove_file(docs.join("old.txt")).unwrap();
    let resumed = rebuild(&data, worker(), &cancel).unwrap();
    assert!(
        resumed.published && resumed.resumed_checkpoint,
        "{resumed:?}"
    );
    assert_eq!(resumed.updated_documents, 1);
    assert_eq!(resumed.reused_documents, 1);
    assert_eq!(resumed.removed_documents, 1);
    assert_eq!(resumed.generation, Some(first.generation.unwrap() + 1));
    assert!(!pending.exists());
    assert_eq!(
        reader
            .search("新增候选", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        1
    );
    assert_eq!(
        reader
            .search("候选旧内容", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        0
    );
    assert_eq!(
        reader
            .search("原有正文", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        0
    );
}

#[test]
fn damaged_or_reauthorized_checkpoint_is_not_reused() {
    for change_settings in [false, true] {
        let tmp = sandbox();
        let data = tmp.path().join("app");
        let docs = tmp.path().join("docs");
        fs::create_dir_all(&docs).unwrap();
        let mut config = settings(&data, &docs);
        let cancel = AtomicBool::new(false);
        fs::write(docs.join("good.txt"), "有效正文").unwrap();
        fs::write(docs.join("bad.docx"), b"invalid zip").unwrap();
        let failed = rebuild(&data, worker(), &cancel).unwrap();
        assert!(
            !failed.published && failed.checkpoints_written > 0,
            "{failed:?}"
        );
        let pending = data.join("content.pending.idx");
        if change_settings {
            config.content_enabled = false;
            config.save(data.join("settings.json")).unwrap();
            config.content_enabled = true;
            config.save(data.join("settings.json")).unwrap();
        } else {
            let mut bytes = fs::read(&pending).unwrap();
            *bytes.last_mut().unwrap() ^= 1;
            fs::write(&pending, bytes).unwrap();
        }
        fs::remove_file(docs.join("bad.docx")).unwrap();
        let result = rebuild(&data, worker(), &cancel).unwrap();
        assert!(result.published && !result.resumed_checkpoint, "{result:?}");
        assert_eq!(result.updated_documents, 1);
        assert!(!pending.exists());
    }
}

#[test]
fn checkpoint_survives_process_exit_and_can_be_discarded_safely() {
    const CHILD_DATA: &str = "XS_TEST_CHECKPOINT_CHILD_DATA";
    if let Some(data) = std::env::var_os(CHILD_DATA) {
        let report = rebuild(Path::new(&data), worker(), &AtomicBool::new(false)).unwrap();
        assert!(
            !report.published && report.checkpoints_written > 0,
            "{report:?}"
        );
        return;
    }
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    let cancel = AtomicBool::new(false);
    fs::write(docs.join("old.txt"), "正式索引内容").unwrap();
    let first = rebuild(&data, worker(), &cancel).unwrap();
    fs::write(docs.join("new.txt"), "新正文内容").unwrap();
    fs::write(docs.join("bad.docx"), b"bad").unwrap();
    let exit = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "checkpoint_survives_process_exit_and_can_be_discarded_safely",
        ])
        .env(CHILD_DATA, &data)
        .status()
        .unwrap();
    assert!(exit.success());
    assert!(data.join("content.pending.idx").is_file());
    fs::remove_file(docs.join("bad.docx")).unwrap();
    let resumed = rebuild(&data, worker(), &cancel).unwrap();
    assert!(
        resumed.published && resumed.resumed_checkpoint,
        "{resumed:?}"
    );
    assert_eq!(resumed.updated_documents, 0);
    assert_eq!(resumed.generation, Some(first.generation.unwrap() + 1));
    fs::write(docs.join("other.txt"), "未发布其他内容").unwrap();
    fs::write(docs.join("bad.docx"), b"bad").unwrap();
    let report = rebuild(&data, worker(), &cancel).unwrap();
    assert!(!report.published && report.checkpoints_written > 0);
    xs_content::discard_pending_checkpoint(&data).unwrap();
    xs_content::discard_pending_checkpoint(&data).unwrap();
    assert!(!data.join("content.pending.idx").exists());
    let mut reader = ContentReader::new(&data);
    let response = reader
        .search("正式索引", &SearchOptions::default(), &cancel)
        .unwrap();
    assert_eq!(response.generation, resumed.generation.unwrap());
    assert_eq!(response.indexed_total, 1);
    assert!(docs.join("new.txt").is_file());
}

#[test]
fn durable_change_queue_targets_files_and_reconciles_uncertain_events() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    let a = docs.join("a.txt");
    let b = docs.join("b.txt");
    let c = docs.join("c.txt");
    fs::write(&a, "original-alpha").unwrap();
    fs::write(&b, "unrelated-beta").unwrap();
    let cancel = AtomicBool::new(false);
    assert!(rebuild(&data, worker(), &cancel).unwrap().scope_reconciled);
    fs::write(&a, "changed-alpha").unwrap();
    fs::write(&c, "new-gamma").unwrap();
    assert_eq!(
        xs_content::enqueue_changes(&data, std::slice::from_ref(&a)).unwrap(),
        1
    );
    assert_eq!(
        xs_content::enqueue_changes(&data, &[a.clone(), c.clone()]).unwrap(),
        2
    );
    let queue = data.join("content.requests.json");
    assert!(queue.is_file());
    let stopped =
        xs_content::apply_queued_changes(&data, worker(), &AtomicBool::new(true)).unwrap();
    assert!(stopped.cancelled && queue.is_file());
    let updated = xs_content::apply_queued_changes(&data, worker(), &cancel).unwrap();
    assert!(
        updated.published && updated.targeted_update && !updated.scope_reconciled,
        "{updated:?}"
    );
    assert_eq!(updated.updated_documents, 2);
    assert_eq!(updated.removed_documents, 0);
    assert!(!queue.exists());
    let mut reader = ContentReader::new(&data);
    for text in ["changed-alpha", "unrelated-beta", "new-gamma"] {
        assert_eq!(
            reader
                .search(text, &SearchOptions::default(), &cancel)
                .unwrap()
                .hits
                .len(),
            1
        );
    }
    fs::remove_file(&b).unwrap();
    xs_content::enqueue_changes(&data, &[b]).unwrap();
    let reconciled = xs_content::apply_queued_changes(&data, worker(), &cancel).unwrap();
    assert!(
        reconciled.published && reconciled.scope_reconciled && !reconciled.targeted_update,
        "{reconciled:?}"
    );
    assert_eq!(reconciled.removed_documents, 1);
    assert_eq!(
        reader
            .search("unrelated-beta", &SearchOptions::default(), &cancel)
            .unwrap()
            .indexed_total,
        0
    );
}

#[test]
fn change_queue_rejects_outside_scope_and_recovers_corruption() {
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    let a = docs.join("a.txt");
    fs::write(&a, "alpha").unwrap();
    let cancel = AtomicBool::new(false);
    rebuild(&data, worker(), &cancel).unwrap();
    assert!(xs_content::enqueue_changes(&data, &[tmp.path().join("outside.txt")]).is_err());
    assert!(!data.join("content.requests.json").exists());
    xs_content::enqueue_changes(&data, std::slice::from_ref(&a)).unwrap();
    fs::write(data.join("content.requests.json"), "invalid-json").unwrap();
    fs::write(&a, "after-corruption").unwrap();
    let recovered = xs_content::apply_queued_changes(&data, worker(), &cancel).unwrap();
    assert!(
        recovered.published && recovered.scope_reconciled && !recovered.targeted_update,
        "{recovered:?}"
    );
    assert!(!data.join("content.requests.json").exists());
    xs_content::enqueue_changes(&data, &[a]).unwrap();
    xs_content::discard_pending_work(&data).unwrap();
    assert!(!data.join("content.requests.json").exists());
    assert!(data.join("content.idx").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn foreground_content_watch_reconciles_real_filesystem_notification() {
    use std::{
        sync::{atomic::Ordering, mpsc},
        time::Duration,
    };
    let tmp = sandbox();
    let data = tmp.path().join("app");
    let docs = tmp.path().join("docs");
    fs::create_dir_all(&docs).unwrap();
    settings(&data, &docs);
    let a = docs.join("watched.txt");
    fs::write(&a, "original-watch").unwrap();
    let cancel = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let thread = scope.spawn(|| {
            xs_content::watch_changes(&data, worker(), &cancel, |report| {
                let _ = tx.send((
                    report.published,
                    report.scope_reconciled,
                    report.updated_documents,
                ));
            })
        });
        let result = (|| -> anyhow::Result<()> {
            let first = rx.recv_timeout(Duration::from_secs(15))?;
            anyhow::ensure!(first == (true, true, 1));
            fs::write(&a, "modified-watch-content")?;
            let next = rx.recv_timeout(Duration::from_secs(15))?;
            anyhow::ensure!(next.0 && next.2 == 1, "unexpected update {next:?}");
            let response = ContentReader::new(&data).search(
                "modified-watch-content",
                &SearchOptions::default(),
                &cancel,
            )?;
            anyhow::ensure!(response.hits.len() == 1);
            Ok(())
        })();
        cancel.store(true, Ordering::Release);
        thread.join().unwrap().unwrap();
        result.unwrap();
    });
}
