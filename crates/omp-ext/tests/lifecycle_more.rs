//! Payload shapes for the retry/compaction/goal/approval/user-shell events and the
//! `assistant_message` / `user_bash` / `user_python` result aggregation (same fixture as `lifecycle.rs`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use omp_ext::lifecycle::{ApprovalMode, CompactingOutcome, CompactionAction, CompactionReason, DiscoveredPath, GoalModeState, LifecycleEvent, ModeStatus, PlanModeState, ResourcesDiscoverReason, SessionModeState, SubagentInvocationKind};
use omp_ext::{ExtHost, HostConfig, HostMessage};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

mod common;

async fn loaded() -> Arc<ExtHost> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cfg = HostConfig::new(root.join("../../ext-host/host.ts"));
    cfg.handler_timeout_ms = Some(2000);
    if std::env::var_os("OMP_EXT_TEST_SHARED").is_some_and(|v| v != "0") {
        let dir = std::env::temp_dir().join(format!("omp-ext-shared-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        cfg.shared_socket = Some(dir.join("ext.sock"));
    }
    let host = ExtHost::spawn(&cfg).await.unwrap();
    let r = host
        .load(
            Some(&[root.join("tests/fixtures/lifecycle.ts")]),
            None,
            "/tmp",
            Some(json!({"sessionId": "s1"})),
        )
        .await
        .unwrap();
    assert!(r.errors.is_empty(), "{r:?}");
    assert_eq!(r.loaded.len(), 1);
    host
}

async fn entry(rx: &mut UnboundedReceiver<HostMessage>, custom_type: &str) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no {custom_type} entry"))
            .expect("host closed");
        if let HostMessage::Notification { method, params } = msg {
            if method == "append_entry" && params["customType"] == custom_type {
                return params["data"].clone();
            }
        }
    }
}

