use serde::Deserialize;
use xs_query::{
    CandidatePlan, EntryRef, MAX_DEPTH, MAX_QUERY_BYTES, MAX_REGEX_BYTES, Query, QueryMetadata,
    fold, path_depth,
};

#[test]
fn imported_unknown_metadata_does_not_become_a_value_under_negation() {
    let entry = EntryRef {
        name: "report.pdf",
        path: "D:/资料/报告/report.pdf",
        size: 0,
        modified: 0,
        is_dir: false,
        source: "offline:test",
    };
    let unknown = QueryMetadata {
        size_known: false,
        modified_known: false,
        ..QueryMetadata::default()
    };
    let known = QueryMetadata {
        attributes: Some(0x22),
        created: Some(1_735_689_600),
        ..QueryMetadata::default()
    };
    let matches = |text, metadata| {
        Query::parse(text)
            .unwrap()
            .matched_score_with_metadata(&entry, None, path_depth(entry.path), metadata)
            .is_some()
    };
    for query in [
        "size:0",
        "!size:>0",
        "!<size:1 | mtime:2025-01-01>",
        "!attrib:h",
    ] {
        assert!(!matches(query, unknown), "{query}");
    }
    assert!(matches("size:0 | report", unknown));
    assert!(matches("!<size:1 reportx>", unknown));
    assert!(matches("size:0 dc:>=2025-01-01 attrib:ah", known));
    assert!(!matches("attrib:0x24", known));
}

#[test]
fn directory_relations_respect_component_boundaries_and_groups() {
    let entry = EntryRef {
        name: "report.pdf",
        path: "D:\\资料\\报告\\report.pdf",
        size: 0,
        modified: 0,
        is_dir: false,
        source: "local",
    };
    for query in [
        r"ancestor:D:\资料 doc:",
        r"parent:D:/资料/报告",
        "ancestor-name:资料",
        "parent-name:报告",
        "!<audio: | pic:>",
    ] {
        assert!(Query::parse(query).unwrap().matches(&entry), "{query}");
    }
    for query in [
        r"ancestor:D:\资",
        r"parent:D:/资料",
        "parent-name:资料",
        "audio:",
        "!<doc: | pic:>",
    ] {
        assert!(!Query::parse(query).unwrap().matches(&entry), "{query}");
    }
}

#[derive(Deserialize)]
struct Record {
    id: u32,
    name: String,
    path: String,
    size: u64,
    modified: i64,
    is_dir: bool,
    source: String,
}
impl Record {
    fn as_ref(&self) -> EntryRef<'_> {
        EntryRef {
            name: &self.name,
            path: &self.path,
            size: self.size,
            modified: self.modified,
            is_dir: self.is_dir,
            source: &self.source,
        }
    }
}
#[derive(Deserialize)]
struct Case {
    query: String,
    expected: Vec<u32>,
}
#[derive(Deserialize)]
struct Golden {
    now_unix_utc: i64,
    records: Vec<Record>,
    cases: Vec<Case>,
    invalid_queries: Vec<String>,
}
fn golden() -> Golden {
    serde_json::from_str(include_str!("fixtures/query-golden-v1.json")).unwrap()
}
fn candidate_matches(plan: &CandidatePlan, folded_name: &str) -> bool {
    match plan {
        CandidatePlan::All => true,
        CandidatePlan::Literal(needle) => folded_name.contains(needle),
        CandidatePlan::Ext(exts) => folded_name
            .rsplit_once('.')
            .is_some_and(|(_, ext)| exts.iter().any(|candidate| candidate == ext)),
        CandidatePlan::Pinyin(needle) => {
            xs_query::pinyin_initials(folded_name).is_some_and(|init| init.contains(needle))
        }
        CandidatePlan::And(parts) => parts
            .iter()
            .all(|part| candidate_matches(part, folded_name)),
        CandidatePlan::Or(parts) => parts
            .iter()
            .any(|part| candidate_matches(part, folded_name)),
    }
}

