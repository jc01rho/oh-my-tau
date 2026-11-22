//! Image token billing: read the pixel size from an image header (PNG `IHDR`, JPEG `SOF0`/`SOF2`,
//! GIF logical screen, WebP `VP8X`) and charge it with a provider patch scheme.
//!
//! Only the header is decoded: the base64 payload is decoded up to [`HEADER_PROBE_BYTES`], so a
//! JPEG whose `SOF` marker sits behind a larger EXIF/ICC block yields `None` (callers then fall back
//! to their unknown-size budget).

use pa_types::ai::ModelTokenizer;

/// Decoded-byte cap for header probing (covers a maximal 64 KiB JPEG APP segment).
pub const HEADER_PROBE_BYTES: usize = 70 * 1024;

/// Pixel dimensions read from an image header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

/// How a provider turns pixels into prompt tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageBilling {
    /// `openai-patch`: one token per 32x32 patch, at most 1536 patches (the image is scaled down
    /// to fit, then snapped to whole patches), as documented for GPT-4.1 / o4-mini class models.
    OpenAiPatch,
    /// `deepseek-patch`: DeepSeek-VL2 dynamic tiling: a 384px global view plus an `m x n` grid of
    /// 384px local tiles (`m * n <= 9`), 14x14 tokens per tile after 2x2 pixel shuffle, plus one
    /// newline token per token row and one view separator.
    DeepSeekPatch,
}

impl ImageBilling {
    /// Wire name of the scheme.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiPatch => "openai-patch",
            Self::DeepSeekPatch => "deepseek-patch",
        }
    }

    /// Billing scheme for a tokenizer family; `None` where no pixel scheme is modelled.
    #[must_use]
    pub const fn for_tokenizer(tokenizer: ModelTokenizer) -> Option<Self> {
        match tokenizer {
            ModelTokenizer::O200k => Some(Self::OpenAiPatch),
            ModelTokenizer::DeepseekV3 => Some(Self::DeepSeekPatch),
            _ => None,
        }
    }

    /// Prompt tokens for an image of `size`.
    #[must_use]
    pub fn tokens(self, size: ImageSize) -> u64 {
        match self {
            Self::OpenAiPatch => openai_patch_tokens(size),
            Self::DeepSeekPatch => deepseek_patch_tokens(size),
        }
    }
}

const OPENAI_PATCH: f64 = 32.0;
const OPENAI_MAX_PATCHES: u64 = 1536;

fn openai_patch_tokens(size: ImageSize) -> u64 {
    let patches = |w: f64, h: f64| ((w / OPENAI_PATCH).ceil() * (h / OPENAI_PATCH).ceil()) as u64;
    let (w, h) = (f64::from(size.width), f64::from(size.height));
    let raw = patches(w, h);
    if raw <= OPENAI_MAX_PATCHES {
        return raw;
    }
    let shrink = (OPENAI_PATCH * OPENAI_PATCH * OPENAI_MAX_PATCHES as f64 / (w * h)).sqrt();
    let (sw, sh) = (w * shrink, h * shrink);
    let snap = ((sw / OPENAI_PATCH).floor() / (sw / OPENAI_PATCH))
        .min((sh / OPENAI_PATCH).floor() / (sh / OPENAI_PATCH));
    patches(sw * snap, sh * snap).min(OPENAI_MAX_PATCHES)
}

const DEEPSEEK_TILE: u64 = 384;
const DEEPSEEK_MAX_TILES: u64 = 9;
const DEEPSEEK_TILE_SIDE_TOKENS: u64 = 14;

