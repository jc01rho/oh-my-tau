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
