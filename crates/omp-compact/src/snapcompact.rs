//! Snapcompact archive write side: text normalization, cell wrapping and
//! pagination, a dependency-free bitmap renderer (PNG), and the
//! `blob:sha256:` frame encoding that keeps image payloads out of the session
//! journal.
//!
//! The replay side lives in [`crate::native`]: [`resolve_archive`] turns
//! persisted `blob:sha256:` frame references back into inline payloads so
//! [`crate::native::history_blocks`] emits image blocks instead of
//! "unavailable" gap markers.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::native::{Archive, Frame};

/// Prefix of a persisted frame payload reference.
pub const BLOB_REFERENCE_PREFIX: &str = "blob:sha256:";
/// Newline glyph: archived text packs line breaks inline (see
/// [`crate::native::to_plain_text`]).
pub const NEWLINE_GLYPH: char = '\u{2588}';
/// Dim span toggles (zero-width; rendered in gray ink).
pub const DIM_ON: char = '\u{000e}';
pub const DIM_OFF: char = '\u{000f}';
/// Font id recorded on rendered frames (shape metadata).
pub const FONT_ID: &str = "tau-3x5";
/// Ink variant recorded on rendered frames.
pub const VARIANT_ID: &str = "bw";
const TAB_WIDTH: usize = 4;
const GLYPH_W: usize = 3;
const GLYPH_H: usize = 5;
/// Glyph plus one pixel of spacing on each axis.
const CELL_W: usize = GLYPH_W + 1;
const CELL_H: usize = GLYPH_H + 1;

/// Frame geometry: a page of `cols` x `rows` character cells, each cell drawn
/// `scale` times its native 4x6 pixel size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub cols: usize,
    pub rows: usize,
    pub scale: usize,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            cols: 160,
            rows: 120,
            scale: 2,
        }
    }
}

impl Shape {
    #[must_use]
    pub fn width(&self) -> usize {
        self.cols * CELL_W * self.scale
    }

    #[must_use]
    pub fn height(&self) -> usize {
        self.rows * CELL_H * self.scale
    }
}

/// Strip ANSI CSI sequences, fold line endings, expand tabs, map other
/// whitespace to a space, drop control characters (except the dim toggles)
/// and pack line breaks as [`NEWLINE_GLYPH`].
#[must_use]
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(NEWLINE_GLYPH);
            }
            '\n' => out.push(NEWLINE_GLYPH),
            '\t' => out.extend(std::iter::repeat_n(' ', TAB_WIDTH)),
            DIM_ON | DIM_OFF => out.push(c),
            c if c.is_control() => {}
            c if c.is_whitespace() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Split normalized text into rows of at most `cols` visible cells. Dim
/// toggles take no cell; a newline glyph occupies one cell and ends its row.
#[must_use]
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut width = 0usize;
    for c in text.chars() {
        if matches!(c, DIM_ON | DIM_OFF) {
            row.push(c);
            continue;
        }
        if width == cols {
            rows.push(std::mem::take(&mut row));
            width = 0;
        }
        row.push(c);
        width += 1;
        if c == NEWLINE_GLYPH {
            rows.push(std::mem::take(&mut row));
            width = 0;
        }
    }
    if width > 0 || !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// Group wrapped rows into pages of `rows_per_page` rows.
#[must_use]
pub fn paginate(rows: &[String], rows_per_page: usize) -> Vec<Vec<String>> {
    rows.chunks(rows_per_page.max(1))
        .map(<[String]>::to_vec)
        .collect()
}

