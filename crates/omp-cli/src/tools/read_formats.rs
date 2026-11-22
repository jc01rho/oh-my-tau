//! Non-text `read` formats: magic-byte/extension detection, SQLite selectors,
//! archive browsing (tar/tar.gz/tgz/zip), JSON `?q=` queries, `.ipynb`
//! notebooks, binary metadata/strings/hexdump, a PDF metadata + text fallback,
//! and the `pi-ast` structural-summary / block helpers.
//!
//! Ports the format half of omp's `read.ts` dispatcher and its satellites
//! (`read-sqlite.ts`, `sqlite-reader.ts`, `read-archive.ts`, `read-json.ts`,
//! `read-binary.ts`, `read-pdf.ts`, `read-summary.ts`, `read-format.ts`) into
//! one dependency-light module. Everything here is a pure function over bytes
//! or a path: `read.rs` owns selectors, hashline rows and result assembly.
//!
//! Deliberate scope limits, each documented at its implementation:
//! - No `zip`/`tar`/`jq`/`pdf` crates. ZIP is a central-directory parser over
//!   `flate2`, tar is a 512-byte-header reader, `?q=` is a jq subset, and PDF is
//!   a literal-string scan over inflated content streams.
//! - Archives are fully buffered (they are read-once) with a decompression cap.
//! - Binary disassembly views (`:overview`, `:imports`, ...) need an IDA/Ghidra
//!   backend that does not exist in this build, so only on-disk metadata,
//!   strings and hexdump are served.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

// --------------------------------------------------------------- constants

/// Inline budget shared with `util::DEFAULT_MAX_BYTES`.
const MAX_BYTES: usize = 50 * 1024;
const MAX_RENDER_WIDTH: usize = 120;
const MAX_COLUMN_WIDTH: usize = 40;
const MIN_COLUMN_WIDTH: usize = 3;
const MIN_CELL_WIDTH: usize = 1;
const COLUMN_SEPARATOR_WIDTH: usize = 3;
const TABLE_FRAME_WIDTH: usize = 1;

const DEFAULT_SQLITE_LIMIT: usize = 20;
const MAX_SQLITE_LIMIT: usize = 500;
const DEFAULT_SCHEMA_SAMPLE_LIMIT: usize = 5;
/// Row cap for raw `?q=SELECT ...` reads (protects against `SELECT *`).
const MAX_RAW_QUERY_ROWS: usize = 1000;
/// Bounded `COUNT(*)` probe: a table larger than this reports a lower bound.
const ROW_COUNT_PROBE_CAP: u64 = 50_000;

const DEFAULT_JSON_LIMIT: usize = 100;
const MAX_JSON_LIMIT: usize = 1000;

/// Cap for gunzipped tar payloads and archive member inflation (zip bombs).
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PDF_TEXT_BYTES: usize = 256 * 1024;
const MAX_STRINGS: usize = 200;

const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

/// ZIP-container document extensions: a bare read converts them, `name:memb`
/// reads a member. All are ZIP archives on disk.
const ZIP_CONTAINER_EXTS: [&str; 13] = [
    ".docx", ".docm", ".dotx", ".xlsx", ".xlsm", ".pptx", ".pptm", ".odt", ".ods", ".odp", ".epub",
    ".jar", ".whl",
];

// -------------------------------------------------------- format detection

/// A file's on-disk format, as far as `read` cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    /// `.jsonl` / `.ndjson`: one JSON value per line.
    JsonLines,
    Notebook,
    Sqlite,
    Zip,
    Tar,
    TarGz,
    Gzip,
    Pdf,
    Image,
    Binary,
}

impl Format {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Format::Text => "text",
            Format::Json => "json",
            Format::JsonLines => "jsonl",
            Format::Notebook => "notebook",
            Format::Sqlite => "sqlite",
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::Gzip => "gzip",
            Format::Pdf => "pdf",
            Format::Image => "image",
            Format::Binary => "binary",
        }
    }
}

/// Content-sniffed format from the leading bytes (omp `sniffArchiveFormat`,
/// `parseImageMetadata`, `isSqliteFile`).
///
/// Magic wins over the extension for every container, because a `.bin` holding
/// an SQLite database must still open as one.
#[must_use]
pub fn sniff_format(head: &[u8]) -> Option<Format> {
    if head.starts_with(SQLITE_MAGIC) {
        return Some(Format::Sqlite);
    }
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Some(Format::Zip);
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return Some(Format::Gzip);
    }
    if head.starts_with(b"%PDF-") {
        return Some(Format::Pdf);
    }
    if head.starts_with(b"ustar") {
        return Some(Format::Tar);
    }
    if image_mime(head).is_some() {
        return Some(Format::Image);
    }
    None
}

/// Image MIME type from magic bytes (`parseImageMetadata` header sniff).
#[must_use]
pub fn image_mime(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if head.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if head.starts_with(b"II\x2a\x00") || head.starts_with(b"MM\x00\x2a") {
        return Some("image/tiff");
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let brand = &head[8..12];
        if brand == b"heic" || brand == b"heix" || brand == b"mif1" || brand == b"msf1" {
            return Some("image/heic");
        }
    }
    None
}

/// Format named by the path's extension (omp `archiveFormatFromPath` and the
/// JSON/notebook extension sets).
#[must_use]
pub fn extension_format(path: &Path) -> Option<Format> {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().to_ascii_lowercase());
    if name.is_empty() {
        return None;
    }
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.ends_with(".tar.zst") {
        return Some(Format::TarGz);
    }
    if name.ends_with(".tar") {
        return Some(Format::Tar);
    }
    if name.ends_with(".zip") || ZIP_CONTAINER_EXTS.iter().any(|e| name.ends_with(e)) {
        return Some(Format::Zip);
    }
    if name.ends_with(".ipynb") {
        return Some(Format::Notebook);
    }
    if name.ends_with(".jsonl") || name.ends_with(".ndjson") {
        return Some(Format::JsonLines);
    }
    if name.ends_with(".json") || name.contains(".json.") {
        return Some(Format::Json);
    }
    if name.ends_with(".sqlite")
        || name.ends_with(".sqlite3")
        || name.ends_with(".db")
        || name.ends_with(".db3")
    {
        return Some(Format::Sqlite);
    }
    if name.ends_with(".pdf") {
        return Some(Format::Pdf);
    }
    if name.ends_with(".gz") {
        return Some(Format::Gzip);
    }
    None
}

/// Format for a path plus its leading bytes: magic first, extension second,
/// [`Format::Text`] when neither speaks.
#[must_use]
pub fn detect_format(path: &Path, head: &[u8]) -> Format {
    if extension_format(path) == Some(Format::Notebook) {
        return Format::Notebook;
    }
    if let Some(magic) = sniff_format(head) {
        // A `.gz` that is not a tar is still a gzip stream, but a `.json`
        // sniffed as text stays JSON; only containers override the extension.
        if !matches!(magic, Format::Image) || extension_format(path) != Some(Format::Pdf) {
            return magic;
        }
    }
    extension_format(path).unwrap_or(Format::Text)
}

// -------------------------------------------------------------- small utils