fn deepseek_patch_tokens(size: ImageSize) -> u64 {
    let (w, h) = (u64::from(size.width), u64::from(size.height));
    let original_area = w * h;
    // `select_best_resolution`: most effective pixels, then least padding.
    let mut best = (1, 1);
    let mut best_key = (0, u64::MAX);
    for rows in 1..=DEEPSEEK_MAX_TILES {
        for cols in 1..=DEEPSEEK_MAX_TILES / rows {
            let (tw, th) = (cols * DEEPSEEK_TILE, rows * DEEPSEEK_TILE);
            let scale = (tw as f64 / w as f64).min(th as f64 / h as f64);
            let scaled = ((w as f64 * scale) as u64) * ((h as f64 * scale) as u64);
            let effective = scaled.min(original_area);
            let wasted = tw * th - effective;
            if effective > best_key.0 || (effective == best_key.0 && wasted < best_key.1) {
                best = (rows, cols);
                best_key = (effective, wasted);
            }
        }
    }
    let (rows, cols) = best;
    let side = DEEPSEEK_TILE_SIDE_TOKENS;
    let global = side * (side + 1);
    let local = side * rows * (side * cols + 1);
    global + 1 + local
}

/// Pixel size from the leading bytes of a PNG, JPEG (`SOF0`/`SOF2`), GIF or WebP (`VP8X`) file.
#[must_use]
pub fn image_size(bytes: &[u8]) -> Option<ImageSize> {
    let size = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        png_size(bytes)
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        jpeg_size(bytes)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some((le16(bytes, 6)?, le16(bytes, 8)?))
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        webp_vp8x_size(bytes)
    } else {
        None
    }?;
    match size {
        (0, _) | (_, 0) => None,
        (width, height) => Some(ImageSize { width, height }),
    }
}

/// [`image_size`] over a base64 payload (a `data:...;base64,` prefix is accepted).
#[must_use]
pub fn image_size_base64(data: &str) -> Option<ImageSize> {
    let payload = data
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once("base64,").map(|(_, b64)| b64))
        .unwrap_or(data);
    image_size(&decode_base64_prefix(payload, HEADER_PROBE_BYTES))
}

fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(12..16) != Some(b"IHDR") {
        return None;
    }
    Some((be32(b, 16)?, be32(b, 20)?))
}

fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 1 < b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        if marker == 0xFF {
            i += 1; // fill byte
            continue;
        }
        i += 2;
        match marker {
            0x01 | 0xD0..=0xD7 => continue, // standalone markers carry no length
            0xD9 | 0xDA => return None,     // EOI / start of scan before any frame header
            _ => {}
        }
        let len = usize::try_from(be16(b, i)?).ok()?;
        if len < 2 {
            return None;
        }
        if matches!(marker, 0xC0 | 0xC2) {
            // length(2) precision(1) height(2) width(2)
            let height = be16(b, i + 3)?;
            let width = be16(b, i + 5)?;
            return Some((width, height));
        }
        i += len;
    }
    None
}

fn webp_vp8x_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.get(12..16) != Some(b"VP8X") {
        return None;
    }
    // chunk size(4) flags(1) reserved(3) canvas width-1 (24-bit LE) canvas height-1 (24-bit LE)
    Some((le24(b, 24)? + 1, le24(b, 27)? + 1))
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 2)?;
    Some(u32::from(u16::from_be_bytes([s[0], s[1]])))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 2)?;
    Some(u32::from(u16::from_le_bytes([s[0], s[1]])))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], 0]))
}

