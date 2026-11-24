//! A single, bounded query implementation shared by every Xunshu search surface.
//!
//! Grammar: `!`/`NOT` binds first, `|`/`OR` second, and whitespace/`AND`
//! last, matching the documented common Everything syntax. Parentheses and
//! angle brackets allow explicit grouping. Unsupported functions are errors.
//! This is an independent implementation from public syntax documentation;
//! it includes no recovered keyword table or proprietary character mappings.

mod parser;
mod phonetic;
pub use phonetic::pinyin_initials;
mod predicates;

use regex::Regex;
use std::{borrow::Cow, cell::OnceCell, error::Error, fmt};
use unicode_normalization::UnicodeNormalization;

pub const MAX_QUERY_BYTES: usize = 8 * 1024;
pub const MAX_TOKENS: usize = 256;
pub const MAX_DEPTH: usize = 24;
pub const MAX_REGEX_BYTES: usize = 2048;
pub const MAX_REGEX_COUNT: usize = 16;
pub const MAX_PHONETIC_CHARS: usize = 128;

/// Borrowed file metadata. Query evaluation never opens a path or file.
#[derive(Debug, Clone, Copy)]
pub struct EntryRef<'a> {
    pub name: &'a str,
    pub path: &'a str,
    pub size: u64,
    /// Seconds from the Unix epoch, UTC.
    pub modified: i64,
    pub is_dir: bool,
    pub source: &'a str,
}

/// 索引的可选属性投影。导入数据缺失的数值保留为未知。
#[derive(Debug, Clone, Copy)]
pub struct QueryMetadata {
    pub size_known: bool,
    pub modified_known: bool,
    pub created: Option<i64>,
    pub attributes: Option<u64>,
}

impl Default for QueryMetadata {
    fn default() -> Self {
        Self {
            size_known: true,
            modified_known: true,
            created: None,
            attributes: None,
        }
    }
}

/// A conservative candidate filter, never a replacement for `Query::matches`.
/// Every returned literal has already passed through [`fold`]. `All` means the
/// query cannot safely narrow the filename index at that branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidatePlan {
    All,
    Literal(String),
    Ext(Vec<String>),
    Pinyin(String),
    And(Vec<CandidatePlan>),
    Or(Vec<CandidatePlan>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// UTF-8 byte offset into the original query.
    pub offset: usize,
    pub message: String,
}

impl QueryError {
    pub(crate) fn new(offset: usize, message: impl Into<String>) -> Self {
        Self {
            offset,
            message: message.into(),
        }
    }
}
impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}（字节位置 {}）", self.message, self.offset)
    }
}
impl Error for QueryError {}

/// Unicode NFKC normalization followed by Unicode lowercase conversion.
/// This deliberately defines a reproducible public normalization rule rather
/// than claiming full Unicode case folding or accent-insensitive matching.
/// Use this function both when building grams and when looking them up.
pub fn fold(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    text.nfkc().flat_map(char::to_lowercase).collect()
}

#[derive(Debug, Clone)]
pub struct Query {
    expr: Expr,
    needs_metadata: bool,
}