/// omp `formatBytes`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn fmt_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Char-count truncation with an ellipsis (no `unicode-width` dependency, so
/// wide CJK glyphs count as one cell — the ASCII table only needs consistency).
fn truncate_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// omp `sanitizeCell`: tabs expand, embedded newlines become a literal `\n`.
fn sanitize_cell(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\t' => out.push_str("    "),
            '\r' => {}
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

fn pad_cell(value: &str, width: usize) -> String {
    let cell = truncate_width(&sanitize_cell(value), width.max(MIN_CELL_WIDTH));
    let visible = cell.chars().count();
    if visible >= width {
        cell
    } else {
        format!("{cell}{}", " ".repeat(width - visible))
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode a query component. `+` becomes a space (matching
/// `URLSearchParams`), so SQL fragments can be written readably; `%2B` keeps a
/// literal plus.
#[must_use]
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 3 <= bytes.len() => {
                match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi * 16 + lo);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a query value for a followable continuation hint.
#[must_use]
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'='
            | b'!'
            | b'*'
            | b'\''
            | b'('
            | b')'
            | b'/'
            | b' '
            | b'<'
            | b'>' => out.push(b as char),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

fn parse_query_params(query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return Vec::new();
    }
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn has_param(params: &[(String, String)], name: &str) -> bool {
    params.iter().any(|(key, _)| key == name)
}

// ------------------------------------------------------------------ SQLite

/// Parsed SQLite selector (`parseSqliteSelector`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteSelector {
    /// No sub-path: list tables.
    List,
    /// `db.sqlite:table`: schema plus sample rows.
    Schema { table: String, sample_limit: usize },
    /// `db.sqlite:table:key`: one row by primary key or ROWID.
    Row { table: String, key: String },
    /// `db.sqlite:table?where=&order=&limit=&offset=`.
    Query {
        table: String,
        limit: usize,
        offset: usize,
        order: Option<String>,
        filter: Option<String>,
    },
    /// `db.sqlite?q=SELECT ...`.
    Raw { sql: String, page: Option<SqlitePage> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlitePage {
    pub limit: usize,
    pub offset: usize,
}

const COMMENT_OR_TERMINATOR_ERROR: &str =
    "SQLite 'where' clause must not contain comments or statement terminators; use '?q=SELECT ...' for raw SQL";
const FORBIDDEN_KEYWORD_ERROR: &str =
    "SQLite 'where' clause must not contain LIMIT/OFFSET/UNION/INTERSECT/EXCEPT/ATTACH/DETACH/PRAGMA; use '?q=SELECT ...' for raw SQL";

const FORBIDDEN_WHERE_KEYWORDS: [&str; 8] = [
    "limit", "offset", "union", "intersect", "except", "attach", "detach", "pragma",
];

/// Scan a `where=` clause, tracking quoted literals, and reject the SQL
/// control syntax that would escape the bound `LIMIT ? OFFSET ?` pagination.
fn find_where_violation(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut token_start: Option<usize> = None;
    let mut keyword_violation = false;
    let mut i = 0;
    while i <= bytes.len() {
        let ch = bytes.get(i).copied();
        let next = bytes.get(i + 1).copied();
        if in_single {
            if ch == Some(b'\'') && next == Some(b'\'') {
                i += 2;
                continue;
            }
            if ch == Some(b'\'') {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if ch == Some(b'"') && next == Some(b'"') {
                i += 2;
                continue;
            }
            if ch == Some(b'"') {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if ch.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
            if token_start.is_none() {
                token_start = Some(i);
            }
            i += 1;
            continue;
        }
        if let Some(start) = token_start.take() {
            let token = String::from_utf8_lossy(&bytes[start..i]).to_ascii_lowercase();
            if FORBIDDEN_WHERE_KEYWORDS.contains(&token.as_str()) {
                keyword_violation = true;
            }
        }
        let Some(ch) = ch else { break };
        match ch {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b';' => return Some(COMMENT_OR_TERMINATOR_ERROR.to_string()),
            b'-' if next == Some(b'-') => return Some(COMMENT_OR_TERMINATOR_ERROR.to_string()),
            b'/' if next == Some(b'*') => return Some(COMMENT_OR_TERMINATOR_ERROR.to_string()),
            b'*' if next == Some(b'/') => return Some(COMMENT_OR_TERMINATOR_ERROR.to_string()),
            _ => {}
        }
        i += 1;
    }
    keyword_violation.then(|| FORBIDDEN_KEYWORD_ERROR.to_string())
}

fn validate_where(filter: Option<&str>) -> Result<Option<String>> {
    let Some(filter) = filter else {
        return Ok(None);
    };
    let trimmed = filter.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Some(violation) = find_where_violation(trimmed) {
        bail!(violation);
    }
    Ok(Some(trimmed.to_string()))
}

fn parse_sqlite_limit(value: Option<&str>, fallback: usize) -> Result<usize> {
    let Some(value) = value else {
        return Ok(fallback);
    };
    if value.trim().is_empty() {
        return Ok(fallback);
    }
    let parsed: usize = value
        .trim()
        .parse()
        .with_context(|| format!("SQLite limit must be a positive integer; got '{value}'"))?;
    if parsed < 1 {
        bail!("SQLite limit must be a positive integer; got '{value}'");
    }
    Ok(parsed.min(MAX_SQLITE_LIMIT))
}

fn parse_sqlite_offset(value: Option<&str>) -> Result<usize> {
    let Some(value) = value else {
        return Ok(0);
    };
    if value.trim().is_empty() {
        return Ok(0);
    }
    value
        .trim()
        .parse()
        .with_context(|| format!("SQLite offset must be a non-negative integer; got '{value}'"))
}

/// Split `:table`, `:table:key` and the `?query` tail (omp `parseSqliteSelector`).
///
/// # Errors
/// Unknown parameters, a row lookup combined with parameters, a `?q=` query
/// with a table selector, or an unsafe `where` clause.
pub fn parse_sqlite_selector(sub_path: &str, query_string: &str) -> Result<SqliteSelector> {
    let sub_path = sub_path.trim_start_matches(':').trim();
    let params = parse_query_params(query_string);

    if let Some(raw) = param(&params, "q") {
        if !sub_path.is_empty() {
            bail!("SQLite ?q= queries run against the whole database; drop ':{sub_path}' from the path");
        }
        for (key, _) in &params {
            if key != "q" && key != "limit" && key != "offset" {
                bail!("SQLite ?q= queries take only limit and offset; put '{key}' into the SQL");
            }
        }
        if raw.trim().is_empty() {
            bail!("SQLite query parameter 'q' cannot be empty");
        }
        let page = if has_param(&params, "limit") || has_param(&params, "offset") {
            Some(SqlitePage {
                limit: parse_sqlite_limit(param(&params, "limit"), DEFAULT_SQLITE_LIMIT)?,
                offset: parse_sqlite_offset(param(&params, "offset"))?,
            })
        } else {
            None
        };
        return Ok(SqliteSelector::Raw {
            sql: raw.to_string(),
            page,
        });
    }

    if sub_path.is_empty() {
        if !params.is_empty() {
            bail!("SQLite query parameters require a table selector or q=SELECT...");
        }
        return Ok(SqliteSelector::List);
    }

    let (table, key) = match sub_path.split_once(':') {
        Some((table, key)) => (table.trim(), Some(key)),
        None => (sub_path, None),
    };
    if table.is_empty() {
        bail!("SQLite selectors must include a table name");
    }

    if let Some(key) = key.filter(|k| !k.is_empty()) {
        if !params.is_empty() {
            bail!("SQLite row lookups cannot be combined with query parameters");
        }
        return Ok(SqliteSelector::Row {
            table: table.to_string(),
            key: key.to_string(),
        });
    }

    let filter = validate_where(param(&params, "where"))?;
    let order = param(&params, "order").map(str::trim).filter(|o| !o.is_empty()).map(str::to_string);
    let has_query_params = has_param(&params, "limit")
        || has_param(&params, "offset")
        || order.is_some()
        || filter.is_some();
    if has_query_params {
        for (key, _) in &params {
            if !matches!(key.as_str(), "limit" | "offset" | "order" | "where") {
                bail!("Unsupported SQLite query parameter '{key}'");
            }
        }
        return Ok(SqliteSelector::Query {
            table: table.to_string(),
            limit: parse_sqlite_limit(param(&params, "limit"), DEFAULT_SQLITE_LIMIT)?,
            offset: parse_sqlite_offset(param(&params, "offset"))?,
            order,
            filter,
        });
    }
    if let Some((key, _)) = params.first() {
        bail!("Unsupported SQLite query parameter '{key}'");
    }
    Ok(SqliteSelector::Schema {
        table: table.to_string(),
        sample_limit: DEFAULT_SCHEMA_SAMPLE_LIMIT,
    })
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn open_sqlite_readonly(path: &Path) -> Result<rusqlite::Connection> {
    let conn = match rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(conn) => conn,
        // A WAL database without its sidecars refuses a read-only open; fall
        // back to a read-write handle, then force query-only.
        Err(_) => rusqlite::Connection::open(path)
            .with_context(|| format!("cannot open SQLite database {}", path.display()))?,
    };
    conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 3000;")
        .context("cannot configure SQLite read connection")?;
    Ok(conn)
}

fn stringify_value(value: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => "NULL".to_string(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => format!("<BLOB {}>", fmt_bytes(b.len())),
    }
}

fn collect_rows(
    stmt: &mut rusqlite::Statement<'_>,
    params: &[&dyn rusqlite::ToSql],
) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    let columns: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut cells = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            cells.push(stringify_value(row.get_ref(index)?));
        }
        out.push(cells);
    }
    Ok((columns, out))
}

fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
    let sql = format!("SELECT * FROM {} LIMIT 0", quote_ident(table));
    let stmt = conn.prepare(&sql)?;
    Ok(stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect())
}

fn table_master_sql(conn: &rusqlite::Connection, table: &str) -> Result<String> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name = ?",
    )?;
    let params: [&dyn rusqlite::ToSql; 1] = [&table];
    let mut rows = stmt.query(&params)?;
    let Some(row) = rows.next()? else {
        bail!("SQLite table '{table}' not found");
    };
    let sql: Option<String> = row.get(0)?;
    sql.ok_or_else(|| anyhow::anyhow!("SQLite schema for table '{table}' is unavailable"))
}

fn table_info(conn: &rusqlite::Connection, table: &str) -> Result<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(table)))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        let kind: String = row.get(2).unwrap_or_default();
        let pk: i64 = row.get(5).unwrap_or(0);
        out.push((name, kind, pk));
    }
    Ok(out)
}

fn probe_row_count(conn: &rusqlite::Connection, table: &str) -> Result<(u64, bool)> {
    let sql = format!(
        "SELECT COUNT(*) FROM (SELECT 1 FROM {} LIMIT {})",
        quote_ident(table),
        ROW_COUNT_PROBE_CAP + 1
    );
    let mut stmt = conn.prepare(&sql)?;
    let counted: u64 = stmt.query_row([], |row| row.get(0)).unwrap_or(0);
    Ok(if counted > ROW_COUNT_PROBE_CAP {
        (ROW_COUNT_PROBE_CAP, true)
    } else {
        (counted, false)
    })
}

fn resolve_order_clause(order: Option<&str>, columns: &[String]) -> Result<String> {
    let Some(order) = order.map(str::trim).filter(|o| !o.is_empty()) else {
        return Ok(String::new());
    };
    let (column, direction) = match order.rsplit_once(':') {
        Some((column, direction)) => (column.trim(), direction.trim().to_ascii_lowercase()),
        None => (order, "asc".to_string()),
    };
    if !columns.iter().any(|candidate| candidate == column) {
        bail!("SQLite order column '{column}' not found in table schema");
    }
    if direction != "asc" && direction != "desc" {
        bail!("SQLite order direction must be 'asc' or 'desc'; got '{direction}'");
    }
    Ok(format!(
        " ORDER BY {} {}",
        quote_ident(column),
        direction.to_ascii_uppercase()
    ))
}

/// Coerce a `:key` lookup to the column's declared affinity.
fn coerce_lookup_value(key: &str, column_type: &str) -> rusqlite::types::Value {
    let normalized = column_type.trim().to_ascii_uppercase();
    if normalized.contains("INT") {
        if let Ok(value) = key.trim().parse::<i64>() {
            return rusqlite::types::Value::Integer(value);
        }
    }
    if normalized.contains("REAL") || normalized.contains("FLOA") || normalized.contains("DOUB") {
        if let Ok(value) = key.trim().parse::<f64>() {
            return rusqlite::types::Value::Real(value);
        }
    }
    rusqlite::types::Value::Text(key.to_string())
}

fn rowid_alias(conn: &rusqlite::Connection, table: &str) -> Result<String> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_xinfo({})", quote_ident(table)))?;
    let mut rows = stmt.query([])?;
    let mut declared = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        declared.push(name.to_ascii_lowercase());
    }
    for alias in ["rowid", "_rowid_", "oid"] {
        if !declared.iter().any(|name| name == alias) {
            return Ok(quote_ident(alias));
        }
    }
    bail!("SQLite table '{table}' shadows all ROWID aliases; row-key access is unavailable")
}

fn table_fits_at_minimum(column_count: usize) -> bool {
    MIN_COLUMN_WIDTH * column_count + COLUMN_SEPARATOR_WIDTH * column_count + TABLE_FRAME_WIDTH
        <= MAX_RENDER_WIDTH
}

/// `psql`-style expanded display, used when a table has too many columns to fit
/// inside [`MAX_RENDER_WIDTH`] (omp `buildVerticalBlocks`).
fn build_vertical_blocks(columns: &[String], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "(no rows)".to_string();
    }
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let mut block = vec![format!("── Row {} ──", index + 1)];
            for (column_index, column) in columns.iter().enumerate() {
                let name = sanitize_cell(column);
                let value = sanitize_cell(row.get(column_index).map_or("", String::as_str));
                block.push(truncate_width(&format!("{name}: {value}"), MAX_RENDER_WIDTH));
            }
            block.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// omp `buildAsciiTable`.
