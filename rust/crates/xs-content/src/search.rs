//! SQLite FTS5 的有界正文查询。分页、精确索引计数和当前文件验证分开记录。

use crate::{load_db, semantic, snapshot::OpenSnapshot, Policy, WorkerOptions, CONTENT_DB};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BinaryHeap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;
use xs_platform::FileRevision;

const MAX_PAGE: usize = 200;
const MAX_QUERY_BYTES: usize = 1024;
const MAX_QUERY_TERMS: usize = 128;
// One bounded ranking prefix per reader. Only IDs/scores are retained, not text.
const MAX_LEXICAL_CACHE_RESULTS: usize = 16_384;
// 当前语义实现为精确线性扫描，预算超限时明确报错，绝不静默截断而谎报全库搜索。
const MAX_SEMANTIC_DOT_PRODUCTS: usize = 200_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Lexical,
    Semantic,
    Hybrid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchOptions {
    pub offset: usize,
    pub limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 40,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Highlight {
    /// 相对于 snippet 的 UTF-8 字节偏移，右边界不包含。
    pub start: usize,
    pub end: usize,
}

/// 可解释检索证据：仅描述确实参与排序的通道，不把相似度冒充确定性匹配。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchEvidence {
    #[default]
    Lexical,
    Semantic,
    LexicalAndSemantic,
}

impl MatchEvidence {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lexical => "正文关键词命中",
            Self::Semantic => "离线语义相似",
            Self::LexicalAndSemantic => "关键词 + 语义共同命中",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContentHit {
    pub path: String,
    pub location: crate::Location,
    pub snippet: String,
    pub highlights: Vec<Highlight>,
    pub score: f64,
    #[serde(default)]
    pub evidence: MatchEvidence,
    pub(crate) revision: FileRevision,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchResponse {
    pub generation: u64,
    pub mode: SearchMode,
    /// 当前快照内的匹配文档数。同一路径有多个匹配片段时只计一次。
    pub indexed_total: usize,
    /// 当前页中，因文件被替换、修改或失去授权而被省略的条目数。
    pub omitted_stale: usize,
    pub hits: Vec<ContentHit>,
    pub(crate) scope_token: String,
    pub(crate) settings_revision: FileRevision,
    pub(crate) catalog_revision: FileRevision,
    #[serde(default)]
    pub(crate) catalog_base_revision: Option<(u8, FileRevision)>,
}

struct Cached {
    revision: FileRevision,
    base_revision: Option<(u8, FileRevision)>,
    generation: u64,
    scope_token: String,
    model: Option<String>,
    db: Connection,
}

pub struct ContentReader {
    dir: PathBuf,
    cached: Option<Cached>,
    model: Option<semantic::ModelClient>,
    lexical_cache: Option<LexicalCache>,
}

impl ContentReader {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.into(),
            cached: None,
            model: None,
            lexical_cache: None,
        }
    }

    pub fn release(&mut self) {
        self.cached = None;
        self.model = None;
        self.lexical_cache = None;
    }

    pub fn search(
        &mut self,
        query: &str,
        options: &SearchOptions,
        cancel: &AtomicBool,
    ) -> Result<SearchResponse> {
        self.search_mode(query, SearchMode::Lexical, options, cancel)
    }

