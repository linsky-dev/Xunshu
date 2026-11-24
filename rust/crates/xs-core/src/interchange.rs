//! Read-only legacy schema4 and EFU catalog import. Catalog filenames are metadata.
//!
//! Search uses the compact integer size / second timestamp projection. The optional
//! catalog payload separately preserves blank cells, original 100 ns FILETIMEs,
//! creation times, attributes, and custom fields for export. CSV column order and
//! quoting are canonicalized; cell values are not. Catalog paths are never opened.
use crate::{CatalogMetadata, FileRecord, Index, IndexBuilder};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{types::ValueRef, Connection, OpenFlags, Row};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{File, Metadata},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

const FILETIME_EPOCH: i128 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: i128 = 10_000_000;
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_COLUMNS: usize = 256;
// A CSV quote can double its on-disk representation. Bound the logical record
// before csv::Reader can allocate a complete, attacker-controlled field.
const MAX_CSV_RECORD_BYTES: usize = 2 * MAX_METADATA_BYTES + 2 * MAX_COLUMNS + 16;
const STANDARD_COLUMNS: [&str; 5] = [
    "Filename",
    "Size",
    "Date Modified",
    "Date Created",
    "Attributes",
];

fn filetime(input: &str) -> Result<Option<u64>> {
    let text = input.trim();
    if text.is_empty() {
        return Ok(None);
    }
    ensure!(text.len() <= 1024, "EFU date exceeds 1024-byte budget");
    // Parse as u64 first: subtraction from i128::MIN must never overflow.
    let ticks = text
        .parse::<u64>()
        .context("EFU dates must be unsigned 64-bit integer FILETIME ticks")?;
    Ok(Some(ticks))
}

fn filetime_seconds(ticks: u64) -> i64 {
    // Every u64 FILETIME fits in i64 whole Unix seconds, including pre-1970.
    ((i128::from(ticks) - FILETIME_EPOCH).div_euclid(TICKS_PER_SECOND)) as i64
}

fn seconds_filetime(seconds: i64) -> Result<String> {
    let ticks = i128::from(seconds) * TICKS_PER_SECOND + FILETIME_EPOCH;
    Ok(u64::try_from(ticks)
        .context("timestamp cannot be represented by an unsigned 64-bit FILETIME")?
        .to_string())
}

fn optional_size(input: &str) -> Result<Option<u64>> {
    let text = input.trim();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text.parse().context("invalid EFU Size")?))
    }
}

fn attributes(input: &str) -> Result<Option<u32>> {
    let text = input.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let value = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16)
    } else {
        text.parse()
    };
    Ok(Some(
        value.context("invalid unsigned 32-bit EFU Attributes")?,
    ))
}

fn canonical_column(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty() && name.len() <= 4096 && !name.contains('\0'),
        "invalid catalog column name"
    );
    Ok(STANDARD_COLUMNS
        .iter()
        .find(|standard| standard.eq_ignore_ascii_case(name))
        .copied()
        .unwrap_or(name)
        .to_owned())
}

fn empty_fields() -> BTreeMap<String, String> {
    STANDARD_COLUMNS
        .into_iter()
        .map(|name| (name.to_owned(), String::new()))
        .collect()
}

fn canonical_metadata(metadata: CatalogMetadata) -> Result<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    let mut seen = HashSet::new();
    for (name, value) in metadata.fields {
        let name = canonical_column(&name)?;
        ensure!(
            seen.insert(name.to_ascii_lowercase()),
            "duplicate catalog column: {name}"
        );
        fields.insert(name, value);
    }
    ensure!(fields.len() <= MAX_COLUMNS, "catalog exceeds 256 columns");
    Ok(fields)
}

#[derive(Clone, Copy)]
enum CsvState {
    FieldStart,
    Unquoted,
    Quoted,
    AfterQuote,
}

struct CsvBudgetReader<R> {
    inner: R,
    state: CsvState,
    record_bytes: usize,
    record_columns: usize,
}

