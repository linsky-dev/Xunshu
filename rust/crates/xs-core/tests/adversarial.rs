use xs_core::{FileRecord, IndexBuilder, SearchOptions};
use xs_query::{EntryRef, Query};

/// The oracle is the semantic evaluator over all records, independent of the
/// posting-list retrieval plan. A wrong plan must never turn a match into a miss.
#[test]
fn inverted_candidates_equal_semantic_full_scan() {
    let mut records = Vec::new();
    let names = [
        "干部报告",
        "寻枢设计",
        "budget_final",
        "ReadMe",
        "alpha beta",
        "annual",
        "项目计划",
        "İstanbul",
        "ＡＢＣ文件",
        "foo_bar",
        "重庆报告",
        "notes",
        "file[x]",
        "Straße",
    ];
    let mut b = IndexBuilder::default();
    for n in 0..350 {
        let r = FileRecord {
            path: format!(
                "/synthetic/{}/{n:04}_{}.{}",
                if n % 2 == 0 { "projects" } else { "archive" },
                names[n % names.len()],
                if n % 3 == 0 { "pdf" } else { "docx" }
            ),
            size: n as u64 * 1024,
            modified: 1700000000 + n as i64,
            is_dir: n % 17 == 0,
            source: if n % 5 == 0 { "offline:test" } else { "local" }.into(),
        };
        b.add(r.clone()).unwrap();
        records.push(r);
    }
    let index = b.finish(5).unwrap();
    let queries = [
        "",
        "budget",
        "干部",
        "报",
        "ＡＢＣ",
        "abc",
        "ReadMe",
        "ext:pdf",
        "file:",
        "folder:",
        "size:>100KB",
        "budget | 报告",
        "!budget",
        "(budget | notes) ext:docx",
        "path:projects ext:pdf",
        "name:annual",
        "parent:archive",
        "regex:^00[0-9]",
        "pinyin:cqbg",
        "pinyin:xssj",
        "fuzzy:bdgt",
        "*.pdf",
        "name:*foo*",
        "case:ReadMe",
        "source:local",
        "size:2KB..200KB",
        "dm:>=2023-11-14",
        "不存在",
        "!ext:pdf !folder:",
    ];
    for text in queries {
        let query = Query::parse(text).unwrap_or_else(|e| panic!("parse {text}: {e}"));
        let mut expected = records
            .iter()
            .filter(|r| {
                let name = xs_core::index::split_path(&r.path).unwrap().1;
                query.matches(&EntryRef {
                    name,
                    path: &r.path,
                    size: r.size,
                    modified: r.modified,
                    is_dir: r.is_dir,
                    source: &r.source,
                })
            })
            .map(|r| r.path.clone())
            .collect::<Vec<_>>();
        let response = index
            .search(
                text,
                &SearchOptions {
                    limit: 1000,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert!(response.complete, "{text}");
        let mut actual = response
            .results
            .into_iter()
            .map(|r| r.path)
            .collect::<Vec<_>>();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected, "candidate mismatch: {text}");
        assert_eq!(response.total, expected.len(), "count: {text}");
    }
}

#[test]
fn malicious_query_never_panics() {
    let mut b = IndexBuilder::default();
    b.add(FileRecord {
        path: "/fixture/a.txt".into(),
        size: 3,
        modified: 0,
        is_dir: false,
        source: "local".into(),
    })
    .unwrap();
    let index = b.finish(1).unwrap();
    for query in [
        "((",
        "regex:(a+)+",
        "regex:(?=x)",
        "size:9999999999999999999999999999999999999999999999999TB",
        "dm:9999-13-99",
        "unknown:value",
        "\0",
        "NOT",
        "name:\"unterminated",
        &"(".repeat(1000),
        &"a".repeat(9000),
    ] {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            index.search(query, &SearchOptions::default(), None)
        }));
        assert!(outcome.is_ok(), "panic for {query:?}");
    }
}

#[test]
fn frecency_influences_global_top_k_before_pagination() {
    let mut builder = IndexBuilder::default();
    for n in 0..300 {
        builder
            .add(FileRecord {
                path: format!("/synthetic/report_{n:04}.txt"),
                size: 1,
                modified: 0,
                is_dir: false,
                source: "local".into(),
            })
            .unwrap();
    }
    let index = builder.finish(1).unwrap();
    let options = SearchOptions {
        limit: 10,
        offset: 0,
        ..Default::default()
    };
    let tail = "/synthetic/report_0299.txt";
    let baseline = index.search("report", &options, None).unwrap();
    assert!(!baseline.results.iter().any(|hit| hit.path == tail));
    let mut boosts = std::collections::HashMap::new();
    boosts.insert(tail.to_string(), 2_000);
    let result = index
        .search_personalized("report", &options, None, &boosts)
        .unwrap();
    assert!(result.results.iter().any(|hit| hit.path == tail));
    assert_eq!(result.total, 300);
}
