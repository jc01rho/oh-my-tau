//! Fixture tests: each scheme's read matches TS behavior on a fake session
//! store (`<session>.jsonl` + sibling artifacts dir), per internal-urls parity.

use std::path::PathBuf;

use omp_urls::schemes::{artifact, core, history, local, RuleEntry};
use omp_urls::{parse_internal_url, UrlCtx, UrlProvider};

/// Serializes the tests that publish a process-global book (`rule://`,
/// `memory://`); they share one process, so unguarded they race.
static GLOBALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_globals() -> std::sync::MutexGuard<'static, ()> {
    GLOBALS.lock().unwrap_or_else(|e| e.into_inner())
}

fn fixture() -> (tempfile::TempDir, PathBuf, UrlCtx) {
    let dir = tempfile::tempdir().unwrap();
    let session_file = dir.path().join("demo.jsonl");
    std::fs::write(
        &session_file,
        "{\"type\":\"session\",\"version\":3,\"id\":\"demo\"}\n",
    )
    .unwrap();
    let artifacts = dir.path().join("demo");
    std::fs::create_dir_all(&artifacts).unwrap();
    let ctx = UrlCtx {
        session_file: Some(session_file),
        ..UrlCtx::default()
    };
    (dir, artifacts, ctx)
}

#[test]
fn artifact_reads_numbered_file() {
    let (_dir, artifacts, ctx) = fixture();
    std::fs::write(artifacts.join("7.log"), "hello\nworld\n").unwrap();
    let provider = artifact::ArtifactProvider;
    let url = parse_internal_url("artifact://7").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(resource.content, "hello\nworld\n");
    assert!(resource.immutable.is_none()); // scheme default applied by Registry::resolve
    assert!(provider.locate(&url, &ctx).unwrap().ends_with("7.log"));
}

#[test]
fn artifact_miss_lists_available() {
    let (_dir, artifacts, ctx) = fixture();
    std::fs::write(artifacts.join("2.log"), "x").unwrap();
    std::fs::write(artifacts.join("10.log"), "y").unwrap();
    let provider = artifact::ArtifactProvider;
    let url = parse_internal_url("artifact://5").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    assert_eq!(err, "Artifact 5 not found. Available: 2, 10");
}

#[test]
fn artifact_rejects_non_numeric_id() {
    let (_dir, _artifacts, ctx) = fixture();
    let provider = artifact::ArtifactProvider;
    let url = parse_internal_url("artifact://abc").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    assert_eq!(err, "artifact:// ID must be numeric, got: abc");
}

#[test]
fn agent_reads_markdown_and_extracts_json() {
    let (_dir, artifacts, ctx) = fixture();
    std::fs::write(
        artifacts.join("Worker.md"),
        "{\"report\": \"done\", \"n\": 3}",
    )
    .unwrap();
    let provider = omp_urls::schemes::agent::AgentProvider;

    let url = parse_internal_url("agent://Worker").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(resource.content, "{\"report\": \"done\", \"n\": 3}");

    // String extraction reads as prose.
    let url = parse_internal_url("agent://Worker/report").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(resource.content, "done");

    // Non-string extraction pretty-prints JSON.
    let url = parse_internal_url("agent://Worker/n").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(resource.content, "3");
}

#[test]
fn agent_all_is_write_only() {
    let (_dir, _artifacts, ctx) = fixture();
    let provider = omp_urls::schemes::agent::AgentProvider;
    let url = parse_internal_url("agent://all").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    assert_eq!(
        err,
        "agent://all is write-only; use it to broadcast a message."
    );
}

#[test]
fn agent_not_found_suggests_closest() {
    let (_dir, artifacts, ctx) = fixture();
    std::fs::write(artifacts.join("Reviewer.md"), "x").unwrap();
    let provider = omp_urls::schemes::agent::AgentProvider;
    let url = parse_internal_url("agent://Reviewr").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    assert_eq!(err, "Not found: Reviewr\nDid you mean: Reviewer");
}

#[test]
fn local_lists_and_reads_files() {
    let (_dir, artifacts, ctx) = fixture();
    let local_root = artifacts.join("local");
    std::fs::create_dir_all(local_root.join("sub")).unwrap();
    std::fs::write(local_root.join("notes.txt"), "alpha\nbeta\n").unwrap();
    std::fs::write(local_root.join("sub/deep.txt"), "deep\n").unwrap();

    let provider = local::LocalProvider;
    let url = parse_internal_url("local://").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert!(resource.content.starts_with("# Local\n\n"));
    assert!(resource
        .content
        .contains("- [notes.txt](local://notes.txt)"));
    assert!(resource
        .content
        .contains("- [sub/deep.txt](local://sub/deep.txt)"));
    assert!(resource.content.contains("2 files available"));

    let url = parse_internal_url("local://notes.txt").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(resource.content, "alpha\nbeta\n");
}