#[test]
fn language_neutral_golden_cases_and_candidate_soundness() {
    let fixture = golden();
    for case in fixture.cases {
        let query = Query::parse_at(&case.query, fixture.now_unix_utc)
            .unwrap_or_else(|error| panic!("{}: {error}", case.query));
        let plan = query.candidate_plan();
        let matched = fixture
            .records
            .iter()
            .filter(|record| query.matches(&record.as_ref()))
            .map(|record| record.id)
            .collect::<Vec<_>>();
        assert_eq!(matched, case.expected, "{}", case.query);
        for record in &fixture.records {
            let entry = record.as_ref();
            assert_eq!(
                query.matches(&entry),
                query.matched_score(&entry).is_some(),
                "{}",
                case.query
            );
            if query.matches(&entry) {
                assert!(
                    candidate_matches(&plan, &fold(&record.name)),
                    "candidate lost result: query={} name={} plan={plan:?}",
                    case.query,
                    record.name
                );
                assert!(query.score(&entry) >= 0);
            } else {
                assert_eq!(query.score(&entry), -1);
            }
        }
    }
}

#[test]
fn malformed_queries_fail_explicitly() {
    let fixture = golden();
    for query in fixture.invalid_queries {
        assert!(
            Query::parse_at(&query, fixture.now_unix_utc).is_err(),
            "unexpected success: {query}"
        );
    }
}

#[test]
fn query_budgets_and_hostile_unicode_never_panic() {
    for query in [
        "a".repeat(MAX_QUERY_BYTES + 1),
        "!".repeat(MAX_DEPTH + 2),
        format!(
            "{}a{}",
            "(".repeat(MAX_DEPTH + 2),
            ")".repeat(MAX_DEPTH + 2)
        ),
        format!("regex:{}", "a".repeat(MAX_REGEX_BYTES + 1)),
        "\0".into(),
        "regex:\"(?:a{100000}){100000}\"".into(),
        "a ".repeat(300),
        "case:".repeat(30) + "a",
        "regex:a ".repeat(17),
    ] {
        let outcome = std::panic::catch_unwind(|| Query::parse(&query));
        assert!(outcome.is_ok(), "parser panicked");
        assert!(
            outcome.unwrap().is_err(),
            "budget did not reject query: {query}"
        );
    }
    for malformed in [
        "dm:😀😀😀😀😀",
        "size:🦀",
        "mtime:2026-11-08T😀:00:00Z",
        "foo:\u{202e}",
    ] {
        assert!(std::panic::catch_unwind(|| Query::parse(malformed)).is_ok());
    }
}

#[test]
fn candidate_plans_preserve_case_sensitive_combining_mark_matches() {
    let entry = EntryRef {
        name: "cafe\u{301}.txt",
        path: "/work/cafe\u{301}.txt",
        size: 0,
        modified: 0,
        is_dir: false,
        source: "local",
    };
    let query = Query::parse("case:e").unwrap();
    assert!(query.matches(&entry));
    assert_eq!(query.candidate_plan(), CandidatePlan::All);
    assert!(!fold(entry.name).contains('e'));
    assert_eq!(
        Query::parse("foo | path:bar").unwrap().candidate_plan(),
        CandidatePlan::All
    );
    assert_eq!(
        Query::parse("foo !bar").unwrap().candidate_plan(),
        CandidatePlan::Literal("foo".into())
    );
}

#[test]
fn text_ranking_is_deterministic_and_nonmatch_cannot_win() {
    let query = Query::parse("report").unwrap();
    let names = [
        "report",
        "report.pdf",
        "report_final.pdf",
        "annual_report.pdf",
        "myreport.pdf",
    ];
    let scores = names.map(|name| {
        query.score(&EntryRef {
            name,
            path: "/docs/file",
            size: 0,
            modified: 0,
            is_dir: false,
            source: "local",
        })
    });
    for pair in scores.windows(2) {
        assert!(pair[0] > pair[1], "{scores:?}");
    }
}