    pub fn search_mode(
        &mut self,
        query: &str,
        mode: SearchMode,
        options: &SearchOptions,
        cancel: &AtomicBool,
    ) -> Result<SearchResponse> {
        ensure!(!cancel.load(Ordering::Acquire), "正文查询已取消");
        // SQLite 的安全回调需要独立所有权。外部借用标志在查询存续期转发。
        let shared = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&shared);
        thread::scope(|scope| {
            let (done, wait) = mpsc::sync_channel::<()>(1);
            scope.spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) =
                    wait.recv_timeout(Duration::from_millis(2))
                {
                    if cancel.load(Ordering::Acquire) {
                        signal.store(true, Ordering::Release);
                        break;
                    }
                }
            });
            let result = self.search_shared(query, mode, options, shared);
            let _ = done.send(());
            ensure!(!cancel.load(Ordering::Acquire), "正文查询已取消");
            result
        })
    }

    pub(crate) fn search_shared(
        &mut self,
        query: &str,
        mode: SearchMode,
        options: &SearchOptions,
        cancel: Arc<AtomicBool>,
    ) -> Result<SearchResponse> {
        ensure!(!cancel.load(Ordering::Acquire), "正文查询已取消");
        ensure!(
            (1..=MAX_PAGE).contains(&options.limit) && options.offset <= usize::MAX - MAX_PAGE,
            "正文分页大小应为 1–200"
        );
        let terms = Terms::parse(query)?;
        let result = self.search_current(query, &terms, mode, options, &cancel);
        if result.is_err() && !cancel.load(Ordering::Acquire) {
            self.release();
        }
        ensure!(!cancel.load(Ordering::Acquire), "正文查询已取消");
        result
    }

    fn search_current(
        &mut self,
        query: &str,
        terms: &Terms,
        mode: SearchMode,
        options: &SearchOptions,
        cancel: &Arc<AtomicBool>,
    ) -> Result<SearchResponse> {
        let policy = Policy::load(&self.dir)?;
        let path = self.dir.join(CONTENT_DB);
        let snapshot = OpenSnapshot::open(&path)?;
        let stamp = snapshot.stamp;
        let revision = stamp.publication;
        if !self
            .cached
            .as_ref()
            .is_some_and(|db| db.revision == revision && db.base_revision == stamp.base)
        {
            self.release();
            let loaded = load_db(snapshot, cancel, false)?;
            self.cached = Some(Cached {
                revision,
                base_revision: stamp.base,
                generation: loaded.generation,
                scope_token: loaded.scope,
                model: loaded.model,
                db: loaded.connection,
            });
        }
        let catalog = self.cached.as_mut().expect("正文库已加载");
        ensure!(
            catalog.scope_token == policy.token,
            "正文范围或检索功能开关已更改，请重建正文索引"
        );
        let vectors = if matches!(mode, SearchMode::Semantic | SearchMode::Hybrid) {
            ensure!(policy.settings.semantic_enabled, "语义搜索尚未启用");
            let fingerprint = catalog
                .model
                .as_deref()
                .context("正文库缺少语义向量，请重建")?;
            if self.model.is_none() {
                let executable = semantic::adjacent_to(&std::env::current_exe()?);
                self.model = Some(semantic::ModelClient::start(
                    &executable,
                    &WorkerOptions::default(),
                    cancel,
                )?);
            }
            let model = self.model.as_mut().context("语义进程未就绪")?;
            ensure!(
                model.fingerprint() == fingerprint,
                "语义模型已更新，请重建正文库"
            );
            match model.embed(query, cancel) {
                Ok(vectors) => {
                    ensure!(!vectors.is_empty(), "查询未产生可用的语义向量");
                    vectors
                }
                Err(error) => {
                    self.model = None;
                    return Err(error);
                }
            }
        } else {
            Vec::new()
        };
        policy.verify(&self.dir)?;
        let signal = Arc::clone(cancel);
        catalog
            .db
            .progress_handler(1000, Some(move || signal.load(Ordering::Acquire)))?;
        let search = (|| {
            let (total, ranked) = match mode {
                SearchMode::Lexical => {
                    cached_lexical_page(catalog, terms, options, cancel, &mut self.lexical_cache)?
                }
                SearchMode::Semantic => semantic_page(catalog, &vectors, options, cancel)?,
                SearchMode::Hybrid => {
                    // 两路必须先得到完整的文档排名再融合，不能用单页候选估计总数：
                    // 否则翻页超过 200 条会出现假空页，也会丢失深页中的双路命中。
                    let (_, lex) = lexical_ranked(catalog, terms, None, cancel)?;
                    let sem = semantic_ranked(catalog, &vectors, cancel)?;
                    rrf_fuse(&lex, &sem, options)
                }
            };
            let source = Ranking {
                total,
                ranked,
                mode,
            };
            collect_hits(catalog, terms, source, &policy, &self.dir, cancel)
        })();
        let reset = catalog.db.progress_handler(0, None::<fn() -> bool>);
        let response = search?;
        reset?;
        policy.verify(&self.dir)?;
        stamp.verify(&path)?;
        Ok(response)
    }
}