impl Query {
    pub fn parse(text: &str) -> Result<Self, QueryError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
            .unwrap_or(0);
        parser::parse(text, now, true).map(|expr| Self {
            needs_metadata: expr.needs_metadata(),
            expr,
        })
    }

    /// Deterministic entry point for date-dependent saved searches and tests.
    /// Relative dates are fixed at parse time and interpreted in UTC.
    pub fn parse_at(text: &str, now_unix_utc: i64) -> Result<Self, QueryError> {
        parser::parse(text, now_unix_utc, false).map(|expr| Self {
            needs_metadata: expr.needs_metadata(),
            expr,
        })
    }

    /// 普通名字查询可以跳过导入属性解析及附加属性读取。
    pub fn needs_metadata(&self) -> bool {
        self.needs_metadata
    }

    pub fn matches(&self, entry: &EntryRef<'_>) -> bool {
        self.expr
            .evaluate(&Evaluation::new(entry, None, QueryMetadata::default()))
            .score()
            .is_some()
    }

    /// Larger values rank first. Non-matches score -1. Filename/stem exact
    /// matches outrank prefixes, word boundaries and ordinary substrings;
    /// path depth only provides a small, deterministic tie-break feature.
    pub fn score(&self, entry: &EntryRef<'_>) -> i64 {
        self.matched_score(entry).unwrap_or(-1)
    }

    /// Evaluate and rank in one traversal, avoiding a separate matches/score
    /// pass in the resident engine.
    pub fn matched_score(&self, entry: &EntryRef<'_>) -> Option<i64> {
        self.matched_score_prepared(entry, None, path_depth(entry.path))
    }

    /// Whether a predicate requires the full path. When false, an engine may
    /// leave EntryRef::path empty if it supplies the real path depth to
    /// `matched_score_prepared`; filename/source/metadata semantics are unchanged.
    pub fn needs_path(&self) -> bool {
        self.expr.needs_path()
    }

    /// Evaluate with a borrowed normalization hint and precomputed path depth.
    ///
    /// `folded_name`, when Some, MUST equal `fold(entry.name)`. The caller must
    /// establish this invariant when building/validating its immutable index.
    /// `depth` must equal `path_depth(actual_full_path)`. A filename-only query
    /// does not otherwise require that full path to be allocated. Invalid hints
    /// can produce incorrect matches/ranking; they never trigger unsafe code.
    /// Passing None preserves ordinary on-demand normalization.
    pub fn matched_score_prepared(
        &self,
        entry: &EntryRef<'_>,
        folded_name: Option<&str>,
        depth: usize,
    ) -> Option<i64> {
        self.matched_score_with_metadata(entry, folded_name, depth, QueryMetadata::default())
    }

    /// 为导入记录及已索引属性执行统一的过滤和评分。
    pub fn matched_score_with_metadata(
        &self,
        entry: &EntryRef<'_>,
        folded_name: Option<&str>,
        depth: usize,
        metadata: QueryMetadata,
    ) -> Option<i64> {
        self.expr
            .evaluate(&Evaluation::new(entry, folded_name, metadata))
            .score()
            .map(|score| {
                score
                    .saturating_add(50)
                    .saturating_sub(depth.min(50) as i64)
            })
    }

    pub fn candidate_plan(&self) -> CandidatePlan {
        self.expr.candidate_plan()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Expr {
    All,
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Leaf(Leaf),
}

#[derive(Debug, Clone)]
pub(crate) enum Leaf {
    Text {
        field: Field,
        pattern: Pattern,
    },
    Directory(bool),
    Extension(Vec<String>),
    Size(predicates::SizePredicate),
    Modified(predicates::DatePredicate),
    Created(predicates::DatePredicate),
    Attributes(u64),
    DirectoryPath {
        path: String,
        case_sensitive: bool,
        recursive: bool,
    },
    DirectoryName {
        needle: String,
        case_sensitive: bool,
        recursive: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Name,
    Path,
    Source,
}

#[derive(Debug, Clone)]
pub(crate) enum Pattern {
    Literal {
        needle: String,
        finder: Box<memchr::memmem::Finder<'static>>,
        case_sensitive: bool,
    },
    Glob {
        regex: Regex,
        case_sensitive: bool,
        required_literal: Option<String>,
    },
    Regex(Regex),
    Pinyin {
        needle: String,
        similar: bool,
    },
    Fuzzy(Vec<char>),
    Typo(Vec<char>),
    Smart {
        needle: String,
        chars: Vec<char>,
    },
}

enum EvalResult {
    Known(Option<i64>),
    Unknown,
}

impl EvalResult {
    fn score(self) -> Option<i64> {
        match self {
            Self::Known(score) => score,
            Self::Unknown => None,
        }
    }
}

impl Expr {
    fn needs_metadata(&self) -> bool {
        match self {
            Self::Leaf(
                Leaf::Size(_) | Leaf::Modified(_) | Leaf::Created(_) | Leaf::Attributes(_),
            ) => true,
            Self::And(parts) | Self::Or(parts) => parts.iter().any(Self::needs_metadata),
            Self::Not(part) => part.needs_metadata(),
            _ => false,
        }
    }
    pub(crate) fn and(parts: Vec<Self>) -> Self {
        let mut out = Vec::new();
        for part in parts {
            match part {
                Self::All => (),
                Self::And(children) => out.extend(children),
                other => out.push(other),
            }
        }
        match out.len() {
            0 => Self::All,
            1 => out.remove(0),
            _ => Self::And(out),
        }
    }

    pub(crate) fn or(parts: Vec<Self>) -> Self {
        let mut out = Vec::new();
        for part in parts {
            match part {
                Self::All => return Self::All,
                Self::Or(children) => out.extend(children),
                other => out.push(other),
            }
        }
        match out.len() {
            0 => Self::All,
            1 => out.remove(0),
            _ => Self::Or(out),
        }
    }

    fn evaluate(&self, evaluation: &Evaluation<'_, '_, '_>) -> EvalResult {
        use EvalResult::{Known, Unknown};
        match self {
            Self::All => Known(Some(0)),
            Self::And(parts) => {
                let mut sum = 0_i64;
                let mut unknown = false;
                for part in parts {
                    match part.evaluate(evaluation) {
                        Known(Some(score)) => sum = sum.saturating_add(score),
                        Known(None) => return Known(None),
                        Unknown => unknown = true,
                    }
                }
                if unknown { Unknown } else { Known(Some(sum)) }
            }
            Self::Or(parts) => {
                let mut best = None;
                let mut unknown = false;
                for part in parts {
                    match part.evaluate(evaluation) {
                        Known(score) => best = best.max(score),
                        Unknown => unknown = true,
                    }
                }
                if best.is_none() && unknown {
                    Unknown
                } else {
                    Known(best)
                }
            }
            Self::Not(part) => match part.evaluate(evaluation) {
                Known(score) => Known(score.is_none().then_some(0)),
                Unknown => Unknown,
            },
            Self::Leaf(leaf) => {
                if leaf.metadata_known(evaluation.metadata) {
                    Known(leaf.evaluate(evaluation))
                } else {
                    Unknown
                }
            }
        }
    }

    fn needs_path(&self) -> bool {
        match self {
            Self::Leaf(
                Leaf::DirectoryPath { .. }
                | Leaf::DirectoryName { .. }
                | Leaf::Text {
                    field: Field::Path, ..
                },
            ) => true,
            Self::And(parts) | Self::Or(parts) => parts.iter().any(Self::needs_path),
            Self::Not(part) => part.needs_path(),
            _ => false,
        }
    }

    fn candidate_plan(&self) -> CandidatePlan {
        match self {
            Self::All | Self::Not(_) => CandidatePlan::All,
            Self::Leaf(leaf) => leaf.candidate_plan(),
            Self::And(parts) => {
                let mut out = parts
                    .iter()
                    .map(Self::candidate_plan)
                    .filter(|p| *p != CandidatePlan::All)
                    .collect::<Vec<_>>();
                match out.len() {
                    0 => CandidatePlan::All,
                    1 => out.remove(0),
                    _ => CandidatePlan::And(out),
                }
            }
            Self::Or(parts) => {
                let mut out = Vec::new();
                for part in parts {
                    match part.candidate_plan() {
                        CandidatePlan::All => return CandidatePlan::All,
                        plan => out.push(plan),
                    }
                }
                match out.len() {
                    0 => CandidatePlan::All,
                    1 => out.remove(0),
                    _ => CandidatePlan::Or(out),
                }
            }
        }
    }
}

impl Leaf {
    fn metadata_known(&self, metadata: QueryMetadata) -> bool {
        match self {
            Self::Size(_) => metadata.size_known,
            Self::Modified(_) => metadata.modified_known,
            Self::Created(_) => metadata.created.is_some(),
            Self::Attributes(_) => metadata.attributes.is_some(),
            _ => true,
        }
    }

    fn evaluate(&self, evaluation: &Evaluation<'_, '_, '_>) -> Option<i64> {
        let entry = evaluation.entry;
        match self {
            Self::Directory(expected) => (entry.is_dir == *expected).then_some(0),
            Self::Extension(extensions) => {
                if entry.is_dir {
                    return None;
                }
                let ext = fold(extension(entry.name));
                extensions
                    .iter()
                    .any(|candidate| candidate == &ext)
                    .then_some(0)
            }
            Self::Size(predicate) => predicate.matches(entry.size).then_some(0),
            Self::Modified(predicate) => predicate.matches(entry.modified).then_some(0),
            Self::Created(predicate) => {
                predicate.matches(evaluation.metadata.created?).then_some(0)
            }
            Self::Attributes(mask) => {
                (evaluation.metadata.attributes? & mask == *mask).then_some(0)
            }
            Self::DirectoryPath {
                path,
                case_sensitive,
                recursive,
            } => {
                let candidate = comparable_path(parent_path(entry.path), *case_sensitive);
                let candidate = trim_directory_end(&candidate);
                let expected = trim_directory_end(path);
                (candidate == expected
                    || *recursive
                        && (expected == "/" && candidate.starts_with('/')
                            || candidate
                                .strip_prefix(expected)
                                .is_some_and(|tail| tail.starts_with('/'))))
                .then_some(0)
            }
            Self::DirectoryName {
                needle,
                case_sensitive,
                recursive,
            } => {
                let parent = canonical_separators(parent_path(entry.path));
                let mut components = parent.rsplit('/').filter(|part| !part.is_empty());
                let contains = |part: &str| {
                    if *case_sensitive {
                        part.contains(needle)
                    } else {
                        fold(part).contains(needle)
                    }
                };
                let matched = if *recursive {
                    components.any(contains)
                } else {
                    components.next().is_some_and(contains)
                };
                matched.then_some(0)
            }
            Self::Text { field, pattern } => {
                pattern.score(evaluation, *field).map(|score| match field {
                    Field::Name => score,
                    Field::Path => score.min(400),
                    Field::Source => 0,
                })
            }
        }
    }

    fn candidate_plan(&self) -> CandidatePlan {
        match self {
            Self::Text {
                field: Field::Name,
                pattern:
                    Pattern::Literal {
                        needle,
                        case_sensitive: false,
                        ..
                    },
            } if !needle.is_empty() => CandidatePlan::Literal(needle.clone()),
            Self::Text {
                field: Field::Name,
                pattern:
                    Pattern::Glob {
                        required_literal: Some(required),
                        case_sensitive: false,
                        ..
                    },
            } if !required.is_empty() => CandidatePlan::Literal(required.clone()),
            Self::Extension(exts)
                if !exts.is_empty() && exts.len() <= 32 && exts.iter().all(|e| !e.is_empty()) =>
            {
                CandidatePlan::Ext(exts.clone())
            }
            Self::Text {
                field: Field::Name,
                pattern: Pattern::Smart { needle, .. },
            } if needle.len() >= 2 => {
                if needle.bytes().all(|b| b.is_ascii_lowercase()) {
                    CandidatePlan::Or(vec![
                        CandidatePlan::Literal(needle.clone()),
                        CandidatePlan::Pinyin(needle.clone()),
                    ])
                } else {
                    CandidatePlan::Literal(needle.clone())
                }
            }
            // A case-sensitive literal is deliberately All: normalization can
            // compose a combining mark across the matched substring boundary.
            // Treating fold(raw_substring) as required could lose such results.
            _ => CandidatePlan::All,
        }
    }
}

impl Pattern {
    fn score(&self, evaluation: &Evaluation<'_, '_, '_>, field: Field) -> Option<i64> {
        let text = evaluation.raw(field, matches!(self, Self::Regex(_)));
        match self {
            Self::Literal {
                needle,
                finder,
                case_sensitive,
            } => {
                let haystack = if *case_sensitive {
                    text
                } else {
                    evaluation.folded(field)
                };
                literal_score(haystack, needle, finder)
            }
            Self::Glob {
                regex,
                case_sensitive,
                ..
            } => {
                let haystack = if *case_sensitive {
                    text
                } else {
                    evaluation.folded(field)
                };
                regex.is_match(haystack).then_some(700)
            }
            Self::Regex(regex) => regex.is_match(text).then_some(650),
            Self::Pinyin { needle, similar } => {
                phonetic::pinyin_matches(text, needle, *similar).then_some(550)
            }
            Self::Fuzzy(needle) => {
                phonetic::fuzzy_score(text, needle).map(|s| 500 + s.clamp(0, 450))
            }
            Self::Typo(needle) => {
                phonetic::fuzzy_score_typo_tolerant(text, needle, 1).map(|s| 460 + s.clamp(0, 450))
            }
            Self::Smart { needle, chars } => {
                let hay = fold(text);
                if let Some(pos) = hay.find(needle.as_str()) {
                    if hay == needle.as_str() {
                        Some(1000)
                    } else if pos == 0 {
                        Some(850)
                    } else {
                        Some(720)
                    }
                } else if phonetic::pinyin_matches(text, needle, true) {
                    Some(620)
                } else {
                    phonetic::fuzzy_score_typo_tolerant(text, chars, 1)
                        .map(|s| 460 + s.clamp(0, 450))
                }
            }
        }
    }
}

/// Per-evaluation lazy text cache: a multi-word query normalizes each field at
/// most once, while pure metadata filters allocate no folded filename at all.
struct Evaluation<'data, 'entry, 'prepared> {
    entry: &'entry EntryRef<'data>,
    prepared_name: Option<&'prepared str>,
    metadata: QueryMetadata,
    normalized_name: OnceCell<String>,
    normalized_path: OnceCell<String>,
    normalized_source: OnceCell<String>,
    path: OnceCell<Cow<'data, str>>,
}
impl<'data, 'entry, 'prepared> Evaluation<'data, 'entry, 'prepared> {
    fn new(
        entry: &'entry EntryRef<'data>,
        prepared_name: Option<&'prepared str>,
        metadata: QueryMetadata,
    ) -> Self {
        Self {
            entry,
            prepared_name,
            metadata,
            normalized_name: OnceCell::new(),
            normalized_path: OnceCell::new(),
            normalized_source: OnceCell::new(),
            path: OnceCell::new(),
        }
    }
    fn raw(&self, field: Field, regex: bool) -> &str {
        match field {
            Field::Name => self.entry.name,
            Field::Source => self.entry.source,
            Field::Path if regex => self.entry.path,
            Field::Path => self
                .path
                .get_or_init(|| canonical_separators(self.entry.path))
                .as_ref(),
        }
    }
    fn folded(&self, field: Field) -> &str {
        match field {
            Field::Name => self
                .prepared_name
                .unwrap_or_else(|| self.normalized_name.get_or_init(|| fold(self.entry.name))),
            Field::Source => self
                .normalized_source
                .get_or_init(|| fold(self.entry.source)),
            Field::Path => self
                .normalized_path
                .get_or_init(|| fold(self.raw(field, false))),
        }
    }
}

fn literal_score(text: &str, needle: &str, finder: &memchr::memmem::Finder<'_>) -> Option<i64> {
    // Both inputs are valid UTF-8; a nonempty UTF-8 needle cannot start inside
    // a continuation byte. Empty needles also match at byte zero.
    let at = finder.find(text.as_bytes())?;
    if at == 0 {
        if text == needle {
            return Some(2000);
        }
        let stem = text
            .rsplit_once('.')
            .map_or(text, |(head, _)| if head.is_empty() { text } else { head });
        if stem == needle {
            return Some(1800);
        }
        return Some(1500);
    }
    let position = text[..at].chars().count().min(200) as i64;
    let previous = text[..at].chars().next_back();
    if previous.is_some_and(|c| !c.is_alphanumeric()) {
        return Some(1100 - position);
    }
    Some(800 - position)
}

pub(crate) fn extension(name: &str) -> &str {
    name.rsplit_once('.')
        .filter(|(head, _)| !head.is_empty())
        .map_or("", |(_, tail)| tail)
}

pub(crate) fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || path.starts_with("\\\\")
        || path.starts_with("//")
}

