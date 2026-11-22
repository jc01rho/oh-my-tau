# ext-host event ledger

Audit of the pi `ExtensionAPI.on()` event surface against `ext-host/host.ts` (handler aggregation) and the Rust core (`crates/omp-ext/src/lifecycle.rs`, `crates/omp-ext/src/hooks.rs`, `crates/omp-cli/src/ext.rs`).

The migration plan counts 45 events (an older upstream snapshot). Upstream `packages/coding-agent/src/extensibility/extensions/types.ts` on oh-my-pi `main` (fetched 2026-11-22) declares 54 `on()` overloads; all of them are listed here.

- **Core**: the Rust emit site, or `-` when the core never emits the event.
- **Host aggregation**: how `emit` in `host.ts` folds handler results; `observe` means results are ignored. `(new)` marks a rule added in Phase 6 (covered by `ext-host/host.test.ts`).

| # | Event | Core | Host aggregation | Status |
|---|---|---|---|---|
| 1 | `session_start` | `LifecycleEvent::SessionStart` | observe | emitted |
| 2 | `session_before_switch` | `emit_session_before_switch` | session_before_*: last result, cancel short-circuits | emitted |
| 3 | `session_switch` | `LifecycleEvent::SessionSwitch` | observe | emitted |
| 4 | `session_clear` | `/clear` -> `Runtime::reset_context` (`runtime.rs`, `emit_session_clear`) | observe (generic) | emitted (6b) |
| 5 | `session_before_branch` | `emit_session_before_branch` | session_before_* | emitted |
| 6 | `session_branch` | `LifecycleEvent::SessionBranch` | observe | emitted |
| 7 | `session_before_compact` | `emit_session_before_compact` | session_before_* | emitted |
| 8 | `session.compacting` | `CompactBridge` -> `emit_session_compacting` (`ext.rs`) | last result wins; `context`/`prompt` feed the summary, `preserveData` lands on the entry | emitted, result applied (6b) |
| 9 | `cache_warming_decision` | `cache_warmer.rs` `step_at` -> `Extensions::cache_warming_decision` | last `action` wins over core proposal | emitted, override applied (6b); `compact` override ignored (no compaction bridge), replay method only, anthropic-messages only |
| 10 | `session_compact` | `LifecycleEvent::SessionCompact` | observe | emitted |
| 11 | `session_before_shutdown` | TUI close paths (`/exit`, Ctrl+D, Ctrl+C twice) -> `Extensions::confirm_close` -> `emit_session_before_shutdown` | session_before_*: `cancel` keeps the session | emitted, result applied (6b) |
| 12 | `session_shutdown` | `LifecycleEvent::SessionShutdown` | parallel, 2s budget, clears timers | emitted |
| 13 | `session_before_tree` | `emit_session_before_tree` | session_before_* | emitted |
| 14 | `session_tree` | `LifecycleEvent::SessionTree` | observe | emitted |
| 15 | `context` | `hooks::context` | chained `messages` replacement | emitted |
| 16 | `before_provider_request` | `on_payload` hook (`ext.rs` `provider_payload`; loop, compaction, side turns) | chained payload replacement | emitted with the real payload, replacement applied (6b) |
| 17 | `after_provider_response` | `on_response` hook (`ext.rs` `after_provider_response_hook`, wired in `agent.rs`) | observe | emitted (6b) |
| 18 | `before_agent_start` | `emit_before_agent_start` | messages collected, systemPrompt chained | emitted |
| 19 | `before_subagent_spawn` | task tool / eval `agent()` (`tools/task.rs`) | block short-circuits, last model wins | emitted, block/model/note applied (6b) |
| 20 | `agent_start` | `LifecycleEvent::AgentStart` | observe | emitted |
| 21 | `agent_end` | `LifecycleEvent::AgentEnd` | observe | emitted |
| 22 | `session_stop` | `emit_session_stop` | block short-circuits; first continue with context | emitted |
| 23 | `turn_start` | `LifecycleEvent::TurnStart` | observe | emitted |
| 24 | `turn_end` | `LifecycleEvent::TurnEnd` | observe | emitted |
| 25 | `message_start` | `LifecycleEvent::MessageStart` | observe | emitted |
| 26 | `message_update` | `LifecycleEvent::MessageUpdate` | observe (delta codec, resync) | emitted |
| 27 | `message_end` | `LifecycleEvent::MessageEnd` | observe | emitted |
| 28 | `assistant_message` | `emit_assistant_message` | chained text-only rewrite | emitted |
| 29 | `tool_execution_start` | `LifecycleEvent::ToolExecutionStart` | observe | emitted |
| 30 | `tool_execution_update` | `LifecycleEvent::ToolExecutionUpdate` | observe | emitted |
| 31 | `tool_execution_end` | `LifecycleEvent::ToolExecutionEnd` | observe | emitted |
| 32 | `auto_compaction_start` | `LifecycleEvent::AutoCompactionStart` | observe | emitted |
| 33 | `auto_compaction_end` | `LifecycleEvent::AutoCompactionEnd` | observe | emitted |
| 34 | `auto_retry_start` | `LifecycleEvent::AutoRetryStart` | observe | emitted |
| 35 | `auto_retry_end` | `LifecycleEvent::AutoRetryEnd` | observe | emitted |
| 36 | `retry_fallback_applied` | `LifecycleEvent::RetryFallbackApplied` | observe | emitted |
| 37 | `retry_fallback_succeeded` | `LifecycleEvent::RetryFallbackSucceeded` | observe | emitted |
| 38 | `ttsr_triggered` | `LifecycleEvent::TtsrTriggered` | observe | emitted |
| 39 | `todo_reminder` | `LifecycleEvent::TodoReminder` | observe | emitted |
| 40 | `goal_updated` | `LifecycleEvent::GoalUpdated` | observe | emitted |
| 41 | `credential_disabled` | `omp-providers` auto soft-disable -> `CredentialDisabledFeed` (`ext.rs`), buffered until attach | observe | emitted (6b) |
| 42 | `input` | `emit_input` | chained text/images; handled short-circuits | emitted |
| 43 | `tool_approval_requested` | `LifecycleEvent::ToolApprovalRequested` | observe | emitted |
| 44 | `tool_approval_resolved` | `LifecycleEvent::ToolApprovalResolved` | observe | emitted |
| 45 | `plan_review_requested` | `features/plan.rs` review opened | observe; `ctx.resolvePlanReview` / `getPendingPlanReview` answered by the core | emitted (6b) |
| 46 | `plan_review_resolved` | `features/plan.rs` approve / off / replaced / dismissed | observe | emitted (6b) |
| 47 | `mode_changed` | `features/modes.rs` (plan, loop, goal, restore) | observe | emitted (6b); vibe and live goal report inactive (tau has neither) |
| 48 | `tool_call` | `hooks::tool_call` | block short-circuits; input/context merged; failure blocks | emitted |
| 49 | `tool_result` | `hooks::after_tool_call` | chained content/details/isError | emitted |
| 50 | `user_bash` | `emit_user_bash` | first result wins | emitted |
| 51 | `user_python` | `emit_user_python` | first result wins | emitted |
| 52 | `mcp_notification` | `McpNotificationSink` (`ext.rs`), after the runtime's own handling, buffered until attach | observe | emitted (6b) |
| 53 | `link_activate` | TUI link hints -> `Extensions::activate_link` -> host `link_activate` request | scheme owner only (`registerLinkScheme`, first claimer), returns handled | routed (6b) |
| 54 | `resources_discover` | after `session_start` and on reload (`ext.rs` `discover_resources`) | paths collected, tagged with extensionPath | emitted, paths fed to skill/prompt/theme loaders (6b) |

