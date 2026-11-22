//! Golden tests for the provider-native compaction lanes: expectations in
//! `fixtures/native-session.expected.json` were produced by running omp's own
//! TypeScript (`buildSessionContext` + `convertToLlm` + `renderHandoffPrompt`,
//! pi-coding-agent 18.6.1) over `fixtures/native-session.jsonl` under bun
//! (script: `fixtures/native-capture.ts`). Four scenarios on one fixture:
//! remote (kept tail skipped), remote with `providerReplayThroughEntryId`,
//! Anthropic native (kept tail + `historyRewriteAt`), snapcompact (blob-referenced
//! frames replay as unavailable gaps).

use omp_compact::context::Leaf;
use omp_compact::{build_session_context, convert_to_llm};
use serde_json::Value;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn entries() -> Vec<Value> {
    fixture("native-session.jsonl")
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| !matches!(e["type"].as_str(), Some("session" | "title")))
        .collect()
}

fn expected() -> Value {
    serde_json::from_str(&fixture("native-session.expected.json")).unwrap()
}

#[test]
fn native_lanes_match_ts() {
    let all = entries();
    let exp = expected();
    for (leaf, want) in exp["scenarios"].as_object().unwrap() {
        let ctx = build_session_context(&all, Leaf::Id(leaf)).unwrap();
        assert_eq!(
            Value::Array(ctx.messages.clone()),
            want["agent"],
            "leaf {leaf}"
        );
        assert_eq!(
            Value::Array(convert_to_llm(&ctx.messages)),
            want["llm"],
            "leaf {leaf}"
        );
    }
}

#[test]
fn remote_replacement_history_carries_provider_payload() {
    let all = entries();
    let ctx = build_session_context(&all, Leaf::Id("aa66bb66")).unwrap();
    let summary = &ctx.messages[0];
    assert_eq!(summary["providerPayload"]["type"], "openaiResponsesHistory");
    assert_eq!(summary["providerPayload"]["provider"], "openai-codex");
    // The kept tail is carried natively; only post-compaction messages emit.
    assert_eq!(ctx.messages.len(), 3);
    assert!(!ctx.messages.iter().any(|m| m["role"] == "user"
        && m["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("kept-tail")));
}

#[test]
fn anthropic_keeps_tail_and_predates_rewrite_marker() {
    let all = entries();
    let ctx = build_session_context(&all, Leaf::Id("cc77cc77")).unwrap();
    assert_eq!(
        ctx.messages[0]["providerPayload"]["type"],
        "anthropicCompaction"
    );
    // kept tail: user turn + assistant answer between firstKept and the compaction
    assert_eq!(ctx.messages.len(), 5);
    let kept_at = all.iter().find(|e| e["id"] == "cc33cc33").unwrap()["timestamp"]
        .as_str()
        .unwrap();
    let kept_ms = omp_compact::context::branch_path(&all, Leaf::Id("cc77cc77")); // path exists
    let _ = kept_ms;
    let ts = chrono_ms(kept_at);
    assert_eq!(ctx.messages[0]["historyRewriteAt"], Value::from(ts - 1));
}

#[test]
fn snapcompact_blob_frames_replay_as_gaps() {
    let all = entries();
    let ctx = build_session_context(&all, Leaf::Id("dd55dd55")).unwrap();
    let blocks = ctx.messages[0]["blocks"].as_array().unwrap();
    let texts: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
    assert!(texts[0].starts_with("HEAD-TEXT"));
    assert!(texts[1].contains("2 archived image frames unavailable here"));
    assert!(texts[1].contains("middle history omitted above"));
    assert!(texts[1].ends_with("TAIL-TEXT"));
    // blob references are never handed to the provider as image data
    assert!(!blocks.iter().any(|b| b["type"] == "image"));
}

fn chrono_ms(iso: &str) -> i64 {
    omp_compact::parse_iso_ms(iso).expect("valid ISO timestamp")
}

#[test]
fn rollover_compaction_replays_as_plain_summary() {
    // A rollover compaction is a real compaction entry whose summary is the
    // static rollover prompt; it must replay like any other summary, not fail
    // the rebuild. Appended after scenario A, it also supersedes the earlier
    // provider-native compaction on the branch.
    let mut all = entries();
    all.push(serde_json::json!({
        "type": "compaction",
        "id": "ee11ee11",
        "parentId": "aa66bb66",
        "timestamp": "2026-10-12T10:00:40.000Z",
        "summary": "ROLLOVER-PROMPT",
        "firstKeptEntryId": "aa55bb55",
        "tokensBefore": 1234,
        "details": {"kind": "experimental-context-rollover", "version": 1}
    }));
    let ctx = build_session_context(&all, Leaf::Last).unwrap();
    let summary = &ctx.messages[0];
    assert_eq!(summary["role"], "compactionSummary");
    assert_eq!(summary["summary"], "ROLLOVER-PROMPT");
    assert_eq!(summary["tokensBefore"], 1234);
    // Nothing provider-native is attached: the rollover carries no payload.
    assert!(summary.get("providerPayload").is_none());
    assert!(summary.get("blocks").is_none());
    // The kept tail from firstKeptEntryId replays verbatim.
    let roles: Vec<&str> = ctx.messages[1..]
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant"]);
    // And it converts like a local compaction summary.
    let llm = convert_to_llm(&ctx.messages);
    let text = llm[0]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("<summary>\nROLLOVER-PROMPT\n</summary>"));
    assert_eq!(llm[0]["historyRewriteAt"], llm[0]["timestamp"]);
}