/// 3x5 glyph rows (MSB = leftmost pixel); `None` for characters without a glyph.
fn glyph(c: char) -> Option<[u8; GLYPH_H]> {
    let rows: &str = match c.to_ascii_uppercase() {
        '0' => "111101101101111",
        '1' => "010110010010111",
        '2' => "111001111100111",
        '3' => "111001111001111",
        '4' => "101101111001001",
        '5' => "111100111001111",
        '6' => "111100111101111",
        '7' => "111001001001001",
        '8' => "111101111101111",
        '9' => "111101111001111",
        'A' => "010101111101101",
        'B' => "110101110101110",
        'C' => "011100100100011",
        'D' => "110101101101110",
        'E' => "111100110100111",
        'F' => "111100110100100",
        'G' => "011100101101011",
        'H' => "101101111101101",
        'I' => "111010010010111",
        'J' => "001001001101010",
        'K' => "101101110101101",
        'L' => "100100100100111",
        'M' => "101111111101101",
        'N' => "110101101101101",
        'O' => "010101101101010",
        'P' => "110101110100100",
        'Q' => "010101101111011",
        'R' => "110101110101101",
        'S' => "011100010001110",
        'T' => "111010010010010",
        'U' => "101101101101111",
        'V' => "101101101101010",
        'W' => "101101111111101",
        'X' => "101101010101101",
        'Y' => "101101010010010",
        'Z' => "111001010100111",
        '.' => "000000000000010",
        ',' => "000000000010100",
        ':' => "000010000010000",
        ';' => "000010000010100",
        '!' => "010010010000010",
        '?' => "111001010000010",
        '-' => "000000111000000",
        '_' => "000000000000111",
        '+' => "000010111010000",
        '=' => "000111000111000",
        '/' => "001001010100100",
        '\\' => "100100010001001",
        '(' => "001010010010001",
        ')' => "100010010010100",
        '[' => "011010010010011",
        ']' => "110010010010110",
        '{' => "011010110010011",
        '}' => "110010011010110",
        '<' => "001010100010001",
        '>' => "100010001010100",
        '\'' => "010010000000000",
        '"' => "101101000000000",
        '`' => "100010000000000",
        '*' => "101010101000000",
        '#' => "101111101111101",
        '|' => "010010010010010",
        '&' => "010101010101011",
        '%' => "101001010100101",
        '$' => "011110010011110",
        '@' => "010101111100011",
        '^' => "010101000000000",
        '~' => "000011110000000",
        NEWLINE_GLYPH => "111111111111111",
        ' ' => "000000000000000",
        _ => return None,
    };
    let bits = rows.as_bytes();
    let mut out = [0u8; GLYPH_H];
    for (r, row) in out.iter_mut().enumerate() {
        for b in &bits[r * GLYPH_W..(r + 1) * GLYPH_W] {
            *row = (*row << 1) | u8::from(*b == b'1');
        }
    }
    Some(out)
}

/// Fallback glyph for characters the font does not cover (a hollow box).
const MISSING_GLYPH: [u8; GLYPH_H] = [0b111, 0b101, 0b101, 0b101, 0b111];

/// Rasterize one page into an 8-bit grayscale bitmap (`width * height`
/// bytes, white background, black ink, gray ink inside dim spans).
#[must_use]
pub fn rasterize(page: &[String], shape: Shape) -> Vec<u8> {
    let (width, height) = (shape.width(), shape.height());
    let mut pixels = vec![255u8; width * height];
    let mut dim = false;
    for (row, line) in page.iter().take(shape.rows).enumerate() {
        let mut col = 0usize;
        for c in line.chars() {
            match c {
                DIM_ON => dim = true,
                DIM_OFF => dim = false,
                _ if col >= shape.cols => {}
                _ => {
                    let ink = if dim { 128 } else { 0 };
                    let bits = glyph(c).unwrap_or(MISSING_GLYPH);
                    for (gy, bits_row) in bits.iter().enumerate() {
                        for gx in 0..GLYPH_W {
                            if bits_row >> (GLYPH_W - 1 - gx) & 1 == 0 {
                                continue;
                            }
                            let x0 = (col * CELL_W + gx) * shape.scale;
                            let y0 = (row * CELL_H + gy) * shape.scale;
                            for y in y0..y0 + shape.scale {
                                let start = y * width + x0;
                                pixels[start..start + shape.scale].fill(ink);
                            }
                        }
                    }
                    col += 1;
                }
            }
        }
    }
    pixels
}