#[derive(Clone, Copy, Debug)]
struct Ranked {
    doc: i64,
    segment: i64,
    score: f64,
    window: Option<(usize, usize)>,
    evidence: MatchEvidence,
}

struct Ranking {
    total: usize,
    ranked: Vec<Ranked>,
    mode: SearchMode,
}

fn page_ranked(ranked: Vec<Ranked>, options: &SearchOptions) -> (usize, Vec<Ranked>) {
    let total = ranked.len();
    let page = ranked
        .into_iter()
        .skip(options.offset)
        .take(options.limit)
        .collect();
    (total, page)
}

/// 对每份文档只保留 BM25 最强命中段落；无页数截断。
fn lexical_ranked(
    catalog: &Cached,
    terms: &Terms,
    page: Option<&SearchOptions>,
    cancel: &AtomicBool,
) -> Result<(usize, Vec<Ranked>)> {
    let mut rank = catalog.db.prepare_cached(
        "SELECT s.doc, s.id, bm25(terms) AS score
         FROM terms JOIN segments s ON s.id=terms.rowid
         WHERE terms MATCH ?1
         ORDER BY score, s.doc, s.id",
    )?;
    let mut rows = rank.query([&terms.expression])?;
    let mut docs = HashSet::new();
    let mut ranked = Vec::new();
    while let Some(row) = rows.next()? {
        ensure!(!cancel.load(Ordering::Acquire), "正文查询已取消");
        let doc: i64 = row.get(0)?;
        if docs.insert(doc) {
            let rank = docs.len() - 1;
            if page.is_none_or(|options| {
                rank >= options.offset && rank < options.offset + options.limit
            }) {
                ranked.push(Ranked {
                    doc,
                    segment: row.get(1)?,
                    score: -row.get::<_, f64>(2)?,
                    window: None,
                    evidence: MatchEvidence::Lexical,
                });
            }
        }
    }
    Ok((docs.len(), ranked))
}

fn lexical_page(
    catalog: &Cached,
    terms: &Terms,
    options: &SearchOptions,
    cancel: &AtomicBool,
) -> Result<(usize, Vec<Ranked>)> {
    lexical_ranked(catalog, terms, Some(options), cancel)
}

struct LexicalCache {
    expression: String,
    total: usize,
    prefix: Vec<Ranked>,
}

impl LexicalCache {
    fn page(&self, expression: &str, options: &SearchOptions) -> Option<(usize, Vec<Ranked>)> {
        if self.expression != expression {
            return None;
        }
        if options.offset >= self.total {
            return Some((self.total, Vec::new()));
        }
        let end = options.offset.saturating_add(options.limit).min(self.total);
        if end > self.prefix.len() {
            return None;
        }
        Some((self.total, self.prefix[options.offset..end].to_vec()))
    }
}

fn cached_lexical_page(
    catalog: &Cached,
    terms: &Terms,
    options: &SearchOptions,
    cancel: &AtomicBool,
    cache: &mut Option<LexicalCache>,
) -> Result<(usize, Vec<Ranked>)> {
    ensure!(!cancel.load(Ordering::Acquire), "正文查询已取消");
    if let Some(hit) = cache
        .as_ref()
        .and_then(|item| item.page(&terms.expression, options))
    {
        return Ok(hit);
    }
    // Very deep pages should not allocate a large prefix that cannot serve them.
    if options.offset.saturating_add(options.limit) > MAX_LEXICAL_CACHE_RESULTS {
        return lexical_page(catalog, terms, options, cancel);
    }
    let (total, prefix) = lexical_page(
        catalog,
        terms,
        &SearchOptions {
            offset: 0,
            limit: MAX_LEXICAL_CACHE_RESULTS,
        },
        cancel,
    )?;
    let next = LexicalCache {
        expression: terms.expression.clone(),
        total,
        prefix,
    };
    let page = next
        .page(&terms.expression, options)
        .context("正文排名缓存边界不一致")?;
    *cache = Some(next);
    Ok(page)
}