#[must_use]
pub fn build_ascii_table(columns: &[String], rows: &[Vec<String>]) -> String {
    if columns.is_empty() {
        return if rows.is_empty() {
            "(no rows)".to_string()
        } else {
            "(rows returned without named columns)".to_string()
        };
    }
    if !table_fits_at_minimum(columns.len()) {
        return build_vertical_blocks(columns, rows);
    }

    let mut widths: Vec<usize> = columns
        .iter()
        .map(|column| {
            MIN_CELL_WIDTH.max(sanitize_cell(column).chars().count().min(MAX_COLUMN_WIDTH))
        })
        .collect();
    for row in rows {
        for index in 0..columns.len() {
            let width = sanitize_cell(row.get(index).map_or("", String::as_str))
                .chars()
                .count()
                .min(MAX_COLUMN_WIDTH);
            widths[index] = widths[index].max(width);
        }
    }

    let overhead = columns.len() * COLUMN_SEPARATOR_WIDTH + TABLE_FRAME_WIDTH;
    let mut total: usize = widths.iter().sum::<usize>() + overhead;
    while total > MAX_RENDER_WIDTH {
        let mut widest = None;
        let mut widest_width = MIN_COLUMN_WIDTH;
        for (index, width) in widths.iter().enumerate() {
            if *width > widest_width {
                widest = Some(index);
                widest_width = *width;
            }
        }
        let Some(index) = widest else { break };
        widths[index] = widths[index].saturating_sub(1).max(MIN_CELL_WIDTH);
        total = widths.iter().sum::<usize>() + overhead;
    }

    let header = format!(
        "| {} |",
        columns
            .iter()
            .enumerate()
            .map(|(index, column)| pad_cell(column, widths[index]))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let divider = format!(
        "| {} |",
        widths
            .iter()
            .map(|width| "-".repeat((*width).max(MIN_CELL_WIDTH)))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let mut lines = vec![header, divider];
    if rows.is_empty() {
        lines.push("(no rows)".to_string());
        return lines.join("\n");
    }
    for row in rows {
        let cells: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(index, _)| pad_cell(row.get(index).map_or("", String::as_str), widths[index]))
            .collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.join("\n")
}

fn row_count_label(count: u64, at_least: bool) -> String {
    if at_least {
        format!("{count}+ rows")
    } else {
        format!("{count} rows")
    }
}

/// omp `renderTableList`.
#[must_use]
pub fn render_table_list(tables: &[(String, u64, bool)]) -> String {
    if tables.is_empty() {
        return "(no tables)".to_string();
    }
    tables
        .iter()
        .map(|(name, count, at_least)| {
            truncate_width(&format!("{name} ({})", row_count_label(*count, *at_least)), MAX_RENDER_WIDTH)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// omp `renderRow`: one `column: value` line per cell.
#[must_use]
pub fn render_row(columns: &[String], row: &[String]) -> String {
    if columns.is_empty() {
        return "(no columns)".to_string();
    }
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            truncate_width(
                &format!("{column}: {}", row.get(index).map_or("", String::as_str)),
                MAX_RENDER_WIDTH,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Meta for [`render_table`]'s continuation hint.
pub struct TableMeta<'a> {
    pub total_count: u64,
    pub offset: usize,
    pub limit: usize,
    pub table: &'a str,
    pub filter: Option<&'a str>,
    pub order: Option<&'a str>,
}

/// omp `renderTable`: the table plus a followable paging hint.
#[must_use]
pub fn render_table(columns: &[String], rows: &[Vec<String>], meta: &TableMeta<'_>) -> String {
    let mut parts = vec![build_ascii_table(columns, rows)];
    let shown = (meta.offset + rows.len()) as u64;
    if shown < meta.total_count {
        let remaining = meta.total_count - shown;
        let next_offset = meta.offset + rows.len();
        let mut query = format!("limit={}&offset={next_offset}", meta.limit);
        if let Some(filter) = meta.filter {
            let _ = write!(query, "&where={}", percent_encode(filter));
        }
        if let Some(order) = meta.order {
            let _ = write!(query, "&order={}", percent_encode(order));
        }
        parts.push(format!(
            "[{remaining} more rows; append :{}?{query} to the database path to continue]",
            meta.table
        ));
    }
    parts.join("\n")
}

/// Read a SQLite database through a selector, rendering omp's text form.
///
/// # Errors
/// Missing table/column, an unsafe `where` clause, or a SQLite failure.
#[allow(clippy::too_many_lines)] // one dispatch, mirrors readSqlite
pub fn sqlite_read(path: &Path, sub_path: &str, query_string: &str) -> Result<String> {
    let selector = parse_sqlite_selector(sub_path, query_string)?;
    let conn = open_sqlite_readonly(path)?;
    match selector {
        SqliteSelector::List => {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name COLLATE NOCASE",
            )?;
            let mut rows = stmt.query([])?;
            let mut names = Vec::new();
            while let Some(row) = rows.next()? {
                let name: String = row.get(0)?;
                names.push(name);
            }
            drop(rows);
            drop(stmt);
            let mut tables = Vec::with_capacity(names.len());
            for name in names {
                let (count, at_least) = probe_row_count(&conn, &name)?;
                tables.push((name, count, at_least));
            }
            Ok(render_table_list(&tables))
        }
        SqliteSelector::Schema { table, sample_limit } => {
            let create_sql = table_master_sql(&conn, &table)?;
            let mut sample = format!("SELECT * FROM {} LIMIT {sample_limit}", quote_ident(&table));
            let (columns, rows) = {
                let mut stmt = conn.prepare(&sample)?;
                collect_rows(&mut stmt, &[])?
            };
            let mut out = create_sql
                .lines()
                .map(|line| truncate_width(&sanitize_cell(line), MAX_RENDER_WIDTH))
                .collect::<Vec<_>>()
                .join("\n");
            let _ = write!(out, "\n\nSample rows:\n{}", build_ascii_table(&columns, &rows));
            if rows.len() < sample_limit {
                // Nothing to page; the hint would be noise.
            } else {
                let _ = write!(
                    out,
                    "\n[append :{table}?limit=20&offset={} to the database path to continue]",
                    rows.len()
                );
            }
            sample.clear();
            Ok(out)
        }
        SqliteSelector::Row { table, key } => {
            let info = table_info(&conn, &table)?;
            let mut primary: Vec<&(String, String, i64)> =
                info.iter().filter(|(_, _, pk)| *pk > 0).collect();
            primary.sort_by_key(|(_, _, pk)| *pk);
            let (column, column_type) = if primary.len() == 1 {
                (primary[0].0.clone(), primary[0].1.clone())
            } else if primary.len() > 1 {
                bail!("SQLite table '{table}' has a composite primary key; use '?where=' instead");
            } else {
                let create_sql = table_master_sql(&conn, &table)?;
                if create_sql.to_ascii_uppercase().contains("WITHOUT ROWID") {
                    bail!("SQLite table '{table}' does not expose ROWID; use '?where=' instead");
                }
                (rowid_alias(&conn, &table)?, String::new())
            };
            let binding = if column.starts_with('"') {
                match key.trim().parse::<i64>() {
                    Ok(value) => rusqlite::types::Value::Integer(value),
                    Err(_) => bail!("SQLite ROWID must be an integer; got '{key}'"),
                }
            } else {
                coerce_lookup_value(&key, &column_type)
            };
            let sql = format!(
                "SELECT * FROM {} WHERE {} = ? LIMIT 1",
                quote_ident(&table),
                if column.starts_with('"') { column.clone() } else { quote_ident(&column) }
            );
            let (columns, rows) = {
                let mut stmt = conn.prepare(&sql)?;
                let params: [&dyn rusqlite::ToSql; 1] = [&binding];
                collect_rows(&mut stmt, &params)?
            };
            match rows.first() {
                Some(row) => Ok(render_row(&columns, row)),
                None => Ok(format!(
                    "No row found in table '{table}' for key '{key}'."
                )),
            }
        }
        SqliteSelector::Query { table, limit, offset, order, filter } => {
            let columns = table_columns(&conn, &table)?;
            let where_clause = filter
                .as_deref()
                .map_or_else(String::new, |filter| format!(" WHERE {filter}"));
            let order_clause = resolve_order_clause(order.as_deref(), &columns)?;
            let sql = format!(
                "SELECT * FROM {}{where_clause}{order_clause} LIMIT ? OFFSET ?",
                quote_ident(&table)
            );
            let (columns, mut rows) = {
                let mut stmt = conn.prepare(&sql)?;
                let probe_limit = i64::try_from(limit + 1).unwrap_or(i64::MAX);
                let probe_offset = i64::try_from(offset).unwrap_or(i64::MAX);
                let params: [&dyn rusqlite::ToSql; 2] = [&probe_limit, &probe_offset];
                collect_rows(&mut stmt, &params)?
            };
            let has_more = rows.len() > limit;
            if has_more {
                rows.truncate(limit);
            }
            let total = if has_more {
                let count_sql = format!(
                    "SELECT COUNT(*) FROM {}{where_clause}",
                    quote_ident(&table)
                );
                let mut stmt = conn.prepare(&count_sql)?;
                stmt.query_row([], |row| row.get::<_, i64>(0)).unwrap_or(0)
            } else {
                i64::try_from(offset + rows.len()).unwrap_or(i64::MAX)
            };
            #[allow(clippy::cast_sign_loss)]
            let total_count = total.max(0) as u64;
            Ok(render_table(
                &columns,
                &rows,
                &TableMeta {
                    total_count,
                    offset,
                    limit,
                    table: &table,
                    filter: filter.as_deref(),
                    order: order.as_deref(),
                },
            ))
        }
        SqliteSelector::Raw { sql, page } => {
            let mut stmt = conn.prepare(&sql)?;
            let columns: Vec<String> = stmt
                .column_names()
                .iter()
                .map(|name| (*name).to_string())
                .collect();
            let limit = page.map_or(MAX_RAW_QUERY_ROWS, |page| page.limit);
            let mut skip = page.map_or(0, |page| page.offset);
            let mut rows = Vec::new();
            let mut truncated = false;
            let mut cursor = stmt.query([])?;
            while let Some(row) = cursor.next()? {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                if rows.len() >= limit {
                    truncated = true;
                    break;
                }
                let mut cells = Vec::with_capacity(columns.len());
                for index in 0..columns.len() {
                    cells.push(stringify_value(row.get_ref(index)?));
                }
                rows.push(cells);
            }
            let mut out = build_ascii_table(&columns, &rows);
            if truncated {
                match page {
                    Some(page) => {
                        let next = format!(
                            "q={}&limit={}&offset={}",
                            percent_encode(&sql),
                            page.limit,
                            page.offset + rows.len()
                        );
                        let _ = write!(out, "\n[More rows; append ?{next} to the database path to continue]");
                    }
                    None => {
                        let _ = write!(
                            out,
                            "\n[Output capped at {MAX_RAW_QUERY_ROWS} rows; add a LIMIT/OFFSET clause to the query to page through more]"
                        );
                    }
                }
            }
            Ok(out)
        }
    }
}

// ---------------------------------------------------------------- archives

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
}

/// One archive member. `data_offset` indexes the index's payload buffer, which
/// is the file itself for zip/tar and the gunzipped stream for `.tar.gz`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub method: u16,
    data_offset: usize,
    compressed_size: usize,
}

impl ArchiveEntry {
    fn display(&self) -> String {
        if self.is_dir {
            format!("{}/", self.path.trim_end_matches('/'))
        } else {
            self.path.clone()
        }
    }
}

/// A parsed archive index plus the payload its offsets refer to.
pub struct ArchiveIndex {
    pub kind: ArchiveKind,
    entries: Vec<ArchiveEntry>,
    payload: Vec<u8>,
}

impl ArchiveIndex {
    #[must_use]
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// Index `bytes` as `kind`. Tar payloads are buffered (and gunzipped first
    /// for `.tar.gz`) so member reads are plain slices.
    ///
    /// # Errors
    /// Truncated/corrupt archives, ZIP64 (unsupported), or a decompression
    /// bomb past [`MAX_ARCHIVE_BYTES`].
    pub fn open(kind: ArchiveKind, bytes: &[u8]) -> Result<Self> {
        let payload = match kind {
            ArchiveKind::TarGz => gunzip(bytes)?,
            ArchiveKind::Tar | ArchiveKind::Zip => bytes.to_vec(),
        };
        let entries = match kind {
            ArchiveKind::Zip => parse_zip_central(&payload)?,
            ArchiveKind::Tar | ArchiveKind::TarGz => parse_tar(&payload)?,
        };
        Ok(Self {
            kind,
            entries,
            payload,
        })
    }

    /// Decode one member's bytes.
    ///
    /// # Errors
    /// A truncated payload, an unsupported compression method, or an inflated
    /// size past [`MAX_ARCHIVE_BYTES`].
    pub fn read(&self, entry: &ArchiveEntry) -> Result<Vec<u8>> {
        let start = entry.data_offset;
        let end = start.saturating_add(entry.compressed_size);
        if end > self.payload.len() {
            bail!("archive entry '{}' is truncated", entry.path);
        }
        let raw = &self.payload[start..end];
        match entry.method {
            0 => Ok(raw.to_vec()),
            8 => {
                let mut decoder = flate2::read::DeflateDecoder::new(raw);
                let mut out = Vec::new();
                decoder
                    .by_ref()
                    .take(MAX_ARCHIVE_BYTES)
                    .read_to_end(&mut out)
                    .with_context(|| format!("cannot inflate archive entry '{}'", entry.path))?;
                Ok(out)
            }
            other => bail!(
                "archive entry '{}' uses unsupported compression method {other}",
                entry.path
            ),
        }
    }
}

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .by_ref()
        .take(MAX_ARCHIVE_BYTES)
        .read_to_end(&mut out)
        .context("cannot decompress gzip stream")?;
    Ok(out)
}

fn le_u16(bytes: &[u8], at: usize) -> u16 {
    bytes
        .get(at..at + 2)
        .map_or(0, |slice| u16::from_le_bytes([slice[0], slice[1]]))
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).map_or(0, |slice| {
        u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]])
    })
}