Totals: all 54 emitted by the core; Phase 6b closed the 14 host-only gaps (limits noted per row).

## runEphemeralTurn

`ctx.runEphemeralTurn(options)` sends a `run_ephemeral_turn` request to the core, which runs the `/btw` side turn (`features::btw::complete_side_turn`): the live conversation, then `history`, then the prompt (`promptText` + `promptImages`), on the current model with no tools. Nothing is appended to the session.

- Validated before inference: `promptText` non-empty, `maxTokens` positive safe integer (default 4096), `tools` only `false`, `history` must decode as provider messages.
- Result: `{replyText, assistantMessage}`; an aborted or failed provider turn rejects with its error.
- Host side: `signal` (pre-aborted or mid-flight) rejects with `signal.reason`; `onUsage` gets the settled message (throws are logged); `onTextDelta` receives text deltas as the provider streams them (a throw rejects); a call from inside a side turn's callback rejects (bounded recursion).
- A core without the responder (`HeadlessResponder`) rejects with `runEphemeralTurn is not supported by the core`.
- Options (Phase 6b): `conversationKey` keys the side session id, `minimizeEffort` lowers the requested reasoning to the model's lowest level (tau has no per-request effort field), `maxContextBytes` rejects an oversized context before inference, `purpose` journals the settled attempt, `dedupeReply` collapses repeated output, `replyMaxBytes` bounds the reply.