/// 每份文档仅提交最佳语义窗口；流式处理避免对所有原始窗口排序。
fn semantic_document_winners(
    catalog: &Cached,
    vectors: &[semantic::Window],
    cancel: &AtomicBool,
    mut visit: impl FnMut(Ranked) -> Result<()>,
) -> Result<()> {
    let total_vectors: i64 = catalog
        .db
        .query_row("SELECT count(*) FROM vectors", [], |row| row.get(0))?;
    check_semantic_budget(usize::try_from(total_vectors)?, vectors.len())?;
    let mut query = catalog.db.prepare_cached(
        "SELECT doc, segment, start, end, embedding FROM vectors ORDER BY doc,segment,start",
    )?;
    let mut rows = query.query([])?;
    let mut current: Option<Ranked> = None;
    while let Some(row) = rows.next()? {
        ensure!(!cancel.load(Ordering::Acquire), "语义查询已取消");
        let ranked = Ranked {
            doc: row.get(0)?,
            segment: row.get(1)?,
            score: semantic::cosine(row.get_ref(4)?.as_blob()?, vectors)?,
            window: Some((row.get(2)?, row.get(3)?)),
            evidence: MatchEvidence::Semantic,
        };
        if let Some(best) = current.as_mut() {
            if ranked.doc == best.doc {
                if ranked.score > best.score {
                    *best = ranked;
                }
                continue;
            }
        }
        if let Some(previous) = current.replace(ranked) {
            visit(previous)?;
        }
    }
    if let Some(previous) = current {
        visit(previous)?;
    }
    Ok(())
}

fn check_semantic_budget(rows: usize, query_windows: usize) -> Result<()> {
    ensure!(query_windows > 0, "语义查询没有有效向量");
    ensure!(rows <= MAX_SEMANTIC_DOT_PRODUCTS / query_windows,
        "当前索引的语义向量扫描超过离线资源预算，请改用关键词搜索或缩小授权范围；不会返回不完整的语义结果");
    Ok(())
}

#[cfg(test)]
mod semantic_budget_tests {
    use super::*;

    #[test]
    fn semantic_budget_never_silently_truncates() {
        assert!(check_semantic_budget(200_000, 1).is_ok());
        assert!(check_semantic_budget(100_000, 2).is_ok());
        assert!(check_semantic_budget(100_001, 2).is_err());
        assert!(check_semantic_budget(200_001, 1).is_err());
        assert!(check_semantic_budget(0, 0).is_err());
        assert!(check_semantic_budget(1, usize::MAX).is_err());
    }
}

/// 大数据量时的文档质量序：高相似度优先，同分以文档 ID 升序稳定排序。
#[derive(Clone, Copy, Debug)]
struct SemanticBest(Ranked);

impl PartialEq for SemanticBest {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for SemanticBest {}
impl PartialOrd for SemanticBest {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SemanticBest {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .score
            .total_cmp(&other.0.score)
            .then_with(|| other.0.doc.cmp(&self.0.doc))
    }
}

fn semantic_ranked(
    catalog: &Cached,
    vectors: &[semantic::Window],
    cancel: &AtomicBool,
) -> Result<Vec<Ranked>> {
    let mut winners = Vec::new();
    semantic_document_winners(catalog, vectors, cancel, |winner| {
        winners.push(winner);
        Ok(())
    })?;
    winners.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then(left.doc.cmp(&right.doc))
    });
    Ok(winners)
}

