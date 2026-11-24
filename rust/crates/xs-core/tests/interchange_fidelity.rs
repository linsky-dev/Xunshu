//! Synthetic migration regressions; catalog paths are metadata and never opened.
use rusqlite::{params, Connection};
use std::{collections::BTreeMap, fs, path::Path};
use xs_core::{
    interchange::{export_efu, import_efu, import_schema4},
    CatalogMetadata, FileRecord, Index, IndexBuilder,
};

fn csv_bytes(headers: &[&str], rows: &[Vec<String>]) -> Vec<u8> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(headers).unwrap();
    for row in rows {
        writer.write_record(row).unwrap();
    }
    writer.into_inner().unwrap()
}

fn export_rows(index: &Index) -> (Vec<String>, Vec<BTreeMap<String, String>>) {
    let mut output = Vec::new();
    export_efu(index, &mut output).unwrap();
    let mut reader = csv::Reader::from_reader(output.as_slice());
    let headers: Vec<_> = reader
        .headers()
        .unwrap()
        .iter()
        .map(str::to_owned)
        .collect();
    let rows = reader
        .records()
        .map(|row| {
            headers
                .iter()
                .cloned()
                .zip(row.unwrap().iter().map(str::to_owned))
                .collect()
        })
        .collect();
    (headers, rows)
}

fn base_record(path: &str) -> FileRecord {
    FileRecord {
        path: path.to_owned(),
        size: 12,
        modified: 1_700_000_000,
        is_dir: false,
        source: "offline:synthetic".to_owned(),
    }
}

fn legacy_fixture(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch("PRAGMA user_version=4;
        CREATE TABLE files(id INTEGER PRIMARY KEY, path TEXT, size INTEGER, mtime REAL, is_dir INTEGER, source TEXT,
            mtime_filetime TEXT, birthtime_filetime TEXT, attributes INTEGER, extra_json TEXT, birthtime REAL);
        CREATE TABLE filelists(source TEXT PRIMARY KEY, path TEXT, enabled INTEGER);").unwrap();
    db
}

#[test]
fn efu_preserves_raw_values_empty_cells_created_time_and_custom_columns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rich.efu");
    let first = vec![
        r"C:\synthetic\报告.txt".to_owned(),
        "00012".to_owned(),
        "0133444736000000001".to_owned(),
        u64::MAX.to_string(),
        "0x00000022".to_owned(),
        " 项目 A ".to_owned(),
        "comma, quote \" and\r\n第二行\0末尾".to_owned(),
    ];
    let second = vec![
        r"\\unopened-server\share\folder\".to_owned(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ];
    let mut bytes = vec![0xef, 0xbb, 0xbf];
    bytes.extend(csv_bytes(
        &[
            "filename",
            "SIZE",
            "Date Modified",
            "Date Created",
            "Attributes",
            "Project",
            "Comment",
        ],
        &[first.clone(), second.clone()],
    ));
    fs::write(&path, bytes).unwrap();
    let index = import_efu(&path, "offline:roundtrip", 7).unwrap();
    assert_eq!(index.len(), 2);
    assert_eq!(index.record(0).unwrap().size, 12);
    assert_eq!(index.record(0).unwrap().modified, 1_700_000_000);
    assert!(index.record(1).unwrap().is_dir);
    let (_, rows) = export_rows(&index);
    let canonical = [
        "Filename",
        "Size",
        "Date Modified",
        "Date Created",
        "Attributes",
        "Project",
        "Comment",
    ];
    for (row, expected) in rows.iter().zip([first, second]) {
        for (column, value) in canonical.iter().zip(expected) {
            assert_eq!(row[*column], value, "column {column}");
        }
    }
    // Copies used by merge/rebuild must not discard the optional arena payload.
    let mut copied = IndexBuilder::default();
    for id in 0..index.len() as u32 {
        copied.add_from_index(&index, id).unwrap();
    }
    assert_eq!(export_rows(&copied.finish(8).unwrap()), export_rows(&index));
}

#[test]
fn exporter_unions_custom_columns_and_uses_raw_empty_over_defaults() {
    let mut builder = IndexBuilder::default();
    builder
        .add_with_metadata(
            base_record("C:\\synthetic\\one.txt"),
            Some(CatalogMetadata {
                fields: BTreeMap::from([
                    ("Date Modified".to_owned(), String::new()),
                    ("Size".to_owned(), String::new()),
                    ("Zeta".to_owned(), "z".to_owned()),
                ]),
            }),
        )
        .unwrap();
    builder
        .add_with_metadata(
            base_record("C:\\synthetic\\two.txt"),
            Some(CatalogMetadata {
                fields: BTreeMap::from([
                    ("Alpha".to_owned(), "a".to_owned()),
                    ("Date Created".to_owned(), "133444736123456789".to_owned()),
                ]),
            }),
        )
        .unwrap();
    let (headers, rows) = export_rows(&builder.finish(1).unwrap());
    assert_eq!(&headers[5..], &["Alpha", "Zeta"]);
    assert_eq!(rows[0]["Size"], "");
    assert_eq!(rows[0]["Date Modified"], "");
    assert_eq!(rows[0]["Alpha"], "");
    assert_eq!(rows[1]["Zeta"], "");
    assert_eq!(rows[1]["Date Created"], "133444736123456789");
    assert_eq!(rows[1]["Date Modified"], "133444736000000000");
}

#[test]
fn unsigned_filetime_extremes_are_bounded_and_retain_exact_text() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("time.efu");
    for text in [
        "0".to_owned(),
        "116444735999999999".to_owned(),
        u64::MAX.to_string(),
    ] {
        fs::write(
            &path,
            csv_bytes(
                &["Filename", "Date Modified", "Date Created"],
                &[vec![
                    "C:\\synthetic\\clock.txt".to_owned(),
                    text.clone(),
                    text.clone(),
                ]],
            ),
        )
        .unwrap();
        let index = import_efu(&path, "offline:clock", 1).unwrap();
        assert_eq!(export_rows(&index).1[0]["Date Modified"], text);
        if text == "116444735999999999" {
            assert_eq!(index.record(0).unwrap().modified, -1);
        }
    }
    for text in [
        i128::MIN.to_string(),
        "18446744073709551616".to_owned(),
        "-1".to_owned(),
        "nan".to_owned(),
        "9".repeat(1025),
    ] {
        fs::write(
            &path,
            csv_bytes(
                &["Filename", "Date Modified"],
                &[vec!["C:\\synthetic\\clock.txt".to_owned(), text]],
            ),
        )
        .unwrap();
        let result = std::panic::catch_unwind(|| import_efu(&path, "offline:clock", 1));
        assert!(result.is_ok(), "invalid FILETIME panicked");
        assert!(result.unwrap().is_err());
    }
}