#[test]
fn local_rejects_traversal_and_unknown() {
    let (_dir, _artifacts, ctx) = fixture();
    let provider = local::LocalProvider;
    let url = parse_internal_url("local://../escape.txt").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    assert_eq!(err, "Path traversal (..) is not allowed in local:// URLs");
    let url = parse_internal_url("local://missing.txt").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    assert_eq!(err, "Local file not found: local://missing.txt");
}

#[test]
fn local_root_falls_back_to_tmp_without_session() {
    let ctx = UrlCtx::default();
    let root = local::resolve_local_root(&ctx);
    assert!(root.ends_with(std::path::Path::new("omp-local").join("session")));
}

#[test]
fn history_renders_index_and_transcript() {
    let (_dir, artifacts, ctx) = fixture();
    let transcript = artifacts.join("Worker.jsonl");
    let header = r#"{"type":"session","version":3,"id":"w"}"#;
    let line1 = r#"{"type":"message","id":"e1","parentId":"root","timestamp":"2026-10-12T00:00:00Z","message":{"role":"user","content":"hi","timestamp":1760000000000}}"#;
    let line2 = r#"{"type":"message","id":"e2","parentId":"e1","timestamp":"2026-10-12T00:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}],"timestamp":1760000001000}}"#;
    std::fs::write(&transcript, format!("{header}\n{line1}\n{line2}\n")).unwrap();

    let provider = history::HistoryProvider;
    let url = parse_internal_url("history://").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(
        resource.content,
        "# Agents\n\n| id | status | kind | parent | last activity |\n|---|---|---|---|---|\n| Worker | on disk | — | — | — |\n\nRead a transcript with `read history://<id>`.\n"
    );

    let url = parse_internal_url("history://Worker").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(
        resource.content,
        "# Worker (on disk)\n\n## user\n\nhi\n\n## assistant\n\nhello\n"
    );
    assert!(provider.locate(&url, &ctx).unwrap() == transcript);
}

#[test]
fn history_unknown_agent_lists_known() {
    let (_dir, artifacts, ctx) = fixture();
    std::fs::write(artifacts.join("Alpha.jsonl"), "").unwrap();
    let provider = history::HistoryProvider;
    let url = parse_internal_url("history://Beta").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    // TS lists registered refs only; without a registry that is "none".
    assert_eq!(
        err,
        "Unknown agent: Beta\nKnown agents: none\nList all with history://"
    );
}

#[test]
fn rule_read_matches_ts_on_an_empty_book() {
    let _guard = lock_globals();
    let (_dir, _artifacts, ctx) = fixture();
    omp_urls::schemes::rule::set_active_rules(Vec::new());
    let provider = omp_urls::schemes::rule::RuleProvider;
    let url = parse_internal_url("rule://tdd").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    assert_eq!(err, "Unknown rule: tdd\nAvailable: none");
    assert!(provider.locate(&url, &ctx).is_none());
    assert!(provider.complete("", &ctx).is_empty());
    assert!(provider.prompt_doc().is_none());
}

#[test]
fn rule_active_book_serves_read_locate_and_complete() {
    let _guard = lock_globals();
    let (dir, _artifacts, ctx) = fixture();
    let file = dir.path().join("tdd.md");
    std::fs::write(&file, "---\nalwaysApply: true\n---\nWrite tests first.\n").unwrap();
    omp_urls::schemes::rule::set_active_rules(vec![RuleEntry {
        name: "tdd".into(),
        path: file.to_string_lossy().into_owned(),
        content: "Write tests first.".into(),
        description: "Test-first discipline".into(),
        globs: vec!["*.rs".into()],
        always_apply: true,
    }]);

    let provider = omp_urls::schemes::rule::RuleProvider;
    let url = parse_internal_url("rule://tdd").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(resource.content, "Write tests first.");
    assert_eq!(resource.immutable, Some(true));
    assert_eq!(provider.locate(&url, &ctx).unwrap(), file);

    let completions = provider.complete("", &ctx);
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].label, "tdd");
    assert_eq!(
        completions[0].detail.as_deref(),
        Some("Test-first discipline")
    );

    // The scheme is advertised once it has addressable rules.
    assert_eq!(
        provider.prompt_doc().as_deref(),
        Some("rule://<name>: details.")
    );

    // A miss lists the installed names.
    let url = parse_internal_url("rule://nope").unwrap();
    assert_eq!(
        provider.read(&url, &ctx).unwrap_err().to_string(),
        "Unknown rule: nope\nAvailable: tdd"
    );

    omp_urls::schemes::rule::set_active_rules(Vec::new());
}