/// 单独语义模式不再对全库文档做排序：仅保存 offset+limit 个最佳文档。
/// 精确匹配文档总数仍完整计数，深页语义不会产生虚假的空页。
fn semantic_page(
    catalog: &Cached,
    vectors: &[semantic::Window],
    options: &SearchOptions,
    cancel: &AtomicBool,
) -> Result<(usize, Vec<Ranked>)> {
    let capacity = options.offset.saturating_add(options.limit);
    let mut heap = BinaryHeap::<std::cmp::Reverse<SemanticBest>>::new();
    let mut total = 0usize;
    semantic_document_winners(catalog, vectors, cancel, |winner| {
        total = total.checked_add(1).context("语义文档计数溢出")?;
        if capacity == 0 {
            return Ok(());
        }
        let item = SemanticBest(winner);
        if heap.len() < capacity {
            heap.push(std::cmp::Reverse(item));
        } else if heap.peek().is_some_and(|worst| item > worst.0) {
            heap.pop();
            heap.push(std::cmp::Reverse(item));
        }
        Ok(())
    })?;
    let mut page: Vec<_> = heap.into_iter().map(|entry| entry.0 .0).collect();
    page.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then(left.doc.cmp(&right.doc))
    });
    Ok((total, page.into_iter().skip(options.offset).collect()))
}

fn collect_hits(
    catalog: &Cached,
    terms: &Terms,
    ranking: Ranking,
    policy: &Policy,
    dir: &Path,
    cancel: &AtomicBool,
) -> Result<SearchResponse> {
    let mut response = SearchResponse {
        generation: catalog.generation,
        mode: ranking.mode,
        indexed_total: ranking.total,
        omitted_stale: 0,
        hits: Vec::with_capacity(ranking.ranked.len()),
        scope_token: catalog.scope_token.clone(),
        settings_revision: policy.revision,
        catalog_revision: catalog.revision,
        catalog_base_revision: catalog.base_revision,
    };
    let mut details = catalog.db.prepare_cached(
        "SELECT d.path, d.revision, s.location, s.body
         FROM segments s JOIN documents d ON d.id=s.doc
         WHERE s.id=?1",
    )?;
    for ranked in ranking.ranked {
        ensure!(!cancel.load(Ordering::Acquire), "正文查询已取消");
        let (path, revision, location, body): (String, String, String, String) = details
            .query_row(params![ranked.segment], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
        let revision = serde_json::from_str(&revision)?;
        let file = Path::new(&path);
        if !policy.allows(dir, file)? || FileRevision::at_path(file).ok() != Some(revision) {
            response.omitted_stale += 1;
            continue;
        }
        let body = match ranked.window {
            Some((start, end)) => body.get(start..end).context("正文库向量分窗位置无效")?,
            None => &body,
        };
        let (snippet, highlights) = terms.snippet(body);
        response.hits.push(ContentHit {
            path,
            location: serde_json::from_str(&location)?,
            snippet,
            highlights,
            score: ranked.score,
            evidence: ranked.evidence,
            revision,
        });
    }
    Ok(response)
}

struct Token {
    value: String,
    start: usize,
    end: usize,
}

fn is_han(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}'
        | '\u{20000}'..='\u{2ffff}' | '\u{30000}'..='\u{323af}')
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut result = Vec::new();
    let mut current: Option<Token> = None;
    for (start, grapheme) in text.grapheme_indices(true) {
        let end = start + grapheme.len();
        for ch in grapheme.nfkc().flat_map(char::to_lowercase) {
            if is_han(ch)
                || !(ch.is_alphanumeric() || unicode_normalization::char::is_combining_mark(ch))
            {
                result.extend(current.take());
                if is_han(ch) {
                    result.push(Token {
                        value: ch.to_string(),
                        start,
                        end,
                    });
                }
            } else {
                let token = current.get_or_insert_with(|| Token {
                    value: String::new(),
                    start,
                    end,
                });
                token.value.push(ch);
                token.end = end;
            }
        }
    }
    result.extend(current);
    result
}

pub(crate) fn index_text(text: &str) -> String {
    tokenize(text)
        .into_iter()
        .map(|token| token.value)
        .collect::<Vec<_>>()
        .join(" ")
}

struct Terms {
    groups: Vec<Vec<String>>,
    expression: String,
}

