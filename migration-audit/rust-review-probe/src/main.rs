//! Independent synthetic review. This crate does not modify core/CLI source.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{fs, path::{Path, PathBuf}, sync::atomic::AtomicBool, time::Instant};
use xs_core::{FileRecord, Index, IndexBuilder, SearchOptions, operations, persistence::Store};
use xs_platform::{Scope, Settings};
use xs_query::{EntryRef, Query};

fn record(path: impl Into<String>, source: &str) -> FileRecord {
    serde_json::from_value(json!({"path":path.into(),"size":123,"modified":1700000000_i64,"is_dir":false,"source":source})).unwrap()
}
fn build(rows: &[FileRecord], token: String, generation: u64) -> Result<Index> {
    let mut b = IndexBuilder::new(token);
    for r in rows { b.add(r.clone())?; }
    b.finish(generation)
}
fn mutate_snapshot(input: &Path, output: &Path, mutate: impl FnOnce(&mut Value, &mut Vec<u8>)) -> Result<()> {
    let bytes = fs::read(input)?;
    let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let mut manifest: Value = serde_json::from_slice(&bytes[32..32+n])?;
    let mut payload = bytes[32+n..].to_vec();
    mutate(&mut manifest, &mut payload);
    let manifest = serde_json::to_vec(&manifest)?;
    let mut output_bytes = bytes[..32].to_vec();
    output_bytes[12..16].copy_from_slice(&(manifest.len() as u32).to_le_bytes());
    output_bytes[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    let mut crc = crc32fast::Hasher::new(); crc.update(&manifest); crc.update(&payload);
    output_bytes[24..28].copy_from_slice(&crc.finalize().to_le_bytes());
    output_bytes.extend(manifest); output_bytes.extend(payload);
    fs::write(output, output_bytes)?;
    Ok(())
}
fn summarize(index: Result<Index>, query: &str, limit: usize) -> Value {
    match index {
        Err(e) => json!({"load_ok":false,"error":format!("{e:#}")}),
        Ok(i) => match i.search(query, &SearchOptions{limit,..Default::default()}, None) {
            Err(e) => json!({"load_ok":true,"search_ok":false,"error":format!("{e:#}")}),
            Ok(r) => json!({"load_ok":true,"search_ok":true,"total":r.total,"complete":r.complete,"relation":r.total_relation,"results":r.results.len(),"first_path_bytes":r.results.first().map(|r|r.path.len()),"elapsed_ms":r.elapsed_ms}),
        }
    }
}
fn differential() -> Result<Value> {
    let names = ["abc", "ABC", "ＡＢＣ", "a\u{301}bc", "ábc", "Åbc", "A\u{30a}bc", "İstanbul", "Straße", "干部报告", "報告", "e\u{301}", "é", "foo．txt", "foo.txt", "abc[1]", "C:drive", "a\\b", "file\nname", "重庆", "foo_bar", "가", "가", "𠀀𠀁", "\u{fb00}oo"];
    let parents = ["/synthetic/", "C:\\synthetic\\", "//catalog/share/", "/synthetic/deep/a\\b/"];
    let mut records = Vec::new();
    for parent in parents { for name in names { for ext in ["", ".txt", ".PDF"] {
        records.push(record(format!("{parent}{name}{ext}"), "offline:review"));
    } } }
    let index = build(&records, String::new(), 17)?;
    let queries = ["", "abc", "ABC", "ＡＢＣ", "case:abc", "case:bc", "a*", "*abc*", "*．txt", "*.txt", "ext:txt", "name:ábc", "name:a\u{301}bc", "é", "case:e", "干部", "报", "報", "𠀀", "𠀀𠀁", "ffo", "foo | é", "foo | path:deep", "!foo", "(foo | é) !ext:pdf", "path:synthetic", "path:deep", "parent:/synthetic", "parent:C:\\synthetic", "pinyin:cq", "regex:foo", "fuzzy:abc", "size:>100", "source:offline", "folder:", "file:", "dm:2023-11-14"];
    let mut failures = Vec::new(); let mut checked = 0;
    for text in queries {
        let q = Query::parse(text).with_context(||text.to_owned())?;
        let mut expected: Vec<_> = records.iter().enumerate().filter_map(|(id,r)| {
            let name=xs_core::index::split_path(&r.path).unwrap().1;
            q.matched_score(&EntryRef{name,path:&r.path,size:r.size,modified:r.modified,is_dir:r.is_dir,source:&r.source}).map(|score|(id as u32,score))
        }).collect();
        expected.sort_unstable_by(|a,b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for offset in [0,1,7,99,300] {
            let response=index.search(text,&SearchOptions{limit:11,offset,time_budget_ms:60000},None)?;
            let actual:Vec<_>=response.results.iter().map(|r|(r.id,r.score)).collect();
            let desired:Vec<_>=expected.iter().copied().skip(offset).take(11).collect();
            checked+=1;
            if !response.complete || response.total != expected.len() || actual!=desired {
                failures.push(json!({"query":text,"offset":offset,"expected_total":expected.len(),"actual_total":response.total,"complete":response.complete,"expected":desired,"actual":actual}));
            }
        }
    }
    Ok(json!({"records":records.len(),"comparisons":checked,"failures":failures}))
}
fn corruption(base:&Path)->Result<Value> {
    let original=base.join("original.xsi");
    build(&[record("/synthetic/ABC.txt","offline:review")],String::new(),1)?.write_snapshot(&original)?;
    let flags=base.join("bad-fold-flag.xsi");
    mutate_snapshot(&original,&flags,|m,p| {
        let start=m["records"]["start"].as_u64().unwrap() as usize;
        p[start+32..start+36].copy_from_slice(&2u32.to_le_bytes());
    })?;
    let parent=base.join("oversize-parent.xsi");
    mutate_snapshot(&original,&parent,|m,_|{m["parents"][0]=json!(format!("/synthetic/{}/","a".repeat(262144)));})?;
    let grams=base.join("empty-postings.xsi");
    mutate_snapshot(&original,&grams,|m,p|{
        let start=m["grams"]["start"].as_u64().unwrap() as usize;
        let len=m["grams"]["len"].as_u64().unwrap() as usize;
        for at in (start..start+len).step_by(24) {p[at+16..at+24].fill(0);}
    })?;
    let truncated=base.join("truncated-postings.xsi");
    mutate_snapshot(&original,&truncated,|m,p|{
        let start=m["postings"]["start"].as_u64().unwrap() as usize;
        p[start..].fill(128);
    })?;
    Ok(json!({"baseline":summarize(Index::open_snapshot(original),"abc",10),"wrong_normalized_flag":summarize(Index::open_snapshot(flags),"abc",10),"oversize_parent":summarize(Index::open_snapshot(parent),"abc",10),"zero_length_postings":summarize(Index::open_snapshot(grams),"abc",10),"unterminated_postings":summarize(Index::open_snapshot(truncated),"abc",10)}))
}
fn cancellation()->Result<Value>{
    let start=Instant::now();let mut builder=IndexBuilder::default();
    for id in 0..25000 {builder.add(record(format!("/synthetic/common_{id:06}.txt"),"offline:review"))?;}
    let index=builder.finish(1)?;let build_ms=start.elapsed().as_secs_f64()*1000.;
    let repeated=std::iter::repeat_n("common",128).collect::<Vec<_>>().join(" | ");
    let cancel=AtomicBool::new(true);
    let cancelled=index.search(&repeated,&SearchOptions{time_budget_ms:1,..Default::default()},Some(&cancel))?;
    let timeout=index.search(&repeated,&SearchOptions{time_budget_ms:1,..Default::default()},None)?;
    Ok(json!({"records":index.len(),"or_branches":128,"build_ms":build_ms,"pre_cancelled":{"elapsed_ms":cancelled.elapsed_ms,"examined":cancelled.examined,"complete":cancelled.complete},"one_ms_budget":{"elapsed_ms":timeout.elapsed_ms,"examined":timeout.examined,"complete":timeout.complete}}))
}
fn scope_invariants(base:&Path)->Result<Value>{
    let allowed=base.join("allowed");let outside=base.join("outside");fs::create_dir_all(&allowed)?;fs::create_dir_all(&outside)?;
    let data=base.join("scope-data");
    let mut settings=Settings::default();settings.roots=vec![Scope::new(allowed.to_str().unwrap())];settings.complete_setup();
    operations::save_settings(&data,&settings)?;
    let file=outside.join("outside-scope-probe.txt");fs::write(&file,"synthetic")?;
    let row=record(file.to_str().unwrap(),"local");
    let store=Store::open(data.join("index"))?;store.publish(&build(&[row.clone()],String::new(),1)?)?;
    let open=operations::authorize_open(&settings,&row).map(|p|p.display().to_string()).map_err(|e|format!("{e:#}"));
    let missing=record(outside.join("missing-scope-probe.txt").to_str().unwrap(),"local");
    let missing_open=operations::authorize_open(&settings,&missing).map(|p|p.display().to_string()).map_err(|e|format!("{e:#}"));
    Ok(json!({"empty_scope_token_with_local_record":summarize(operations::load_index(&data),"",10),"outside_scope_open":open,"missing_outside_scope_open":missing_open,"outside_fixture":file}))
}
fn overflow(base:&Path)->Result<Value>{
    let path=base.join("i128-min.efu");fs::write(&path,"Filename,Date Modified\nC:\\synthetic\\a.txt,-170141183460469231731687303715884105728\n")?;
    let old=std::panic::take_hook();std::panic::set_hook(Box::new(|_|{}));
    let outcome=std::panic::catch_unwind(||xs_core::interchange::import_efu(&path,"offline:review",1));
    std::panic::set_hook(old);
    Ok(match outcome{Err(_)=>json!({"panicked":true}),Ok(Err(e))=>json!({"panicked":false,"error":format!("{e:#}")}),Ok(Ok(i))=>json!({"panicked":false,"accepted":i.len()})})
}
fn main()->Result<()> {
    let stamp=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let base=PathBuf::from("/tmp/xunshu-rust-validation-036c199d3807").join(format!("rust-review-probe-{}-{stamp}",std::process::id()));
    fs::create_dir_all(&base)?;
    let only=std::env::args().nth(1).unwrap_or_default();
    let report=if only=="scope" {json!({"fixture_dir":base,"scope":scope_invariants(&base)?})}
    else if only=="core" {json!({"fixture_dir":base,"phase":"core_only","differential":differential()?,"corruption":corruption(&base)?,"cancellation":cancellation()?,"scope":scope_invariants(&base)?})}
    else {json!({"fixture_dir":base,"differential":differential()?,"corruption":corruption(&base)?,"cancellation":cancellation()?,"scope":scope_invariants(&base)?,"efu_overflow":overflow(&base)?})};
    println!("{}",serde_json::to_string_pretty(&report)?);Ok(())
}
