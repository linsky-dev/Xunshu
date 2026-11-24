//! Independent bounded phonetic/subsequence matching. Pinyin readings come from
//! the permissively licensed `pinyin` crate, never a proprietary dictionary.
use crate::fold;
use pinyin::ToPinyinMulti;

pub(crate) const MAX_PHONETIC_NAME_BYTES: usize = 4096;
const MAX_FUZZY_CELLS: usize = 262_144;

/// An NFA over query byte offsets. Each Han character contributes its literal
/// text, each public dictionary pronunciation, or its initial. This avoids
/// multiplying the pronunciation combinations of a multi-character filename.
/// Substrings may start at any filename-character boundary; separators may be
/// skipped, and the final syllable may be partial for search-as-you-type.
pub(crate) fn pinyin_matches(text: &str, query: &str, similar: bool) -> bool {
    if text.len() > MAX_PHONETIC_NAME_BYTES {
        return false;
    }
    let text = fold(text);
    if text.contains(query) {
        return true;
    }
    let mut active = vec![false; query.len() + 1];
    let mut next = vec![false; query.len() + 1];
    active[0] = true;
    let mut static_options: Vec<&'static str> = Vec::with_capacity(8);
    let mut alternatives: Vec<String> = Vec::with_capacity(4);
    let mut char_buf = [0u8; 4];
    for ch in text.chars() {
        next.fill(false);
        next[0] = true;
        static_options.clear();
        alternatives.clear();
        let literal: &str = ch.encode_utf8(&mut char_buf);
        if let Some(readings) = ch.to_pinyin_multi() {
            for reading in readings {
                let plain = reading.plain();
                static_options.push(plain);
                if similar {
                    add_similar_readings(plain, &mut alternatives);
                }
                if let Some(initial) = plain.get(..1) {
                    static_options.push(initial);
                }
            }
        }
        for (offset, is_active) in active.iter().enumerate() {
            if !is_active || offset == query.len() || !query.is_char_boundary(offset) {
                continue;
            }
            if ch.is_whitespace() || matches!(ch, '-' | '_' | '.' | '·') {
                next[offset] = true;
            }
            let remaining = &query[offset..];
            for option in std::iter::once(literal)
                .chain(static_options.iter().copied())
                .chain(alternatives.iter().map(String::as_str))
            {
                if remaining.starts_with(option) {
                    let end = offset + option.len();
                    if end == query.len() {
                        return true;
                    }
                    next[end] = true;
                } else if option.starts_with(remaining) {
                    // A partial final syllable is useful while typing. This
                    // test is only at a valid UTF-8 query boundary.
                    return true;
                }
            }
        }
        std::mem::swap(&mut active, &mut next);
    }
    false
}

/// 常见声母、韵尾近音组合。只展开单个音节，整词继续使用有界状态机。
fn add_similar_readings(reading: &str, alternatives: &mut Vec<String>) {
    let alternate_start = [("zh", "z"), ("ch", "c"), ("sh", "s"), ("n", "l")]
        .iter()
        .find_map(|(first, second)| {
            reading
                .strip_prefix(first)
                .map(|tail| format!("{second}{tail}"))
                .or_else(|| {
                    reading
                        .strip_prefix(second)
                        .map(|tail| format!("{first}{tail}"))
                })
        });
    let alternate_end = |syllable: &str| {
        [("ang", "an"), ("eng", "en"), ("ing", "in")]
            .iter()
            .find_map(|(first, second)| {
                syllable
                    .strip_suffix(first)
                    .map(|head| format!("{head}{second}"))
                    .or_else(|| {
                        syllable
                            .strip_suffix(second)
                            .map(|head| format!("{head}{first}"))
                    })
            })
    };
    alternatives.extend(alternate_end(reading));
    if let Some(alternate_start) = alternate_start {
        alternatives.extend(alternate_end(&alternate_start));
        alternatives.push(alternate_start);
    }
}