impl<R: Read> CsvBudgetReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            state: CsvState::FieldStart,
            record_bytes: 0,
            record_columns: 1,
        }
    }

    fn consume(&mut self, byte: u8) -> io::Result<()> {
        self.record_bytes += 1;
        if self.record_bytes > MAX_CSV_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "EFU logical CSV record exceeds byte budget",
            ));
        }
        self.state = match (self.state, byte) {
            (CsvState::FieldStart, b'"') | (CsvState::AfterQuote, b'"') => CsvState::Quoted,
            (CsvState::Quoted, b'"') => CsvState::AfterQuote,
            (CsvState::Quoted, _) => CsvState::Quoted,
            (_, b'\r' | b'\n') => {
                self.record_bytes = 0;
                self.record_columns = 1;
                CsvState::FieldStart
            }
            (_, b',') => {
                self.record_columns += 1;
                if self.record_columns > MAX_COLUMNS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "EFU logical CSV record exceeds 256 columns",
                    ));
                }
                CsvState::FieldStart
            }
            _ => CsvState::Unquoted,
        };
        Ok(())
    }
}

impl<R: Read> Read for CsvBudgetReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let length = self.inner.read(output)?;
        for byte in &output[..length] {
            self.consume(*byte)?;
        }
        if length == 0 && !output.is_empty() && matches!(self.state, CsvState::Quoted) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unterminated quoted EFU field",
            ));
        }
        Ok(length)
    }
}

fn unchanged(before: &Metadata, file: &File, label: &str) -> Result<()> {
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "{label} changed during import; no result was applied"
    );
    Ok(())
}

pub fn import_efu(path: &Path, source: &str, generation: u64) -> Result<Index> {
    xs_platform::validate_local_path(path, false)?;
    ensure!(
        source.starts_with("offline:")
            && source.len() > 8
            && source.len() <= 4096
            && !source.contains('\0'),
        "EFU source must begin offline: and fit the source-name budget"
    );
    let file = xs_platform::open_local_read(path)?;
    let before = file.metadata()?;
    ensure!(
        before.len() <= 2 * 1024 * 1024 * 1024,
        "EFU exceeds 2 GiB budget"
    );
    let mut input = BufReader::new(file);
    let builder = parse_efu(&mut input, source)?;
    unchanged(&before, input.get_ref(), "EFU")?;
    builder.finish(generation)
}

fn parse_efu(mut input: impl BufRead, source: &str) -> Result<IndexBuilder> {
    // Strip the optional UTF-8 BOM before both parsers see the first quote.
    if input.fill_buf()?.starts_with(&[0xef, 0xbb, 0xbf]) {
        input.consume(3);
    }
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(CsvBudgetReader::new(input));
    let mut headers = Vec::new();
    let mut seen_columns = HashSet::new();
    for name in reader.headers()?.iter() {
        let name = canonical_column(name)?;
        ensure!(
            seen_columns.insert(name.to_ascii_lowercase()),
            "duplicate EFU column: {name}"
        );
        headers.push(name);
    }
    ensure!(headers.len() <= MAX_COLUMNS, "EFU exceeds 256 columns");
    ensure!(
        headers.iter().any(|name| name == "Filename"),
        "missing EFU Filename column"
    );
    let mut builder = IndexBuilder::default();
    let mut seen_paths = HashSet::new();
    for row in reader.records() {
        let row = row.context("invalid EFU CSV record")?;
        let mut fields = empty_fields();
        for (name, value) in headers.iter().zip(row.iter()) {
            fields.insert(name.clone(), value.to_owned());
        }
        let record = efu_record(&fields, source)?;
        ensure!(
            seen_paths.insert(record.path.clone()),
            "duplicate EFU path: {}",
            record.path
        );
        builder.add_with_metadata(record, Some(CatalogMetadata { fields }))?;
    }
    Ok(builder)
}

/// 使用与磁盘导入相同的 CSV 解析和索引构建流程，供本地模糊测试使用。
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_efu(input: &[u8]) -> Result<Index> {
    parse_efu(input, "offline:fuzz")?.finish(1)
}

fn efu_record(fields: &BTreeMap<String, String>, source: &str) -> Result<FileRecord> {
    let path = &fields["Filename"];
    ensure!(
        !path.is_empty() && path.len() <= 32768 && !path.contains('\0'),
        "EFU 文件名无效或过长"
    );
    let size = optional_size(&fields["Size"])?;
    let modified = filetime(&fields["Date Modified"])?;
    filetime(&fields["Date Created"])?;
    let is_dir = match attributes(&fields["Attributes"])? {
        Some(value) => value & 16 != 0,
        None => crate::index::split_path(path)?.1.is_empty(),
    };
    Ok(FileRecord {
        path: path.clone(),
        size: size.unwrap_or(0),
        modified: modified.map(filetime_seconds).unwrap_or(0),
        is_dir,
        source: source.to_owned(),
    })
}