#[test]
fn duplicate_headers_and_missing_or_invalid_filenames_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid.efu");
    for input in [
        "Filename,Filename\na,b\n",
        "Filename,filename\na,b\n",
        "Filename,Tag,tag\na,x,y\n",
        "Filename,\na,b\n",
        "Size\n1\n",
        "Filename\n\"\"\n",
        "Filename\n\"bad\0name\"\n",
        "Filename\nC:\\synthetic\\same.txt\nC:\\synthetic\\same.txt\n",
        "Filename\n\"unterminated\n",
    ] {
        fs::write(&path, input).unwrap();
        assert!(
            import_efu(&path, "offline:invalid", 1).is_err(),
            "accepted {input:?}"
        );
    }
}

#[test]
fn logical_csv_record_budget_counts_embedded_newlines_and_escaped_quotes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.efu");
    // A multiline field cannot evade the logical-record limit by inserting LF.
    let enormous = "\n\"x".repeat(750_000);
    fs::write(
        &path,
        csv_bytes(
            &["Filename", "Comment"],
            &[vec!["C:\\synthetic\\large.txt".to_owned(), enormous]],
        ),
    )
    .unwrap();
    let error = import_efu(&path, "offline:large", 1)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("invalid EFU CSV record"), "{error}");
    // The BOM and a quoted, multiline first header use the same bounded scanner.
    let mut bytes = vec![0xef, 0xbb, 0xbf];
    bytes.extend_from_slice(b"\"");
    bytes.extend("x\n".repeat(1_100_000).as_bytes());
    bytes.extend_from_slice(b"\",Filename\nvalue,C:\\synthetic\\a.txt\n");
    fs::write(&path, bytes).unwrap();
    assert!(import_efu(&path, "offline:large", 1).is_err());
}