/// ZIP central-directory reader (no `zip` crate). CRC values are not verified:
/// a corrupt member surfaces as an inflate error instead.
fn parse_zip_central(payload: &[u8]) -> Result<Vec<ArchiveEntry>> {
    if payload.len() < 22 {
        bail!("zip archive is too short to hold an end-of-central-directory record");
    }
    // The EOCD sits in the last 64 KiB plus its own 22-byte body (comment).
    let search_start = payload.len().saturating_sub(22 + 0xffff);
    let mut eocd = None;
    let mut cursor = payload.len() - 22;
    loop {
        if le_u32(payload, cursor) == 0x0605_4b50 {
            eocd = Some(cursor);
            break;
        }
        if cursor == search_start || cursor == 0 {
            break;
        }
        cursor -= 1;
    }
    let Some(eocd) = eocd else {
        bail!("zip archive has no end-of-central-directory record");
    };
    let entry_count = le_u16(payload, eocd + 10) as usize;
    let central_size = le_u32(payload, eocd + 12) as usize;
    let central_offset = le_u32(payload, eocd + 16) as usize;
    if central_offset == u32::MAX as usize || central_size == u32::MAX as usize {
        bail!("ZIP64 archives are not supported by this build");
    }
    if central_offset.saturating_add(central_size) > payload.len() {
        bail!("zip central directory is truncated");
    }

    let mut entries = Vec::with_capacity(entry_count);
    let mut at = central_offset;
    for _ in 0..entry_count {
        if le_u32(payload, at) != 0x0201_4b50 {
            bail!("zip central directory entry is corrupt");
        }
        let method = le_u16(payload, at + 10);
        let compressed_size = le_u32(payload, at + 20) as usize;
        let uncompressed_size = le_u32(payload, at + 24) as usize;
        let name_len = le_u16(payload, at + 28) as usize;
        let extra_len = le_u16(payload, at + 30) as usize;
        let comment_len = le_u16(payload, at + 32) as usize;
        let local_offset = le_u32(payload, at + 42) as usize;
        if local_offset == u32::MAX as usize
            || compressed_size == u32::MAX as usize
            || uncompressed_size == u32::MAX as usize
        {
            bail!("ZIP64 archives are not supported by this build");
        }
        let name_start = at + 46;
        let name_end = name_start.saturating_add(name_len);
        if name_end > payload.len() {
            bail!("zip central directory entry name is truncated");
        }
        let path = String::from_utf8_lossy(&payload[name_start..name_end]).into_owned();
        // The local header's own name/extra lengths decide where data begins.
        if le_u32(payload, local_offset) != 0x0403_4b50 {
            bail!("zip local header for '{path}' is corrupt");
        }
        let local_name_len = le_u16(payload, local_offset + 26) as usize;
        let local_extra_len = le_u16(payload, local_offset + 28) as usize;
        let data_offset = local_offset + 30 + local_name_len + local_extra_len;
        let normalized = path.trim_start_matches("./").to_string();
        entries.push(ArchiveEntry {
            is_dir: normalized.ends_with('/'),
            path: normalized,
            size: uncompressed_size as u64,
            method,
            data_offset,
            compressed_size,
        });
        at = name_end + extra_len + comment_len;
    }
    Ok(entries)
}

fn parse_tar_octal(field: &[u8]) -> u64 {
    // GNU base-256 extension: the high bit of the first byte marks a binary size.
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        let mut value: u64 = 0;
        for (index, byte) in field.iter().enumerate() {
            let byte = if index == 0 { byte & 0x7f } else { *byte };
            value = value.wrapping_shl(8).wrapping_add(u64::from(byte));
        }
        return value;
    }
    let text: String = field
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as char)
        .collect();
    u64::from_str_radix(text.trim(), 8).unwrap_or(0)
}

fn tar_field(bytes: &[u8], start: usize, len: usize) -> String {
    let slice = bytes.get(start..start + len).unwrap_or(&[]);
    let end = slice.iter().position(|byte| *byte == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).trim().to_string()
}

/// ustar (POSIX) + GNU long-name tar reader.
fn parse_tar(payload: &[u8]) -> Result<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    let mut pending_name: Option<String> = None;
    while at + 512 <= payload.len() {
        let header = &payload[at..at + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = parse_tar_octal(&payload[at + 124..at + 136]);
        let typeflag = payload[at + 156];
        let name = tar_field(payload, at, 100);
        let prefix = tar_field(payload, at + 345, 155);
        let data_offset = at + 512;
        let padded = ((size as usize).saturating_add(511)) / 512 * 512;
        let compressed_size = size as usize;
        if data_offset.saturating_add(compressed_size) > payload.len() {
            bail!("tar archive is truncated at entry '{name}'");
        }
        match typeflag {
            // GNU long name: the next header's name is this block's body.
            b'L' => {
                pending_name = Some(
                    String::from_utf8_lossy(&payload[data_offset..data_offset + compressed_size])
                        .trim_end_matches('\0')
                        .to_string(),
                );
                at = data_offset + padded;
                continue;
            }
            // PAX headers describe the next entry; their bodies are not entries.
            b'x' | b'g' => {
                at = data_offset + padded;
                continue;
            }
            _ => {}
        }
        let full_name = pending_name.take().unwrap_or_else(|| {
            if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            }
        });
        let is_dir = matches!(typeflag, b'5') || full_name.ends_with('/');
        entries.push(ArchiveEntry {
            path: full_name.trim_start_matches("./").to_string(),
            size: if is_dir { 0 } else { size },
            is_dir,
            method: 0,
            data_offset,
            compressed_size: if is_dir { 0 } else { compressed_size },
        });
        at = data_offset + padded;
    }
    Ok(entries)
}

/// Archive kind named by a path, including ZIP-container documents.
#[must_use]
pub fn archive_kind_for_path(path: &str) -> Option<ArchiveKind> {
    match extension_format(Path::new(path)) {
        Some(Format::TarGz) => Some(ArchiveKind::TarGz),
        Some(Format::Tar) => Some(ArchiveKind::Tar),
        Some(Format::Zip) => Some(ArchiveKind::Zip),
        _ => None,
    }
}

/// Split `archive.zip:member` at the first `:` that ends an archive path
/// (omp `parseArchivePathCandidates`, single-candidate form).
#[must_use]
pub fn split_archive_member(raw: &str) -> Option<(String, String)> {
    for (index, _) in raw.match_indices(':') {
        // `C:/dir` is a drive letter, not a member separator.
        if index == 1 {
            continue;
        }
        let (path, rest) = raw.split_at(index);
        if archive_kind_for_path(path).is_some() {
            return Some((path.to_string(), rest[1..].to_string()));
        }
    }
    None
}

/// Render an entry listing (`formatArchiveEntryLines`): path column, size column.
#[must_use]
pub fn render_entries<'a>(entries: impl IntoIterator<Item = &'a ArchiveEntry>) -> String {
    let entries: Vec<&ArchiveEntry> = entries.into_iter().collect();
    if entries.is_empty() {
        return "(empty archive)".to_string();
    }
    let labels: Vec<String> = entries.iter().map(|entry| entry.display()).collect();
    let width = labels
        .iter()
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0)
        .min(MAX_RENDER_WIDTH);
    entries
        .iter()
        .zip(labels)
        .map(|(entry, label)| {
            let label = truncate_width(&label, width);
            if entry.is_dir {
                format!("{label}{}", " ".repeat(width - label.chars().count().min(width)))
            } else {
                format!(
                    "{label}{}{:>8}",
                    " ".repeat(width - label.chars().count().min(width) + 2),
                    fmt_bytes(usize::try_from(entry.size).unwrap_or(usize::MAX))
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Listing of the whole archive or of one directory inside it.
#[must_use]
pub fn render_archive_listing(index: &ArchiveIndex) -> String {
    render_entries(index.entries())
}

fn render_archive_subtree(index: &ArchiveIndex, dir: &str) -> String {
    let prefix = format!("{}/", dir.trim_matches('/'));
    render_entries(
        index
            .entries()
            .iter()
            .filter(|entry| entry.path.starts_with(&prefix)),
    )
}

/// Browse (`member = None`) or read (`member = Some`) an archive.
///
/// # Errors
/// Corrupt archives, an unknown member, or a member that is not UTF-8 text.
pub fn archive_read(path: &Path, kind: ArchiveKind, member: Option<&str>) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let index = ArchiveIndex::open(kind, &bytes)?;
    let member = member.unwrap_or("").trim_matches('/');
    if member.is_empty() {
        return Ok(render_archive_listing(&index));
    }
    if let Some(entry) = index
        .entries()
        .iter()
        .find(|entry| entry.path.trim_end_matches('/') == member)
    {
        if entry.is_dir {
            return Ok(render_archive_subtree(&index, member));
        }
        let data = index.read(entry)?;
        if let Some(text) = decode_utf8_text(&data) {
            return Ok(text);
        }
        if entry.path.to_ascii_lowercase().ends_with(".pdf") {
            return Ok(pdf_read(&data));
        }
        return Ok(format!(
            "[Cannot read binary archive entry '{}' ({})]",
            entry.path,
            fmt_bytes(data.len())
        ));
    }
    let subtree = render_archive_subtree(&index, member);
    if subtree == "(empty archive)" {
        bail!("Path '{member}' not found inside archive");
    }
    Ok(subtree)
}

/// Strict UTF-8 decode: `None` when the bytes are not text (omp `decodeUtf8Text`).
#[must_use]
pub fn decode_utf8_text(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.contains('\0') {
        return None;
    }
    Some(text.strip_prefix('\u{feff}').unwrap_or(text).to_string())
}

// -------------------------------------------------------------------- JSON

/// One page of query results requested via `offset`/`limit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonPage {
    pub offset: usize,
    pub limit: usize,
}

/// Options parsed from a `file.json?q=<filter>` read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonSelector {
    pub query: String,
    pub raw: bool,
    pub compact: bool,
    pub page: Option<JsonPage>,
}

/// Split a read target at its first `?` (extension-independent; admission is
/// decided by [`parse_json_selector`] plus the file's own shape).
#[must_use]
pub fn split_json_query_target(raw: &str) -> Option<(String, String)> {
    let (path, query) = raw.split_once('?')?;
    Some((path.to_string(), query.to_string()))
}

/// Parse `q`/`query`, `raw`, `compact`, `offset`, `limit`.
///
/// Returns `None` when the query string names none of them, so `?q=` on a video
/// or PDF keeps its own route. Modifiers without a filter imply `.`.
#[must_use]
pub fn parse_json_selector(query_string: &str) -> Option<JsonSelector> {
    let params = parse_query_params(query_string);
    let known = params.iter().any(|(key, _)| {
        matches!(
            key.as_str(),
            "q" | "query" | "raw" | "compact" | "offset" | "limit"
        )
    });
    if !known {
        return None;
    }
    let query = params
        .iter()
        .find(|(key, _)| key == "q" || key == "query")
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| ".".to_string());
    let flag = |name: &str| {
        params
            .iter()
            .any(|(key, value)| key == name && (value == "true" || value == "1"))
    };
    let count = |name: &str| {
        params
            .iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
    };
    let offset = count("offset");
    let limit = count("limit");
    let page = match (offset, limit) {
        (None, None) => None,
        (offset, limit) => Some(JsonPage {
            offset: offset.unwrap_or(0),
            limit: limit
                .unwrap_or(DEFAULT_JSON_LIMIT)
                .min(MAX_JSON_LIMIT)
                .max(1),
        }),
    };
    Some(JsonSelector {
        query: if query.trim().is_empty() {
            ".".to_string()
        } else {
            query
        },
        raw: flag("raw"),
        compact: flag("compact"),
        page,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathStep {
    Key(String),
    Index(isize),
    Slice(Option<isize>, Option<isize>),
    Iter,
}

fn split_slice(inner: &str) -> Option<(Option<isize>, Option<isize>)> {
    let (left, right) = inner.split_once(':')?;
    let parse = |text: &str| -> Option<Option<isize>> {
        let text = text.trim();
        if text.is_empty() {
            Some(None)
        } else {
            text.parse::<isize>().ok().map(Some)
        }
    };
    Some((parse(left)?, parse(right)?))
}

fn parse_path_steps(path: &str) -> Result<Vec<PathStep>> {
    let chars: Vec<char> = path.chars().collect();
    let mut steps = Vec::new();
    let mut i = 0;
    if chars.first() == Some(&'.') {
        i = 1;
        if chars.len() == 1 {
            return Ok(steps);
        }
    }
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                if start == i {
                    bail!("empty path component in JSON filter '{path}'");
                }
                steps.push(PathStep::Key(chars[start..i].iter().collect()));
            }
            '[' => {
                let mut end = i + 1;
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end >= chars.len() {
                    bail!("unterminated '[' in JSON filter '{path}'");
                }
                let inner: String = chars[i + 1..end].iter().collect();
                let inner = inner.trim().to_string();
                if inner.is_empty() {
                    steps.push(PathStep::Iter);
                } else if let Some((from, to)) = split_slice(&inner) {
                    steps.push(PathStep::Slice(from, to));
                } else if inner.len() >= 2 && inner.starts_with('"') && inner.ends_with('"') {
                    steps.push(PathStep::Key(inner[1..inner.len() - 1].to_string()));
                } else {
                    let index: isize = inner
                        .parse()
                        .with_context(|| format!("bad array index '[{inner}]' in JSON filter '{path}'"))?;
                    steps.push(PathStep::Index(index));
                }
                i = end + 1;
            }
            _ => {
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                if start == i {
                    bail!("unsupported JSON filter '{path}'");
                }
                steps.push(PathStep::Key(chars[start..i].iter().collect()));
            }
        }
    }
    Ok(steps)
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn normalize_index(index: isize, len: usize) -> Option<usize> {
    let len = isize::try_from(len).unwrap_or(isize::MAX);
    let resolved = if index < 0 { len + index } else { index };
    (resolved >= 0 && resolved < len).then_some(resolved as usize)
}

fn apply_steps(steps: &[PathStep], value: &Value) -> Result<Vec<Value>> {
    let mut current = vec![value.clone()];
    for step in steps {
        let mut next = Vec::new();
        for value in &current {
            match step {
                PathStep::Key(key) => {
                    if let Some(found) = value.get(key) {
                        next.push(found.clone());
                    }
                }
                PathStep::Index(index) => {
                    let Some(array) = value.as_array() else {
                        bail!("cannot index {} with [{index}]", json_type(value));
                    };
                    if let Some(found) = normalize_index(*index, array.len()) {
                        next.push(array[found].clone());
                    }
                }
                PathStep::Slice(from, to) => {
                    let Some(array) = value.as_array() else {
                        bail!("cannot slice {} with [:{}]", json_type(value), to.map_or(0, |v| v));
                    };
                    let len = isize::try_from(array.len()).unwrap_or(isize::MAX);
                    let resolve = |bound: isize| {
                        if bound < 0 {
                            (len + bound).max(0)
                        } else {
                            bound.min(len)
                        }
                    };
                    let start = from.map_or(0, resolve);
                    let end = to.map_or(len, resolve);
                    if start < end {
                        let start = start as usize;
                        let end = end as usize;
                        next.extend(array[start..end].iter().cloned());
                    }
                }
                PathStep::Iter => match value {
                    Value::Array(items) => next.extend(items.iter().cloned()),
                    Value::Object(map) => next.extend(map.values().cloned()),
                    other => bail!("cannot iterate over {}", json_type(other)),
                },
            }
        }
        current = next;
    }
    Ok(current)
}

const SUPPORTED_FILTERS: &str =
    "paths (.a.b[0][]), pipes (|), keys, values, length, type, tostring, first, last, empty";

fn apply_function(name: &str, value: &Value) -> Result<Vec<Value>> {
    match name {
        "keys" => match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Ok(vec![Value::Array(
                    keys.into_iter().map(|key| Value::String(key.clone())).collect(),
                )])
            }
            Value::Array(items) => Ok(vec![Value::Array(
                (0..items.len())
                    .map(|index| Value::from(index as u64))
                    .collect(),
            )]),
            other => bail!("keys is not defined for {}", json_type(other)),
        },
        "values" => match value {
            Value::Object(map) => Ok(map.values().cloned().collect()),
            Value::Array(items) => Ok(items.clone()),
            other => bail!("values is not defined for {}", json_type(other)),
        },
        "length" => Ok(vec![Value::from(match value {
            Value::Array(items) => items.len() as u64,
            Value::Object(map) => map.len() as u64,
            Value::String(text) => text.chars().count() as u64,
            Value::Null => 0,
            _ => bail!("length is not defined for {}", json_type(value)),
        })]),
        "type" => Ok(vec![Value::String(json_type(value).to_string())]),
        "tostring" => Ok(vec![Value::String(match value {
            Value::String(text) => text.clone(),
            other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
        })]),
        "first" => Ok(value
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .into_iter()
            .collect()),
        "last" => Ok(value
            .as_array()
            .and_then(|items| items.last())
            .cloned()
            .into_iter()
            .collect()),
        "empty" => Ok(Vec::new()),
        other => bail!("JSON filter '{other}' is not supported by this build; supported: {SUPPORTED_FILTERS}"),
    }
}