/// The fixture re-serializes the event it received; it must equal our serialization byte for byte.
async fn assert_wire(
    host: &ExtHost,
    rx: &mut UnboundedReceiver<HostMessage>,
    ev: LifecycleEvent,
    expected: &str,
) {
    assert_eq!(serde_json::to_string(&ev).unwrap(), expected);
    let ty = ev.to_value()["type"].as_str().unwrap().to_owned();
    let r = host.emit_event(&ev, None).await.unwrap();
    assert!(r.errors.is_empty(), "{ty}: {:?}", r.errors);
    assert_eq!(
        entry(rx, &format!("seen:{ty}")).await,
        json!(expected),
        "{ty}"
    );
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn retry_compaction_goal_approval_and_user_shell_payloads() {
    if common::bun_missing("retry_compaction_goal_approval_and_user_shell_payloads") {
        return;
    }
    let host = loaded().await;
    let mut rx = host.take_incoming().unwrap();
    let s = String::from;
    let cases: Vec<(LifecycleEvent, &str)> = vec![
        (
            LifecycleEvent::AutoCompactionStart {
                reason: CompactionReason::Threshold,
                action: CompactionAction::ContextFull,
            },
            r#"{"type":"auto_compaction_start","reason":"threshold","action":"context-full"}"#,
        ),
        (
            LifecycleEvent::AutoCompactionEnd {
                action: CompactionAction::Remote,
                result: Some(json!({"summary":"s"})),
                aborted: false,
                will_retry: true,
                error_message: None,
                skipped: Some(false),
            },
            r#"{"type":"auto_compaction_end","action":"remote","result":{"summary":"s"},"aborted":false,"willRetry":true,"skipped":false}"#,
        ),
        (
            LifecycleEvent::AutoCompactionEnd {
                action: CompactionAction::Shake,
                result: None,
                aborted: true,
                will_retry: false,
                error_message: Some(s("boom")),
                skipped: None,
            },
            r#"{"type":"auto_compaction_end","action":"shake","aborted":true,"willRetry":false,"errorMessage":"boom"}"#,
        ),
        (
            LifecycleEvent::AutoRetryStart {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 2000,
                error_message: s("429"),
                error_id: Some(7),
            },
            r#"{"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,"errorMessage":"429","errorId":7}"#,
        ),
        (
            LifecycleEvent::AutoRetryEnd {
                success: false,
                attempt: 3,
                final_error: Some(s("x")),
                retry_errors: Some(vec![json!({"entryId":"e","note":"n"})]),
            },
            r#"{"type":"auto_retry_end","success":false,"attempt":3,"finalError":"x","retryErrors":[{"entryId":"e","note":"n"}]}"#,
        ),
        (
            LifecycleEvent::AutoRetryEnd {
                success: true,
                attempt: 2,
                final_error: None,
                retry_errors: None,
            },
            r#"{"type":"auto_retry_end","success":true,"attempt":2}"#,
        ),
        (
            LifecycleEvent::RetryFallbackApplied {
                from: s("a/b"),
                to: s("c/d"),
                role: s("default"),
                reason: Some(s("quota")),
            },
            r#"{"type":"retry_fallback_applied","from":"a/b","to":"c/d","role":"default","reason":"quota"}"#,
        ),
        (
            LifecycleEvent::RetryFallbackSucceeded {
                model: s("c/d"),
                role: s("default"),
            },
            r#"{"type":"retry_fallback_succeeded","model":"c/d","role":"default"}"#,
        ),
        (
            LifecycleEvent::TtsrTriggered {
                rules: vec![json!({"name":"r"})],
            },
            r#"{"type":"ttsr_triggered","rules":[{"name":"r"}]}"#,
        ),
        (
            LifecycleEvent::TodoReminder {
                todos: vec![json!({"content":"t","status":"pending"})],
                attempt: 1,
                max_attempts: 3,
            },
            r#"{"type":"todo_reminder","todos":[{"content":"t","status":"pending"}],"attempt":1,"maxAttempts":3}"#,
        ),
        (
            LifecycleEvent::GoalUpdated {
                goal: None,
                state: None,
            },
            r#"{"type":"goal_updated","goal":null}"#,
        ),
        (
            LifecycleEvent::GoalUpdated {
                goal: Some(json!({"id":"g"})),
                state: Some(json!({"mode":"on"})),
            },
            r#"{"type":"goal_updated","goal":{"id":"g"},"state":{"mode":"on"}}"#,
        ),
        (
            LifecycleEvent::ToolApprovalRequested {
                session_id: s("s1"),
                tool_name: s("bash"),
                tool_call_id: s("c1"),
                reason: Some(s("exec")),
                approval_mode: ApprovalMode::AlwaysAsk,
            },
            r#"{"type":"tool_approval_requested","sessionId":"s1","toolName":"bash","toolCallId":"c1","reason":"exec","approvalMode":"always-ask"}"#,
        ),
        (
            LifecycleEvent::ToolApprovalRequested {
                session_id: s("s1"),
                tool_name: s("bash"),
                tool_call_id: s("c1"),
                reason: None,
                approval_mode: ApprovalMode::Yolo,
            },
            r#"{"type":"tool_approval_requested","sessionId":"s1","toolName":"bash","toolCallId":"c1","approvalMode":"yolo"}"#,
        ),
        (
            LifecycleEvent::ToolApprovalResolved {
                session_id: s("s1"),
                tool_name: s("bash"),
                tool_call_id: s("c1"),
                approved: false,
                reason: Some(s("no UI")),
            },
            r#"{"type":"tool_approval_resolved","sessionId":"s1","toolName":"bash","toolCallId":"c1","approved":false,"reason":"no UI"}"#,
        ),
        (
            LifecycleEvent::UserBash {
                command: s("ls"),
                exclude_from_context: true,
                cwd: s("/tmp"),
            },
            r#"{"type":"user_bash","command":"ls","excludeFromContext":true,"cwd":"/tmp"}"#,
        ),
        (
            LifecycleEvent::UserPython {
                code: s("1+1"),
                exclude_from_context: false,
                cwd: s("/tmp"),
            },
            r#"{"type":"user_python","code":"1+1","excludeFromContext":false,"cwd":"/tmp"}"#,
        ),
        (
            LifecycleEvent::ResourcesDiscover {
                cwd: s("/work"),
                reason: ResourcesDiscoverReason::Startup,
            },
            r#"{"type":"resources_discover","cwd":"/work","reason":"startup"}"#,
        ),
        (
            LifecycleEvent::AssistantMessage {
                message: json!({"role":"assistant","content":[{"type":"text","text":"a"}]}),
            },
            r#"{"type":"assistant_message","message":{"role":"assistant","content":[{"type":"text","text":"a"}]}}"#,
        ),
        (
            LifecycleEvent::ModeChanged {
                previous: SessionModeState::default(),
                state: SessionModeState::default(),
            },
            r#"{"type":"mode_changed","previous":{"plan":{"status":"inactive"},"vibe":{"status":"inactive"},"goal":{"status":"inactive"},"loop":{"status":"inactive"}},"state":{"plan":{"status":"inactive"},"vibe":{"status":"inactive"},"goal":{"status":"inactive"},"loop":{"status":"inactive"}}}"#,
        ),
        (
            LifecycleEvent::ModeChanged {
                previous: SessionModeState::default(),
                state: SessionModeState {
                    plan: PlanModeState {
                        status: ModeStatus::Active,
                        plan_file_path: Some(s("/tmp/plan.md")),
                    },
                    goal: GoalModeState {
                        status: ModeStatus::Paused,
                        objective: Some(s("ship it")),
                        token_budget: Some(50_000),
                    },
                    ..SessionModeState::default()
                },
            },
            r#"{"type":"mode_changed","previous":{"plan":{"status":"inactive"},"vibe":{"status":"inactive"},"goal":{"status":"inactive"},"loop":{"status":"inactive"}},"state":{"plan":{"status":"active","planFilePath":"/tmp/plan.md"},"vibe":{"status":"inactive"},"goal":{"status":"paused","objective":"ship it","tokenBudget":50000},"loop":{"status":"inactive"}}}"#,
        ),
    ];
    for (ev, expected) in cases {
        assert_wire(&host, &mut rx, ev, expected).await;
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn before_subagent_spawn_payload_and_result() {
    if common::bun_missing("before_subagent_spawn_payload_and_result") {
        return;
    }
    let host = loaded().await;
    let mut rx = host.take_incoming().unwrap();
    let s = String::from;
    let cases: Vec<(LifecycleEvent, &str)> = vec![
        (
            LifecycleEvent::BeforeSubagentSpawn {
                agent: s("task"),
                invocation_kind: SubagentInvocationKind::Task,
                model_role: Some(s("smol")),
                patterns: vec![s("a/b:high"), s("c/d")],
                spawn_key: Some(s("agent-1")),
            },
            r#"{"type":"before_subagent_spawn","agent":"task","invocationKind":"task","modelRole":"smol","patterns":["a/b:high","c/d"],"spawnKey":"agent-1"}"#,
        ),
        (
            // An explicit selector has no role alias, and a spawn without a key omits it.
            LifecycleEvent::BeforeSubagentSpawn {
                agent: s("reviewer"),
                invocation_kind: SubagentInvocationKind::Eval,
                model_role: None,
                patterns: Vec::new(),
                spawn_key: None,
            },
            r#"{"type":"before_subagent_spawn","agent":"reviewer","invocationKind":"eval","patterns":[]}"#,
        ),
    ];
    for (ev, expected) in cases {
        assert_wire(&host, &mut rx, ev, expected).await;
    }

    // The host aggregates the handlers: the last `model` choice wins with its `note`.
    let out = host
        .emit_before_subagent_spawn(
            s("task"),
            SubagentInvocationKind::Task,
            None,
            vec![s("a/b")],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(!out.block);
    assert_eq!(
        out.model,
        Some(vec![s("fx/one:high"), s("fx/two")]),
        "the handler's model list replaces the spawn's patterns"
    );
    assert_eq!(out.note.as_deref(), Some("fixture route"));
    host.shutdown().await.unwrap();
}

#[tokio::test]

async fn session_compacting_payload_and_result() {
    if common::bun_missing("session_compacting_payload_and_result") {
        return;
    }
    let host = loaded().await;
    let mut rx = host.take_incoming().unwrap();
    let messages = vec![json!({"role":"user","content":[{"type":"text","text":"hi"}]})];
    // Upstream shape: `{type, sessionId, messages}` with the summarized window verbatim.
    assert_wire(
        &host,
        &mut rx,
        LifecycleEvent::SessionCompacting {
            session_id: "s1".into(),
            messages: messages.clone(),
        },
        r#"{"type":"session.compacting","sessionId":"s1","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
    )
    .await;

    // The fixture answers `{context, prompt, preserveData}`; the host keeps the last result and
    // the core parses it into the outcome it applies to the summary and the entry.
    let out = host
        .emit_session_compacting("s1".into(), messages, None)
        .await
        .unwrap();
    assert_eq!(
        out,
        CompactingOutcome {
            context: vec!["ctx line".into()],
            prompt: Some("CUSTOM PROMPT".into()),
            preserve_data: Some(json!({"keep": 1})),
        }
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn assistant_message_rewrite_and_user_shell_results_aggregate() {
    if common::bun_missing("assistant_message_rewrite_and_user_shell_results_aggregate") {
        return;
    }
    let host = loaded().await;
    let message = json!({"role":"assistant","content":[
        {"type":"thinking","thinking":"hm"},
        {"type":"text","text":"hello","textSignature":"sig"},
    ]});
    let out = host.emit_assistant_message(message, None).await.unwrap();
    assert_eq!(
        out.content,
        Some(vec![
            json!({"type":"thinking","thinking":"hm"}),
            json!({"type":"text","text":"HELLO"})
        ])
    );
    let none = host
        .emit_assistant_message(
            json!({"role":"assistant","content":[{"type":"text","text":"ABC"}]}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        none.content, None,
        "uppercase text is unchanged, so nothing is rewritten"
    );

    let bash = host
        .emit_user_bash("ls".into(), false, "/tmp".into(), None)
        .await
        .unwrap();
    assert_eq!(bash.result, Some(json!({"output":"handled","exitCode":0})));
    let py = host
        .emit_user_python("1".into(), false, "/tmp".into(), None)
        .await
        .unwrap();
    assert_eq!(py.result, Some(json!({"output":"py","exitCode":0})));
    host.shutdown().await.unwrap();
}

/// `resources_discover`: the handler's three path lists come back tagged with the extension that
/// returned them (`ExtensionRunner.emitResourcesDiscover`), for both reasons.
#[tokio::test]
async fn resources_discover_collects_tagged_paths() {
    if common::bun_missing("resources_discover_collects_tagged_paths") {
        return;
    }
    // Both reasons serialize to the host's `{type, cwd, reason}` event.
    for (reason, wire) in [
        (
            ResourcesDiscoverReason::Startup,
            r#"{"type":"resources_discover","cwd":"/work","reason":"startup"}"#,
        ),
        (
            ResourcesDiscoverReason::Reload,
            r#"{"type":"resources_discover","cwd":"/work","reason":"reload"}"#,
        ),
    ] {
        let ev = LifecycleEvent::ResourcesDiscover {
            cwd: "/work".into(),
            reason,
        };
        assert_eq!(serde_json::to_string(&ev).unwrap(), wire);
    }

    let host = loaded().await;
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lifecycle.ts");
    let ext = fixture.to_string_lossy().into_owned();
    for reason in [
        ResourcesDiscoverReason::Startup,
        ResourcesDiscoverReason::Reload,
    ] {
        let out = host
            .emit_resources_discover("/work".into(), reason, None)
            .await
            .unwrap();
        // A shared test host may hold the fixture twice; compare the distinct tagged paths.
        let distinct = |paths: &[DiscoveredPath]| {
            let mut v: Vec<(String, String)> = paths
                .iter()
                .map(|p| (p.path.clone(), p.extension_path.clone()))
                .collect();
            v.sort();
            v.dedup();
            v
        };
        assert_eq!(
            distinct(&out.skill_paths),
            vec![("skills/ext-a".to_string(), ext.clone())],
            "{reason:?}"
        );
        assert_eq!(
            distinct(&out.prompt_paths),
            vec![("prompts/ext-b.md".to_string(), ext.clone())]
        );
        assert_eq!(
            distinct(&out.theme_paths),
            vec![("themes/ext-c.json".to_string(), ext.clone())]
        );
    }
    host.shutdown().await.unwrap();
}
