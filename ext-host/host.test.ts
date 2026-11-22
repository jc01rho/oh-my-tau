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
  /** Host -> core notifications (no id): `log`, `append_entry`, … */
  notifications: Json[] = [];
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
        else this.notifications.push(msg);
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

/** The host's side-turn id on the request it just sent (echoed back on `ephemeral_delta`). */
const streamOf = (params: Json) => params.streamId as number;

/** Stream `deltas` to a running side turn, in order, then settle it with `replyText`. */
async function streamReply(core: Core, params: Json, deltas: string[], replyText: string): Promise<Json> {
  for (const delta of deltas) {
    const r = await core.call("ephemeral_delta", { streamId: streamOf(params), delta });
    if (r.error) throw new Error(`ephemeral_delta failed: ${r.error.message}`);
  }
  return { replyText, assistantMessage: { ...REPLY.assistantMessage, content: [{ type: "text", text: replyText }] } };
}

test("runEphemeralTurn forwards options to the core and returns EphemeralTurnResult", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) => {
    const deltas = []; const usage = [];
    const r = await ctx.runEphemeralTurn({ promptText: "why?", maxTokens: 32, tools: false, history: [],
      onTextDelta: (d) => void deltas.push(d), onUsage: (m) => void usage.push(m.stopReason) });
    return { r, deltas, usage };
  } });`);
  // The reply reaches the caller through the deltas the core streams; this core streams none.
  core.answer.run_ephemeral_turn = () => REPLY;
  const r = await core.call("call_command", { name: "side", args: "" });
  expect(r.result).toEqual({ r: REPLY, deltas: [], usage: ["stop"] });
  const req = core.requests.find((m) => m.method === "run_ephemeral_turn")!;
  // Callbacks and the signal never cross the wire: only their announcement does.
  const { streamId, ...rest } = req.params;
  expect(rest).toEqual({ promptText: "why?", maxTokens: 32, tools: false, history: [], usageCallback: true });
  expect(typeof streamId).toBe("number");
});

test("runEphemeralTurn streams onTextDelta deltas as the core produces them", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) => {
    const deltas = [];
    const r = await ctx.runEphemeralTurn({ promptText: "q", onTextDelta: async (d) => { await Promise.resolve(); deltas.push(d); } });
    return { reply: r.replyText, deltas };
  } });`);
  core.answer.run_ephemeral_turn = (params) => streamReply(core, params, ["Hel", "lo, ", "world"], "Hello, world");
  const r = await core.call("call_command", { name: "side", args: "" });
  expect(r.result).toEqual({ reply: "Hello, world", deltas: ["Hel", "lo, ", "world"] });
});

