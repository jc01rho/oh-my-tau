//! Token estimation (pi-agent-core `Tokenizer.countMessage`).
//!
//! omp counts with a model-specific native tokenizer when the catalog names one and with a
//! `ceil(bytes / 4)` estimate otherwise (and always in tests). The estimate is the default here;
//! inject a [`Tokenizer`] for exact counts.

use pa_types::ai::ModelTokenizer;
use serde_json::Value;

use crate::image_tokens::{image_size_base64, ImageBilling};
use crate::json::{message_role, str_field};

/// Counts tokens in one text fragment. Fragments of a message are counted independently and
/// summed, like the TS `countTokens(string[])`.
pub trait Tokenizer: Sync {
    fn count_text(&self, text: &str) -> u64;

    /// Pixel billing scheme for image blocks; `None` keeps the unknown-size budgets.
    fn image_billing(&self) -> Option<ImageBilling> {
        None
    }

    /// Estimated tokens for one image block: the decoded header size billed with
    /// [`Tokenizer::image_billing`] when both are known, else the unknown-size patch budget (never
    /// undercounts).
    fn count_image(&self, block: &Value) -> u64 {
        if let Some(tokens) = billed_image_tokens(self.image_billing(), block) {
            return tokens;
        }
        match str_field(block, "detail") {
            Some("low") => 308,
            Some("high") => 3000,
            _ => 12_000,
        }
    }
}

/// `(utf8 bytes + 3) >> 2`: omp's fallback when no native tokenizer applies.
#[derive(Debug, Clone, Copy, Default)]
pub struct ByteEstimate;

impl Tokenizer for ByteEstimate {
    fn count_text(&self, text: &str) -> u64 {
        (text.len() as u64 + 3) >> 2
    }
}

/// Byte estimate for text plus a pixel [`ImageBilling`] scheme for image blocks and frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchEstimate {
    pub billing: ImageBilling,
}

impl PatchEstimate {
    /// Estimator for a tokenizer family with a modelled pixel scheme.
    #[must_use]
    pub const fn for_tokenizer(tokenizer: ModelTokenizer) -> Option<Self> {
        match ImageBilling::for_tokenizer(tokenizer) {
            Some(billing) => Some(Self { billing }),
            None => None,
        }
    }
}

impl Tokenizer for PatchEstimate {
    fn count_text(&self, text: &str) -> u64 {
        ByteEstimate.count_text(text)
    }

    fn image_billing(&self) -> Option<ImageBilling> {
        Some(self.billing)
    }
}

fn billed_image_tokens(billing: Option<ImageBilling>, block: &Value) -> Option<u64> {
    let billing = billing?;
    let size = image_size_base64(str_field(block, "data")?)?;
    Some(billing.tokens(size))
}

/// Token cost of one snapcompact frame (`snapcompact.FRAME_TOKEN_ESTIMATE`).
const FRAME_TOKEN_ESTIMATE: u64 = 5024;

/// Tokens for one snapcompact frame image: its header size billed with the tokenizer's
/// [`ImageBilling`] when known, else [`FRAME_TOKEN_ESTIMATE`].
#[must_use]
pub fn frame_tokens(t: &dyn Tokenizer, frame: &Value) -> u64 {
    billed_image_tokens(t.image_billing(), frame).unwrap_or(FRAME_TOKEN_ESTIMATE)
}

/// Options for [`count_message`] (`MessageCountOptions`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CountOptions {
    /// Drop opaque provider reasoning payloads (signatures, redacted thinking, native server-tool
    /// blocks) from the estimate.
    pub exclude_encrypted_reasoning: bool,
}

fn json_string(v: &Value) -> String {
    omp_session::entries::to_js_string(v).unwrap_or_else(|_| "null".to_owned())
}

