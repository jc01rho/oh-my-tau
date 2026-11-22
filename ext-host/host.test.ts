// Conformance tests for host.ts: spawns the real host over stdio and plays the core.
// Run: bun test ext-host/
import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

type Json = Record<string, any>;

class Core {
  proc = Bun.spawn(["bun", join(import.meta.dir, "host.ts")], { stdin: "pipe", stdout: "pipe", stderr: "ignore" });
  next = 1;
  waiters = new Map<number, (m: Json) => void>();
  /** Answers host -> core requests (method -> handler); unknown methods get an error. */
  answer: Record<string, (params: Json) => unknown> = {};
  requests: Json[] = [];
  constructor() {
    void this.read();
  }
  async read() {
    const dec = new TextDecoder();
    let buf = "";
    for await (const chunk of this.proc.stdout) {
      buf += dec.decode(chunk);
      let i: number;
      while ((i = buf.indexOf("\n")) >= 0) {
        const msg = JSON.parse(buf.slice(0, i)) as Json;
        buf = buf.slice(i + 1);
        if (msg.method !== undefined && msg.id !== undefined) void this.serve(msg);
        else if (msg.method === undefined) this.waiters.get(msg.id)?.(msg);
      }
    }
  }
  async serve(msg: Json) {
    this.requests.push(msg);
    const h = this.answer[msg.method];
    try {
      if (!h) throw new Error(`${msg.method} is not supported by the core`);
      this.send({ id: msg.id, result: await h(msg.params) });
    } catch (e) {
      this.send({ id: msg.id, error: { code: -32000, message: (e as Error).message } });
    }
  }
  send(m: Json) {
    this.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...m })}\n`);
    this.proc.stdin.flush();
  }
  call(method: string, params: Json): Promise<Json> {
    const id = this.next++;
    const { promise, resolve } = Promise.withResolvers<Json>();
    const timer = setTimeout(() => resolve({ error: { message: `timeout waiting for ${method}` } }), 10_000);
    this.waiters.set(id, (m) => (clearTimeout(timer), resolve(m)));
    this.send({ id, method, params });
    return promise;
  }
  close() {
    this.proc.kill();
  }
}

const cores: Core[] = [];
afterEach(() => {
  for (const c of cores.splice(0)) c.close();
});

async function hostWith(source: string): Promise<Core> {
  const dir = mkdtempSync(join(tmpdir(), "omt-ext-"));
  const path = join(dir, "probe.ts");
  writeFileSync(path, source);
  const core = new Core();
  cores.push(core);
  const r = await core.call("load", { paths: [path], cwd: dir });
  expect(r.result.errors).toEqual([]);
  return core;
}

const REPLY = { replyText: "side answer", assistantMessage: { role: "assistant", content: [{ type: "text", text: "side answer" }], stopReason: "stop" } };

test("runEphemeralTurn forwards options to the core and returns EphemeralTurnResult", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) => {
    const deltas = []; const usage = [];
    const r = await ctx.runEphemeralTurn({ promptText: "why?", maxTokens: 32, tools: false, history: [],
      onTextDelta: (d) => void deltas.push(d), onUsage: (m) => void usage.push(m.stopReason) });
    return { r, deltas, usage };
  } });`);
  core.answer.run_ephemeral_turn = () => REPLY;
  const r = await core.call("call_command", { name: "side", args: "" });
  expect(r.result).toEqual({ r: REPLY, deltas: ["side answer"], usage: ["stop"] });
  const req = core.requests.find((m) => m.method === "run_ephemeral_turn")!;
  // Callbacks and the signal never cross the wire.
  expect(req.params).toEqual({ promptText: "why?", maxTokens: 32, tools: false, history: [] });
});