impl Terms {
    fn parse(query: &str) -> Result<Self> {
        ensure!(query.len() <= MAX_QUERY_BYTES, "正文查询超出长度限制");
        let groups: Vec<Vec<String>> = query
            .split_whitespace()
            .map(|word| {
                tokenize(word)
                    .into_iter()
                    .map(|token| token.value)
                    .collect::<Vec<_>>()
            })
            .filter(|tokens| !tokens.is_empty())
            .collect();
        let count: usize = groups.iter().map(Vec::len).sum();
        ensure!(
            (1..=MAX_QUERY_TERMS).contains(&count),
            "正文查询应包含 1–128 个词项"
        );
        // 输入经词项拆分后只剩字母、数字和组合符，参数绑定同时隔离 SQL。
        let expression = groups
            .iter()
            .map(|tokens| format!("\"{}\"", tokens.join(" ")))
            .collect::<Vec<_>>()
            .join(" AND ");
        Ok(Self { groups, expression })
    }

    fn snippet(&self, body: &str) -> (String, Vec<Highlight>) {
        let tokens = tokenize(body);
        let mut spans = Vec::new();
        for group in &self.groups {
            for slice in tokens.windows(group.len()) {
                if slice
                    .iter()
                    .zip(group)
                    .all(|(token, term)| &token.value == term)
                {
                    spans.push((slice[0].start, slice[slice.len() - 1].end));
                }
            }
        }
        spans.sort_unstable();
        let (first_start, first_end) = spans.first().copied().unwrap_or_default();
        let start = body[..first_start]
            .char_indices()
            .rev()
            .nth(63)
            .map_or(0, |(at, _)| at);
        let end = body[first_end..]
            .char_indices()
            .nth(192)
            .map_or(body.len(), |(at, _)| at + first_end);
        let mut highlights: Vec<Highlight> = Vec::new();
        for (left, right) in spans {
            if left >= start && right <= end {
                let left = left - start;
                let right = right - start;
                if let Some(last) = highlights.last_mut().filter(|last| left <= last.end) {
                    last.end = last.end.max(right);
                } else {
                    highlights.push(Highlight {
                        start: left,
                        end: right,
                    });
                }
            }
        }
        (body[start..end].to_owned(), highlights)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xs_platform::{FileIdentity, FileTimestamp};

    #[test]
    fn semantic_ranking_considers_last_document_and_every_window() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE vectors(doc INTEGER,segment INTEGER,start INTEGER,end INTEGER,embedding BLOB,
                                  PRIMARY KEY(doc, segment, start)) WITHOUT ROWID;",
        ).unwrap();
        let mut weak = vec![0.0; semantic::DIMENSION];
        weak[0] = 1.0;
        let mut strong = vec![0.0; semantic::DIMENSION];
        strong[1] = 1.0;
        let query = semantic::Window {
            start: 0,
            end: 1,
            vector: strong,
        };
        let weak = semantic::Window {
            start: 0,
            end: 1,
            vector: weak,
        }
        .bytes();
        let strong = query.bytes();
        let tx = db.transaction().unwrap();
        {
            let mut insert = tx
                .prepare("INSERT INTO vectors VALUES (?1,?1,?2,?3,?4)")
                .unwrap();
            for doc in 1..=20_001 {
                insert.execute(params![doc, 0, 1, &weak]).unwrap();
            }
            insert.execute(params![20_001, 1, 2, &strong]).unwrap();
        }
        tx.commit().unwrap();
        let stamp = FileTimestamp {
            seconds: 0,
            nanoseconds: 0,
        };
        let catalog = Cached {
            revision: FileRevision {
                identity: FileIdentity {
                    volume: 0,
                    object: [0; 16],
                },
                size: 0,
                modified: stamp,
                changed: stamp,
            },
            base_revision: None,
            generation: 1,
            scope_token: String::new(),
            model: None,
            db,
        };
        let (count, page) = semantic_page(
            &catalog,
            &[query],
            &SearchOptions {
                limit: 1,
                offset: 0,
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(count, 20_001);
        assert_eq!(page[0].doc, 20_001);
        assert_eq!(page[0].segment, 20_001);
        assert_eq!(page[0].window, Some((1, 2)));
        assert_eq!(page[0].score, 1.0);
    }
}

#[cfg(test)]
mod semantic_topk_tests {
    use super::*;
    use xs_platform::{FileIdentity, FileTimestamp};