pub(crate) fn canonical_separators(path: &str) -> Cow<'_, str> {
    if is_windows_path(path) && path.contains('\\') {
        Cow::Owned(path.replace('\\', "/"))
    } else {
        Cow::Borrowed(path)
    }
}

fn comparable_path(path: &str, case_sensitive: bool) -> String {
    let path = canonical_separators(path);
    if case_sensitive {
        path.into_owned()
    } else {
        fold(&path)
    }
}

fn trim_directory_end(path: &str) -> &str {
    let minimum = if path.starts_with('/') { 1 } else { 0 };
    let trimmed = path.trim_end_matches('/');
    if trimmed.len() < minimum {
        "/"
    } else {
        trimmed
    }
}

pub(crate) fn parent_path(path: &str) -> &str {
    let windows = is_windows_path(path);
    let path = path.trim_end_matches(|c| c == '/' || windows && c == '\\');
    match path.rfind(|c| c == '/' || windows && c == '\\') {
        Some(0) => &path[..1],
        Some(2) if windows && path.as_bytes().get(1) == Some(&b':') => &path[..3],
        Some(at) => &path[..at],
        None => "",
    }
}

/// Count path separators using the path's lexical flavor, independently of the
/// host OS. A backslash in a POSIX filename is not a directory separator.
/// For a valid filename with no separators, this equals the depth of its
/// interned parent prefix, allowing engines to cache the value per directory.
pub fn path_depth(path: &str) -> usize {
    let windows = is_windows_path(path);
    path.chars()
        .filter(|c| *c == '/' || windows && *c == '\\')
        .count()
}