fn split_stages(query: &str) -> Vec<&str> {
    let mut stages = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut start = 0usize;
    for (index, ch) in query.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '[' if !in_string => depth += 1,
            ']' if !in_string => depth = depth.saturating_sub(1),
            '|' if !in_string && depth == 0 => {
                stages.push(&query[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    stages.push(&query[start..]);
    stages
}

/// Evaluate the supported jq subset over one JSON document.
///
/// # Errors
/// A malformed filter, an unsupported function, or a type mismatch.
pub fn eval_json_filter(root: &Value, query: &str) -> Result<Vec<Value>> {
    let mut values = vec![root.clone()];
    for stage in split_stages(query) {
        let stage = stage.trim();
        if stage.is_empty() || stage == "." {
            continue;
        }
        let mut next = Vec::new();
        for value in &values {
            if stage.starts_with('.') || stage.starts_with('[') {
                let steps = parse_path_steps(stage)?;
                next.extend(apply_steps(&steps, value)?);
            } else {
                next.extend(apply_function(stage, value)?);
            }
        }
        values = next;
    }
    Ok(values)
}

fn render_json_value(value: &Value, selector: &JsonSelector) -> String {
    if selector.raw {
        if let Value::String(text) = value {
            return text.clone();
        }
    }
    if selector.compact {
        serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
    } else {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string())
    }
}

fn json_hint(selector: &JsonSelector, page: &JsonPage, next: usize, remaining: Option<usize>) -> String {
    let mut flags = String::new();
    if selector.raw {
        flags.push_str("&raw=true");
    }
    if selector.compact {
        flags.push_str("&compact=true");
    }
    let count = remaining.map_or_else(
        || "more results".to_string(),
        |remaining| format!("{remaining} more items"),
    );
    format!(
        "[{count}; append ?q={}{flags}&limit={}&offset={next} to continue]",
        percent_encode(&selector.query),
        page.limit
    )
}

fn render_json_page(values: &[Value], selector: &JsonSelector) -> String {
    let Some(page) = selector.page else {
        return values
            .iter()
            .map(|value| render_json_value(value, selector))
            .collect::<Vec<_>>()
            .join("\n");
    };
    // A single array result pages its elements and stays one array.
    if values.len() == 1 {
        if let Value::Array(items) = &values[0] {
            let slice: Vec<Value> = items
                .iter()
                .skip(page.offset)
                .take(page.limit)
                .cloned()
                .collect();
            let remaining = items.len().saturating_sub(slice.len());
            let slice_len = slice.len();
            let body = if selector.compact {
                serde_json::to_string(&Value::Array(slice))
            } else {
                serde_json::to_string_pretty(&Value::Array(slice))
            }
            .unwrap_or_default();
            if remaining > 0 {
                return format!(
                    "{body}\n{}",
                    json_hint(selector, &page, page.offset + slice_len, Some(remaining))
                );
            }
            return body;
        }
    }
    let slice: Vec<Value> = values
        .iter()
        .skip(page.offset)
        .take(page.limit)
        .cloned()
        .collect();
    let mut lines: Vec<String> = slice
        .iter()
        .map(|value| render_json_value(value, selector))
        .collect();
    let next = page.offset + slice.len();
    if next < values.len() {
        lines.push(json_hint(selector, &page, next, None));
    } else if slice.is_empty() && page.offset > 0 {
        lines.push(format!(
            "[offset {} is past the last result ({} total)]",
            page.offset,
            values.len()
        ));
    }
    lines.join("\n")
}

/// Run a selector over JSON text. A document that fails to parse as one value
/// is retried line by line (`.jsonl`/`.ndjson` streams).
///
/// # Errors
/// Invalid JSON, or an unsupported/ill-typed filter.
pub fn json_query(text: &str, selector: &JsonSelector) -> Result<String> {
    let roots: Vec<Value> = match serde_json::from_str::<Value>(text) {
        Ok(value) => vec![value],
        Err(error) => {
            let mut roots = Vec::new();
            for (index, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(value) => roots.push(value),
                    Err(_) => bail!("invalid JSON on line {}: {error}", index + 1),
                }
            }
            if roots.is_empty() {
                bail!("invalid JSON: {error}");
            }
            roots
        }
    };
    let mut values = Vec::new();
    for root in &roots {
        values.extend(eval_json_filter(root, &selector.query)?);
    }
    if values.is_empty() {
        return Ok("(no results)".to_string());
    }
    Ok(render_json_page(&values, selector))
}

/// Read `path` and run `selector` over it.
///
/// # Errors
/// Unreadable file, invalid JSON, or a filter failure.
pub fn json_read(path: &Path, selector: &JsonSelector) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    json_query(&text, selector)
}

/// Pretty-print JSON text without a filter (the bare `.json` read).
#[must_use]
pub fn json_pretty(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_string()),
        Err(_) => text.to_string(),
    }
}

// --------------------------------------------------------------- notebooks

fn join_source(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    }
}