/// 更新或删除一行，使用原始文件版本作为审核凭据，只创建新的输出文件。
pub fn edit_efu(
    input: &Path,
    output: &Path,
    expected: &xs_platform::FileRevision,
    row: usize,
    updates: Option<&[(String, String)]>,
) -> Result<xs_platform::LocalWriteOutcome> {
    const SOURCE: &str = "offline:edited";
    ensure!(
        xs_platform::FileRevision::at_path(input)? == *expected,
        "EFU 清单在审核后已变化，请重新检查"
    );
    let original = import_efu(input, SOURCE, 1)?;
    ensure!((1..=original.len()).contains(&row), "EFU 行号不存在");
    let target = (row - 1) as u32;
    let replacement = updates
        .map(|updates| -> Result<_> {
            let mut fields = export_fields(&original, target)?;
            let mut seen = HashSet::new();
            for (name, value) in updates {
                let name = canonical_column(name)?;
                let folded = name.to_ascii_lowercase();
                ensure!(seen.insert(folded.clone()), "EFU 列重复修改：{name}");
                if let Some(existing) = fields
                    .keys()
                    .find(|key| key.eq_ignore_ascii_case(&name))
                    .cloned()
                {
                    fields.remove(&existing);
                    fields.insert(existing, value.clone());
                } else {
                    fields.insert(name, value.clone());
                }
            }
            let record = efu_record(&fields, SOURCE)?;
            Ok((record, CatalogMetadata { fields }))
        })
        .transpose()?;
    let mut builder = IndexBuilder::default();
    for id in 0..original.len() as u32 {
        if id == target {
            if let Some((record, metadata)) = &replacement {
                builder.add_with_metadata(record.clone(), Some(metadata.clone()))?;
            }
            continue;
        }
        if let Some((record, _)) = &replacement {
            ensure!(
                original.record(id)?.path != record.path,
                "EFU 修改会产生重复路径"
            );
        }
        builder.add_from_index(&original, id)?;
    }
    let edited = builder.finish(1)?;
    Ok(xs_platform::write_local_new(output, |file| {
        export_efu(&edited, file).map_err(io::Error::other)?;
        let current = xs_platform::FileRevision::at_path(input).map_err(io::Error::other)?;
        if current != *expected {
            return Err(io::Error::other("写入期间原 EFU 清单已变化"));
        }
        Ok(())
    })?)
}

fn export_fields(index: &Index, id: u32) -> Result<BTreeMap<String, String>> {
    let record = index.record(id)?;
    let mut fields = index
        .record_metadata(id)?
        .map(canonical_metadata)
        .transpose()?
        .unwrap_or_default();
    // Check presence, not nonemptiness: an original blank must remain blank.
    fields.entry("Filename".to_owned()).or_insert(record.path);
    fields
        .entry("Size".to_owned())
        .or_insert_with(|| record.size.to_string());
    if !fields.contains_key("Date Modified") {
        fields.insert(
            "Date Modified".to_owned(),
            seconds_filetime(record.modified)?,
        );
    }
    fields.entry("Date Created".to_owned()).or_default();
    fields
        .entry("Attributes".to_owned())
        .or_insert_with(|| if record.is_dir { "16" } else { "0" }.to_owned());
    ensure!(
        fields.len() <= MAX_COLUMNS,
        "catalog exceeds 256 columns including standard EFU fields"
    );
    Ok(fields)
}

pub fn export_efu(index: &Index, mut output: impl Write) -> Result<()> {
    // Preflight all rows before emitting bytes. Keep only column names, not all
    // records, so heterogeneous catalogs do not require a second in-memory copy.
    let mut custom = BTreeMap::<String, String>::new();
    for id in 0..index.len() as u32 {
        for name in export_fields(index, id)?.into_keys() {
            if STANDARD_COLUMNS.contains(&name.as_str()) {
                continue;
            }
            if let Some(existing) = custom.insert(name.to_ascii_lowercase(), name.clone()) {
                ensure!(
                    existing == name,
                    "ambiguous catalog column spellings: {existing} / {name}"
                );
            }
        }
        ensure!(
            custom.len() + STANDARD_COLUMNS.len() <= MAX_COLUMNS,
            "export union exceeds 256 columns"
        );
    }
    let mut headers = STANDARD_COLUMNS
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    headers.extend(custom.into_values().collect::<BTreeSet<_>>());
    let mut writer = csv::WriterBuilder::new().from_writer(&mut output);
    writer.write_record(&headers)?;
    for id in 0..index.len() as u32 {
        let mut fields = export_fields(index, id)?;
        writer.write_record(
            headers
                .iter()
                .map(|name| fields.remove(name).unwrap_or_default()),
        )?;
    }
    writer.flush()?;
    Ok(())
}