/// Ordered-character matching with affine gap penalties and word-boundary
/// bonuses. All query characters must match: this is a launcher-style
/// subsequence matcher, not edit-distance spelling correction. The DP keeps
/// two rows, O(filename characters) memory, and never recursively backtracks.
pub(crate) fn fuzzy_score(text: &str, query: &[char]) -> Option<i64> {
    if text.len() > MAX_PHONETIC_NAME_BYTES {
        return None;
    }
    let haystack = fold(text).chars().collect::<Vec<_>>();
    if query.is_empty() || query.len() > haystack.len() {
        return None;
    }
    // On unusually long catalog names, retain correct subsequence recall with
    // a linear scoring fallback instead of spending an unbounded DP budget.
    if query.len().saturating_mul(haystack.len()) > MAX_FUZZY_CELLS {
        let mut position = 0;
        for ch in &haystack {
            if *ch == query[position] {
                position += 1;
            }
            if position == query.len() {
                return Some(0);
            }
        }
        return None;
    }
    let negative = i64::MIN / 4;
    let mut previous_match = vec![negative; haystack.len()];
    let mut previous_gap = vec![negative; haystack.len()];
    let mut current_match = vec![negative; haystack.len()];
    let mut current_gap = vec![negative; haystack.len()];
    for (i, wanted) in query.iter().enumerate() {
        current_match.fill(negative);
        current_gap.fill(negative);
        for (j, candidate) in haystack.iter().enumerate() {
            if j > 0 {
                current_gap[j] = (current_match[j - 1] - 8).max(current_gap[j - 1] - 1);
            }
            if wanted != candidate {
                continue;
            }
            let base = if i == 0 {
                100 - j.min(32) as i64
            } else if j == 0 {
                negative
            } else {
                (previous_match[j - 1] + 8).max(previous_gap[j - 1])
            };
            if base <= negative / 2 {
                continue;
            }
            let boundary_bonus = if j == 0 || !haystack[j - 1].is_alphanumeric() {
                12
            } else {
                0
            };
            current_match[j] = base + 16 + boundary_bonus;
        }
        std::mem::swap(&mut previous_match, &mut current_match);
        std::mem::swap(&mut previous_gap, &mut current_gap);
    }
    let score = previous_match.into_iter().max()?;
    (score > negative / 2).then_some(score)
}

/// 提取文件名的拼音首字母序列（仅当包含汉字时返回 Some，用于构建紧凑拼音倒排索引）。
pub fn pinyin_initials(text: &str) -> Option<String> {
    if text.len() > MAX_PHONETIC_NAME_BYTES {
        return None;
    }
    let mut has_han = false;
    let mut out = String::with_capacity(text.len().min(64));
    for ch in text.chars() {
        if let Some(initial) = ch
            .to_pinyin_multi()
            .and_then(|r| r.into_iter().next())
            .and_then(|first| first.plain().chars().next())
        {
            has_han = true;
            out.push(initial);
        } else if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        }
    }
    has_han.then_some(out)
}

/// 容错模糊匹配：在精确子序列未命中且查询长度 >= 4 时，允许最多 1 个错别字（替换或漏字）。
#[allow(dead_code)]
pub fn fuzzy_score_typo_tolerant(text: &str, query: &[char], max_typos: usize) -> Option<i64> {
    if let Some(exact) = fuzzy_score(text, query) {
        return Some(exact + 64);
    }
    if max_typos == 0 || query.len() < 4 || text.len() > MAX_PHONETIC_NAME_BYTES {
        return None;
    }
    let mut best: Option<i64> = None;
    let mut reduced = Vec::with_capacity(query.len() - 1);
    for skip in 0..query.len() {
        reduced.clear();
        reduced.extend_from_slice(&query[..skip]);
        reduced.extend_from_slice(&query[skip + 1..]);
        if let Some(score) = fuzzy_score(text, &reduced) {
            let penalized = score - 40;
            best = Some(best.map_or(penalized, |prev| prev.max(penalized)));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_initial_mixed_and_polyphonic_without_cartesian_expansion() {
        for query in ["xunshusheji", "xssj", "xun枢sj", "xunshu", "shej", "sj"] {
            assert!(pinyin_matches("寻枢设计.docx", query, false), "{query}");
        }
        assert!(pinyin_matches("重庆报告.pdf", "chongqing", false));
        assert!(pinyin_matches("重庆报告.pdf", "zhongqing", false));
        assert!(pinyin_matches("寻枢-设计.docx", "xssj", false));
        assert!(!pinyin_matches("寻枢设计.docx", "xunsujian", false));
        assert!(!pinyin_matches("中文文件.docx", "codex", false));
        assert!(pinyin_matches(&"重".repeat(128), "chongzhongchong", false));
        assert!(pinyin_matches("张平.docx", "zanpin", true));
        assert!(!pinyin_matches("张平.docx", "zanpin", false));
    }
    #[test]
    fn fuzzy_requires_all_characters_in_order() {
        let q = "rpt".chars().collect::<Vec<_>>();
        assert!(fuzzy_score("report_final.pdf", &q).is_some());
        assert!(fuzzy_score("rpt.pdf", &q).unwrap() > fuzzy_score("report.pdf", &q).unwrap());
        assert!(fuzzy_score("ptr.pdf", &q).is_none());
        assert!(fuzzy_score("报告文档", &['报', '文']).is_some());
        assert!(fuzzy_score("ab", &['a', 'a']).is_none());
    }

    #[test]
    fn typo_tolerant_fuzzy_recovers_single_character_mistake() {
        let typo: Vec<char> = "reprt".chars().collect();
        assert!(fuzzy_score_typo_tolerant("report_2026.pdf", &typo, 1).is_some());
        let bad_typo: Vec<char> = "repxrt".chars().collect();
        assert!(fuzzy_score_typo_tolerant("report_2026.pdf", &bad_typo, 1).is_some());
        assert!(fuzzy_score_typo_tolerant("report_2026.pdf", &bad_typo, 0).is_none());
    }
}