/// Render a `.ipynb` notebook as cell-ordered text (`read-notebook` shape):
/// a markdown cell shows its source, a code cell shows its source plus stdout,
/// results and errors.
///
/// # Errors
/// Invalid notebook JSON.
pub fn format_notebook(text: &str) -> Result<String> {
    let doc: Value = serde_json::from_str(text).context("invalid notebook JSON")?;
    let major = doc.get("nbformat").and_then(Value::as_u64).unwrap_or(4);
    let minor = doc.get("nbformat_minor").and_then(Value::as_u64).unwrap_or(0);
    let language = doc
        .pointer("/metadata/kernelspec/language")
        .or_else(|| doc.pointer("/metadata/language_info/name"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let cells = doc
        .get("cells")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Notebook v{major}.{minor} · {} cells · kernel: {language}",
        cells.len()
    );
    for (index, cell) in cells.iter().enumerate() {
        let number = index + 1;
        let kind = cell
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let source = join_source(cell.get("source"));
        match kind {
            "markdown" => {
                let _ = write!(out, "\n── cell {number} [markdown] ──\n{source}");
                if !source.ends_with('\n') {
                    out.push('\n');
                }
            }
            "raw" => {
                let _ = write!(out, "\n── cell {number} [raw] ──\n{source}");
                if !source.ends_with('\n') {
                    out.push('\n');
                }
            }
            "code" => {
                let execution = cell
                    .get("execution_count")
                    .and_then(Value::as_u64)
                    .map_or_else(|| ", exec -".to_string(), |count| format!(", exec {count}"));
                let _ = write!(out, "\n── cell {number} [code{execution}] ──\n{source}");
                if !source.ends_with('\n') {
                    out.push('\n');
                }
                let outputs = cell
                    .get("outputs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for output in &outputs {
                    render_notebook_output(&mut out, output);
                }
            }
            other => {
                let _ = write!(out, "\n── cell {number} [{other}] ──\n{source}");
                if !source.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    Ok(out.trim_end().to_string())
}

fn render_notebook_output(out: &mut String, output: &Value) {
    let kind = output
        .get("output_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match kind {
        "stream" => {
            let text = join_source(output.get("text"));
            if text.trim().is_empty() {
                return;
            }
            let _ = write!(out, "[out]\n{}", ensure_trailing_newline(&text));
        }
        "execute_result" | "display_data" => {
            let data = output.get("data");
            let plain = join_source(data.and_then(|data| data.get("text/plain")));
            if !plain.trim().is_empty() {
                let _ = write!(out, "[out]\n{}", ensure_trailing_newline(&plain));
            }
            if let Some(data) = data.and_then(Value::as_object) {
                let mut images = Vec::new();
                for (mime, value) in data {
                    if mime.starts_with("image/") {
                        let bytes = value.as_str().map_or(0, str::len);
                        images.push(format!("{mime} {}", fmt_bytes(bytes)));
                    }
                }
                if !images.is_empty() {
                    let _ = writeln!(out, "[image] {}", images.join(", "));
                }
            }
        }
        "error" => {
            let name = output.get("ename").and_then(Value::as_str).unwrap_or("Error");
            let value = output.get("evalue").and_then(Value::as_str).unwrap_or("");
            let _ = writeln!(out, "[error] {name}: {value}");
            for line in output
                .get("traceback")
                .and_then(Value::as_array)
                .map(|lines| lines.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default()
            {
                let _ = writeln!(out, "  {line}");
            }
        }
        other => {
            let _ = writeln!(out, "[{other}]");
        }
    }
}

fn ensure_trailing_newline(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

// ------------------------------------------------------------------ binary

/// Content-sniffed kind label for a binary blob.
#[must_use]
pub fn binary_kind_label(head: &[u8]) -> &'static str {
    if head.starts_with(b"\x7fELF") {
        return "ELF executable";
    }
    if head.starts_with(&[0xfe, 0xed, 0xfa, 0xce]) || head.starts_with(&[0xce, 0xfa, 0xed, 0xfe]) {
        return "Mach-O (32-bit)";
    }
    if head.starts_with(&[0xfe, 0xed, 0xfa, 0xcf]) || head.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        return "Mach-O (64-bit)";
    }
    if head.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) {
        return "Mach-O universal binary";
    }
    if head.starts_with(b"MZ") {
        return "PE / DOS executable";
    }
    if head.starts_with(b"\0asm") {
        return "WebAssembly module";
    }
    if head.starts_with(b"\xca\xfe\xba\xbe") {
        return "Java class";
    }
    if head.starts_with(b"!<arch>") {
        return "ar archive";
    }
    if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return "7z archive";
    }
    if head.starts_with(b"Rar!") {
        return "RAR archive";
    }
    if head.starts_with(b"\xfd7zXZ\0") {
        return "xz stream";
    }
    if head.starts_with(b"BZh") {
        return "bzip2 stream";
    }
    if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return "zstd stream";
    }
    if head.starts_with(SQLITE_MAGIC) {
        return "SQLite database";
    }
    if head.starts_with(b"%PDF-") {
        return "PDF document";
    }
    if head.starts_with(b"PK\x03\x04") {
        return "ZIP archive";
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return "gzip stream";
    }
    if image_mime(head).is_some() {
        return "image";
    }
    "data"
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for byte in bytes {
        counts[usize::from(*byte)] += 1;
    }
    #[allow(clippy::cast_precision_loss)]
    let total = bytes.len() as f64;
    let mut entropy = 0.0f64;
    for count in counts {
        if count == 0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let probability = count as f64 / total;
        entropy -= probability * probability.log2();
    }
    entropy
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// On-disk metadata block for a binary (omp `readBinaryMetadata`, minus the
/// disassembler's slice table): size, sniffed kind, digest, entropy, printable
/// ratio, magic bytes and the views this build can serve.
#[must_use]
pub fn binary_metadata(display_path: &str, bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(32)];
    let printable = bytes
        .iter()
        .filter(|byte| (0x20..=0x7e).contains(*byte) || matches!(**byte, b'\n' | b'\r' | b'\t'))
        .count();
    #[allow(clippy::cast_precision_loss)]
    let printable_ratio = if bytes.is_empty() {
        0.0
    } else {
        printable as f64 * 100.0 / bytes.len() as f64
    };
    let magic: String = head
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");

    let mut out = String::new();
    let _ = writeln!(out, "path: {display_path}");
    let _ = writeln!(out, "size: {} ({} bytes)", fmt_bytes(bytes.len()), bytes.len());
    let _ = writeln!(out, "kind: {}", binary_kind_label(head));
    let _ = writeln!(out, "sha256: {}", sha256_hex(bytes));
    let _ = writeln!(out, "entropy: {:.2} bits/byte", shannon_entropy(bytes));
    let _ = writeln!(out, "printable: {printable_ratio:.1}%");
    let _ = writeln!(out, "magic: {magic}");
    let _ = writeln!(
        out,
        "views: :strings (printable runs), :hex (hexdump), :hex:OFFSET:LEN (slice)"
    );
    let _ = write!(
        out,
        "analysis: disassembly views (:overview, :imports, :exports, :xrefs:) need an IDA/Ghidra backend, which this build does not ship"
    );
    out
}

/// Printable ASCII runs of at least `min_len` bytes (omp `strings`).
#[must_use]
pub fn extract_strings(bytes: &[u8], min_len: usize) -> Vec<String> {
    let min_len = min_len.max(1);
    let mut out = Vec::new();
    let mut run = Vec::new();
    for byte in bytes {
        if (0x20..=0x7e).contains(byte) || *byte == b'\t' {
            run.push(*byte);
            continue;
        }
        if run.len() >= min_len {
            out.push(String::from_utf8_lossy(&run).into_owned());
            if out.len() >= MAX_STRINGS {
                return out;
            }
        }
        run.clear();
    }
    if run.len() >= min_len && out.len() < MAX_STRINGS {
        out.push(String::from_utf8_lossy(&run).into_owned());
    }
    out
}

/// Classic `xxd`-shaped hexdump: offset, 16 hex pairs, ASCII gutter.
#[must_use]
pub fn hexdump(bytes: &[u8], offset: usize, len: usize) -> String {
    let start = offset.min(bytes.len());
    let end = start.saturating_add(len).min(bytes.len());
    if start >= end {
        return format!(
            "(no bytes in range {offset}..{}; file is {})",
            offset.saturating_add(len),
            fmt_bytes(bytes.len())
        );
    }
    let mut out = String::new();
    let mut at = start;
    while at < end {
        let line_end = (at + 16).min(end);
        let chunk = &bytes[at..line_end];
        let _ = write!(out, "{at:08x}  ");
        for index in 0..16 {
            if index == 8 {
                out.push(' ');
            }
            match chunk.get(index) {
                Some(byte) => {
                    let _ = write!(out, "{byte:02x} ");
                }
                None => out.push_str("   "),
            }
        }
        out.push_str(" |");
        for byte in chunk {
            out.push(if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
        at = line_end;
    }
    out
}

/// Render a binary view: `""` metadata, `strings`, `hex`, or `hex:OFFSET:LEN`.
///
/// # Errors
/// A malformed `hex:OFFSET:LEN` view.
pub fn binary_read(display_path: &str, bytes: &[u8], view: &str) -> Result<String> {
    let view = view.trim();
    if view.is_empty() {
        return Ok(binary_metadata(display_path, bytes));
    }
    if view.eq_ignore_ascii_case("strings") {
        let strings = extract_strings(bytes, 4);
        if strings.is_empty() {
            return Ok("(no printable strings)".to_string());
        }
        let mut out = String::new();
        for text in strings {
            let _ = writeln!(out, "{text}");
        }
        return Ok(out.trim_end().to_string());
    }
    if view.eq_ignore_ascii_case("hex") {
        return Ok(hexdump(bytes, 0, MAX_BYTES).trim_end().to_string());
    }
    if let Some(rest) = view.strip_prefix("hex:") {
        let (offset, len) = rest.split_once(':').unwrap_or((rest, "256"));
        let offset: usize = offset
            .trim()
            .parse()
            .with_context(|| format!("hexdump offset must be an integer; got '{offset}'"))?;
        let len: usize = len
            .trim()
            .parse()
            .with_context(|| format!("hexdump length must be an integer; got '{len}'"))?;
        return Ok(hexdump(bytes, offset, len.min(MAX_BYTES)).trim_end().to_string());
    }
    bail!("Unsupported binary view ':{view}'; use :strings, :hex or :hex:OFFSET:LEN")
}

// --------------------------------------------------------------------- PDF

fn pdf_version(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(1024)];
    let at = head.windows(5).position(|window| window == b"%PDF-")?;
    let version: String = head[at + 5..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit() || **byte == b'.')
        .map(|byte| *byte as char)
        .collect();
    (!version.is_empty()).then_some(version)
}

fn pdf_page_count(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut at = 0;
    while let Some(found) = find_subslice(&bytes[at..], b"/Type") {
        let mut cursor = at + found + 5;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes[cursor..].starts_with(b"/Page")
            && !bytes
                .get(cursor + 5)
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
        {
            count += 1;
        }
        at += found + 5;
        if at >= bytes.len() {
            break;
        }
    }
    count
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read a PDF literal string starting at `open` (the `(` byte).
fn read_pdf_literal(bytes: &[u8], open: usize) -> (String, usize) {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let Some(next) = bytes.get(i + 1).copied() else {
                    break;
                };
                i += 2;
                match next {
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'0'..=b'7' => {
                        let mut value = u32::from(next - b'0');
                        let mut taken = 1;
                        while taken < 3 {
                            match bytes.get(i).copied() {
                                Some(digit @ b'0'..=b'7') => {
                                    value = value * 8 + u32::from(digit - b'0');
                                    i += 1;
                                    taken += 1;
                                }
                                _ => break,
                            }
                        }
                        if let Some(ch) = char::from_u32(value) {
                            out.push(ch);
                        }
                    }
                    other => out.push(other as char),
                }
            }
            b'(' => {
                depth += 1;
                if depth > 1 {
                    out.push('(');
                }
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (out, i + 1);
                }
                out.push(')');
                i += 1;
            }
            byte => {
                out.push(byte as char);
                i += 1;
            }
        }
    }
    (out, bytes.len())
}

fn read_pdf_hex(bytes: &[u8], open: usize) -> (String, usize) {
    let mut digits = Vec::new();
    let mut i = open + 1;
    while i < bytes.len() && bytes[i] != b'>' {
        if let Some(value) = hex_val(bytes[i]) {
            digits.push(value);
        }
        i += 1;
    }
    let mut decoded = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let high = pair[0];
        let low = pair.get(1).copied().unwrap_or(0);
        decoded.push(high * 16 + low);
    }
    (String::from_utf8_lossy(&decoded).into_owned(), i + 1)
}