fn check_wal(path: &Path) -> Result<()> {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    let wal = PathBuf::from(name);
    match std::fs::symlink_metadata(&wal) {
        Ok(_) => {
            xs_platform::validate_local_path(&wal, false)?;
            ensure!(
                xs_platform::open_local_read(&wal)?.metadata()?.len() == 0,
                "旧数据库存在未合并的 WAL；请先退出旧程序并完成 checkpoint，再导入副本。"
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn table_columns(db: &Connection, table: &str) -> Result<Option<HashSet<String>>> {
    // Callers supply only these static table names, never external SQL text.
    ensure!(
        matches!(table, "files" | "filelists"),
        "invalid internal table name"
    );
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let mut statement = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .map(|name| name.map(|name| name.to_ascii_lowercase()))
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(Some(columns))
}

fn nullable_text(
    row: &Row<'_>,
    column: usize,
    limit: usize,
    label: &str,
) -> Result<Option<String>> {
    match row.get_ref(column)? {
        ValueRef::Null => Ok(None),
        ValueRef::Text(value) => {
            ensure!(value.len() <= limit, "legacy {label} exceeds byte budget");
            Ok(Some(
                std::str::from_utf8(value)
                    .with_context(|| format!("invalid UTF-8 in legacy {label}"))?
                    .to_owned(),
            ))
        }
        _ => bail!("legacy {label} is not TEXT or NULL"),
    }
}

fn legacy_seconds(value: Option<f64>) -> Result<Option<i64>> {
    value
        .map(|number| {
            ensure!(
                number.is_finite() && number >= i64::MIN as f64 && number < i64::MAX as f64,
                "invalid legacy timestamp"
            );
            Ok(number.floor() as i64)
        })
        .transpose()
}

fn legacy_date(raw: Option<String>, seconds: Option<f64>) -> Result<(String, Option<i64>)> {
    let projected = legacy_seconds(seconds)?;
    if let Some(raw) = raw {
        ensure!(
            raw.len() <= 1024 && !raw.contains('\0'),
            "invalid legacy date text"
        );
        let text = raw.trim();
        let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
        if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            // Raw integer precision takes precedence over the lossy REAL column.
            let ticks = filetime(text)?.context("missing legacy FILETIME")?;
            return Ok((raw, Some(filetime_seconds(ticks))));
        }
        // Older schema4 records may contain calendar-form text. Preserve it
        // exactly; their REAL column remains the search projection.
        return Ok((raw, projected));
    }
    match (seconds, projected) {
        (Some(value), Some(projected)) => {
            // No original ticks exist. Preserve the precision available in REAL.
            let ticks = (value * TICKS_PER_SECOND as f64).round();
            ensure!(
                ticks.is_finite() && ticks >= i128::MIN as f64 && ticks < i128::MAX as f64,
                "invalid legacy FILETIME projection"
            );
            let ticks = (ticks as i128)
                .checked_add(FILETIME_EPOCH)
                .context("legacy FILETIME overflow")?;
            Ok((
                u64::try_from(ticks)
                    .context("legacy timestamp outside FILETIME range")?
                    .to_string(),
                Some(projected),
            ))
        }
        _ => Ok((String::new(), None)),
    }
}

fn skip_space(bytes: &[u8], at: &mut usize) {
    while bytes
        .get(*at)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        *at += 1;
    }
}

fn json_string_end(bytes: &[u8], start: usize) -> Result<usize> {
    ensure!(bytes.get(start) == Some(&b'"'), "expected JSON string");
    let mut at = start + 1;
    while let Some(byte) = bytes.get(at) {
        match byte {
            b'"' => return Ok(at + 1),
            b'\\' => at += 2,
            _ => at += 1,
        }
    }
    bail!("unterminated JSON string")
}

fn extra_fields(text: &str) -> Result<BTreeMap<String, String>> {
    ensure!(
        text.len() <= MAX_METADATA_BYTES,
        "legacy extra_json exceeds 1 MiB"
    );
    let bytes = text.as_bytes();
    let mut at = 0;
    skip_space(bytes, &mut at);
    ensure!(
        bytes.get(at) == Some(&b'{'),
        "legacy extra_json must be an object"
    );
    at += 1;
    let mut fields = BTreeMap::new();
    let mut seen = HashSet::new();
    skip_space(bytes, &mut at);
    if bytes.get(at) != Some(&b'}') {
        loop {
            let end = json_string_end(bytes, at)?;
            let name: String = serde_json::from_str(&text[at..end])?;
            let name = canonical_column(&name)?;
            ensure!(
                !STANDARD_COLUMNS.contains(&name.as_str()),
                "extra_json shadows standard EFU column: {name}"
            );
            ensure!(
                seen.insert(name.to_ascii_lowercase()),
                "duplicate legacy extra_json column: {name}"
            );
            at = end;
            skip_space(bytes, &mut at);
            ensure!(
                bytes.get(at) == Some(&b':'),
                "expected colon in legacy extra_json"
            );
            at += 1;
            skip_space(bytes, &mut at);
            let start = at;
            let mut depth = 0usize;
            while let Some(byte) = bytes.get(at) {
                match byte {
                    b'"' => at = json_string_end(bytes, at)?,
                    b'[' | b'{' => {
                        depth += 1;
                        ensure!(depth <= 64, "legacy extra_json nesting exceeds budget");
                        at += 1;
                    }
                    b']' | b'}' if depth > 0 => {
                        depth -= 1;
                        at += 1;
                    }
                    b',' | b'}' if depth == 0 => break,
                    _ => at += 1,
                }
            }
            let raw = text[start..at].trim();
            let value = match serde_json::from_str::<serde_json::Value>(raw)? {
                serde_json::Value::String(value) => value,
                serde_json::Value::Null => String::new(),
                _ => raw.to_owned(),
            };
            fields.insert(name, value);
            ensure!(
                fields.len() + STANDARD_COLUMNS.len() <= MAX_COLUMNS,
                "legacy catalog exceeds 256 columns"
            );
            match bytes.get(at) {
                Some(b',') => {
                    at += 1;
                    skip_space(bytes, &mut at);
                }
                Some(b'}') => break,
                _ => bail!("unterminated legacy extra_json object"),
            }
        }
    }
    ensure!(
        bytes.get(at) == Some(&b'}'),
        "unterminated legacy extra_json object"
    );
    at += 1;
    skip_space(bytes, &mut at);
    ensure!(at == bytes.len(), "trailing data after legacy extra_json");
    Ok(fields)
}

/// Legacy metadata is deliberately tagged offline. It never grants permission to
/// scan, read content, or open any filename recorded in the old database.
pub fn import_schema4(path: &Path, generation: u64) -> Result<Index> {
    xs_platform::validate_local_path(path, false)?;
    check_wal(path)?;
    let pinned = xs_platform::open_local_read(path)?;
    let before = pinned.metadata()?;
    #[cfg(target_os = "linux")]
    let uri = {
        use std::os::fd::AsRawFd;
        format!(
            "file:/proc/self/fd/{}?mode=ro&immutable=1",
            pinned.as_raw_fd()
        )
    };
    #[cfg(not(target_os = "linux"))]
    let uri = {
        let raw = path
            .to_str()
            .context("database path is not UTF-8")?
            .replace('\\', "/");
        let mut encoded = String::new();
        for byte in raw.bytes() {
            if byte.is_ascii_alphanumeric() || b"/:-._~".contains(&byte) {
                encoded.push(byte as char)
            } else {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
        format!("file:{encoded}?mode=ro&immutable=1")
    };
    let db = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version == 4,
        "only schema4 databases are supported (found {version})"
    );
    let columns = table_columns(&db, "files")?.context("missing legacy files table")?;
    for required in ["id", "path", "size", "mtime", "is_dir", "source"] {
        ensure!(
            columns.contains(required),
            "missing legacy files.{required} column"
        );
    }
    let optional = |name: &str| {
        if columns.contains(name) {
            format!("f.{name}")
        } else {
            "NULL".to_owned()
        }
    };
    let filter = if let Some(columns) = table_columns(&db, "filelists")? {
        ensure!(
            columns.contains("source") && columns.contains("enabled"),
            "invalid legacy filelists schema"
        );
        " WHERE NOT EXISTS (SELECT 1 FROM filelists AS l WHERE l.source=f.source AND l.enabled=0)"
    } else {
        ""
    };
    let query = format!(
        "SELECT f.path,f.size,f.mtime,f.is_dir,f.source,{},{},{},{},{} FROM files AS f{} ORDER BY f.id",
        optional("mtime_filetime"), optional("birthtime_filetime"), optional("attributes"), optional("extra_json"), optional("birthtime"), filter,
    );
    let mut statement = db.prepare(&query)?;
    let mut rows = statement.query([])?;
    let mut builder = IndexBuilder::default();
    let mut seen = HashSet::new();
    while let Some(row) = rows.next()? {
        let path = nullable_text(row, 0, 32768, "path")?.context("NULL legacy filename")?;
        ensure!(
            !path.is_empty() && !path.contains('\0'),
            "invalid legacy filename"
        );
        let source = nullable_text(row, 4, 4096 - "offline:schema4:".len(), "source")?
            .context("NULL legacy source")?;
        ensure!(!source.contains('\0'), "invalid legacy source");
        ensure!(
            seen.insert((source.clone(), path.clone())),
            "duplicate legacy source/path"
        );
        let size: Option<i64> = row.get(1)?;
        ensure!(
            size.is_none_or(|value| value >= 0),
            "negative legacy file size"
        );
        let modified: Option<f64> = row.get(2)?;
        let is_dir: i64 = row.get(3)?;
        ensure!(matches!(is_dir, 0 | 1), "invalid legacy is_dir value");
        let raw_modified = nullable_text(row, 5, 1024, "mtime_filetime")?;
        let raw_created = nullable_text(row, 6, 1024, "birthtime_filetime")?;
        let attributes: Option<i64> = row.get(7)?;
        ensure!(
            attributes.is_none_or(|value| (0..=u32::MAX as i64).contains(&value)),
            "invalid legacy attributes"
        );
        let extra = nullable_text(row, 8, MAX_METADATA_BYTES, "extra_json")?;
        let created: Option<f64> = row.get(9)?;
        let (raw_modified, projected) = legacy_date(raw_modified, modified)?;
        let (raw_created, _) = legacy_date(raw_created, created)?;
        let mut fields = match extra {
            Some(text) => extra_fields(&text)?,
            None => BTreeMap::new(),
        };
        fields.insert("Filename".to_owned(), path.clone());
        fields.insert(
            "Size".to_owned(),
            size.map(|value| value.to_string()).unwrap_or_default(),
        );
        fields.insert("Date Modified".to_owned(), raw_modified);
        fields.insert("Date Created".to_owned(), raw_created);
        fields.insert(
            "Attributes".to_owned(),
            attributes
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        builder.add_with_metadata(
            FileRecord {
                path,
                size: size.unwrap_or(0) as u64,
                modified: projected.unwrap_or(0),
                is_dir: is_dir != 0,
                source: format!("offline:schema4:{source}"),
            },
            Some(CatalogMetadata { fields }),
        )?;
    }
    unchanged(&before, &pinned, "legacy database")?;
    check_wal(path)?;
    builder.finish(generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn efu_windows_and_network_names_never_opened() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.efu");
        std::fs::write(&path, "Filename,Size,Date Modified,Date Created,Attributes\n\"C:\\资料\\报告.docx\",12,133444736000000000,,0\n\"\\\\server\\share\\secret.pdf\",20,,,0\n").unwrap();
        let index = import_efu(&path, "offline:test", 1).unwrap();
        assert_eq!(index.len(), 2);
        assert_eq!(index.record(0).unwrap().path, "C:\\资料\\报告.docx");
        assert!(index.record(1).unwrap().source.starts_with("offline:"));
    }

    #[test]
    fn schema4_read_only_roundtrip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.db");
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA user_version=4; CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT,size INTEGER,mtime REAL,is_dir INTEGER,source TEXT); INSERT INTO files VALUES(1,'C:\\docs\\report.pdf',10,1700000000,0,'local');").unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let index = import_schema4(&path, 1).unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index.record(0).unwrap().source, "offline:schema4:local");
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
}