/// Sum token estimates over text fragments without collecting them: identical
/// results to buffering `String`s, but no per-message text is cloned (this runs
/// over the whole session context at every post-turn threshold check).
fn content_blocks(content: &Value, total: &mut u64, extra: &mut u64, t: &dyn Tokenizer) {
    match content {
        Value::String(s) => *total += t.count_text(s),
        Value::Array(blocks) => {
            for block in blocks {
                match str_field(block, "type") {
                    Some("text") => {
                        if let Some(text) = str_field(block, "text").filter(|s| !s.is_empty()) {
                            *total += t.count_text(text);
                        }
                    }
                    Some("image") => *extra += t.count_image(block),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Port of `Tokenizer.countMessage` over a JSON agent message (user, developer, assistant,
/// toolResult, custom, hookMessage, bashExecution, branchSummary, compactionSummary; any other
/// role counts as 0).
#[must_use]
pub fn count_message(t: &dyn Tokenizer, message: &Value, options: CountOptions) -> u64 {
    let mut total = 0u64;
    let mut extra = 0u64;
    match message_role(message) {
        "bashExecution" => {
            for key in ["command", "output"] {
                if let Some(s) = str_field(message, key) {
                    total += t.count_text(s);
                }
            }
            return total;
        }
        "assistant" => {
            for block in message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                match str_field(block, "type") {
                    Some("text") => {
                        total += t.count_text(str_field(block, "text").unwrap_or(""));
                    }
                    Some("thinking") => {
                        total += t.count_text(str_field(block, "thinking").unwrap_or(""));
                        if let Some(sig) =
                            str_field(block, "thinkingSignature").filter(|s| !s.is_empty())
                        {
                            if !options.exclude_encrypted_reasoning {
                                total += t.count_text(sig);
                            }
                        }
                    }
                    Some("toolCall") => {
                        total += t.count_text(str_field(block, "name").unwrap_or(""));
                        total += t.count_text(
                            &block
                                .get("arguments")
                                .map_or_else(|| "null".to_owned(), json_string),
                        );
                    }
                    Some("redactedThinking") => {
                        if !options.exclude_encrypted_reasoning {
                            total += t.count_text(str_field(block, "data").unwrap_or(""));
                        }
                    }
                    Some("anthropicServerTool") if !options.exclude_encrypted_reasoning => {
                        total += t.count_text(
                            &block
                                .get("block")
                                .map_or_else(|| "null".to_owned(), json_string),
                        );
                    }
                    _ => {}
                }
            }
        }
        "user" | "developer" | "custom" | "hookMessage" | "toolResult" => {
            if let Some(content) = message.get("content") {
                content_blocks(content, &mut total, &mut extra, t);
            }
        }
        role @ ("branchSummary" | "compactionSummary") => {
            total += t.count_text(str_field(message, "summary").unwrap_or(""));
            if role == "compactionSummary" {
                if let Some(blocks) = message.get("blocks").and_then(Value::as_array) {
                    for block in blocks {
                        if str_field(block, "type") == Some("text") {
                            total += t.count_text(str_field(block, "text").unwrap_or(""));
                        } else {
                            extra += frame_tokens(t, block);
                        }
                    }
                } else if let Some(images) = message.get("images").and_then(Value::as_array) {
                    extra += images
                        .iter()
                        .map(|image| frame_tokens(t, image))
                        .sum::<u64>();
                }
            }
        }
        _ => return 0,
    }
    extra + total
}

/// Sum of [`count_message`] over `messages`.
#[must_use]
pub fn count_messages(t: &dyn Tokenizer, messages: &[Value], options: CountOptions) -> u64 {
    messages.iter().map(|m| count_message(t, m, options)).sum()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::image_tokens::tests::{b64, png};

    fn image(width: u32, height: u32) -> Value {
        json!({"type": "image", "data": b64(&png(width, height)), "mimeType": "image/png"})
    }

    #[test]
    fn count_image_bills_decoded_size_when_scheme_known() {
        let openai = PatchEstimate::for_tokenizer(ModelTokenizer::O200k).unwrap();
        let deepseek = PatchEstimate::for_tokenizer(ModelTokenizer::DeepseekV3).unwrap();
        assert!(PatchEstimate::for_tokenizer(ModelTokenizer::ClaudeV47).is_none());

        let block = image(1024, 1024);
        assert_eq!(openai.count_image(&block), 1024);
        assert_eq!(deepseek.count_image(&block), 211 + 14 * 3 * (14 * 3 + 1));
        assert_eq!(ByteEstimate.count_image(&block), 12_000);

        let undecodable = json!({"type": "image", "data": "!!", "detail": "low"});
        assert_eq!(openai.count_image(&undecodable), 308);
    }

    #[test]
    fn count_message_sums_text_and_billed_images() {
        let message = json!({
            "role": "user",
            "content": [{"type": "text", "text": "abcdefgh"}, image(64, 64)],
        });
        let openai = PatchEstimate {
            billing: ImageBilling::OpenAiPatch,
        };
        assert_eq!(
            count_message(&openai, &message, CountOptions::default()),
            2 + 4
        );
        assert_eq!(
            count_message(&ByteEstimate, &message, CountOptions::default()),
            2 + 12_000
        );
    }

    #[test]
    fn frame_tokens_bill_frames_or_fall_back_to_estimate() {
        let openai = PatchEstimate {
            billing: ImageBilling::OpenAiPatch,
        };
        let frame = image(320, 64);
        assert_eq!(frame_tokens(&openai, &frame), 10 * 2);
        assert_eq!(frame_tokens(&ByteEstimate, &frame), FRAME_TOKEN_ESTIMATE);
        assert_eq!(
            frame_tokens(&openai, &json!({"type": "image"})),
            FRAME_TOKEN_ESTIMATE
        );

        let summary = json!({
            "role": "compactionSummary",
            "summary": "abcd",
            "blocks": [{"type": "text", "text": "abcd"}, frame],
        });
        let options = CountOptions::default();
        assert_eq!(count_message(&openai, &summary, options), 1 + 1 + 20);
        assert_eq!(
            count_message(&ByteEstimate, &summary, options),
            1 + 1 + FRAME_TOKEN_ESTIMATE
        );

        let legacy = json!({
            "role": "compactionSummary",
            "summary": "",
            "images": [frame, {"data": "", "mimeType": "image/png"}],
        });
        assert_eq!(
            count_message(&openai, &legacy, options),
            20 + FRAME_TOKEN_ESTIMATE
        );
    }
}