/// Encode an 8-bit grayscale bitmap as PNG (stored deflate blocks: no
/// compression dependency, deterministic bytes).
#[must_use]
pub fn encode_png(pixels: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width + 1) * height);
    for row in pixels.chunks(width) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    if blocks.peek().is_none() {
        zlib.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        zlib.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &zlib);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in bytes {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard (padded) base64.
#[must_use]
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Strict standard base64 decode; `None` on any invalid input.
#[must_use]
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (q, quad) in bytes.chunks(4).enumerate() {
        let last = q + 1 == bytes.len() / 4;
        let pad = quad.iter().rev().take_while(|b| **b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (i, b) in quad.iter().enumerate() {
            let v = if i >= 4 - pad {
                0
            } else {
                B64.iter().position(|c| c == b)? as u32
            };
            n |= v << (18 - 6 * i);
        }
        out.extend(n.to_be_bytes()[1..4 - pad].iter());
    }
    Some(out)
}

/// `blob:sha256:<hex>` reference for a payload's bytes.
#[must_use]
pub fn blob_reference(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(BLOB_REFERENCE_PREFIX.len() + 64);
    out.push_str(BLOB_REFERENCE_PREFIX);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Canonical form of a blob reference: trimmed, case-insensitive prefix,
/// exactly 64 hex digits, lowercased. `None` when `text` is not a reference.
#[must_use]
pub fn normalize_blob_reference(text: &str) -> Option<String> {
    let text = text.trim();
    let prefix = text.get(..BLOB_REFERENCE_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(BLOB_REFERENCE_PREFIX) {
        return None;
    }
    let hex = &text[BLOB_REFERENCE_PREFIX.len()..];
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!(
        "{BLOB_REFERENCE_PREFIX}{}",
        hex.to_ascii_lowercase()
    ))
}

/// Content-addressed store for frame payload bytes.
pub trait BlobStore {
    /// Store `bytes`, returning their canonical reference.
    fn put(&mut self, bytes: &[u8]) -> String;
    /// Bytes for a (canonical) reference.
    fn get(&self, reference: &str) -> Option<Vec<u8>>;
}

/// In-memory [`BlobStore`].
#[derive(Debug, Default, Clone)]
pub struct MemoryBlobStore {
    blobs: HashMap<String, Vec<u8>>,
}

impl MemoryBlobStore {
    #[must_use]
    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blobs.is_empty()
    }
}

impl BlobStore for MemoryBlobStore {
    fn put(&mut self, bytes: &[u8]) -> String {
        let reference = blob_reference(bytes);
        self.blobs
            .entry(reference.clone())
            .or_insert_with(|| bytes.to_vec());
        reference
    }

    fn get(&self, reference: &str) -> Option<Vec<u8>> {
        self.blobs.get(reference).cloned()
    }
}

/// One rendered archive frame with the grid metadata persisted beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedFrame {
    pub frame: Frame,
    pub cols: usize,
    pub rows: usize,
    /// Visible characters on the page (dim toggles excluded).
    pub chars: usize,
}

impl RenderedFrame {
    /// The persisted `preserveData.snapcompact.frames[]` record.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("data".into(), self.frame.data.clone().into());
        obj.insert("mimeType".into(), self.frame.mime_type.clone().into());
        obj.insert("cols".into(), self.cols.into());
        obj.insert("rows".into(), self.rows.into());
        obj.insert("chars".into(), self.chars.into());
        if let Some(font) = &self.frame.font {
            obj.insert("font".into(), font.clone().into());
        }
        if let Some(variant) = &self.frame.variant {
            obj.insert("variant".into(), variant.clone().into());
        }
        if let Some(detail) = &self.frame.detail {
            obj.insert("detail".into(), detail.clone().into());
        }
        Value::Object(obj)
    }
}