#[test]
fn serialized_metadata_and_column_budgets_reject_amplified_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("budget.efu");
    // NUL has a short CSV encoding but a six-byte JSON encoding in the arena.
    fs::write(
        &path,
        csv_bytes(
            &["Filename", "Comment"],
            &[vec![
                "C:\\synthetic\\nulls.txt".to_owned(),
                "\0".repeat(200_000),
            ]],
        ),
    )
    .unwrap();
    assert!(import_efu(&path, "offline:budget", 1).is_err());
    let headers: Vec<String> = std::iter::once("Filename".to_owned())
        .chain((0..256).map(|n| format!("Extra{n}")))
        .collect();
    let header_refs: Vec<_> = headers.iter().map(String::as_str).collect();
    let mut row = vec!["C:\\synthetic\\columns.txt".to_owned()];
    row.extend((0..256).map(|_| String::new()));
    fs::write(&path, csv_bytes(&header_refs, &[row])).unwrap();
    assert!(import_efu(&path, "offline:budget", 1).is_err());
}

#[test]
fn invalid_efu_numbers_do_not_silently_become_zero() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numbers.efu");
    for (size, created, attributes) in [
        ("-1", "", ""),
        ("18446744073709551616", "", ""),
        ("1.5", "", ""),
        ("", "-1", ""),
        ("", "18446744073709551616", ""),
        ("", "", "4294967296"),
        ("", "", "0x100000000"),
        ("", "", "-1"),
    ] {
        fs::write(
            &path,
            csv_bytes(
                &["Filename", "Size", "Date Created", "Attributes"],
                &[vec![
                    "C:\\synthetic\\invalid.txt".to_owned(),
                    size.to_owned(),
                    created.to_owned(),
                    attributes.to_owned(),
                ]],
            ),
        )
        .unwrap();
        assert!(import_efu(&path, "offline:numbers", 1).is_err());
    }
}

#[test]
fn schema4_preserves_precision_nulls_extra_fields_and_filters_disabled_sources() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.db");
    {
        let db = legacy_fixture(&path);
        db.execute_batch("INSERT INTO filelists VALUES('efu:disabled','C:\\synthetic\\disabled.efu',0);
            INSERT INTO filelists VALUES('efu:enabled','C:\\synthetic\\enabled.efu',1);
            INSERT INTO files VALUES(1,'C:\\synthetic\\hidden.txt',1,1700000000,0,'efu:disabled','133444736000000001','133444736123456789',34,'{\"Project\":\"hidden\"}',NULL);
            INSERT INTO files VALUES(2,'C:\\synthetic\\visible.txt',2,1700000000,0,'efu:enabled','133444736000000002','133444736123456790',34,'{\"Project\":\"visible\",\"Blank\":null,\"Numeric\":1.2300e+2,\"Nested\":{\"x\":[1,true]}}',NULL);
            INSERT INTO files VALUES(3,'C:\\synthetic\\unknown.txt',NULL,NULL,0,'local',NULL,NULL,NULL,NULL,NULL);").unwrap();
    }
    let before = fs::read(&path).unwrap();
    let before_names = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let index = import_schema4(&path, 9).unwrap();
    assert_eq!(index.len(), 2);
    assert_eq!(
        index.record(0).unwrap().source,
        "offline:schema4:efu:enabled"
    );
    assert_eq!(index.record(1).unwrap().source, "offline:schema4:local");
    let (_, rows) = export_rows(&index);
    assert_eq!(rows[0]["Date Modified"], "133444736000000002");
    assert_eq!(rows[0]["Date Created"], "133444736123456790");
    assert_eq!(rows[0]["Attributes"], "34");
    assert_eq!(rows[0]["Project"], "visible");
    assert_eq!(rows[0]["Blank"], "");
    assert_eq!(rows[0]["Numeric"], "1.2300e+2");
    assert_eq!(rows[0]["Nested"], "{\"x\":[1,true]}");
    for column in ["Size", "Date Modified", "Date Created", "Attributes"] {
        assert_eq!(rows[1][column], "", "{column}");
    }
    assert_eq!(before, fs::read(&path).unwrap());
    assert_eq!(
        before_names,
        fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    );
}

#[test]
fn schema4_optional_columns_and_negative_fractional_dates_work() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("minimal.db");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA user_version=4; CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT,size INTEGER,mtime REAL,is_dir INTEGER,source TEXT);
            INSERT INTO files VALUES(1,'C:\\synthetic\\before-epoch.txt',0,-0.25,0,'local');").unwrap();
    }
    let index = import_schema4(&path, 1).unwrap();
    assert_eq!(index.record(0).unwrap().modified, -1);
    let row = &export_rows(&index).1[0];
    assert_eq!(row["Date Modified"], "116444735997500000");
    assert_eq!(row["Date Created"], "");
    assert_eq!(row["Attributes"], "");
}