    #[test]
    fn top_k_matches_full_sort_with_ties_and_multiple_windows() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE vectors(doc INTEGER,segment INTEGER,start INTEGER,end INTEGER,embedding BLOB,PRIMARY KEY(doc,segment,start)) WITHOUT ROWID;").unwrap();
        let mut query_vector = vec![0.0f32; semantic::DIMENSION];
        query_vector[0] = 1.0;
        let query = semantic::Window {
            start: 0,
            end: 1,
            vector: query_vector,
        };
        let mut weak = vec![0.0f32; semantic::DIMENSION];
        weak[1] = 1.0;
        let weak = semantic::Window {
            start: 0,
            end: 1,
            vector: weak,
        }
        .bytes();
        let strong = query.bytes();
        let tx = db.transaction().unwrap();
        {
            let mut insert = tx
                .prepare("INSERT INTO vectors VALUES (?1, ?2, ?3, ?4, ?5)")
                .unwrap();
            for doc in 1..=500 {
                insert.execute(params![doc, doc * 10, 0, 1, &weak]).unwrap();
                if doc % 7 == 0 {
                    insert
                        .execute(params![doc, doc * 10 + 1, 1, 2, &strong])
                        .unwrap();
                }
            }
        }
        tx.commit().unwrap();
        let stamp = FileTimestamp {
            seconds: 0,
            nanoseconds: 0,
        };
        let catalog = Cached {
            revision: FileRevision {
                identity: FileIdentity {
                    volume: 0,
                    object: [0; 16],
                },
                size: 0,
                modified: stamp,
                changed: stamp,
            },
            base_revision: None,
            generation: 1,
            scope_token: String::new(),
            model: None,
            db,
        };
        let cancel = AtomicBool::new(false);
        let expected = semantic_ranked(&catalog, std::slice::from_ref(&query), &cancel).unwrap();
        assert_eq!(expected.len(), 500);
        assert_eq!(expected[0].doc, 7);
        for offset in [0, 1, 7, 100, 301, 499, 500] {
            let options = SearchOptions { offset, limit: 17 };
            let (total, page) =
                semantic_page(&catalog, std::slice::from_ref(&query), &options, &cancel).unwrap();
            assert_eq!(total, expected.len());
            let want: Vec<_> = expected
                .iter()
                .skip(offset)
                .take(17)
                .map(|r| (r.doc, r.segment, r.score))
                .collect();
            let got: Vec<_> = page.iter().map(|r| (r.doc, r.segment, r.score)).collect();
            assert_eq!(got, want, "offset={offset}");
        }
        cancel.store(true, Ordering::Release);
        assert!(semantic_page(&catalog, &[query], &SearchOptions::default(), &cancel).is_err());
    }
}

