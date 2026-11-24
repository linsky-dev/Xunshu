#![no_main]

use libfuzzer_sys::fuzz_target;
use xs_query::{EntryRef, Query};

fuzz_target!(|bytes: &[u8]| {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    let query = match Query::parse_at(text, 1_710_057_600) {
        Ok(query) => query,
        Err(error) => {
            assert!(error.offset <= text.len());
            return;
        }
    };
    let entry = EntryRef {
        name: "预算计划_2024-03-10.pdf",
        path: "/synthetic/预算计划_2024-03-10.pdf",
        size: 512,
        modified: 1_710_057_600,
        is_dir: false,
        source: "offline:fuzz",
    };
    let _ = query.candidate_plan();
    assert_eq!(query.matches(&entry), query.matched_score(&entry).is_some());
});