test("runEphemeralTurn rejects with the core's error instead of a silent reply", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) => {
    try { await ctx.runEphemeralTurn({ promptText: "q" }); return "resolved"; } catch (e) { return String(e.message); }
  } });`);
  const r = await core.call("call_command", { name: "side", args: "" });
  expect(r.result).toBe("run_ephemeral_turn is not supported by the core");
});

test("runEphemeralTurn honours an already-aborted and a mid-flight abort signal", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) => {
    const pre = new AbortController(); pre.abort(new Error("pre"));
    const a = await ctx.runEphemeralTurn({ promptText: "q", signal: pre.signal }).then(() => "resolved", (e) => e.message);
    const mid = new AbortController();
    const p = ctx.runEphemeralTurn({ promptText: "q", signal: mid.signal });
    pi.events.on("abort-now", () => mid.abort(new Error("mid")));
    const b = await p.then(() => "resolved", (e) => e.message);
    return [a, b];
  } });`);
  // The core never settles the side turn; it fires the abort over the event bus once the request arrives.
  core.answer.run_ephemeral_turn = () => {
    core.send({ id: 9_999, method: "bus_emit", params: { channel: "abort-now", data: null } });
    return new Promise(() => {});
  };
  const r = await core.call("call_command", { name: "side", args: "" });
  expect(r.result).toEqual(["pre", "mid"]);
  expect(core.requests.filter((m) => m.method === "run_ephemeral_turn")).toHaveLength(1);
});

test("runEphemeralTurn cannot be started from inside a side turn's callbacks", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) => {
    let nested = "not called";
    await ctx.runEphemeralTurn({ promptText: "q", onTextDelta: async () => {
      nested = await ctx.runEphemeralTurn({ promptText: "again" }).then(() => "resolved", (e) => e.message);
    } });
    return nested;
  } });`);
  core.answer.run_ephemeral_turn = () => REPLY;
  const r = await core.call("call_command", { name: "side", args: "" });
  expect(r.result).toBe("runEphemeralTurn cannot start inside a running side turn");
  expect(core.requests.filter((m) => m.method === "run_ephemeral_turn")).toHaveLength(1);
});

test("runEphemeralTurn rejects a malformed core result", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) =>
    ctx.runEphemeralTurn({ promptText: "q" }).then(() => "resolved", (e) => e.message) });`);
  core.answer.run_ephemeral_turn = () => ({ replyText: 3 });
  const r = await core.call("call_command", { name: "side", args: "" });
  expect(r.result).toBe("runEphemeralTurn: the core returned a malformed result");
});

const TWO = (body1: string, body2: string, ev: string) => `export default (pi) => {
  pi.on("${ev}", ${body1});
  pi.on("${ev}", ${body2});
};`;

test("resources_discover collects every handler's paths tagged with the extension", async () => {
  const core = await hostWith(TWO(`() => ({ skillPaths: ["a"] })`, `(e) => ({ skillPaths: ["b"], promptPaths: [e.reason] })`, "resources_discover"));
  const r = await core.call("emit", { event: { type: "resources_discover", cwd: "/w", reason: "startup" } });
  const ext = r.result.result.skillPaths[0].extensionPath;
  expect(ext).toEndWith("probe.ts");
  expect(r.result.result).toEqual({
    skillPaths: [{ path: "a", extensionPath: ext }, { path: "b", extensionPath: ext }],
    promptPaths: [{ path: "startup", extensionPath: ext }],
    themePaths: [],
  });
});

test("before_provider_request chains payload replacements", async () => {
  const core = await hostWith(TWO(`(e) => ({ ...e.payload, a: 1 })`, `(e) => ({ ...e.payload, b: e.payload.a + 1 })`, "before_provider_request"));
  const r = await core.call("emit", { event: { type: "before_provider_request", payload: { x: 0 } } });
  expect(r.result.result).toEqual({ payload: { x: 0, a: 1, b: 2 } });
  const none = await hostWith(`export default (pi) => pi.on("before_provider_request", () => undefined);`);
  expect((await none.call("emit", { event: { type: "before_provider_request", payload: {} } })).result.result).toBeNull();
});

test("cache_warming_decision: last action wins over the core's proposal", async () => {
  const core = await hostWith(TWO(`() => ({ action: "skip" })`, `() => undefined`, "cache_warming_decision"));
  const r = await core.call("emit", { event: { type: "cache_warming_decision", action: "warm" } });
  expect(r.result.result).toEqual({ action: "skip" });
});