/// RRF 在文档 ID 级融合，而非正文片段 ID 级。强词法命中保留可高亮的原段落。
fn rrf_fuse(
    lexical: &[Ranked],
    semantic: &[Ranked],
    options: &SearchOptions,
) -> (usize, Vec<Ranked>) {
    use std::collections::HashMap;
    const RRF_K: f64 = 60.0;
    let mut fused: HashMap<i64, Ranked> = HashMap::new();
    for (rank, item) in lexical.iter().enumerate() {
        // 词法和语义分数不同尺度，创建条目时不得沿用原始分数。
        let entry = fused.entry(item.doc).or_insert(Ranked {
            score: 0.0,
            ..*item
        });
        entry.score += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    for (rank, item) in semantic.iter().enumerate() {
        let entry = fused.entry(item.doc).or_insert(Ranked {
            score: 0.0,
            ..*item
        });
        entry.score += 1.0 / (RRF_K + rank as f64 + 1.0);
        if entry.evidence == MatchEvidence::Lexical {
            entry.evidence = MatchEvidence::LexicalAndSemantic;
        }
        // 词法命中优先作展示证据，确保摘要能高亮输入的原词。
        // 纯语义命中展示语义得分最强的分窗。
    }
    let mut combined: Vec<Ranked> = fused.into_values().collect();
    combined.sort_unstable_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
    page_ranked(combined, options)
}

#[cfg(test)]
mod hybrid_rrf_tests {
    use super::*;

    #[test]
    fn rrf_fuses_distinct_segments_of_same_document() {
        let lex = vec![
            Ranked {
                doc: 1,
                segment: 11,
                score: 10.0,
                window: None,
                evidence: MatchEvidence::Lexical,
            },
            Ranked {
                doc: 2,
                segment: 21,
                score: 8.0,
                window: None,
                evidence: MatchEvidence::Lexical,
            },
        ];
        let sem = vec![
            Ranked {
                doc: 2,
                segment: 22,
                score: 0.92,
                window: Some((0, 32)),
                evidence: MatchEvidence::Semantic,
            },
            Ranked {
                doc: 3,
                segment: 31,
                score: 0.90,
                window: Some((0, 32)),
                evidence: MatchEvidence::Semantic,
            },
        ];
        let (total, page) = rrf_fuse(
            &lex,
            &sem,
            &SearchOptions {
                offset: 0,
                limit: 10,
            },
        );
        assert_eq!(total, 3);
        assert_eq!(page[0].doc, 2);
        assert_eq!(
            page[0].segment, 21,
            "词法证据可高亮，跨段落命中不能产生两个文件"
        );
        assert!(page[0].window.is_none());
        assert_eq!(page[0].evidence, MatchEvidence::LexicalAndSemantic);
        assert_eq!(page[1].evidence, MatchEvidence::Lexical);
        assert_eq!(page[2].evidence, MatchEvidence::Semantic);
        assert_eq!(page.iter().filter(|hit| hit.doc == 2).count(), 1);
    }

    #[test]
    fn hybrid_deep_pagination_has_no_artificial_200_document_ceiling() {
        let lex: Vec<Ranked> = (0..350)
            .map(|doc| Ranked {
                doc,
                segment: doc * 10,
                score: 1.0,
                window: None,
                evidence: MatchEvidence::Lexical,
            })
            .collect();
        let sem: Vec<Ranked> = (100..450)
            .map(|doc| Ranked {
                doc,
                segment: doc * 10 + 1,
                score: 0.8,
                window: Some((0, 1)),
                evidence: MatchEvidence::Semantic,
            })
            .collect();
        let mut ids = std::collections::HashSet::new();
        for offset in (0..450).step_by(37) {
            let (total, page) = rrf_fuse(&lex, &sem, &SearchOptions { offset, limit: 37 });
            assert_eq!(total, 450);
            for hit in page {
                assert!(ids.insert(hit.doc), "分页重复文档");
            }
        }
        assert_eq!(ids.len(), 450);
        let (_, past_end) = rrf_fuse(
            &lex,
            &sem,
            &SearchOptions {
                offset: 450,
                limit: 37,
            },
        );
        assert!(past_end.is_empty());
    }
}

#[cfg(test)]
mod lexical_cache_tests {
    use super::*;

    #[test]
    fn cache_preserves_total_and_never_claims_missing_prefix_is_empty() {
        let prefix = (0..10)
            .map(|doc| Ranked {
                doc,
                segment: doc,
                score: 1.0,
                window: None,
                evidence: MatchEvidence::Lexical,
            })
            .collect();
        let cache = LexicalCache {
            expression: "query".into(),
            total: 100,
            prefix,
        };
        let (total, page) = cache
            .page(
                "query",
                &SearchOptions {
                    offset: 8,
                    limit: 2,
                },
            )
            .unwrap();
        assert_eq!(total, 100);
        assert_eq!(
            page.iter().map(|row| row.doc).collect::<Vec<_>>(),
            vec![8, 9]
        );
        assert!(cache
            .page(
                "query",
                &SearchOptions {
                    offset: 8,
                    limit: 3
                }
            )
            .is_none());
        assert!(cache.page("other", &SearchOptions::default()).is_none());
        assert!(cache
            .page(
                "query",
                &SearchOptions {
                    offset: 100,
                    limit: 1
                }
            )
            .unwrap()
            .1
            .is_empty());
    }
}
