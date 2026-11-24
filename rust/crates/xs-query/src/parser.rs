use crate::{
    Expr, Field, Leaf, MAX_DEPTH, MAX_PHONETIC_CHARS, MAX_QUERY_BYTES, MAX_REGEX_BYTES,
    MAX_REGEX_COUNT, MAX_TOKENS, Pattern, QueryError, canonical_separators, fold, predicates,
};
use regex::RegexBuilder;

#[derive(Debug, Clone)]
struct Word {
    text: String,
    offset: usize,
    leading_quoted: bool,
    first_quote_at: Option<usize>,
}
#[derive(Debug, Clone)]
enum Token {
    Word(Word),
    And(usize),
    Or(usize),
    Not(usize),
    Open(char, usize),
    Close(char, usize),
}
impl Token {
    fn offset(&self) -> usize {
        match self {
            Self::Word(word) => word.offset,
            Self::And(at)
            | Self::Or(at)
            | Self::Not(at)
            | Self::Open(_, at)
            | Self::Close(_, at) => *at,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Regex,
    Pinyin,
    SimilarPinyin,
    Fuzzy,
    Typo,
    Smart,
}
#[derive(Clone, Copy)]
struct Context {
    field: Field,
    case_sensitive: bool,
    mode: Mode,
    explicit_field: bool,
}
impl Default for Context {
    fn default() -> Self {
        Self {
            field: Field::Name,
            case_sensitive: false,
            mode: Mode::Text,
            explicit_field: false,
        }
    }
}

pub(crate) fn parse(text: &str, now: i64, local_dates: bool) -> Result<Expr, QueryError> {
    if text.len() > MAX_QUERY_BYTES {
        return Err(QueryError::new(
            0,
            format!("查询超过 {MAX_QUERY_BYTES} 字节预算"),
        ));
    }
    if text.contains('\0') {
        return Err(QueryError::new(0, "查询不能包含 NUL 字符"));
    }
    let tokens = lex(text)?;
    if tokens.is_empty() {
        return Ok(Expr::All);
    }
    let mut parser = Parser {
        tokens,
        at: 0,
        regex_count: 0,
        end: text.len(),
        now,
        local_dates,
    };
    let expr = parser.parse_and(Context::default(), 0)?;
    if let Some(token) = parser.peek() {
        return Err(QueryError::new(token.offset(), "多余的结束分组或运算符"));
    }
    Ok(expr)
}

fn lex(text: &str) -> Result<Vec<Token>, QueryError> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let ch = text[cursor..].chars().next().unwrap();
        if ch.is_whitespace() {
            cursor += ch.len_utf8();
            continue;
        }
        if tokens.len() >= MAX_TOKENS {
            return Err(QueryError::new(
                cursor,
                format!("查询超过 {MAX_TOKENS} 个词元预算"),
            ));
        }
        let start = cursor;
        match ch {
            '|' => {
                tokens.push(Token::Or(start));
                cursor += 1;
            }
            '!' => {
                tokens.push(Token::Not(start));
                cursor += 1;
            }
            '(' | '<' => {
                tokens.push(Token::Open(ch, start));
                cursor += 1;
            }
            ')' | '>' => {
                tokens.push(Token::Close(ch, start));
                cursor += 1;
            }
            _ => {
                let leading_quoted = ch == '"';
                let mut value = String::new();
                let mut quoted = false;
                let mut saw_quote = false;
                let mut first_quote_at = None;
                let mut quoted_value_closed = false;
                let mut regex_mode = false;
                let mut regex_depth = 0_usize;
                let mut regex_class = false;
                while cursor < text.len() {
                    let current = text[cursor..].chars().next().unwrap();
                    if current == '\\' && regex_mode {
                        value.push(current);
                        cursor += 1;
                        if let Some(escaped) = text[cursor..].chars().next() {
                            value.push(escaped);
                            cursor += escaped.len_utf8();
                        }
                        continue;
                    }
                    if current == '"' {
                        saw_quote = true;
                        first_quote_at.get_or_insert(value.len());
                        quoted_value_closed |= quoted;
                        quoted = !quoted;
                        cursor += 1;
                        continue;
                    }
                    if !quoted {
                        if current.is_whitespace() {
                            break;
                        }
                        if regex_mode {
                            if quoted_value_closed && matches!(current, '|' | '(' | ')' | '<' | '>')
                            {
                                break;
                            }
                            if current == '[' {
                                regex_class = true;
                            }
                            if current == ']' {
                                regex_class = false;
                            }
                            if !regex_class {
                                if current == '(' {
                                    regex_depth += 1;
                                }
                                if current == ')' {
                                    if regex_depth == 0 {
                                        break;
                                    }
                                    regex_depth -= 1;
                                }
                                if matches!(current, '<' | '>') && regex_depth == 0 {
                                    break;
                                }
                            }
                        } else if matches!(current, '|' | '(' | ')' | '<' | '>') {
                            // A '<' or '>' immediately after a numeric/date
                            // function is a comparison, not a group delimiter.
                            let comparison = matches!(current, '<' | '>')
                                && [
                                    "size:",
                                    "dm:",
                                    "mtime:",
                                    "date-modified:",
                                    "dc:",
                                    "ctime:",
                                    "date-created:",
                                ]
                                .iter()
                                .any(|suffix| value.to_ascii_lowercase().ends_with(suffix));
                            if !comparison {
                                break;
                            }
                        }
                    }
                    value.push(current);
                    cursor += current.len_utf8();
                    if current == ':' && !quoted && first_quote_at.is_none() {
                        regex_mode = value
                            .split(':')
                            .any(|piece| piece.eq_ignore_ascii_case("regex"));
                    }
                }
                if quoted {
                    return Err(QueryError::new(start, "双引号未闭合"));
                }
                if cursor == start {
                    return Err(QueryError::new(start, "无法识别的查询符号"));
                }
                let token = if !saw_quote {
                    match value.as_str() {
                        "AND" => Token::And(start),
                        "OR" => Token::Or(start),
                        "NOT" => Token::Not(start),
                        _ => Token::Word(Word {
                            text: value,
                            offset: start,
                            leading_quoted,
                            first_quote_at,
                        }),
                    }
                } else {
                    Token::Word(Word {
                        text: value,
                        offset: start,
                        leading_quoted,
                        first_quote_at,
                    })
                };
                tokens.push(token);
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    end: usize,
    regex_count: usize,
    now: i64,
    local_dates: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }
    fn current_offset(&self) -> usize {
        self.peek().map_or(self.end, Token::offset)
    }
    fn check_depth(&self, depth: usize) -> Result<(), QueryError> {
        if depth > MAX_DEPTH {
            Err(QueryError::new(
                self.current_offset(),
                format!("查询分组或修饰符超过 {MAX_DEPTH} 层预算"),
            ))
        } else {
            Ok(())
        }
    }

    // OR binds more tightly than AND, as in `a|b c` == `(a|b) AND c`.
    fn parse_and(&mut self, context: Context, depth: usize) -> Result<Expr, QueryError> {
        let mut parts = vec![self.parse_or(context, depth)?];
        loop {
            match self.peek() {
                None | Some(Token::Close(_, _)) => break,
                Some(Token::And(_)) => {
                    self.at += 1;
                    parts.push(self.parse_or(context, depth)?);
                }
                Some(Token::Word(_)) | Some(Token::Open(_, _)) | Some(Token::Not(_)) => {
                    parts.push(self.parse_or(context, depth)?);
                }
                Some(other) => return Err(QueryError::new(other.offset(), "此处需要查询词或 AND")),
            }
        }
        Ok(Expr::and(parts))
    }

    fn parse_or(&mut self, context: Context, depth: usize) -> Result<Expr, QueryError> {
        let mut parts = vec![self.parse_unary(context, depth)?];
        while matches!(self.peek(), Some(Token::Or(_))) {
            self.at += 1;
            parts.push(self.parse_unary(context, depth)?);
        }
        Ok(Expr::or(parts))
    }

    fn parse_unary(&mut self, context: Context, depth: usize) -> Result<Expr, QueryError> {
        self.check_depth(depth)?;
        if matches!(self.peek(), Some(Token::Not(_))) {
            self.at += 1;
            return self
                .parse_unary(context, depth + 1)
                .map(|part| Expr::Not(Box::new(part)));
        }
        self.parse_atom(context, depth)
    }

    fn parse_atom(&mut self, context: Context, depth: usize) -> Result<Expr, QueryError> {
        self.check_depth(depth)?;
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| QueryError::new(self.end, "运算符或修饰符后缺少查询词"))?;
        self.at += 1;
        match token {
            Token::Open(open, offset) => {
                if matches!(self.peek(), Some(Token::Close(_, _))) {
                    return Err(QueryError::new(offset, "分组不能为空"));
                }
                let expression = self.parse_and(context, depth + 1)?;
                let expected = if open == '(' { ')' } else { '>' };
                match self.peek() {
                    Some(Token::Close(close, _)) if *close == expected => {
                        self.at += 1;
                        Ok(expression)
                    }
                    Some(token) => Err(QueryError::new(
                        token.offset(),
                        format!("此处需要闭合分组 {expected}"),
                    )),
                    None => Err(QueryError::new(
                        offset,
                        format!("分组未闭合，缺少 {expected}"),
                    )),
                }
            }
            Token::Word(word) => self.parse_word(word, context, depth),
            other => Err(QueryError::new(
                other.offset(),
                "此处需要查询词，不能使用空的布尔分支",
            )),
        }
    }

    fn parse_word(
        &mut self,
        word: Word,
        mut context: Context,
        depth: usize,
    ) -> Result<Expr, QueryError> {
        if word.leading_quoted {
            return self.text_term(&word.text, context, word.offset);
        }
        let mut rest = word.text.as_str();
        let mut constraints = Vec::new();
        let mut modifiers = 0;
        let mut needs_value = false;
        loop {
            self.check_depth(depth + modifiers)?;
            let consumed = word.text.len() - rest.len();
            if word.first_quote_at.is_some_and(|at| at <= consumed) {
                break;
            }
            if rest.is_empty() {
                if matches!(self.peek(), Some(Token::Open(_, _))) {
                    constraints.push(self.parse_atom(context, depth + modifiers + 1)?);
                    return Ok(Expr::and(constraints));
                }
                if !constraints.is_empty() && !needs_value {
                    return Ok(Expr::and(constraints));
                }
                return Err(QueryError::new(
                    word.offset,
                    "修饰符缺少值；分组请使用 name:<词1 词2> 形式",
                ));
            }
            if let Some(negative) = rest.strip_prefix('!') {
                let first_quote_at = word
                    .first_quote_at
                    .and_then(|at| at.checked_sub(consumed + 1));
                let inner = Word {
                    text: negative.to_owned(),
                    offset: word.offset,
                    leading_quoted: first_quote_at == Some(0),
                    first_quote_at,
                };
                constraints.push(Expr::Not(Box::new(self.parse_word(
                    inner,
                    context,
                    depth + modifiers + 1,
                )?)));
                return Ok(Expr::and(constraints));
            }
            let Some((prefix, suffix)) = rest.split_once(':') else {
                break;
            };
            if word
                .first_quote_at
                .is_some_and(|at| at <= consumed + prefix.len())
            {
                break;
            }
            let key = prefix.to_ascii_lowercase();
            match key.as_str() {
                "case" => {
                    context.case_sensitive = true;
                    needs_value = true;
                }
                "nocase" => {
                    context.case_sensitive = false;
                    needs_value = true;
                }
                "name" => {
                    context.field = Field::Name;
                    context.explicit_field = true;
                    needs_value = true;
                }
                "path" => {
                    context.field = Field::Path;
                    context.explicit_field = true;
                    needs_value = true;
                }
                "source" => {
                    context.field = Field::Source;
                    context.explicit_field = true;
                    needs_value = true;
                }
                "regex" => {
                    context.mode = Mode::Regex;
                    needs_value = true;
                }
                "pinyin" | "py" => {
                    context.mode = Mode::Pinyin;
                    needs_value = true;
                }
                "pinyin-fuzzy" | "pyf" => {
                    context.mode = Mode::SimilarPinyin;
                    needs_value = true;
                }
                "fuzzy" => {
                    context.mode = Mode::Fuzzy;
                    needs_value = true;
                }
                "typo" => {
                    context.mode = Mode::Typo;
                    needs_value = true;
                }
                "smart" => {
                    context.mode = Mode::Smart;
                    needs_value = true;
                }
                "file" => constraints.push(Expr::Leaf(Leaf::Directory(false))),
                "folder" | "dir" => constraints.push(Expr::Leaf(Leaf::Directory(true))),
                "ext" => {
                    if context.mode != Mode::Text {
                        return Err(QueryError::new(
                            word.offset,
                            "ext: 不支持正则、拼音或模糊修饰符",
                        ));
                    }
                    let mut extensions = Vec::new();
                    for ext in suffix.split(';') {
                        let ext = ext.strip_prefix('.').unwrap_or(ext);
                        if ext.len() > 128
                            || ext
                                .chars()
                                .any(|c| c.is_control() || "/\\:*?\"<>|".contains(c))
                        {
                            return Err(QueryError::new(
                                word.offset,
                                "ext: 需要以分号分隔的扩展名，不接受路径或通配符",
                            ));
                        }
                        extensions.push(fold(ext));
                    }
                    extensions.sort();
                    extensions.dedup();
                    constraints.push(Expr::Leaf(Leaf::Extension(extensions)));
                    return Ok(Expr::and(constraints));
                }
                "size" => {
                    self.require_plain(context, word.offset)?;
                    constraints.push(Expr::Leaf(Leaf::Size(predicates::SizePredicate::parse(
                        suffix,
                        word.offset,
                    )?)));
                    return Ok(Expr::and(constraints));
                }
                "dm" | "mtime" | "date-modified" => {
                    self.require_plain(context, word.offset)?;
                    constraints.push(Expr::Leaf(Leaf::Modified(
                        predicates::DatePredicate::parse(
                            suffix,
                            self.now,
                            word.offset,
                            self.local_dates,
                        )?,
                    )));
                    return Ok(Expr::and(constraints));
                }
                "dc" | "ctime" | "date-created" => {
                    self.require_plain(context, word.offset)?;
                    constraints.push(Expr::Leaf(Leaf::Created(predicates::DatePredicate::parse(
                        suffix,
                        self.now,
                        word.offset,
                        self.local_dates,
                    )?)));
                    return Ok(Expr::and(constraints));
                }
                "attr" | "attrib" | "attributes" => {
                    self.require_plain(context, word.offset)?;
                    constraints.push(Expr::Leaf(Leaf::Attributes(predicates::attribute_mask(
                        suffix,
                        word.offset,
                    )?)));
                    return Ok(Expr::and(constraints));
                }
                "parent" | "ancestor" => {
                    self.require_plain(context, word.offset)?;
                    if suffix.is_empty() || suffix.contains(['*', '?']) {
                        return Err(QueryError::new(
                            word.offset,
                            "目录关系需要明确路径，不接受通配符",
                        ));
                    }
                    if key == "ancestor"
                        && !suffix.starts_with('/')
                        && !crate::is_windows_path(suffix)
                    {
                        return Err(QueryError::new(word.offset, "ancestor: 需要绝对路径"));
                    }
                    constraints.push(Expr::Leaf(Leaf::DirectoryPath {
                        path: crate::comparable_path(suffix, context.case_sensitive),
                        case_sensitive: context.case_sensitive,
                        recursive: key == "ancestor",
                    }));
                    return Ok(Expr::and(constraints));
                }
                "parent-name" | "ancestor-name" => {
                    self.require_plain(context, word.offset)?;
                    if suffix.is_empty() || suffix.contains(['*', '?', '/', '\\']) {
                        return Err(QueryError::new(word.offset, "目录名称关系需要字面名称片段"));
                    }
                    constraints.push(Expr::Leaf(Leaf::DirectoryName {
                        needle: if context.case_sensitive {
                            suffix.to_owned()
                        } else {
                            fold(suffix)
                        },
                        case_sensitive: context.case_sensitive,
                        recursive: key == "ancestor-name",
                    }));
                    return Ok(Expr::and(constraints));
                }
                _ => {
                    if let Some(extensions) = predicates::category_extensions(&key) {
                        self.require_plain(context, word.offset)?;
                        constraints.push(Expr::Leaf(Leaf::Extension(
                            extensions.iter().map(|ext| (*ext).to_owned()).collect(),
                        )));
                        modifiers += 1;
                        rest = suffix;
                        continue;
                    }
                    if prefix.len() == 1
                        && prefix.as_bytes()[0].is_ascii_alphabetic()
                        && (suffix.is_empty() || suffix.starts_with(['/', '\\']))
                    {
                        if !context.explicit_field {
                            context.field = Field::Path;
                        }
                        context.explicit_field = true;
                        break;
                    }
                    // Once an explicit textual function or regex has consumed
                    // its value, colons belong to that value (Windows paths,
                    // regex groups, or literal names), not a guessed function.
                    if context.explicit_field || context.mode == Mode::Regex {
                        break;
                    }
                    return Err(QueryError::new(
                        word.offset,
                        format!("不支持的搜索字段：{prefix}:"),
                    ));
                }
            }
            modifiers += 1;
            rest = suffix;
        }
        constraints.push(self.text_term(rest, context, word.offset)?);
        Ok(Expr::and(constraints))
    }

    fn require_plain(&self, context: Context, offset: usize) -> Result<(), QueryError> {
        if context.mode == Mode::Text {
            Ok(())
        } else {
            Err(QueryError::new(
                offset,
                "该属性暂不支持正则、拼音或模糊修饰符",
            ))
        }
    }

    fn bounded_regex(
        &mut self,
        pattern: &str,
        case_insensitive: bool,
        offset: usize,
    ) -> Result<regex::Regex, QueryError> {
        if pattern.len() > MAX_REGEX_BYTES {
            return Err(QueryError::new(
                offset,
                format!("正则表达式超过 {MAX_REGEX_BYTES} 字节预算"),
            ));
        }
        self.regex_count += 1;
        if self.regex_count > MAX_REGEX_COUNT {
            return Err(QueryError::new(
                offset,
                format!("正则或通配符超过 {MAX_REGEX_COUNT} 项预算"),
            ));
        }
        RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .size_limit(512 * 1024)
            .dfa_size_limit(256 * 1024)
            .nest_limit(32)
            .build()
            .map_err(|error| QueryError::new(offset, format!("正则表达式无效或超出预算：{error}")))
    }

    fn text_term(
        &mut self,
        value: &str,
        mut context: Context,
        offset: usize,
    ) -> Result<Expr, QueryError> {
        if !context.explicit_field && context.mode == Mode::Text && value.contains(['/', '\\']) {
            context.field = Field::Path;
        }
        if matches!(
            context.mode,
            Mode::Pinyin | Mode::SimilarPinyin | Mode::Fuzzy | Mode::Typo | Mode::Smart
        ) && context.field != Field::Name
        {
            return Err(QueryError::new(offset, "拼音与模糊匹配目前仅适用于文件名"));
        }
        let value = if context.field == Field::Path && context.mode != Mode::Regex {
            if !value.starts_with('/') && value.contains('\\') {
                std::borrow::Cow::Owned(value.replace('\\', "/"))
            } else {
                canonical_separators(value)
            }
        } else {
            std::borrow::Cow::Borrowed(value)
        };
        let pattern = match context.mode {
            Mode::Regex => {
                Pattern::Regex(self.bounded_regex(&value, !context.case_sensitive, offset)?)
            }
            Mode::Pinyin | Mode::SimilarPinyin => {
                let needle = fold(&value)
                    .chars()
                    .filter(|c| !c.is_whitespace() && *c != '\'')
                    .collect::<String>();
                let count = needle.chars().count();
                if !(2..=MAX_PHONETIC_CHARS).contains(&count) {
                    return Err(QueryError::new(
                        offset,
                        format!("拼音查询需要 2 至 {MAX_PHONETIC_CHARS} 个字符"),
                    ));
                }
                if needle.contains(['*', '?']) {
                    return Err(QueryError::new(offset, "拼音查询不支持通配符"));
                }
                if context.case_sensitive {
                    return Err(QueryError::new(
                        offset,
                        "拼音查询不区分大小写，不支持 case: 修饰符",
                    ));
                }
                Pattern::Pinyin {
                    needle,
                    similar: context.mode == Mode::SimilarPinyin,
                }
            }
            Mode::Fuzzy => {
                let needle = fold(&value).chars().collect::<Vec<_>>();
                if needle.is_empty() || needle.len() > MAX_PHONETIC_CHARS {
                    return Err(QueryError::new(
                        offset,
                        format!("模糊查询需要 1 至 {MAX_PHONETIC_CHARS} 个字符"),
                    ));
                }
                if context.case_sensitive {
                    return Err(QueryError::new(
                        offset,
                        "模糊查询不区分大小写，不支持 case: 修饰符",
                    ));
                }
                Pattern::Fuzzy(needle)
            }
            Mode::Typo => {
                let needle = fold(&value).chars().collect::<Vec<_>>();
                if needle.is_empty() || needle.len() > MAX_PHONETIC_CHARS {
                    return Err(QueryError::new(
                        offset,
                        format!("容错模糊查询需要 1 至 {MAX_PHONETIC_CHARS} 个字符"),
                    ));
                }
                if context.case_sensitive {
                    return Err(QueryError::new(
                        offset,
                        "容错模糊查询不区分大小写，不支持 case: 修饰符",
                    ));
                }
                Pattern::Typo(needle)
            }
            Mode::Smart => {
                let folded = fold(&value);
                let chars = folded.chars().collect::<Vec<_>>();
                if chars.is_empty() || chars.len() > MAX_PHONETIC_CHARS {
                    return Err(QueryError::new(
                        offset,
                        format!("智能查询需要 1 至 {MAX_PHONETIC_CHARS} 个字符"),
                    ));
                }
                Pattern::Smart {
                    needle: folded,
                    chars,
                }
            }
            Mode::Text if value.contains(['*', '?']) => {
                let mut expression = String::from("(?s:\\A");
                let mut chunks = Vec::new();
                let mut raw_chunk = String::new();
                let flush =
                    |expression: &mut String, chunks: &mut Vec<String>, raw: &mut String| {
                        if !raw.is_empty() {
                            let literal = if context.case_sensitive {
                                std::mem::take(raw)
                            } else {
                                let value = fold(raw);
                                raw.clear();
                                value
                            };
                            expression.push_str(&regex::escape(&literal));
                            chunks.push(literal);
                        }
                    };
                for ch in value.chars() {
                    if matches!(ch, '*' | '?') {
                        flush(&mut expression, &mut chunks, &mut raw_chunk);
                        expression.push_str(if ch == '*' { ".*" } else { "." });
                    } else {
                        raw_chunk.push(ch);
                    }
                }
                flush(&mut expression, &mut chunks, &mut raw_chunk);
                expression.push_str("\\z)");
                let required_literal = chunks.into_iter().max_by_key(|chunk| chunk.chars().count());
                Pattern::Glob {
                    regex: self.bounded_regex(&expression, false, offset)?,
                    case_sensitive: context.case_sensitive,
                    required_literal,
                }
            }
            Mode::Text => Pattern::Literal {
                needle: if context.case_sensitive {
                    value.into_owned()
                } else {
                    fold(&value)
                },
                case_sensitive: context.case_sensitive,
            },
        };
        Ok(Expr::Leaf(Leaf::Text {
            field: context.field,
            pattern,
        }))
    }
}