#[test]
fn memory_root_reads_project_artifacts() {
    let (dir, _artifacts, _ctx) = fixture();
    let root = dir.path().join("memories/--proj--");
    std::fs::create_dir_all(root.join("skills/demo")).unwrap();
    std::fs::write(root.join("memory_summary.md"), "Summary.\n").unwrap();
    std::fs::write(root.join("MEMORY.md"), "# Memory\n").unwrap();
    std::fs::write(root.join("skills/demo/SKILL.md"), "Skill").unwrap();
    let ctx = UrlCtx {
        memory_root: Some(root.clone()),
        ..UrlCtx::default()
    };

    let provider = omp_urls::schemes::memory::MemoryProvider;
    let url = parse_internal_url("memory://root").unwrap();
    assert_eq!(provider.read(&url, &ctx).unwrap().content, "Summary.\n");

    let url = parse_internal_url("memory://root/MEMORY.md").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert_eq!(resource.content, "# Memory\n");
    assert_eq!(provider.locate(&url, &ctx).unwrap(), root.join("MEMORY.md"));

    let url = parse_internal_url("memory://root/skills/demo").unwrap();
    assert_eq!(provider.read(&url, &ctx).unwrap().content, "SKILL.md");
}

#[test]
fn memory_errors_match_ts() {
    let _guard = lock_globals();
    let (dir, _artifacts, _ctx) = fixture();
    omp_urls::schemes::memory::set_memory_records(Vec::new());
    let ctx = UrlCtx {
        memory_root: Some(dir.path().join("absent")),
        ..UrlCtx::default()
    };
    let provider = omp_urls::schemes::memory::MemoryProvider;

    let url = parse_internal_url("memory://root").unwrap();
    assert_eq!(
        provider.read(&url, &ctx).unwrap_err().to_string(),
        omp_urls::schemes::memory::ROOT_UNAVAILABLE
    );

    let url = parse_internal_url("memory://").unwrap();
    assert_eq!(
        provider.read(&url, &ctx).unwrap_err().to_string(),
        "memory:// URL requires a namespace: memory://root or memory://<memory-id>"
    );

    // Without a published book, an unknown host is an unknown namespace.
    let url = parse_internal_url("memory://m-1").unwrap();
    let err = provider.read(&url, &ctx).unwrap_err().to_string();
    assert!(err.starts_with("Unknown memory namespace: m-1."), "{err}");
}

#[test]
fn memory_id_reads_published_row() {
    let _guard = lock_globals();
    let (dir, _artifacts, _ctx) = fixture();
    let root = dir.path().join("memories/--proj--");
    std::fs::create_dir_all(&root).unwrap();
    omp_urls::schemes::memory::set_memory_records(vec![omp_urls::schemes::memory::MemoryRecord {
        id: "m-1".into(),
        bank: "proj".into(),
        store: "working".into(),
        content: "Body.".into(),
        importance: Some(0.5),
        ..omp_urls::schemes::memory::MemoryRecord::default()
    }]);
    let ctx = UrlCtx {
        memory_root: Some(root),
        ..UrlCtx::default()
    };
    let provider = omp_urls::schemes::memory::MemoryProvider;

    let url = parse_internal_url("memory://m-1").unwrap();
    let resource = provider.read(&url, &ctx).unwrap();
    assert!(resource
        .content
        .starts_with("---\nid: m-1\nbank: proj\nstore: working\n"));
    assert!(resource.content.ends_with("\n---\n\nBody."));

    let labels: Vec<String> = provider
        .complete("", &ctx)
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert_eq!(labels, vec!["root".to_string(), "<memory-id>".to_string()]);

    omp_urls::schemes::memory::set_memory_records(Vec::new());
}

#[test]
fn fuzzy_suggest_ranks_subsequence() {
    let candidates = vec!["Reviewer".to_string(), "Cleanup4".to_string()];
    assert_eq!(
        core::fuzzy_suggest(&candidates, "Reviewr", 5),
        vec!["Reviewer"]
    );
    assert!(core::fuzzy_suggest(&candidates, "Zzz", 5).is_empty());
}

#[test]
fn format_byte_size_matches_ts() {
    assert_eq!(core::format_byte_size(512), "512 B");
    assert_eq!(core::format_byte_size(1536), "1.5 KiB");
    assert_eq!(core::format_byte_size(2 * 1024 * 1024), "2.0 MiB");
}

fn _paths(_: &PathBuf) {}