test("runEphemeralTurn rejects the side turn when onTextDelta throws", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) => {
    const deltas = [];
    const outcome = await ctx.runEphemeralTurn({ promptText: "q", onTextDelta: (d) => {
      deltas.push(d);
      if (d === "boom") throw new Error("callback exploded");
    } }).then(() => "resolved", (e) => e.message);
    return { outcome, deltas };
  } });`);
  // The core sees the rejected delivery and must not report a settled reply.
  core.answer.run_ephemeral_turn = async (params) => streamReply(core, params, ["ok", "boom"], "unused");
  const r = await core.call("call_command", { name: "side", args: "" });
  expect(r.result.deltas).toEqual(["ok", "boom"]);
  expect(r.result.outcome).toContain("ephemeral_delta failed");
});

test("ephemeral_delta for a side turn that is not running is refused", async () => {
  const core = await hostWith(`export default (pi) => pi.registerCommand("side", { handler: async (_a, ctx) =>
    ctx.runEphemeralTurn({ promptText: "q" }) });`);
  core.answer.run_ephemeral_turn = () => REPLY;
  await core.call("call_command", { name: "side", args: "" });
  const r = await core.call("ephemeral_delta", { streamId: 999, delta: "late" });
  expect(r.error.message).toBe("ephemeral_delta: no running side turn 999");
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
  core.answer.run_ephemeral_turn = (params) => streamReply(core, params, ["x"], "x");
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

test("session_before_shutdown: handlers get a live signal and a cancel short-circuits", async () => {
  const core = await hostWith(TWO(`(e) => ({ cancel: e.signal instanceof AbortSignal && !e.signal.aborted })`, `() => { throw new Error("must not run"); }`, "session_before_shutdown"));
  const r = await core.call("emit", { event: { type: "session_before_shutdown" }, abortId: 1 });
  expect(r.result.result).toEqual({ cancel: true });
  expect(r.result.handlers).toHaveLength(1);
});

test("session_before_shutdown: abort_emit aborts the signal and drops the late cancel", async () => {
  const core = await hostWith(`export default (pi) => pi.on("session_before_shutdown", async (e, ctx) => {
    await ctx.ui.confirm("Quit?", "sure");
    if (!e.signal.aborted) await new Promise((r) => e.signal.addEventListener("abort", r, { once: true }));
    return { cancel: true };
  });`);
  // The core stops waiting while the handler's dialog is open.
  core.answer.ui_confirm = () => {
    core.send({ id: 9_998, method: "abort_emit", params: { abortId: 7 } });
    return true;
  };
  const r = await core.call("emit", { event: { type: "session_before_shutdown" }, abortId: 7 });
  expect(r.result.result).toBeNull();
  expect(core.requests.filter((m) => m.method === "ui_confirm")).toHaveLength(1);
});

test("session_before_shutdown: an abort that arrives first skips every handler", async () => {
  const core = await hostWith(`export default (pi) => pi.on("session_before_shutdown", () => ({ cancel: true }));`);
  expect((await core.call("abort_emit", { abortId: 5 })).result).toBe(true);
  const r = await core.call("emit", { event: { type: "session_before_shutdown" }, abortId: 5 });
  expect(r.result.result).toBeNull();
  expect(r.result.handlers).toEqual([]);
});

// ---------------------------------------------------------------- link_activate
/** Load `sources` as extensions in one host; returns the core plus the load result. */
async function hostWithSources(sources: string[]): Promise<{ core: Core; loaded: Json[]; errors: Json[] }> {
  const dir = mkdtempSync(join(tmpdir(), "omt-ext-"));
  const paths = sources.map((source, i) => {
    const path = join(dir, `probe-${i}.ts`);
    writeFileSync(path, source);
    return path;
  });
  const core = new Core();
  cores.push(core);
  const r = await core.call("load", { paths, cwd: dir });
  return { core, loaded: r.result.loaded, errors: r.result.errors };
}

/** An extension claiming `scheme`, recording every `link_activate` and serving it as `cmd`. */
const LINKER = (scheme: string, cmd: string, body = "seen.push(e.scheme + '|' + e.url);") => `export default (pi) => {
  pi.registerLinkScheme("${scheme}");
  const seen: string[] = [];
  pi.on("link_activate", (e) => { ${body} });
  pi.registerCommand("${cmd}", { handler: () => seen });
};`;

test("registerLinkScheme validates the scheme and reports it on load", async () => {
  const { core, loaded, errors } = await hostWithSources([
    `export default (pi) => {
      const rejected: string[] = [];
      for (const bad of ["a", "cc stretch", "1cc", "http", "HTTPS", "memory", "omp", ""]) {
        try { pi.registerLinkScheme(bad); rejected.push("accepted:" + bad); } catch (e) { rejected.push(String(e.message)); }
      }
      pi.registerLinkScheme("CC-Stretch");
      pi.registerLinkScheme("cc-stretch");
      pi.registerCommand("rejected", { handler: () => rejected });
    };`,
  ]);
  expect(errors).toEqual([]);
  expect(loaded[0].linkSchemes).toEqual(["cc-stretch"]);
  const rejected = (await core.call("call_command", { name: "rejected", args: "" })).result;
  expect(rejected.some((m: string) => m.startsWith("accepted:"))).toBe(false);
  expect(rejected[0]).toContain("is not a URL scheme of two or more characters");
  expect(rejected[3]).toBe('registerLinkScheme: "http" is reserved for standard or omp-internal links');
  expect(rejected[5]).toBe('registerLinkScheme: "memory" is reserved for standard or omp-internal links');
});

test("link_activate reaches only the extension that claimed the scheme", async () => {
  const { core } = await hostWithSources([LINKER("cc-stretch", "seen-a"), LINKER("other", "seen-b")]);
  const r = await core.call("link_activate", { url: "cc-stretch:open/1792/0/2" });
  expect(r.result).toEqual({ handled: true });
  expect((await core.call("call_command", { name: "seen-a", args: "" })).result).toEqual([
    "cc-stretch|cc-stretch:open/1792/0/2",
  ]);
  expect((await core.call("call_command", { name: "seen-b", args: "" })).result).toEqual([]);
  // The scheme half of a URL is case-insensitive; the URL itself is passed through verbatim.
  await core.call("link_activate", { url: "CC-Stretch:open/1" });
  expect((await core.call("call_command", { name: "seen-a", args: "" })).result).toEqual([
    "cc-stretch|cc-stretch:open/1792/0/2",
    "cc-stretch|CC-Stretch:open/1",
  ]);
});

test("link_activate reports an unowned link as handled=false and runs nobody", async () => {
  const { core } = await hostWithSources([LINKER("cc-stretch", "seen-a")]);
  expect((await core.call("link_activate", { url: "https://example.com" })).result).toEqual({ handled: false });
  expect((await core.call("link_activate", { url: "cc-stretch" })).result).toEqual({ handled: false });
  expect((await core.call("link_activate", { url: ":nothing" })).result).toEqual({ handled: false });
  expect((await core.call("call_command", { name: "seen-a", args: "" })).result).toEqual([]);
});

test("link_activate is owned even without a handler, and a failing handler is reported not thrown", async () => {
  const { core } = await hostWithSources([
    `export default (pi) => { pi.registerLinkScheme("quiet"); };`,
    `export default (pi) => {
      pi.registerLinkScheme("loud");
      pi.on("link_activate", () => { throw new Error("boom"); });
    };`,
  ]);
  expect((await core.call("link_activate", { url: "quiet:op" })).result).toEqual({ handled: true });
  expect((await core.call("link_activate", { url: "loud:op" })).result).toEqual({ handled: true });
  // The failure is reported to the core (pi warns and emits an error), never thrown back.
  const warned = core.notifications.filter((n) => n.method === "log" && String(n.params.message).includes("boom"));
  expect(warned.length).toBe(1);
  // The host keeps serving after the handler error.
  expect((await core.call("link_activate", { url: "quiet:op" })).result).toEqual({ handled: true });
});

test("emit link_activate is owner-routed, never broadcast", async () => {
  const { core } = await hostWithSources([LINKER("cc-stretch", "seen-a"), LINKER("other", "seen-b")]);
  const r = await core.call("emit", { event: { type: "link_activate", url: "other:go" } });
  expect(r.result.result).toEqual({ handled: true });
  expect(r.result.handlers).toHaveLength(1);
  expect((await core.call("call_command", { name: "seen-b", args: "" })).result).toEqual(["other|other:go"]);
  expect((await core.call("call_command", { name: "seen-a", args: "" })).result).toEqual([]);
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