test("before_subagent_spawn: block short-circuits, otherwise last model wins", async () => {
  const pick = await hostWith(TWO(`() => ({ model: "a", note: "n1" })`, `() => ({ model: ["b", "c"] })`, "before_subagent_spawn"));
  expect((await pick.call("emit", { event: { type: "before_subagent_spawn" } })).result.result).toEqual({ model: ["b", "c"] });
  const block = await hostWith(TWO(`() => ({ block: true, reason: "no" })`, `() => { throw new Error("must not run"); }`, "before_subagent_spawn"));
  const r = await block.call("emit", { event: { type: "before_subagent_spawn" } });
  expect(r.result.result).toEqual({ block: true, reason: "no" });
  expect(r.result.handlers).toHaveLength(1);
});

const REVIEW = {
  type: "plan_review_requested", reviewId: "r1", sessionId: "s1", title: "T", planFilePath: "local://T-plan.md", planContent: "# T",
  options: [
    { id: "execute", label: "Approve and execute", disabled: false },
    { id: "compact", label: "Approve and compact context", disabled: true },
    { id: "refine", label: "Refine plan", disabled: false },
  ],
};
const REVIEW_PROBE = `export default (pi) => pi.registerCommand("review", { handler: async (args, ctx) => {
  const pending = ctx.getPendingPlanReview();
  if (pending) pending.options[0].disabled = true; // a copy: cannot change the host's review
  const refused = [
    ctx.resolvePlanReview("other", "execute"),
    ctx.resolvePlanReview("r1", "save"),
    ctx.resolvePlanReview("r1", "compact"),
    ctx.resolvePlanReview("r1", "refine"),
    ctx.resolvePlanReview("r1", "refine", { feedback: "  " }),
    ctx.resolvePlanReview("r1", "execute", { role: "smol" }),
  ];
  const ok = args === "refine" ? ctx.resolvePlanReview("r1", "refine", { feedback: "add tests" }) : ctx.resolvePlanReview("r1", "execute");
  return { pending: pending ?? null, refused, ok, again: ctx.resolvePlanReview("r1", "execute"), after: ctx.getPendingPlanReview() ?? null };
} });`;

test("getPendingPlanReview mirrors the core's open review; resolvePlanReview forwards an accepted answer", async () => {
  const core = await hostWith(REVIEW_PROBE);
  core.answer.resolve_plan_review = () => true;
  expect((await core.call("call_command", { name: "review", args: "" })).result.pending).toBeNull();
  await core.call("set_ctx", { ctx: { pendingPlanReview: REVIEW } });
  const r = (await core.call("call_command", { name: "review", args: "" })).result;
  expect(r.pending).toEqual({ ...REVIEW, options: [{ ...REVIEW.options[0], disabled: true }, ...REVIEW.options.slice(1)] });
  expect(r.refused).toEqual([false, false, false, false, false, false]);
  expect(r).toMatchObject({ ok: true, again: false, after: null });
  const reqs = core.requests.filter((m) => m.method === "resolve_plan_review");
  expect(reqs.map((m) => m.params)).toEqual([{ reviewId: "r1", choice: "execute", input: {} }]);
});

test("resolvePlanReview refine carries the feedback; the resolved emit clears the pending review", async () => {
  const core = await hostWith(REVIEW_PROBE);
  core.answer.resolve_plan_review = () => true;
  await core.call("emit", { event: REVIEW, ctx: { pendingPlanReview: REVIEW } });
  const r = (await core.call("call_command", { name: "review", args: "refine" })).result;
  expect(r.ok).toBe(true);
  const req = core.requests.find((m) => m.method === "resolve_plan_review")!;
  expect(req.params).toEqual({ reviewId: "r1", choice: "refine", input: { feedback: "add tests" } });
  await core.call("emit", { event: REVIEW, ctx: { pendingPlanReview: REVIEW } });
  await core.call("emit", { event: { type: "plan_review_resolved", reviewId: "r1", sessionId: "s1", by: "local" }, ctx: { pendingPlanReview: null } });
  expect((await core.call("call_command", { name: "review", args: "" })).result.pending).toBeNull();
});
