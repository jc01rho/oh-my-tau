#!/usr/bin/env bun
// Runs a real `omt --mode rpc` against a local mock provider and parses what it answers with the
// zod schemas the OmOMeow gateway (omo-nyang, commit b66925a) parses worker answers with. A
// failure here is a canary that would not start (or would misjudge a lane) on the gateway side.
//
//   OMT_BIN=target/debug/omt bun tests-e2e/gateway-schema.ts
//
// `zod` is resolved by bun's auto-install; there is no package.json on purpose.
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { z } from "zod";

// src/relay/agentd-sessions.ts:11-12 (session move: quiesce) and :203 (abort awaitIdle).
const stateSchema = z.object({ isStreaming: z.boolean(), isCompacting: z.boolean(), isBashRunning: z.boolean().default(false),
  pendingMessageCount: z.number(), sessionFile: z.string() });
const abortSchema = z.object({ idle: z.boolean() });
// src/relay/agentd-sessions.ts: identity probe in verify().
const identitySchema = z.object({ sessionFile: z.string().nullish(), sessionId: z.string().nullish() }).passthrough();
// src/relay/agentd-sessions.ts: request() envelope.
const envelope = z.object({ type: z.literal("response"), id: z.string(), success: z.boolean(), data: z.unknown().optional(), error: z.string().optional() });
// src/lane-idle.ts:17.
const sessionStateSchema = z.object({ isStreaming: z.boolean(), isCompacting: z.boolean(), isBashRunning: z.boolean().default(false), pendingMessageCount: z.number().default(0) }).passthrough();
// src/brain.ts:18-26.
const usageTotalsSchema = z.object({ input: z.number(), output: z.number(), cacheRead: z.number(), cacheWrite: z.number(), cost: z.number() });
const runtimeStateSchema = z.object({
  isStreaming: z.boolean(), isCompacting: z.boolean().optional(), isBashRunning: z.boolean().optional(),
  pendingMessageCount: z.number().optional(), pendingQuestions: z.array(z.unknown()).optional(),
  usageTotals: usageTotalsSchema.optional(),
}).passthrough();
// src/worker-rpc.ts:25 and :216 (usage, session identity, cwd).
const workerStateSchema = z.object({ usageTotals: usageTotalsSchema.optional(), sessionId: z.string().nullish(), cwd: z.string().nullish() }).passthrough();
// src/relay/agentd-sessions.ts:17-27 observeActivity.
const eventShape = z.object({ type: z.string() }).passthrough();
type Observed = { turn: "running" | "idle"; paused: boolean };
function observeActivity(state: Observed, event: unknown): void {
  const parsed = eventShape.safeParse(event);
  if (!parsed.success) return;
  const { type } = parsed.data;
  if (type === "agent_start") state.turn = "running";
  else if (type === "agent_idle" || type === "session_exit") state.turn = "idle";
  else if (type === "session_paused") state.paused = true;
  else if (type === "session_resumed") state.paused = false;
}
// Answers the gateway has no schema for (the `interrupt` and `resume_session` replies mhc gives).
const interruptSchema = z.object({ interrupted: z.boolean(), turnId: z.string().optional() });
const resumeSchema = z.object({ resumed: z.boolean(), pauseSeq: z.number(), turnStarted: z.boolean(), deliveredNotices: z.number(), deliveredHeldInputs: z.number() });