/// Render archive text into PNG frames. Payloads are stored in `store` and
/// the frames carry their `blob:sha256:` references.
#[must_use]
pub fn render_frames(text: &str, shape: Shape, store: &mut dyn BlobStore) -> Vec<RenderedFrame> {
    let rows = wrap(&normalize(text), shape.cols);
    paginate(&rows, shape.rows)
        .into_iter()
        .map(|page| {
            let chars = page
                .iter()
                .flat_map(|r| r.chars())
                .filter(|c| !matches!(*c, DIM_ON | DIM_OFF))
                .count();
            let png = encode_png(&rasterize(&page, shape), shape.width(), shape.height());
            RenderedFrame {
                frame: Frame {
                    data: store.put(&png),
                    mime_type: "image/png".into(),
                    font: Some(FONT_ID.into()),
                    variant: Some(VARIANT_ID.into()),
                    detail: None,
                },
                cols: shape.cols,
                rows: shape.rows,
                chars,
            }
        })
        .collect()
}

/// The persisted `preserveData.snapcompact` archive object for rendered
/// frames plus the kept source text.
#[must_use]
pub fn archive_value(frames: &[RenderedFrame], text: Option<&str>, truncated_chars: f64) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "frames".into(),
        Value::Array(frames.iter().map(RenderedFrame::to_value).collect()),
    );
    obj.insert("truncatedChars".into(), json!(truncated_chars));
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        obj.insert("text".into(), normalize(text).into());
    }
    Value::Object(obj)
}

/// Move inline base64 frame payloads into `store`, replacing them with their
/// references; non-canonical references are normalized. Payloads that are
/// neither valid base64 nor references are left untouched.
#[must_use]
pub fn externalize_frames(archive: &Archive, store: &mut dyn BlobStore) -> Archive {
    let mut out = archive.clone();
    for frame in &mut out.frames {
        if let Some(reference) = normalize_blob_reference(&frame.data) {
            frame.data = reference;
        } else if let Some(bytes) = base64_decode(&frame.data) {
            frame.data = store.put(&bytes);
        }
    }
    out
}