/// Pull the text-showing literals out of one content stream (inside `BT`/`ET`,
/// with `Td`/`TD`/`T*` starting a new line).
fn pdf_extract_text(content: &[u8]) -> String {
    let mut out = String::new();
    let mut line = String::new();
    let mut in_text = false;
    let mut i = 0;
    while i < content.len() {
        let byte = content[i];
        if byte.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match byte {
            b'(' => {
                let (text, next) = read_pdf_literal(content, i);
                i = next;
                if in_text {
                    line.push_str(&text);
                }
            }
            b'<' if content.get(i + 1) != Some(&b'<') => {
                let (text, next) = read_pdf_hex(content, i);
                i = next;
                if in_text {
                    line.push_str(&text);
                }
            }
            b'[' | b']' | b'<' | b'>' => i += 1,
            _ => {
                let start = i;
                while i < content.len()
                    && !content[i].is_ascii_whitespace()
                    && !matches!(
                        content[i],
                        b'(' | b')' | b'[' | b']' | b'<' | b'>' | b'/' | b'%'
                    )
                {
                    i += 1;
                }
                if start == i {
                    i += 1;
                    continue;
                }
                let token = &content[start..i];
                match token {
                    b"BT" => in_text = true,
                    b"ET" => {
                        if !line.trim().is_empty() {
                            out.push_str(line.trim_end());
                            out.push('\n');
                            line.clear();
                        }
                        in_text = false;
                    }
                    b"T*" | b"Td" | b"TD" => {
                        if in_text && !line.trim().is_empty() {
                            out.push_str(line.trim_end());
                            out.push('\n');
                            line.clear();
                        }
                    }
                    _ => {}
                }
            }
        }
        if out.len() >= MAX_PDF_TEXT_BYTES {
            break;
        }
    }
    if !line.trim().is_empty() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Every `stream ... endstream` body, inflated when it is zlib-compressed.
fn pdf_stream_bodies(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut bodies = Vec::new();
    let mut at = 0;
    while let Some(found) = find_subslice(&bytes[at..], b"stream") {
        let mut start = at + found + 6;
        if bytes.get(start) == Some(&b'\r') {
            start += 1;
        }
        if bytes.get(start) == Some(&b'\n') {
            start += 1;
        }
        let Some(length) = find_subslice(&bytes[start..], b"endstream") else {
            break;
        };
        let body = &bytes[start..start + length];
        if let Ok(inflated) = inflate_zlib(body) {
            bodies.push(inflated);
        } else {
            bodies.push(body.to_vec());
        }
        at = start + length + 9;
        if bodies.len() > 64 {
            break;
        }
    }
    bodies
}

fn inflate_zlib(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = flate2::read::ZlibDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .by_ref()
        .take(MAX_PDF_TEXT_BYTES as u64 * 4)
        .read_to_end(&mut out)?;
    Ok(out)
}

/// An `/Info` dictionary string: `(literal)` or `<hex>` after `key`.
fn pdf_info_string(bytes: &[u8], key: &str) -> Option<String> {
    let needle = key.as_bytes();
    let mut at = 0;
    while let Some(found) = find_subslice(&bytes[at..], needle) {
        let mut cursor = at + found + needle.len();
        // Skip whitespace between the key and its value.
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        match bytes.get(cursor) {
            Some(b'(') => {
                let (text, _) = read_pdf_literal(bytes, cursor);
                if !text.trim().is_empty() {
                    return Some(text.trim().to_string());
                }
            }
            Some(b'<') if bytes.get(cursor + 1) != Some(&b'<') => {
                let (text, _) = read_pdf_hex(bytes, cursor);
                if !text.trim().is_empty() {
                    return Some(text.trim().to_string());
                }
            }
            _ => {}
        }
        at += found + needle.len();
        if at >= bytes.len() {
            break;
        }
    }
    None
}

/// PDF metadata plus the extracted text layer, or a note when the document has
/// none (a scanned/image-only PDF needs the page-screenshot route).
#[must_use]
pub fn pdf_read(bytes: &[u8]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "PDF document");
    if let Some(version) = pdf_version(bytes) {
        let _ = writeln!(out, "version: {version}");
    }
    let pages = pdf_page_count(bytes);
    let _ = writeln!(out, "pages: {pages}");
    for (label, key) in [
        ("title", "/Title"),
        ("author", "/Author"),
        ("subject", "/Subject"),
        ("creator", "/Creator"),
        ("producer", "/Producer"),
        ("created", "/CreationDate"),
    ] {
        if let Some(value) = pdf_info_string(bytes, key) {
            let _ = writeln!(out, "{label}: {value}");
        }
    }
    if find_subslice(bytes, b"/Encrypt").is_some() {
        let _ = writeln!(out, "encrypted: yes (text extraction may be unavailable)");
    }
    let _ = writeln!(out, "size: {}", fmt_bytes(bytes.len()));

    let mut text = String::new();
    for body in pdf_stream_bodies(bytes) {
        let extracted = pdf_extract_text(&body);
        if extracted.trim().is_empty() {
            continue;
        }
        text.push_str(&extracted);
        if text.len() >= MAX_PDF_TEXT_BYTES {
            break;
        }
    }
    if text.trim().is_empty() {
        let _ = write!(
            out,
            "\n[No extractable text layer in this PDF ({pages} pages); it is likely scanned or image-only]"
        );
    } else {
        let trimmed = text.trim();
        let body = if trimmed.len() > MAX_PDF_TEXT_BYTES {
            format!(
                "{}\n[… {} more bytes of text elided]",
                truncate_width(trimmed, MAX_PDF_TEXT_BYTES),
                trimmed.len() - MAX_PDF_TEXT_BYTES
            )
        } else {
            trimmed.to_string()
        };
        let _ = write!(out, "\n{body}");
    }
    out
}

// ------------------------------------------------------ pi-ast integration

/// A rendered structural summary (`read-summary.ts` shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSummary {
    pub language: Option<String>,
    pub total_lines: u32,
    pub elided_lines: u32,
    pub rendered: String,
}