/// Decode standard base64 until `max` bytes, padding, or the first invalid character.
fn decode_base64_prefix(data: &str, max: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(max.min(data.len() / 4 * 3 + 3));
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in data.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => break,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn b64(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |a, (i, &b)| a | u32::from(b) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
            for _ in chunk.len()..3 {
                s.push('=');
            }
        }
        s
    }

    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b
    }

    fn jpeg(sof: u8, width: u16, height: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8];
        // APP0 (JFIF) segment, then fill byte, restart marker, and the frame header.
        b.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        b.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        b.extend_from_slice(&[0xFF, 0xFF, 0xD0]);
        b.extend_from_slice(&[0xFF, sof, 0x00, 0x11, 0x08]);
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        b
    }

    fn size(width: u32, height: u32) -> ImageSize {
        ImageSize { width, height }
    }

    #[test]
    fn parses_png_ihdr() {
        assert_eq!(image_size(&png(1920, 1080)), Some(size(1920, 1080)));
    }

    #[test]
    fn parses_jpeg_sof0_and_sof2_past_app_segments() {
        for sof in [0xC0, 0xC2] {
            assert_eq!(image_size(&jpeg(sof, 640, 480)), Some(size(640, 480)));
        }
        // SOF1 (extended sequential) is not a supported frame header.
        assert_eq!(image_size(&jpeg(0xC1, 640, 480)), None);
        // Start of scan before any frame header.
        assert_eq!(image_size(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
    }

    #[test]
    fn parses_gif_logical_screen() {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&300u16.to_le_bytes());
        b.extend_from_slice(&200u16.to_le_bytes());
        assert_eq!(image_size(&b), Some(size(300, 200)));
    }

    #[test]
    fn parses_webp_vp8x_canvas() {
        let mut b = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\x10\0\0\0".to_vec();
        b.extend_from_slice(&(4000u32 - 1).to_le_bytes()[..3]);
        b.extend_from_slice(&(3000u32 - 1).to_le_bytes()[..3]);
        assert_eq!(image_size(&b), Some(size(4000, 3000)));
        // Simple lossy (`VP8 `) files carry no VP8X canvas header.
        assert_eq!(image_size(b"RIFF\0\0\0\0WEBPVP8 \0\0\0\0"), None);
    }

    #[test]
    fn rejects_truncated_unknown_and_zero_sized_headers() {
        assert_eq!(image_size(&png(1, 1)[..20]), None);
        assert_eq!(image_size(&png(0, 10)), None);
        assert_eq!(image_size(b"not an image"), None);
        assert_eq!(image_size(&[]), None);
    }

    #[test]
    fn base64_probe_reads_header_and_data_urls() {
        let data = b64(&png(800, 600));
        assert_eq!(image_size_base64(&data), Some(size(800, 600)));
        assert_eq!(
            image_size_base64(&format!("data:image/png;base64,{data}")),
            Some(size(800, 600))
        );
        assert_eq!(image_size_base64("!!!"), None);
        assert_eq!(decode_base64_prefix("aGVsbG8=", 3), b"hel");
        assert_eq!(decode_base64_prefix("aGVs\nbG8=", 64), b"hello");
    }

    #[test]
    fn openai_patch_matches_documented_examples() {
        let bill = |w, h| ImageBilling::OpenAiPatch.tokens(size(w, h));
        // 1024x1024 -> 32x32 patches.
        assert_eq!(bill(1024, 1024), 1024);
        // 1800x2400 -> scaled to 1056x1408 -> 33x44 patches.
        assert_eq!(bill(1800, 2400), 1452);
        assert_eq!(bill(1, 1), 1);
        assert_eq!(bill(33, 1), 2);
        // 10000x10000 -> scaled to 1248x1248 -> 39x39 patches.
        assert_eq!(bill(10_000, 10_000), 1521);
        assert!(bill(100_000, 20) <= 1536);
    }

    #[test]
    fn deepseek_patch_tiles_like_vl2() {
        let bill = |w, h| ImageBilling::DeepSeekPatch.tokens(size(w, h));
        // Global view 14x15 + separator + one local tile 14x15.
        assert_eq!(bill(100, 100), 421);
        assert_eq!(bill(384, 384), 421);
        // 2 columns x 1 row: 211 + 14 * (28 + 1).
        assert_eq!(bill(768, 384), 617);
        // 1 column x 2 rows: 211 + 28 * 15.
        assert_eq!(bill(384, 768), 631);
        // Large square images cap at 3x3 tiles: 211 + 42 * 43.
        assert_eq!(bill(4000, 4000), 2017);
    }

    #[test]
    fn billing_per_tokenizer() {
        assert_eq!(
            ImageBilling::for_tokenizer(ModelTokenizer::O200k),
            Some(ImageBilling::OpenAiPatch)
        );
        assert_eq!(
            ImageBilling::for_tokenizer(ModelTokenizer::DeepseekV3),
            Some(ImageBilling::DeepSeekPatch)
        );
        assert_eq!(ImageBilling::for_tokenizer(ModelTokenizer::ClaudeV3), None);
        assert_eq!(ImageBilling::OpenAiPatch.as_str(), "openai-patch");
        assert_eq!(ImageBilling::DeepSeekPatch.as_str(), "deepseek-patch");
    }
}