#[cfg(test)]
mod smart_query_tests {
    use super::*;

    #[test]
    fn smart_and_typo_operators_unify_literal_pinyin_and_typo_recall() {
        let cn = EntryRef {
            name: "寻枢设计方案.docx",
            path: "/docs/寻枢设计方案.docx",
            size: 1024,
            modified: 0,
            is_dir: false,
            source: "local",
        };
        let en = EntryRef {
            name: "report_final_2026.pdf",
            path: "/docs/report_final_2026.pdf",
            size: 2048,
            modified: 0,
            is_dir: false,
            source: "local",
        };
        assert!(Query::parse("smart:xssj").unwrap().matches(&cn));
        assert!(Query::parse("smart:xunshu").unwrap().matches(&cn));
        assert!(Query::parse("smart:reprt").unwrap().matches(&en));
        assert!(Query::parse("typo:repxrt").unwrap().matches(&en));
        assert!(!Query::parse("smart:totallyunrelated").unwrap().matches(&cn));
    }
}

#[cfg(test)]
mod literal_finder_tests {
    use super::*;

    fn reference(text: &str, needle: &str) -> Option<i64> {
        let at = text.find(needle)?;
        if text == needle {
            return Some(2000);
        }
        let stem = text
            .rsplit_once('.')
            .map_or(text, |(head, _)| if head.is_empty() { text } else { head });
        if stem == needle {
            return Some(1800);
        }
        let position = text[..at].chars().count().min(200) as i64;
        if at == 0 {
            return Some(1500);
        }
        if text[..at]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_alphanumeric())
        {
            return Some(1100 - position);
        }
        Some(800 - position)
    }

    #[test]
    fn prepared_literal_finder_preserves_unicode_and_exact_scores() {
        let long_name = format!("{}报告.txt", "甲".repeat(220));
        for name in [
            "",
            "budget",
            "budget.pdf",
            "budget_final.pdf",
            "meeting_budget.txt",
            ".budget",
            "干部任免报告.docx",
            "甲_报告.pdf",
            "甲报告.pdf",
            "e\u{301}.pdf",
            "😀报告.pdf",
            &long_name,
        ] {
            for needle in [
                "", "budget", "meeting", ".", "报告", "甲", "😀", "e\u{301}", "é", "absent",
            ] {
                let finder = memchr::memmem::Finder::new(needle.as_bytes());
                assert_eq!(
                    literal_score(name, needle, &finder),
                    reference(name, needle),
                    "{name:?} {needle:?}"
                );
            }
        }
    }
}