/// Resolve `blob:sha256:` frame references back into inline base64 payloads.
/// A reference the resolver cannot satisfy, or whose bytes do not hash to
/// the reference, stays a reference and replays as an "unavailable" gap.
#[must_use]
pub fn resolve_archive(archive: &Archive, resolve: &dyn Fn(&str) -> Option<Vec<u8>>) -> Archive {
    let mut out = archive.clone();
    for frame in &mut out.frames {
        let Some(reference) = normalize_blob_reference(&frame.data) else {
            continue;
        };
        if let Some(bytes) = resolve(&reference).filter(|b| blob_reference(b) == reference) {
            frame.data = base64_encode(&bytes);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{get_preserved_archive, history_blocks};

    #[test]
    fn normalize_folds_endings_tabs_controls_and_ansi() {
        let text = "a\r\nb\rc\n\td\u{1b}[31mred\u{1b}[0m\u{7}\u{a0}\u{000e}x\u{000f}";
        assert_eq!(
            normalize(text),
            "a\u{2588}b\u{2588}c\u{2588}    dred \u{000e}x\u{000f}"
        );
    }

    #[test]
    fn wrap_breaks_on_width_and_newline_glyph_with_zero_width_dim() {
        let rows = wrap("\u{000e}abcde\u{000f}\u{2588}fg", 3);
        assert_eq!(rows, vec!["\u{000e}abc", "de\u{000f}\u{2588}", "fg"]);
        assert_eq!(wrap("", 3), Vec::<String>::new());
        assert_eq!(paginate(&rows, 2).len(), 2);
    }

    #[test]
    fn base64_round_trips_and_rejects_garbage() {
        for len in 0..8 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 5) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&bytes)), Some(bytes));
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_decode("TW=u"), None);
        assert_eq!(base64_decode("TWF"), None);
        assert_eq!(base64_decode("T!Fu"), None);
    }

    #[test]
    fn blob_reference_is_sha256_hex_and_normalizes() {
        let reference = blob_reference(b"abc");
        assert_eq!(
            reference,
            "blob:sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let shouted = format!("  BLOB:SHA256:{}  ", reference[12..].to_ascii_uppercase());
        assert_eq!(
            normalize_blob_reference(&shouted).as_deref(),
            Some(reference.as_str())
        );
        assert_eq!(normalize_blob_reference("blob:sha256:abc"), None);
        assert_eq!(normalize_blob_reference("iVBORw0KGgo="), None);
    }

    #[test]
    fn png_has_valid_signature_header_and_checksums() {
        let shape = Shape {
            cols: 2,
            rows: 1,
            scale: 1,
        };
        let pixels = rasterize(&["A\u{000e}b".to_owned()], shape);
        assert_eq!(pixels.len(), 8 * 6);
        assert!(pixels.contains(&0) && pixels.contains(&128) && pixels.contains(&255));
        let png = encode_png(&pixels, 8, 6);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 8);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 6);
        let ihdr_crc = u32::from_be_bytes(png[29..33].try_into().unwrap());
        assert_eq!(ihdr_crc, crc32(&png[12..29]));
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn rendered_archive_round_trips_through_blob_store_and_replay() {
        let mut store = MemoryBlobStore::default();
        let shape = Shape {
            cols: 8,
            rows: 2,
            scale: 1,
        };
        let text = "user: fix the parser\nassistant: done, tests pass";
        let frames = render_frames(text, shape, &mut store);
        assert_eq!(frames.len(), 4);
        assert!(frames
            .iter()
            .all(|f| normalize_blob_reference(&f.frame.data).as_deref() == Some(&*f.frame.data)));
        assert_eq!(frames.iter().map(|f| f.chars).sum::<usize>(), 48);
        assert!(store.len() <= frames.len() && !store.is_empty());

        let preserve = json!({"snapcompact": archive_value(&frames, Some(text), 0.0)});
        let archive = get_preserved_archive(Some(&preserve)).expect("archive");
        assert_eq!(archive.frames.len(), 4);

        let unresolved = history_blocks(&archive, None);
        assert_eq!(unresolved.len(), 1);
        assert!(unresolved[0]["text"]
            .as_str()
            .unwrap()
            .contains("4 archived image frames unavailable"));

        let resolved = resolve_archive(&archive, &|r| store.get(r));
        let blocks = history_blocks(&resolved, None);
        assert_eq!(blocks.len(), 4);
        for block in &blocks {
            assert_eq!(block["type"], "image");
            assert_eq!(block["mimeType"], "image/png");
            let png = base64_decode(block["data"].as_str().unwrap()).unwrap();
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        }

        let mut tampered = MemoryBlobStore::default();
        let first = archive.frames[0].data.clone();
        tampered.blobs.insert(first, b"not the frame".to_vec());
        let partial = resolve_archive(&archive, &|r| tampered.get(r));
        assert!(partial
            .frames
            .iter()
            .all(|f| f.data.starts_with(BLOB_REFERENCE_PREFIX)));
    }

    #[test]
    fn externalize_moves_inline_payloads_and_normalizes_references() {
        let png = encode_png(&[0, 255], 2, 1);
        let reference = blob_reference(&png);
        let frame = |data: String| Frame {
            data,
            mime_type: "image/png".into(),
            font: None,
            variant: None,
            detail: None,
        };
        let archive = Archive {
            frames: vec![
                frame(base64_encode(&png)),
                frame(
                    reference
                        .to_ascii_uppercase()
                        .replace("BLOB:SHA256:", "blob:sha256:"),
                ),
                frame("%%not-base64%%".into()),
            ],
            truncated_chars: 0.0,
            text: None,
            text_head: None,
            text_tail: None,
        };
        let mut store = MemoryBlobStore::default();
        let out = externalize_frames(&archive, &mut store);
        assert_eq!(out.frames[0].data, reference);
        assert_eq!(out.frames[1].data, reference);
        assert_eq!(out.frames[2].data, "%%not-base64%%");
        assert_eq!(store.get(&reference), Some(png.clone()));
        let back = resolve_archive(&out, &|r| store.get(r));
        assert_eq!(back.frames[0].data, base64_encode(&png));
    }
}