/// Render `pi_ast` summary segments with omp's elision marker and the
/// "re-read needed ranges" footer.
#[must_use]
pub fn render_summary(result: &pi_ast::summary::SummaryResult, selector_base: &str) -> (String, u32) {
    let mut out = String::new();
    let mut elided = 0u32;
    let mut ranges: Vec<String> = Vec::new();
    for segment in &result.segments {
        if segment.kind == "elided" {
            let lines = segment.end_line.saturating_sub(segment.start_line) + 1;
            elided = elided.saturating_add(lines);
            ranges.push(format!("{}-{}", segment.start_line, segment.end_line));
            let _ = writeln!(out, "[… {lines} lines elided]");
        } else if let Some(text) = &segment.text {
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    if elided > 0 {
        let _ = write!(
            out,
            "\n[…{elided} lines elided; re-read needed ranges with {selector_base}:{}]",
            ranges.join(",")
        );
    }
    (out.trim_end().to_string(), elided)
}

/// Structural summary of `code`, or `None` when the language is unknown or the
/// source does not parse (the caller then falls back to the plain text read).
///
/// # Errors
/// A tree-sitter parse failure (never a syntax error: that yields `Ok(None)`).
pub fn summarize_source(
    code: &str,
    path: &Path,
    unfold_until: Option<u32>,
) -> Result<Option<SourceSummary>> {
    let options = pi_ast::summary::SummaryOptions {
        code: code.to_string(),
        lang: None,
        path: Some(path.to_string_lossy().into_owned()),
        min_body_lines: None,
        min_comment_lines: None,
        unfold_until_lines: unfold_until,
        unfold_limit_lines: unfold_until.map(|lines| lines.saturating_mul(2)),
    };
    let result = pi_ast::summary::summarize_code(options)?;
    if !result.parsed {
        return Ok(None);
    }
    let (rendered, elided_lines) = render_summary(&result, &path.to_string_lossy());
    Ok(Some(SourceSummary {
        language: result.language,
        total_lines: result.total_lines,
        elided_lines,
        rendered,
    }))
}

/// The syntactic block that *begins* on `line` (omp `buildLineEntriesWithBlockContext`).
///
/// # Errors
/// A tree-sitter parse failure; an unknown language or a line with no block is
/// `Ok(None)`.
pub fn block_at(code: &str, path: &Path, line: u32) -> Result<Option<(u32, u32)>> {
    let options = pi_ast::block::BlockRangeOptions {
        code: code.to_string(),
        lang: None,
        path: Some(path.to_string_lossy().into_owned()),
        line,
    };
    Ok(pi_ast::block::block_range_at(options)?.map(|range| (range.start_line, range.end_line)))
}

/// Boundary lines to show alongside a visible window: the closing line of a
/// block whose opener is visible, and vice versa.
///
/// # Errors
/// A tree-sitter parse failure.
pub fn enclosing_boundaries(
    code: &str,
    path: &Path,
    ranges: &[(u32, u32)],
) -> Result<Option<Vec<u32>>> {
    let options = pi_ast::block::EnclosingBoundaryOptions {
        code: code.to_string(),
        lang: None,
        path: Some(path.to_string_lossy().into_owned()),
        ranges: ranges
            .iter()
            .map(|(start_line, end_line)| pi_ast::block::LineRange {
                start_line: *start_line,
                end_line: *end_line,
            })
            .collect(),
    };
    pi_ast::block::enclosing_block_boundaries(options)
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------- detection

    #[test]
    fn sniffs_formats_by_magic_and_extension() {
        assert_eq!(sniff_format(b"SQLite format 3\0rest"), Some(Format::Sqlite));
        assert_eq!(sniff_format(b"PK\x03\x04rest"), Some(Format::Zip));
        assert_eq!(sniff_format(b"\x1f\x8b\x08rest"), Some(Format::Gzip));
        assert_eq!(sniff_format(b"%PDF-1.7"), Some(Format::Pdf));
        assert_eq!(sniff_format(b"\x89PNG\r\n\x1a\n"), Some(Format::Image));
        assert_eq!(sniff_format(b"plain text"), None);

        assert_eq!(extension_format(Path::new("nb.ipynb")), Some(Format::Notebook));
        assert_eq!(extension_format(Path::new("a.json")), Some(Format::Json));
        assert_eq!(extension_format(Path::new("a.jsonl")), Some(Format::JsonLines));
        assert_eq!(extension_format(Path::new("a.tar.gz")), Some(Format::TarGz));
        assert_eq!(extension_format(Path::new("a.tgz")), Some(Format::TarGz));
        assert_eq!(extension_format(Path::new("report.docx")), Some(Format::Zip));

        // Magic wins for containers; the notebook extension wins over JSON magic.
        assert_eq!(
            detect_format(Path::new("state.bin"), b"SQLite format 3\0"),
            Format::Sqlite
        );
        assert_eq!(
            detect_format(Path::new("nb.ipynb"), b"{\"cells\":[]}"),
            Format::Notebook
        );
        assert_eq!(detect_format(Path::new("a.txt"), b"hello"), Format::Text);
    }

    // ------------------------------------------------------------- sqlite

    #[test]
    fn sqlite_selector_parses_every_shape() {
        assert_eq!(parse_sqlite_selector("", "").unwrap(), SqliteSelector::List);
        assert_eq!(
            parse_sqlite_selector(":users", "").unwrap(),
            SqliteSelector::Schema {
                table: "users".to_string(),
                sample_limit: DEFAULT_SCHEMA_SAMPLE_LIMIT
            }
        );
        assert_eq!(
            parse_sqlite_selector("users:42", "").unwrap(),
            SqliteSelector::Row {
                table: "users".to_string(),
                key: "42".to_string()
            }
        );
        assert_eq!(
            parse_sqlite_selector("users", "order=name:desc&limit=5&offset=10").unwrap(),
            SqliteSelector::Query {
                table: "users".to_string(),
                limit: 5,
                offset: 10,
                order: Some("name:desc".to_string()),
                filter: None
            }
        );
        assert_eq!(
            parse_sqlite_selector("", "q=SELECT%201").unwrap(),
            SqliteSelector::Raw {
                sql: "SELECT 1".to_string(),
                page: None
            }
        );
        assert_eq!(
            parse_sqlite_selector("", "q=SELECT%201&limit=2&offset=4").unwrap(),
            SqliteSelector::Raw {
                sql: "SELECT 1".to_string(),
                page: Some(SqlitePage { limit: 2, offset: 4 })
            }
        );
    }

    #[test]
    fn sqlite_selector_rejects_unsafe_where_and_mixed_forms() {
        let err = parse_sqlite_selector("users", "where=1%3D1%3B%20DROP%20TABLE%20users").unwrap_err();
        assert!(
            err.to_string().contains("comments or statement terminators"),
            "{err}"
        );
        let err = parse_sqlite_selector("users", "where=id%3E1%20LIMIT%205").unwrap_err();
        assert!(err.to_string().contains("LIMIT/OFFSET"), "{err}");
        // A quoted literal may contain the forbidden word.
        assert!(parse_sqlite_selector("users", "where=name%3D'limit'").is_ok());
        assert!(parse_sqlite_selector("users:3", "limit=2").is_err());
        assert!(parse_sqlite_selector("users", "q=SELECT%201").is_err());
        assert!(parse_sqlite_selector("", "limit=2").is_err());
        assert!(parse_sqlite_selector("users", "bogus=1").is_err());
    }

    #[test]
    fn sqlite_reader_lists_pages_and_reads_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score REAL);\
                 INSERT INTO users (name, score) VALUES ('ada', 9.5), ('grace', 8.0), ('linus', 7.25);",
            )
            .unwrap();
        }
        assert_eq!(sniff_format(&std::fs::read(&path).unwrap()), Some(Format::Sqlite));

        let listing = sqlite_read(&path, "", "").unwrap();
        assert!(listing.contains("users (3 rows)"), "{listing}");

        let schema = sqlite_read(&path, "users", "").unwrap();
        assert!(schema.contains("CREATE TABLE users"), "{schema}");
        assert!(schema.contains("Sample rows:"), "{schema}");

        let page = sqlite_read(&path, "users", "order=name:asc&limit=2").unwrap();
        assert!(page.contains("ada") && page.contains("grace"), "{page}");
        assert!(!page.contains("linus"), "{page}");
        assert!(page.contains("1 more rows"), "{page}");

        let row = sqlite_read(&path, "users:2", "").unwrap();
        assert!(row.contains("grace"), "{row}");

        let missing = sqlite_read(&path, "users:99", "").unwrap();
        assert!(missing.contains("No row found"), "{missing}");

        let filtered = sqlite_read(&path, "users", "where=score%3E8").unwrap();
        assert!(filtered.contains("ada") && !filtered.contains("grace"), "{filtered}");

        let raw = sqlite_read(&path, "", "q=SELECT%20count(*)%20AS%20n%20FROM%20users").unwrap();
        assert!(raw.contains("| n |"), "{raw}");
        assert!(raw.contains("| 3 |"), "{raw}");
    }

    #[test]
    fn wide_tables_fall_back_to_vertical_blocks() {
        let columns: Vec<String> = (0..25).map(|index| format!("c{index}")).collect();
        let rows = vec![columns.clone()];
        let table = build_ascii_table(&columns, &rows);
        assert!(table.contains("── Row 1 ──"), "{table}");
        assert!(table.contains("c0:"), "{table}");

        let narrow = build_ascii_table(&["a".to_string(), "b".to_string()], &[]);
        assert!(narrow.contains("(no rows)"), "{narrow}");
    }

    // ------------------------------------------------------------ archives

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = u32::try_from(out.len()).unwrap();
            let size = u32::try_from(data.len()).unwrap();
            let name_len = u16::try_from(name.len()).unwrap();
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // crc (unchecked)
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // stored
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&name_len.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = u32::try_from(out.len()).unwrap();
        let cd_size = u32::try_from(central.len()).unwrap();
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn tar_header(name: &str, size: usize) -> [u8; 512] {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        header
    }

    fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            out.extend_from_slice(&tar_header(name, data.len()));
            out.extend_from_slice(data);
            let pad = (512 - data.len() % 512) % 512;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn zip_central_directory_lists_and_reads() {
        let zip = build_zip(&[
            ("notes/readme.txt", b"hello zip"),
            ("data.json", b"{\"a\":1}"),
            ("notes/", b""),
        ]);
        let index = ArchiveIndex::open(ArchiveKind::Zip, &zip).unwrap();
        let listing = render_archive_listing(&index);
        assert!(listing.contains("notes/readme.txt"), "{listing}");
        assert!(listing.contains("notes/"), "{listing}");
        let entry = index
            .entries()
            .iter()
            .find(|entry| entry.path == "notes/readme.txt")
            .unwrap();
        assert_eq!(index.read(entry).unwrap(), b"hello zip");
        assert!(index.entries().iter().any(|entry| entry.path == "notes/" && entry.is_dir));
    }

    #[test]
    fn tar_and_targz_members_roundtrip() {
        let tar = build_tar(&[("a.txt", b"alpha"), ("dir/b.txt", b"beta")]);
        let index = ArchiveIndex::open(ArchiveKind::Tar, &tar).unwrap();
        assert_eq!(index.entries().len(), 2);
        let entry = index
            .entries()
            .iter()
            .find(|entry| entry.path == "dir/b.txt")
            .unwrap();
        assert_eq!(index.read(entry).unwrap(), b"beta");

        let compressed = gzip(&tar);
        assert_eq!(sniff_format(&compressed), Some(Format::Gzip));
        let index = ArchiveIndex::open(ArchiveKind::TarGz, &compressed).unwrap();
        assert_eq!(index.entries().len(), 2);
        let entry = index
            .entries()
            .iter()
            .find(|entry| entry.path == "a.txt")
            .unwrap();
        assert_eq!(index.read(entry).unwrap(), b"alpha");
    }

    #[test]
    fn splits_archive_members_and_rejects_line_selectors() {
        assert_eq!(
            split_archive_member("pkg.tar.gz:src/main.rs").unwrap(),
            ("pkg.tar.gz".to_string(), "src/main.rs".to_string())
        );
        assert_eq!(
            split_archive_member("report.docx:word/document.xml")
                .unwrap()
                .1,
            "word/document.xml"
        );
        assert!(split_archive_member("notes.txt:12-20").is_none());
        assert!(split_archive_member("C:/tmp/a.zip").is_none());
    }

    #[test]
    fn archive_read_browses_and_extracts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.zip");
        std::fs::write(
            &path,
            build_zip(&[("docs/a.md", b"# doc\n"), ("docs/b.md", b"# b\n")]),
        )
        .unwrap();
        let listing = archive_read(&path, ArchiveKind::Zip, None).unwrap();
        assert!(listing.contains("docs/a.md"), "{listing}");
        let member = archive_read(&path, ArchiveKind::Zip, Some("docs/a.md")).unwrap();
        assert_eq!(member, "# doc\n");
        let subtree = archive_read(&path, ArchiveKind::Zip, Some("docs")).unwrap();
        assert!(subtree.contains("docs/b.md"), "{subtree}");
        assert!(archive_read(&path, ArchiveKind::Zip, Some("nope.txt")).is_err());
    }

    // ---------------------------------------------------------------- json

    #[test]
    fn json_filter_paths_pipes_and_paging() {
        let doc = r#"{"users":[{"name":"ada","age":36},{"name":"grace","age":85},{"name":"linus","age":54}]}"#;

        let selector = parse_json_selector("q=.users[].name").unwrap();
        assert_eq!(
            json_query(doc, &selector).unwrap(),
            "\"ada\"\n\"grace\"\n\"linus\""
        );

        let selector = parse_json_selector("q=.users[].name&raw=true").unwrap();
        assert_eq!(json_query(doc, &selector).unwrap(), "ada\ngrace\nlinus");

        let selector = parse_json_selector("q=.users|length").unwrap();
        assert_eq!(json_query(doc, &selector).unwrap(), "3");

        let selector = parse_json_selector("q=.users[0].name&raw=true").unwrap();
        assert_eq!(json_query(doc, &selector).unwrap(), "ada");

        let selector = parse_json_selector("q=.users&limit=2&offset=1").unwrap();
        let page = json_query(doc, &selector).unwrap();
        assert!(page.contains("grace") && page.contains("linus"), "{page}");
        assert!(!page.contains("ada"), "{page}");
        assert!(page.contains("1 more items"), "{page}");

        let selector = parse_json_selector("q=keys").unwrap();
        assert!(json_query(doc, &selector).unwrap().contains("users"));

        assert!(parse_json_selector("nope=1").is_none());
        assert_eq!(parse_json_selector("raw=true").unwrap().query, ".");
    }

    #[test]
    fn json_query_handles_streams_and_bad_filters() {
        let stream = "{\"n\":1}\n{\"n\":2}\n";
        let selector = parse_json_selector("q=.n&raw=true").unwrap();
        assert_eq!(json_query(stream, &selector).unwrap(), "1\n2");

        let selector = parse_json_selector("q=.a[]").unwrap();
        let err = json_query("{\"a\":1}", &selector).unwrap_err();
        assert!(err.to_string().contains("cannot iterate over number"), "{err}");

        let selector = parse_json_selector("q=map(.a)").unwrap();
        let err = json_query("{}", &selector).unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");

        assert_eq!(json_pretty("{\"a\":1}"), "{\n  \"a\": 1\n}");
    }

    // ------------------------------------------------------------ notebook

    #[test]
    fn notebook_formats_cells_and_outputs() {
        let notebook = r##"{
            "nbformat": 4,
            "nbformat_minor": 5,
            "metadata": {"kernelspec": {"language": "python"}},
            "cells": [
                {"cell_type": "markdown", "source": ["# Title\n", "body\n"]},
                {"cell_type": "code", "execution_count": 2, "source": ["print(1)"],
                 "outputs": [{"output_type": "stream", "name": "stdout", "text": ["one\n"]}]},
                {"cell_type": "code", "execution_count": 3, "source": ["boom()"],
                 "outputs": [{"output_type": "error", "ename": "ValueError", "evalue": "bad",
                              "traceback": ["line a", "line b"]}]}
            ]
        }"##;
        let out = format_notebook(notebook).unwrap();
        assert!(out.contains("3 cells"), "{out}");
        assert!(out.contains("kernel: python"), "{out}");
        assert!(out.contains("── cell 1 [markdown] ──"), "{out}");
        assert!(out.contains("# Title"), "{out}");
        assert!(out.contains("── cell 2 [code, exec 2] ──"), "{out}");
        assert!(out.contains("[out]\none"), "{out}");
        assert!(out.contains("[error] ValueError: bad"), "{out}");
        assert!(out.contains("  line b"), "{out}");
        assert!(format_notebook("not json").is_err());
    }

    // -------------------------------------------------------------- binary

    #[test]
    fn binary_metadata_strings_and_hexdump() {
        let mut bytes = b"\x7fELF\x02\x01\x01\x00".to_vec();
        bytes.extend_from_slice(b"\x00\x00hello world\x00\x00");

        let meta = binary_metadata("x.bin", &bytes);
        assert!(meta.contains("kind: ELF executable"), "{meta}");
        assert!(meta.contains("sha256: "), "{meta}");
        assert!(meta.contains("entropy: "), "{meta}");

        let strings = extract_strings(&bytes, 4);
        assert!(strings.iter().any(|text| text == "hello world"), "{strings:?}");

        let dump = hexdump(&bytes, 0, 16);
        assert!(dump.starts_with("00000000  7f 45 4c 46"), "{dump}");
        assert!(dump.contains("|.ELF"), "{dump}");

        let view = binary_read("x.bin", &bytes, "strings").unwrap();
        assert!(view.contains("hello world"), "{view}");
        assert!(binary_read("x.bin", &bytes, "hex:8:8").unwrap().starts_with("00000008"));
        assert!(binary_read("x.bin", &bytes, "bogus").is_err());
    }

    // ----------------------------------------------------------------- pdf

    #[test]
    fn pdf_metadata_and_text_fallback() {
        use std::io::Write as _;
        let content = b"BT /F1 12 Tf 72 720 Td (Hello PDF) Tj T* (Second line) Tj ET";
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(content).unwrap();
        let stream = encoder.finish().unwrap();

        let mut pdf = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R >>\nstream\n".to_vec();
        pdf.extend_from_slice(&stream);
        pdf.extend_from_slice(
            b"\nendstream\nendobj\ntrailer\n<< /Info 4 0 R /Root 1 0 R >>\n4 0 obj\n<< /Title (Sample Doc) /Author (Ada) >>\nendobj\n%%EOF\n",
        );

        let out = pdf_read(&pdf);
        assert!(out.contains("version: 1.7"), "{out}");
        assert!(out.contains("pages: 1"), "{out}");
        assert!(out.contains("title: Sample Doc"), "{out}");
        assert!(out.contains("author: Ada"), "{out}");
        assert!(out.contains("Hello PDF"), "{out}");
        assert!(out.contains("Second line"), "{out}");
    }

    #[test]
    fn pdf_without_a_text_layer_says_so() {
        let pdf = b"%PDF-1.4\n1 0 obj\n<< /Type /Page >>\nendobj\n%%EOF\n".to_vec();
        let out = pdf_read(&pdf);
        assert!(out.contains("pages: 1"), "{out}");
        assert!(out.contains("No extractable text layer"), "{out}");
    }

    // -------------------------------------------------------------- pi-ast

    #[test]
    fn summarizes_rust_bodies_with_pi_ast() {
        let code = "fn a() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n}\n";
        let summary = summarize_source(code, Path::new("x.rs"), None)
            .unwrap()
            .expect("rust parses");
        assert!(summary.rendered.contains("fn a() {"), "{}", summary.rendered);
        assert!(
            summary.rendered.contains("lines elided"),
            "{}",
            summary.rendered
        );
        assert!(summary.elided_lines >= 1);
        assert_eq!(summary.language.as_deref(), Some("rust"));
        assert_eq!(summary.total_lines, 5);

        // An unsupported language falls back rather than lying about elisions.
        assert!(
            summarize_source("plain\n", Path::new("x.txt"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolves_blocks_and_boundaries_with_pi_ast() {
        let code = "fn a() {\n  work();\n}\n";
        assert_eq!(
            block_at(code, Path::new("x.rs"), 1).unwrap(),
            Some((1, 3))
        );
        assert_eq!(block_at(code, Path::new("x.rs"), 3).unwrap(), None);
        assert_eq!(
            enclosing_boundaries(code, Path::new("x.rs"), &[(1, 1)]).unwrap(),
            Some(vec![3])
        );
    }
}