#[test]
fn schema4_preserves_raw_calendar_dates_without_claiming_a_new_projection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("raw-date.db");
    {
        let db = legacy_fixture(&path);
        db.execute(
            "INSERT INTO files VALUES(1,?1,1,1700000000,0,'local',?2,?3,32,'{}',NULL)",
            params![
                "C:\\synthetic\\raw.txt",
                "2023-11-14T22:13:20.0000001Z",
                "2020-01-01"
            ],
        )
        .unwrap();
    }
    let index = import_schema4(&path, 1).unwrap();
    assert_eq!(index.record(0).unwrap().modified, 1_700_000_000);
    assert_eq!(
        export_rows(&index).1[0]["Date Modified"],
        "2023-11-14T22:13:20.0000001Z"
    );
    assert_eq!(export_rows(&index).1[0]["Date Created"], "2020-01-01");
}

#[test]
fn schema4_invalid_extra_json_and_overflow_fail_without_changing_source() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("invalid.db");
    let db = legacy_fixture(&path);
    db.execute_batch("INSERT INTO files VALUES(1,'C:\\synthetic\\bad.txt',1,1700000000,0,'local',NULL,NULL,32,'{}',NULL);").unwrap();
    for value in [
        "{\"X\":1,\"X\":2}",
        "{\"X\":1,\"x\":2}",
        "{\"Filename\":\"replacement\"}",
        "[]",
        "{\"a\":1,}",
        "{\"a\":1} trailing",
    ] {
        db.execute("UPDATE files SET extra_json=?1", [value])
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert!(import_schema4(&path, 1).is_err(), "accepted {value}");
        assert_eq!(before, fs::read(&path).unwrap());
    }
    db.execute(
        "UPDATE files SET extra_json='{}',mtime_filetime=?1",
        [i128::MIN.to_string()],
    )
    .unwrap();
    let result = std::panic::catch_unwind(|| import_schema4(&path, 1));
    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
}

#[test]
fn schema4_rejects_nonempty_wal_instead_of_reading_stale_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("wal.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA user_version=4;
        CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT,size INTEGER,mtime REAL,is_dir INTEGER,source TEXT);
        INSERT INTO files VALUES(1,'C:\\synthetic\\wal.txt',1,0,0,'local');").unwrap();
    let wal = directory.path().join("wal.db-wal");
    assert!(fs::metadata(&wal).unwrap().len() > 0);
    let before = fs::read(&wal).unwrap();
    assert!(import_schema4(&path, 1).is_err());
    assert_eq!(before, fs::read(&wal).unwrap());
}

#[test]
fn exporter_preflights_unrepresentable_dates_and_ambiguous_column_union() {
    let mut builder = IndexBuilder::default();
    let mut record = base_record("C:\\synthetic\\too-early.txt");
    record.modified = i64::MIN;
    builder.add(record).unwrap();
    let mut output = Vec::new();
    assert!(export_efu(&builder.finish(1).unwrap(), &mut output).is_err());
    assert!(output.is_empty());
    let mut builder = IndexBuilder::default();
    for (path, column) in [
        ("C:\\synthetic\\one.txt", "Tag"),
        ("C:\\synthetic\\two.txt", "tag"),
    ] {
        builder
            .add_with_metadata(
                base_record(path),
                Some(CatalogMetadata {
                    fields: BTreeMap::from([(column.to_owned(), "a".to_owned())]),
                }),
            )
            .unwrap();
    }
    assert!(export_efu(&builder.finish(1).unwrap(), &mut output).is_err());
    assert!(output.is_empty());
}

#[test]
fn directory_inference_respects_path_flavor_and_explicit_attributes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("path-flavor.efu");
    let cases = [
        ("/synthetic/posix-backslash-empty\\", "", false),
        ("/synthetic/posix-backslash-zero\\", "0", false),
        ("/synthetic/posix-directory/", "", true),
        ("C:\\synthetic\\explicit-directory", "16", true),
        ("C:\\synthetic\\inferred-directory\\", "", true),
        ("C:\\synthetic\\explicit-file\\", "0", false),
        ("\\\\unopened-server\\share\\directory\\", "", true),
    ];
    let rows = cases
        .iter()
        .map(|(name, attributes, _)| vec![(*name).to_owned(), (*attributes).to_owned()])
        .collect::<Vec<_>>();
    fs::write(&path, csv_bytes(&["Filename", "Attributes"], &rows)).unwrap();
    let index = import_efu(&path, "offline:path-flavor", 1).unwrap();
    for (id, (name, _, expected)) in cases.iter().enumerate() {
        let record = index.record(id as u32).unwrap();
        assert_eq!(record.path, *name);
        assert_eq!(record.is_dir, *expected, "{name:?}");
    }
}