const chunk = (model: string, delta: object, finish: string | null): string =>
  `data: ${JSON.stringify({ id: "chatcmpl-mock", object: "chat.completion.chunk", created: 0, model, choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
let release: () => void = () => {};
const held = new Promise<void>(done => { release = done; });
const encoder = new TextEncoder();
const provider = Bun.serve({
  port: 0,
  async fetch(request) {
    const { pathname } = new URL(request.url);
    if (request.method === "GET" && pathname.endsWith("/models")) return Response.json({ object: "list", data: [] });
    const body = (await request.json()) as { model?: string };
    const model = body.model ?? "text";
    const hold = model.endsWith("hold");
    const stream = new ReadableStream<Uint8Array>({
      async start(controller) {
        controller.enqueue(encoder.encode(chunk(model, { role: "assistant", content: hold ? "HOLD-START " : "MOCK-" }, null)));
        if (hold) await held;
        controller.enqueue(encoder.encode(chunk(model, { content: hold ? "HOLD-END" : "OK" }, null)));
        controller.enqueue(encoder.encode(chunk(model, {}, "stop")));
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
        controller.close();
      },
    });
    return new Response(stream, { headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" } });
  },
});

type Frame = Record<string, unknown> & { type?: string };
class Lane {
  readonly frames: Frame[] = [];
  private readonly waiters: Array<() => void> = [];
  private readonly proc: Bun.Subprocess<"pipe", "pipe", "pipe">;
  private seq = 0;
  readonly observed: Observed = { turn: "idle", paused: false };
  constructor(bin: string, home: string, work: string, args: string[]) {
    this.proc = Bun.spawn([bin, "--mode", "rpc", "--model", "mock/text", "--no-extensions", ...args], {
      cwd: work, stdin: "pipe", stdout: "pipe", stderr: "pipe",
      env: { ...process.env, HOME: home, OMP_RS_EXT_SHARED: "0", OMP_RS_NO_TITLE: "1", NO_COLOR: "1" },
    });
    void this.read();
  }
  private async read(): Promise<void> {
    const decoder = new TextDecoder();
    let pending = "";
    for await (const part of this.proc.stdout) {
      pending += decoder.decode(part, { stream: true });
      for (let at = pending.indexOf("\n"); at >= 0; at = pending.indexOf("\n")) {
        const line = pending.slice(0, at).trim();
        pending = pending.slice(at + 1);
        if (!line) continue;
        const frame = JSON.parse(line) as Frame;
        observeActivity(this.observed, frame);
        this.frames.push(frame);
        for (const wake of this.waiters.splice(0)) wake();
      }
    }
  }
  // Resolves with the first frame at or after `from` that `match` accepts; rejects at `timeoutMs`.
  async expect(match: (frame: Frame) => boolean, from = 0, timeoutMs = 30_000): Promise<Frame> {
    const deadline = Date.now() + timeoutMs;
    let cursor = from;
    for (;;) {
      for (; cursor < this.frames.length; cursor += 1) if (match(this.frames[cursor]!)) return this.frames[cursor]!;
      const left = deadline - Date.now();
      if (left <= 0) throw new Error(`timed out waiting for a frame (from ${from}); saw ${JSON.stringify(this.frames.slice(from).map(f => f.type))}`);
      await new Promise<void>((wake, fail) => {
        const timer = setTimeout(() => fail(new Error("frame wait timed out")), left);
        this.waiters.push(() => { clearTimeout(timer); wake(); });
      });
    }
  }
  async request(command: Record<string, unknown>): Promise<unknown> {
    const id = `gw-${(this.seq += 1)}`;
    const from = this.frames.length;
    this.proc.stdin.write(`${JSON.stringify({ ...command, id })}\n`);
    this.proc.stdin.flush();
    const reply = envelope.parse(await this.expect(f => f.type === "response" && f.id === id, from));
    if (!reply.success) throw new Error(`${String(command.type)} failed: ${reply.error ?? "unknown"}`);
    return reply.data;
  }
  async close(): Promise<void> {
    this.proc.stdin.end();
    await this.proc.exited;
  }
}

let failures = 0;
function check(name: string, run: () => void): void {
  try { run(); console.log(`ok   ${name}`); } catch (error) {
    failures += 1;
    console.log(`FAIL ${name}: ${error instanceof Error ? error.message : String(error)}`);
  }
}
const assert = (condition: boolean, message: string): void => { if (!condition) throw new Error(message); };

const bin = resolve(process.env.OMT_BIN ?? "target/debug/omt");
const root = mkdtempSync(join(tmpdir(), "omt-gateway-schema-"));
try {
  const home = join(root, "home");
  const work = join(root, "work");
  const agent = join(home, ".omp/agent");
  mkdirSync(agent, { recursive: true });
  mkdirSync(work, { recursive: true });
  writeFileSync(join(agent, "models.yml"), `providers:\n  mock:\n    baseUrl: http://127.0.0.1:${provider.port}/v1\n    apiKey: e2e-not-a-secret\n    api: openai-completions\n    compat:\n      supportsDeveloperRole: true\n    models:\n      - id: text\n        name: Mock Text\n        contextWindow: 128000\n        maxTokens: 4096\n      - id: hold\n        name: Mock Hold\n        contextWindow: 128000\n        maxTokens: 4096\n`);
  writeFileSync(join(agent, "config.yml"), "startup:\n  setupWizard: false\n  checkUpdate: false\n  changelogMode: hidden\n");

  // A persistent lane, the way the gateway launches one.
  const lane = new Lane(bin, home, work, ["--session", join(root, "lanes/lane.jsonl")]);
  await lane.expect(f => f.type === "ready");

  const idleState = await lane.request({ type: "get_state" });
  check("get_state parses as the move quiesce state", () => {
    const state = stateSchema.parse(idleState);
    assert(state.sessionFile !== "", "a persistent lane reports its session file");
    assert(state.pendingMessageCount === 0 && !state.isBashRunning && !state.isStreaming, "an untouched lane is idle");
  });
  check("get_state parses as the idle sweep and brain state", () => {
    assert(sessionStateSchema.parse(idleState).pendingMessageCount === 0, "pendingMessageCount");
    runtimeStateSchema.parse(idleState);
    identitySchema.parse(idleState);
  });
  check("get_state carries usageTotals (USD cost), cwd and persistent", () => {
    const worker = workerStateSchema.parse(idleState);
    assert(worker.usageTotals !== undefined, "usageTotals");
    assert(worker.cwd === work || (worker.cwd ?? "").endsWith("/work"), `cwd ${String(worker.cwd)}`);
    assert((idleState as { persistent?: unknown }).persistent === true, "persistent: true");
  });

  const settled = lane.frames.length;
  await lane.request({ type: "prompt", message: "say ok", streamingBehavior: "followUp" });
  await lane.expect(f => f.type === "agent_idle", settled);
  check("a turn opens with agent_start and closes with agent_idle", () => {
    const types = lane.frames.slice(settled).map(f => f.type);
    assert(types.includes("agent_start"), "agent_start");
    assert(types.indexOf("agent_settled") < types.indexOf("agent_idle"), `agent_settled before agent_idle: ${JSON.stringify(types)}`);
    assert(lane.observed.turn === "idle", "the gateway lists the lane idle");
  });
  const afterTurn = await lane.request({ type: "get_state" });
  check("usageTotals after a turn parses and the lane is idle again", () => {
    const state = runtimeStateSchema.parse(afterTurn);
    assert(state.usageTotals !== undefined && state.usageTotals.cost >= 0, "usageTotals");
    assert(sessionStateSchema.parse(afterTurn).pendingMessageCount === 0, "pendingMessageCount");
  });

  // Interrupt and move-abort a held turn.
  await lane.request({ type: "set_client_info", width: 80, capabilities: ["interactive_user"] });
  await lane.request({ type: "set_model", provider: "mock", modelId: "hold" });
  let mark = lane.frames.length;
  await lane.request({ type: "prompt", message: "hold one", streamingBehavior: "followUp" });
  await lane.expect(f => f.type === "agent_start", mark);
  check("the gateway lists the lane running after agent_start", () => assert(lane.observed.turn === "running", "running"));
  const interrupted = await lane.request({ type: "interrupt" });
  check("interrupt names the turn it stopped", () => {
    const answer = interruptSchema.parse(interrupted);
    assert(answer.interrupted && answer.turnId === "2", JSON.stringify(answer));
  });
  await lane.expect(f => f.type === "agent_idle", mark);

  mark = lane.frames.length;
  await lane.request({ type: "prompt", message: "hold two", streamingBehavior: "followUp" });
  await lane.expect(f => f.type === "agent_start", mark);
  const effectId = `move-abort-${crypto.randomUUID()}`;
  const aborted = await lane.request({ type: "abort", awaitIdle: true, effectId });
  check("abort awaitIdle answers {idle:true} after the turn ended", () => {
    assert(abortSchema.parse(aborted).idle, "idle");
    const types = lane.frames.slice(mark).map(f => (f.type === "response" ? `response:${String(f.command)}` : f.type));
    assert(types.indexOf("agent_end") >= 0 && types.indexOf("agent_end") < types.indexOf("response:abort"), JSON.stringify(types));
  });
  const retried = await lane.request({ type: "abort", awaitIdle: true, effectId });
  check("a retry with the same effectId gets the same answer", () => assert(JSON.stringify(abortSchema.parse(retried)) === JSON.stringify(abortSchema.parse(aborted)), "same answer"));
  await lane.expect(f => f.type === "agent_idle", mark);

  // Stop, then resume.
  mark = lane.frames.length;
  await lane.request({ type: "prompt", message: "hold three", streamingBehavior: "followUp" });
  await lane.expect(f => f.type === "agent_start", mark);
  const stopped = await lane.request({ type: "interrupt", intent: "stop" });
  check("interrupt stop names its turn and pauses the lane", () => {
    const answer = interruptSchema.parse(stopped);
    assert(answer.interrupted && answer.turnId === "4", JSON.stringify(answer));
  });
  await lane.expect(f => f.type === "agent_idle", mark);
  check("the gateway lists the lane paused", () => assert(lane.observed.paused, "paused"));
  const resumed = await lane.request({ type: "resume_session" });
  check("resume_session answers what it resumed", () => {
    const answer = resumeSchema.parse(resumed);
    assert(answer.resumed && answer.pauseSeq === 2, JSON.stringify(answer));
  });
  await lane.expect(f => f.type === "session_resumed", mark);
  check("the gateway lists the lane resumed", () => assert(!lane.observed.paused, "not paused"));
  release();
  await lane.close();

  // A `--no-session` lane has no file to move: it says so instead of omitting the key.
  const volatile = new Lane(bin, home, work, ["--no-session"]);
  await volatile.expect(f => f.type === "ready");
  const volatileState = await volatile.request({ type: "get_state" });
  check("a --no-session lane reports sessionFile \"\" and persistent false", () => {
    assert(stateSchema.parse(volatileState).sessionFile === "", "sessionFile is an empty string");
    assert((volatileState as { persistent?: unknown }).persistent === false, "persistent: false");
  });
  const idleAbort = await volatile.request({ type: "abort", awaitIdle: true, effectId: "e1" });
  check("abort awaitIdle on an idle lane answers {idle:true}", () => assert(abortSchema.parse(idleAbort).idle, "idle"));
  await volatile.close();
} finally {
  release();
  provider.stop(true);
  rmSync(root, { recursive: true, force: true });
}

if (failures > 0) {
  console.log(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("gateway schema: all checks passed");