#[test]
fn punctuation_fuzz_smoke_and_soundness() {
    let fixture = golden();
    let atoms = [
        "foo",
        "报告",
        "\"foo bar\"",
        "(foo|bar)",
        "ext:pdf",
        "size:>10",
        "!file:",
        "pinyin:bg",
        "fuzzy:rpt",
        "case:Report",
        "path:C:/Docs",
        "regex:\"[a-z]+\"",
        "<foo bar>",
        "未知字段:值",
        "\"",
        "<>",
    ];
    for left in atoms {
        for right in atoms {
            for operation in [" ", "|", " AND ", " NOT "] {
                let text = format!("{left}{operation}{right}");
                let parsed =
                    std::panic::catch_unwind(|| Query::parse_at(&text, fixture.now_unix_utc));
                assert!(parsed.is_ok(), "{text}");
                if let Ok(query) = parsed.unwrap() {
                    let plan = query.candidate_plan();
                    for record in &fixture.records {
                        if query.matches(&record.as_ref()) {
                            assert!(
                                candidate_matches(&plan, &fold(&record.name)),
                                "{text}: {}",
                                record.name
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn explicit_name_does_not_turn_a_windows_style_name_into_a_path_search() {
    let query = Query::parse("name:C:\\file").unwrap();
    let entry = EntryRef {
        name: "C:\\file.txt",
        path: "/work/C:\\file.txt",
        size: 0,
        modified: 0,
        is_dir: false,
        source: "local",
    };
    assert!(query.matches(&entry));
}

#[test]
fn quoted_values_are_data_and_not_reinterpreted_as_query_syntax() {
    let entry = EntryRef {
        name: "file:foo!bar.txt",
        path: "/work/file:foo!bar.txt",
        size: 0,
        modified: 0,
        is_dir: true,
        source: "local",
    };
    for text in [
        "name:\"file:foo\"",
        "name:\"!bar\"",
        "regex:\"file:foo!bar\"",
        "name:\"file:foo\"|absent",
    ] {
        assert!(Query::parse(text).unwrap().matches(&entry), "{text}");
    }
    let plain = EntryRef {
        name: "baz.txt",
        is_dir: false,
        ..entry
    };
    assert!(Query::parse("regex:\"foo\"|baz").unwrap().matches(&plain));
}

#[test]
fn glob_normalizes_literal_runs_as_whole_unicode_sequences() {
    let entry = EntryRef {
        name: "café.txt",
        path: "/café.txt",
        size: 0,
        modified: 0,
        is_dir: false,
        source: "local",
    };
    for text in ["*cafe\u{301}*", "*café*", "caf?.txt"] {
        let query = Query::parse(text).unwrap();
        assert!(query.matches(&entry), "{text}");
        assert!(candidate_matches(
            &query.candidate_plan(),
            &fold(entry.name)
        ));
    }
}

#[test]
fn prepared_evaluation_is_identical_for_every_golden_query() {
    let fixture = golden();
    for case in fixture.cases {
        let query = Query::parse_at(&case.query, fixture.now_unix_utc).unwrap();
        for record in &fixture.records {
            let full = record.as_ref();
            let mut prepared = full;
            if !query.needs_path() {
                prepared.path = "";
            }
            let name = fold(full.name);
            let depth = xs_query::path_depth(full.path);
            assert_eq!(
                query.matched_score(&full),
                query.matched_score_prepared(&prepared, Some(&name), depth),
                "prepared {} {}",
                case.query,
                record.name
            );
            assert_eq!(
                query.matched_score(&full),
                query.matched_score_prepared(&prepared, None, depth),
                "unprepared {} {}",
                case.query,
                record.name
            );
        }
    }
}

#[test]
fn path_requirement_covers_nested_negative_and_implicit_paths() {
    for text in [
        "path:docs",
        "parent:/work",
        "!path:docs",
        "foo|(bar parent:/work)",
        "C:\\Docs\\",
        "\\Docs",
        "path:regex:\".*docs.*\"",
    ] {
        assert!(Query::parse(text).unwrap().needs_path(), "{text}");
    }
    for text in [
        "name:docs",
        "!foo",
        "size:>1MB ext:pdf",
        "pinyin:bg",
        "fuzzy:rpt",
        "source:local",
        "name:C:\\file",
        "regex:\"path:docs\"",
    ] {
        assert!(!Query::parse(text).unwrap().needs_path(), "{text}");
    }
    assert_eq!(xs_query::path_depth("/work/literal\\backslash.txt"), 2);
    assert_eq!(xs_query::path_depth("C:\\work\\file.txt"), 2);
}
