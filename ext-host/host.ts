#!/usr/bin/env bun
// omp extension host: loads omp ExtensionAPI extensions (unchanged .ts files) and serves them to the Rust core
// over JSON-RPC 2.0 on stdio (newline-delimited). Handler aggregation mirrors pi-coding-agent's ExtensionRunner.
//
// core -> host requests: load, emit, list_tools, call_tool, cancel_tool, list_commands, call_command, set_ctx, bus_emit, abort_emit, shutdown
// host -> core:          notifications append_entry, send_message, send_user_message, log, ui_notify, set_label,
//                        provider_registered, provider_unregistered;
//                        requests compact, navigate_tree, ui_confirm (core may answer with an error → treated as unsupported)
// Additive RPC:          list_flags, set_flag_value, list_providers (flag/provider state is also queryable host-side).
//
// Env: OMP_EXT_TRACE=<path> appends one JSON line per emit (per-handler action/ms) to <path>;
//      OMP_EXT_TIMEOUT_MS handler budget (default 30000); OMP_PKG_ROOT omp's pi-coding-agent package dir (for typebox/zod).
import { AsyncLocalStorage } from "node:async_hooks";
import { existsSync, appendFileSync, readdirSync, statSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { homedir } from "node:os";
import { spawn } from "node:child_process";

type Json = Record<string, unknown>;
type Handler = (event: Json, ctx: unknown) => unknown;
interface Ext {
  path: string;
  name: string;
  handlers: Map<string, Handler[]>;
  tools: Map<string, Json>;
  commands: Map<string, Json>;
  flags: Map<string, Json>;
  /** Link schemes claimed with `registerLinkScheme`, lowercase, in claim order. */
  linkSchemes: string[];
  /** Extension RPC names this extension registered (`api.rpc.handle`). */
  rpc: string[];
  label?: string;
}

// Per-session mutable: the shared daemon runs one Worker per session, and each
// Worker gets its own module instance of this file, so module-level state is
// per-session exactly as in separate processes. `load` may additionally
// override TIMEOUT_MS/TRACE per session (the daemon's env is shared).
let TIMEOUT_MS = Number(process.env.OMP_EXT_TIMEOUT_MS) || 30_000;
const SHUTDOWN_TIMEOUT_MS = 2_000;
let TRACE = process.env.OMP_EXT_TRACE;
const PKG_ROOT =
  process.env.OMP_PKG_ROOT ?? join(homedir(), ".bun/install/global/node_modules/@oh-my-pi/pi-coding-agent");

// ---------------------------------------------------------------- link schemes
/** URL scheme syntax an extension may claim (`registerLinkScheme`, pi loader.ts). */
const LINK_SCHEME_PATTERN = /^[a-z][a-z0-9+.-]+$/;
/** Schemes the terminal or the browser owns: never an extension's to claim. */
const STANDARD_LINK_SCHEMES = new Set([
  "about",
  "blob",
  "data",
  "file",
  "ftp",
  "http",
  "https",
  "javascript",
  "mailto",
  "ssh",
  "ws",
  "wss",
]);
/** omp-internal schemes (crates/omp-urls `schemes::register_all`): the core routes these itself. */
const BUILTIN_LINK_SCHEMES = new Set([
  "agent",
  "artifact",
  "cfg",
  "history",
  "local",
  "mcp",
  "memory",
  "omp",
  "rule",
  "skill",
  "xd",
]);
/** Schemes already reported as multiply claimed (pi's `#warnedLinkSchemes`). */
const warnedLinkSchemes = new Set<string>();

// ---------------------------------------------------------------- transport
// In stdio mode `send` writes to stdout; in shared-daemon Worker mode it posts
// the line to the daemon, which writes it to the session's unix socket.
let emitLine: (line: string) => void = () => {};
let closeSession: (() => void) | null = null;
const out = (msg: Json) => emitLine(`${JSON.stringify({ jsonrpc: "2.0", ...msg })}\n`);
// Extensions (and libs they import) log to stdout; the channel is stdout, so route console.* to stderr.
const toStderr = (...a: unknown[]) => process.stderr.write(`${a.map(String).join(" ")}\n`);
console.log = toStderr;
console.info = toStderr;
console.debug = toStderr;

let nextId = 1;
const pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();
// Fire-and-forget emits (`emit` with `nowait`) run strictly serially, and every
// later awaited `emit`/`call_tool` waits for them (wire order == execution
// order, as when the core awaited each one). `inNowait` marks async contexts
// running such a handler: while one of them is blocked on a core request
// (e.g. a handler calling ctx.compact(), which makes the core emit
// session_before_compact), awaited emits stop waiting so that request cannot
// deadlock on the barrier it is itself holding open.
const inNowait = new AsyncLocalStorage<true>();
let nowaitBlocked = 0;
// Every awaited emit/call_tool parked on the barrier (parallel tool calls can
// park several at once); a blocking fire-and-forget handler wakes them all.
const nowaitWakers = new Set<() => void>();
function request(method: string, params: Json): Promise<unknown> {
  const id = nextId++;
  const { promise, resolve, reject } = Promise.withResolvers<unknown>();
  pending.set(id, { resolve, reject });
  out({ id, method, params });
  if (!inNowait.getStore()) return promise;
  nowaitBlocked++;
  for (const wake of nowaitWakers) wake();
  nowaitWakers.clear();
  return promise.finally(() => void nowaitBlocked--);
}
const notify = (method: string, params: Json) => out({ method, params });
const log = (level: string, message: string, extra?: Json) => notify("log", { level, message, ...extra });

// ---------------------------------------------------------------- state
const exts: Ext[] = [];
const bus = new Map<string, Set<(d: unknown) => void>>();
const timers = new Set<ReturnType<typeof setTimeout>>();
// Flag values / providers mirror pi-coding-agent's ExtensionRuntime: registerFlag seeds
// flagValues with the default; the core may update values via the set_flag_value RPC.
const flagValues = new Map<string, boolean | string>();
const providers = new Map<string, { name: string; config: Json; extension: string }>();
// Every write to `flagValues` (a load's `registerFlag` seed, or the core's
// `set_flag_value`) takes the next revision, and `flagWrites` remembers which
// revision currently holds each name. `rollbackLoad` undoes a flag only while
// its failed load is still the latest writer: a write that landed afterwards
// (a subsequent extension load, or the core) is not that load's to clobber.
let writeRev = 0;
const flagWrites = new Map<string, number>();
let state: Json = { cwd: process.cwd(), hasUI: false, agentKind: "main", branch: [] };
let ctxCache: unknown;

// ---------------------------------------------------------------- schema libs (lazy)
// `api.typebox` / `api.zod` are imported synchronously on FIRST access (Bun's
// `require` handles TS/ESM). Eagerly importing them at `load` costs ~39 MB RSS
// (omptype + pi-ai's schema barrel) in every session, including sessions whose
// extensions never touch either. typebox is a vendored copy of omp's
// legacy-typebox.ts that pulls two leaf functions instead of pi-ai's whole
// schema index (see legacy-typebox.ts). zod resolution is anchored at the omp
// package so it resolves exactly as omp's own import would.
import { createRequire } from "node:module";
const ompRequire = createRequire(join(PKG_ROOT, "src", "extensibility", "noop.js"));
let typebox: unknown;
let zod: unknown;
function loadSchemaLib(which: "typebox" | "zod"): unknown {
  if (which === "typebox" ? typebox !== undefined : zod !== undefined) return which === "typebox" ? typebox : zod;
  let mod: unknown;
  try {
    mod = which === "typebox" ? require("./legacy-typebox.ts") : ompRequire("@oh-my-pi/omptype/zod");
  } catch (e) {
    log("warn", `${which} unavailable (api.${which} undefined): ${e instanceof Error ? e.message : e}`);
    return undefined;
  }
  if (which === "typebox") typebox = mod;
  else zod = mod;
  return mod;
}

const events = {
  on(ch: string, fn: (d: unknown) => void) {
    const s = bus.get(ch) ?? new Set();
    s.add(fn);
    bus.set(ch, s);
    return () => s.delete(fn);
  },
  emit(ch: string, data: unknown) {
    for (const fn of bus.get(ch) ?? []) {
      try {
        void Promise.resolve(fn(data)).catch((e) => log("warn", `event bus handler ${ch}: ${e}`));
      } catch (e) {
        log("warn", `event bus handler ${ch}: ${e}`);
      }
    }
  },
};

function execCommand(command: string, args: string[], cwd: string, opts: Json = {}) {
  return new Promise<Json>((res) => {
    const child = spawn(command, args, { cwd, stdio: ["ignore", "pipe", "pipe"], signal: opts.signal as AbortSignal | undefined });
    let stdout = "";
    let stderr = "";
    let killed = false;
    const t = typeof opts.timeout === "number" ? setTimeout(() => ((killed = true), child.kill("SIGKILL")), opts.timeout) : undefined;
    child.stdout.on("data", (d) => (stdout += d));
    child.stderr.on("data", (d) => (stderr += d));
    child.on("error", (e) => {
      clearTimeout(t);
      res({ stdout, stderr: stderr || String(e), code: -1, killed: killed || (e as Error).name === "AbortError" });
    });
    child.on("close", (code) => {
      clearTimeout(t);
      res({ stdout, stderr, code: code ?? -1, killed });
    });
  });
}

const sm = () => ({
  getCwd: () => state.cwd,
  getSessionId: () => state.sessionId,
  getSessionFile: () => state.sessionFile,
  getSessionDir: () => state.sessionFile ? dirname(String(state.sessionFile)) : undefined,
  getLeafId: () => state.leafId ?? null,
  getBranch: () => (state.branch as Json[]) ?? [],
  getEntries: () => (state.branch as Json[]) ?? [],
  getEntry: (id: string) => ((state.branch as Json[]) ?? []).find((e) => e.id === id),
  getSessionName: () => state.sessionName,
});

const orUndefined = (v: unknown) => (typeof v === "string" ? v : undefined);
// Unimplemented ExtensionAPI registration/service members throw instead of
// doing nothing: a registration that cannot take effect must not let the
// extension load and report success while nothing it registered is ever shown
// (docs/research/09-extension-model-comparison.md). Only those registration and
// service methods throw; the headless `makeCtx().ui` surface stays no-op, as in
// pi-coding-agent's noOpUIContext.
const unsupportedNow = (what: string): never => {
  throw new Error(`${what} is not available in the Rust extension host`);
};
// `ctx.runEphemeralTurn` (TS EphemeralTurnOptions -> EphemeralTurnResult): the core runs the
// /btw side turn (`run_ephemeral_turn` request); nothing enters the session history and tool
// calls are never executed. Callbacks cannot cross the wire, so the request announces them: with
// `onTextDelta` it carries a `streamId` the core echoes on each `ephemeral_delta` request (awaited
// in arrival order, so the callback sees the provider's deltas as they stream), and with `onUsage`
// a `usageCallback` flag (the core skips its own `purpose` journal then, as TS does). Callbacks run
// marked as inside the side turn, so they cannot start another one (bounded recursion, as in TS).
const inEphemeral = new AsyncLocalStorage<true>();
/** Live `onTextDelta` deliveries of running side turns, keyed by the id the core echoes back. */
const ephemeralDeltas = new Map<number, (delta: string) => unknown>();
let nextStreamId = 1;
async function runEphemeralTurn(options: Json): Promise<{ replyText: string; assistantMessage: Json }> {
  if (inEphemeral.getStore()) throw new Error("runEphemeralTurn cannot start inside a running side turn");
  if (!options || typeof options !== "object") throw new Error("runEphemeralTurn: options must be an object");
  const { signal, onTextDelta, onUsage, ...wire } = options as Json & {
    signal?: AbortSignal;
    onTextDelta?: (delta: string) => unknown;
    onUsage?: (message: Json) => unknown;
  };
  signal?.throwIfAborted();
  const streamId = onTextDelta ? nextStreamId++ : undefined;
  if (streamId !== undefined && onTextDelta) {
    ephemeralDeltas.set(streamId, (delta) => inEphemeral.run(true, () => onTextDelta(delta)));
    wire.streamId = streamId;
  }
  if (onUsage) wire.usageCallback = true;
  let onAbort: (() => void) | undefined;
  const aborted = new Promise<never>((_, reject) => {
    onAbort = () => reject(signal?.reason ?? new Error("runEphemeralTurn aborted"));
    signal?.addEventListener("abort", onAbort, { once: true });
  });
  try {
    const reply = (await Promise.race([request("run_ephemeral_turn", wire), aborted])) as Json | null;
    if (!reply || typeof reply.replyText !== "string" || !reply.assistantMessage || typeof reply.assistantMessage !== "object") {
      throw new Error("runEphemeralTurn: the core returned a malformed result");
    }
    const result = { replyText: reply.replyText, assistantMessage: reply.assistantMessage as Json };
    // TS reports usage where the provider settles; the core already streamed every delta.
    await inEphemeral.run(true, async () => {
      if (onUsage) {
        try {
          await onUsage(result.assistantMessage);
        } catch (e) {
          log("warn", `runEphemeralTurn onUsage failed: ${e instanceof Error ? e.message : e}`);
        }
      }
    });
    return result;
  } finally {
    if (onAbort) signal?.removeEventListener("abort", onAbort);
    if (streamId !== undefined) ephemeralDeltas.delete(streamId);
  }
}
// `ctx.resolvePlanReview` / `ctx.getPendingPlanReview` (TS ExtensionContext). The core owns the plan
// review and mirrors the open one into `state.pendingPlanReview` (the `plan_review_*` emits carry it).
// TS answers synchronously, so the same acceptance rules run here against that mirror; an accepted
// answer is forwarded to the core (`resolve_plan_review`), which re-checks it against the live review
// and closes it with `by: "extension"`. Should the core have closed the review in the meantime it
// refuses the answer, which is logged.
const PLAN_APPROVALS = new Set(["execute", "compact", "keep"]);
function getPendingPlanReview(): Json | undefined {
  const pending = state.pendingPlanReview as Json | null | undefined;
  return pending ? structuredClone(pending) : undefined;
}
function resolvePlanReview(reviewId: string, choice: string, input?: { feedback?: string; role?: string }): boolean {
  const pending = state.pendingPlanReview as Json | null | undefined;
  if (!pending || pending.reviewId !== reviewId) return false;
  const option = ((pending.options as Json[] | undefined) ?? []).find((o) => o.id === choice);
  if (!option) return false;
  if (choice === "refine" && !(typeof input?.feedback === "string" && input.feedback.trim())) return false;
  if (input?.role !== undefined) {
    const tiers = (pending.tiers as Json[] | undefined) ?? [];
    if (!PLAN_APPROVALS.has(choice) || !tiers.some((t) => t.role === input.role)) return false;
  } else if (option.disabled) {
    return false;
  }
  // Answered: the review takes no second answer (the core's `plan_review_resolved` clears it too).
  state = { ...state, pendingPlanReview: null };
  void request("resolve_plan_review", { reviewId, choice, input: { feedback: input?.feedback, role: input?.role } }).then(
    (accepted) => {
      if (accepted !== true) log("warn", `resolvePlanReview(${reviewId}, ${choice}): the core refused the answer`);
    },
    (e) => log("warn", `resolvePlanReview(${reviewId}, ${choice}) failed: ${e instanceof Error ? e.message : e}`),
  );
  return true;
}
function makeCtx() {
  const noop = () => {};
  return {
    ui: {
      notify: (message: string, type?: string) => notify("ui_notify", { message, type: type ?? "info" }),
      confirm: async (title: string, message: string) => {
        try {
          return (await request("ui_confirm", { title, message })) === true;
        } catch {
          return false;
        }
      },
      setStatus: noop,
      setWidget: noop,
      setTitle: noop,
      // Remaining members mirror pi-coding-agent's noOpUIContext (headless defaults, runner.ts).
      setWorkingMessage: noop,
      setFooter: noop,
      setHeader: noop,
      setEditorText: noop,
      pasteToEditor: noop,
      getEditorText: () => "",
      addAutocompleteProvider: noop,
      setEditorComponent: noop,
      onTerminalInput: () => () => {},
      custom: async () => undefined,
      getToolsExpanded: () => false,
      setToolsExpanded: noop,
      get theme() {
        return undefined;
      },
      getAllThemes: () => Promise.resolve([]),
      getTheme: () => Promise.resolve(undefined),
      setTheme: () => Promise.resolve({ success: false as const, error: "UI not available" }),
      select: async (title: string, options: string[]) => orUndefined(await request("ui_select", { title, options }).catch(() => null)),
      input: async (title: string, placeholder?: string) => orUndefined(await request("ui_input", { title, placeholder }).catch(() => null)),
      editor: async (title: string, prefill?: string) => orUndefined(await request("ui_editor", { title, prefill }).catch(() => null)),
    },
    mode: state.mode ?? "rpc",
    get hasUI() {
      return state.hasUI === true;
    },
    get cwd() {
      return String(state.cwd);
    },
    get model() {
      return state.model;
    },
    get agent() {
      return { kind: state.agentKind ?? "main", ...((state.agent as Json) ?? {}) };
    },
    sessionManager: sm(),
    modelRegistry: {
      getApiKey: async () => undefined,
      getAvailable: () => [],
      getAll: () => [],
      complete: async (model: Json, context: Json, options: Json) => request("model_complete", {
        provider: String(model.provider), model: String(model.id), context, options,
      }),
    },
    getContextUsage: () => state.contextUsage,
    getAsyncJobSnapshot: () => (state.asyncJobs as Json) ?? null,
    compact: (o?: unknown) => request("compact", { options: o as Json }).then(() => undefined),
    navigateTree: (target: string, o?: unknown) => request("navigate_tree", { target, options: o as Json }),
    isIdle: () => state.idle !== false,
    abort: () => notify("abort", {}),
    hasPendingMessages: () => state.pending === true,
    shutdown: () => notify("shutdown_requested", {}),
    isProjectTrusted: () => true,
    getSystemPrompt: () => (state.systemPrompt as string[]) ?? [],
    runEphemeralTurn: (options: Json) => runEphemeralTurn(options),
    resolvePlanReview: (reviewId: string, choice: string, input?: { feedback?: string; role?: string }) =>
      resolvePlanReview(reviewId, choice, input),
    getPendingPlanReview: () => getPendingPlanReview(),
    setInterval: (cb: (...a: unknown[]) => void, ms?: number, ...a: unknown[]) => {
      const t = setInterval(() => safe(cb, a), ms);
      t.unref?.();
      timers.add(t);
      return t;
    },
    setTimeout: (cb: (...a: unknown[]) => void, ms?: number, ...a: unknown[]) => {
      const t = setTimeout(() => safe(cb, a), ms);
      t.unref?.();
      timers.add(t);
      return t;
    },
  };
}
function safe(cb: (...a: unknown[]) => unknown, a: unknown[]) {
  try {
    void Promise.resolve(cb(...a)).catch((e) => log("warn", `timer callback failed: ${e}`));
  } catch (e) {
    log("warn", `timer callback failed: ${e}`);
  }
}
function ctx() {
  return (ctxCache ??= makeCtx());
}

type FlagValue = boolean | string;
type ProviderReg = { name: string; config: Json; extension: string };

/** Undo log for one `loadOne` call: what it registered on host-wide state. */
interface LoadTracker {
  subs: (() => void)[];
  /** Extension RPC names this load registered (`api.rpc.handle`). */
  rpc: string[];
  /** Providers this load wrote, with the registration they replaced (if any). */
  providers: { name: string; prior?: ProviderReg }[];
  /** `flagValues` entries this load overwrote, recorded before the write. */
  flags: Map<string, FlagWrite>;
}

/**
 * Extension RPC registry: `<name>` -> the extension that owns it.
 * Names are dotted and prefixed with the extension's own namespace (`omomeow.*`, `omo.*`).
 */
const rpcHandlers = new Map<string, { ext: string; fn: (data: unknown) => unknown }>();

const RPC_NAME_RE = /^[a-z0-9][a-z0-9-]*(\.[a-z0-9][a-z0-9-]*)+$/;

/** Validate an extension RPC name; the contract fixes the shape, not the namespace contents. */
function checkRpcName(name: string): string {
  const n = name.trim();
  if (!n) throw new Error("extension rpc name cannot be empty");
  if (n.length > 64) throw new Error(`extension rpc name too long (${n.length} > 64): ${n}`);
  if (!RPC_NAME_RE.test(n)) throw new Error(`extension rpc name must be dotted lowercase: ${n}`);
  return n;
}

/** One `flagValues` write a load made: the value it replaced, and its revision. */
interface FlagWrite {
  had: boolean;
  prior?: FlagValue;
  /** Revision of the write this one replaced (absent: nobody had written the name). */
  priorRev?: number;
  /** Revision this load's write holds; reset if the load writes the name again. */
  rev: number;
}

/** Undo everything a failed load registered, so nothing of it outlives the error. */
function rollbackLoad(ext: Ext, track: LoadTracker) {
  for (const name of track.rpc.splice(0)) {
    if (rpcHandlers.get(name)?.ext === ext.name) rpcHandlers.delete(name);
  }
  for (const off of track.subs.splice(0).reverse()) {
    try {
      off();
    } catch (e) {
      log("warn", `${ext.path} cleanup: ${e instanceof Error ? e.message : e}`);
    }
  }
  for (const [name, { had, prior, priorRev, rev }] of track.flags) {
    // A later write owns the name now: undoing ours would revert somebody else's value.
    if (flagWrites.get(name) !== rev) continue;
    if (had) flagValues.set(name, prior as FlagValue);
    else flagValues.delete(name);
    if (priorRev === undefined) flagWrites.delete(name);
    else flagWrites.set(name, priorRev);
  }
  track.flags.clear();
  for (const { name, prior } of track.providers.splice(0).reverse()) {
    const live = providers.get(name);
    // Undo only this load's own write: a registration made after the failed load
    // (another extension's load) is not ours to clobber, and a name the load just
    // unregistered is absent, which is still ours to restore.
    if (live !== undefined && live.extension !== ext.name) continue;
    if (prior) {
      // The load replaced a live registration: put it back and say so, so the
      // core's registry never ends up pointing at the extension that failed.
      providers.set(name, prior);
      notify("provider_registered", { name, config: prior.config, extension: prior.extension });
    } else if (live !== undefined) {
      providers.delete(name);
      notify("provider_unregistered", { name, extension: ext.name });
    }
  }
}

function makeApi(ext: Ext, track: LoadTracker) {
  const api = {
    logger: {
      debug: (m: string, x?: Json) => log("debug", m, x),
      info: (m: string, x?: Json) => log("info", m, x),
      warn: (m: string, x?: Json) => log("warn", m, x),
      error: (m: string, x?: Json) => log("error", m, x),
    },
    get typebox() {
      return loadSchemaLib("typebox");
    },
    get zod() {
      return loadSchemaLib("zod");
    },
    // `events.on` subscribes on the host-wide bus, so the unsubscribe is recorded
    // here and replayed by `rollbackLoad` when this extension fails to load.
    events: {
      on(ch: string, fn: (d: unknown) => void) {
        const off = events.on(ch, fn);
        // Self-unsubscribing wrapper: a caller that unsubscribes before the load
        // fails must not leave a dead entry for `rollbackLoad` to replay.
        const entry = () => {
          const i = track.subs.indexOf(entry);
          if (i >= 0) track.subs.splice(i, 1);
          off();
        };
        track.subs.push(entry);
        return entry;
      },
      emit: events.emit,
    },
    pi: {},
    on(event: string, handler: Handler) {
      const l = ext.handlers.get(event) ?? [];
      l.push(handler);
      ext.handlers.set(event, l);
    },
    registerTool(tool: Json) {
      ext.tools.set(String(tool.name), tool);
    },
    registerCommand(name: string, options: Json) {
      ext.commands.set(name, { name, ...options });
    },
    // Claim a link scheme (`cc-stretch:…`): a click on such a link is delivered to this extension as
    // `link_activate` instead of reaching the terminal or the browser (pi loader.ts validation).
    registerLinkScheme(scheme: string) {
      if (typeof scheme !== "string") {
        throw new TypeError(`registerLinkScheme expects a string, got ${typeof scheme}`);
      }
      const normalized = scheme.toLowerCase();
      if (!LINK_SCHEME_PATTERN.test(normalized)) {
        throw new Error(
          `registerLinkScheme: "${scheme}" is not a URL scheme of two or more characters (letters, digits, "+", "-", ".")`,
        );
      }
      if (STANDARD_LINK_SCHEMES.has(normalized) || BUILTIN_LINK_SCHEMES.has(normalized)) {
        throw new Error(`registerLinkScheme: "${scheme}" is reserved for standard or omp-internal links`);
      }
      if (!ext.linkSchemes.includes(normalized)) ext.linkSchemes.push(normalized);
    },
    // `api.rpc.handle(name, fn)` answers `extension_request` from the core. Registration is
    // synchronous so a bad name or a duplicate fails the load instead of surfacing on call.
    rpc: {
      handle(name: string, fn: (data: unknown) => unknown) {
        const n = checkRpcName(String(name));
        if (typeof fn !== "function") throw new Error(`extension rpc handler must be a function: ${n}`);
        const live = rpcHandlers.get(n);
        if (live) throw new Error(`duplicate extension rpc handler: ${n} (already registered by ${live.ext})`);
        rpcHandlers.set(n, { ext: ext.name, fn });
        ext.rpc.push(n);
        track.rpc.push(n);
      },
    },
    registerShortcut: () => unsupportedNow("registerShortcut"),
    // Semantics mirror pi-coding-agent loader.ts: the default seeds flagValues; getFlag only
    // sees flags this extension registered (omp scopes flags per extension).
    registerFlag(name: string, options: Json) {
      ext.flags.set(name, { name, ...options });
      if (options.default !== undefined) {
        const rev = ++writeRev;
        const rec = track.flags.get(name);
        // The first write records what to hand back; a repeat write in the same
        // load only takes over the revision (the prior value is unchanged).
        if (rec) {
          // Unless somebody else wrote the name in between: then the value to
          // hand back is that later write, not the one our first write replaced.
          if (flagWrites.get(name) !== rec.rev) {
            rec.had = flagValues.has(name);
            rec.prior = flagValues.get(name);
            rec.priorRev = flagWrites.get(name);
          }
          rec.rev = rev;
        } else {
          track.flags.set(name, { had: flagValues.has(name), prior: flagValues.get(name), priorRev: flagWrites.get(name), rev });
        }
        flagValues.set(name, options.default as FlagValue);
        flagWrites.set(name, rev);
      }
    },
    getFlag(name: string) {
      if (!ext.flags.has(name)) return undefined;
      return flagValues.get(name);
    },
    // Unimplemented: this host has no renderer/composer/fallback pipeline, and a
    // silent no-op would make an extension that installs one look installed.
    registerMessageRenderer: () => unsupportedNow("registerMessageRenderer"),
    registerAssistantThinkingRenderer: () => unsupportedNow("registerAssistantThinkingRenderer"),
    registerComposerShape: () => unsupportedNow("registerComposerShape"),
    registerFileWriteFallback: () => unsupportedNow("registerFileWriteFallback"),
    registerFileDeleteFallback: () => unsupportedNow("registerFileDeleteFallback"),
    // Provider registrations are forwarded to the core as notifications (the Rust core owns
    // the model registry), and kept host-side so list_providers can report them.
    registerProvider(name: string, config: Json) {
      // Only the first write of a name records the prior registration: that is
      // the one a failed load has to hand back to the host.
      if (!track.providers.some((p) => p.name === name)) track.providers.push({ name, prior: providers.get(name) });
      providers.set(name, { name, config, extension: ext.name });
      notify("provider_registered", { name, config, extension: ext.name });
    },
    unregisterProvider(name: string) {
      // A load may unregister a provider it never registered here: the undo record
      // is what lets `rollbackLoad` hand that registration back if the load fails.
      if (!track.providers.some((p) => p.name === name)) track.providers.push({ name, prior: providers.get(name) });
      if (!providers.delete(name)) return;
      // The undo record stays: if this load fails later, the registration it
      // replaced still has to come back (see `rollbackLoad`).
      notify("provider_unregistered", { name, extension: ext.name });
    },
    setLabel: (label: string) => {
      ext.label = label;
      notify("set_label", { extension: ext.name, label });
    },
    sendMessage: (message: Json, options?: Json) => notify("send_message", { extension: ext.name, message, options }),
    sendUserMessage: (content: unknown, options?: Json) => notify("send_user_message", { extension: ext.name, content, options }),
    appendEntry: (customType: string, data?: unknown) => notify("append_entry", { extension: ext.name, customType, data }),
    exec: (command: string, args: string[], options?: Json) => execCommand(command, args, String(options?.cwd ?? state.cwd), options),
    getActiveTools: () => (state.activeTools as string[]) ?? [],
    getAllTools: () => (state.allTools as Json[]) ?? [],
    setActiveTools: async (names: string[]) => void (await request("set_active_tools", { names })),
    getCommands: () => exts.flatMap((e) => [...e.commands.values()]),
    setModel: async (model: unknown) => (await request("set_model", { model })) === true,
    getThinkingLevel: () => state.thinkingLevel,
    setThinkingLevel: (level: string, persist?: boolean) => notify("set_thinking_level", { level, persist }),
    // Unimplemented: nothing here knows the core's service tiers, so an empty
    // tier map would be a wrong answer rather than a missing one.
    getServiceTiers: () => unsupportedNow("getServiceTiers"),
    setServiceTier: () => unsupportedNow("setServiceTier"),
    getSessionName: () => state.sessionName,
    setSessionName: async (name: string) => void (await request("set_session_name", { name })),
  };
  return api;
}

// ---------------------------------------------------------------- loading
function discover(dir: string): string[] {
  if (!existsSync(dir)) return [];
  const files: string[] = [];
  for (const name of readdirSync(dir).sort()) {
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isFile() && /\.(ts|js)$/.test(name)) files.push(p);
    else if (st.isDirectory()) {
      const idx = ["index.ts", "index.js"].find((i) => existsSync(join(p, i)));
      if (idx) files.push(join(p, idx));
    }
  }
  return files;
}

async function loadOne(path: string): Promise<{ ext?: Ext; error?: string }> {
  const resolved = resolve(path);
  let mod: { default?: unknown };
  try {
    mod = (await import(resolved)) as { default?: unknown };
  } catch (e) {
    return { error: `Failed to load extension: ${e instanceof Error ? e.message : e}` };
  }
  const factory = typeof mod === "function" ? mod : mod.default;
  if (typeof factory !== "function") return { error: `Extension does not export a valid factory function: ${path}` };
  const base = basename(path).replace(/\.(ts|js)$/, "");
  const ext: Ext = { path: resolved, name: base === "index" ? basename(resolve(path, "..")) : base, handlers: new Map(), tools: new Map(), commands: new Map(), flags: new Map(), linkSchemes: [], rpc: [] };
  const track: LoadTracker = { subs: [], rpc: [], providers: [], flags: new Map() };
  const api = makeApi(ext, track);
  try {
    await (factory as (a: unknown) => unknown)(api);
  } catch (e) {
    // A factory that subscribes and then throws (e.g. on an unsupported
    // registerShortcut) must not leave a live bus listener behind: emits would
    // keep calling a handler whose extension never loaded.
    rollbackLoad(ext, track);
    return { error: `Failed to load extension: ${e instanceof Error ? e.message : e}` };
  }
  return { ext };
}

async function load(p: Json) {
  if (p.ctx) state = { ...state, ...(p.ctx as Json) };
  if (typeof p.cwd === "string") state.cwd = p.cwd;
  // Shared-daemon mode: the daemon's env is process-wide, so per-session
  // handler budget and trace file travel with the load request.
  if (typeof p.timeoutMs === "number" && p.timeoutMs > 0) TIMEOUT_MS = p.timeoutMs;
  if (typeof p.trace === "string") TRACE = p.trace;
  const paths = (p.paths as string[] | undefined) ?? discover(String(p.dir ?? join(homedir(), ".omp/agent/extensions")));
  const loaded: Json[] = [];
  const errors: Json[] = [];
  for (const path of paths) {
    const { ext, error } = await loadOne(path);
    if (error || !ext) {
      errors.push({ path, error });
      continue;
    }
    exts.push(ext);
    loaded.push({
      path,
      name: ext.name,
      handlers: Object.fromEntries([...ext.handlers].map(([k, v]) => [k, v.length])),
      tools: [...ext.tools.keys()],
      commands: [...ext.commands.keys()],
      rpc: ext.rpc.slice(),
      flags: [...ext.flags.values()].map((f) => ({ ...f })),
      linkSchemes: [...ext.linkSchemes],
    });
  }
  // Several extensions may claim one scheme; the first loaded gets its clicks (pi warns once).
  const claims = new Map<string, string>();
  for (const ext of exts)
    for (const scheme of ext.linkSchemes) {
      const owner = claims.get(scheme);
      if (owner === undefined) claims.set(scheme, ext.path);
      else if (owner !== ext.path && !warnedLinkSchemes.has(scheme)) {
        warnedLinkSchemes.add(scheme);
        log("warn", `Link scheme "${scheme}" is claimed by several extensions; the first loaded gets its clicks`, { owner, ignored: ext.path });
      }
    }
  return { loaded, errors };
}

// ---------------------------------------------------------------- emit
const TIMEOUT = Symbol("timeout");
interface HandlerRecord { ext: string; event: string; tool?: string; ms: number; action: string; error?: string }

async function runHandler(ext: Ext, h: Handler, event: Json, timeoutMs: number | null, rec: HandlerRecord[]): Promise<{ value?: unknown; failed?: "timeout" | "error"; message?: string }> {
  const t0 = performance.now();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const base = { ext: ext.name, event: String(event.type), tool: typeof event.toolName === "string" ? event.toolName : undefined };
  try {
    const run = Promise.resolve().then(() => h(event, ctx()));
    // `null` = no budget: the user's own action (a command, a link click) may open a dialog.
    const value =
      timeoutMs === null
        ? await run
        : await Promise.race([
            run,
            new Promise<typeof TIMEOUT>((r) => (timer = setTimeout(() => r(TIMEOUT), timeoutMs))),
          ]);
    const ms = Math.round((performance.now() - t0) * 10) / 10;
    if (value === TIMEOUT) {
      const message = `handler timed out after ${timeoutMs}ms`;
      rec.push({ ...base, ms, action: "error", error: message });
      log("warn", `${ext.path} ${base.event}: ${message}`);
      return { failed: "timeout", message };
    }
    rec.push({ ...base, ms, action: classify(value) });
    return { value };
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    rec.push({ ...base, ms: Math.round((performance.now() - t0) * 10) / 10, action: "error", error: message });
    log("warn", `${ext.path} ${base.event} failed: ${message}`, { stack: e instanceof Error ? e.stack : undefined });
    return { failed: "error", message };
  } finally {
    clearTimeout(timer);
  }
}

// Same classification as the user's lib/hook-trace.ts so traces compare across hosts.
function classify(v: unknown): string {
  const o = v as { block?: unknown; content?: unknown; message?: unknown } | null | undefined;
  if (v === undefined || v === null) return "none";
  if (o?.block === true) return "block";
  if (Array.isArray(o?.content)) return "replace";
  if (o?.message) return "inject";
  return "other";
}

const isNonBlank = (s: unknown): s is string => typeof s === "string" && s.trim().length > 0;
const joinCtx = (xs: string[]) => {
  const u = [...new Set(xs)];
  return u.length ? u.join("\n\n") : undefined;
};

// ---------------------------------------------------------------- message_update deltas
// The core (crates/omp-cli/src/events.rs `ExtUpdateEncoder`) sends the full
// message once per assistant message and then only what moved:
//   full:  {type, message, assistantMessageEvent, delta: {s, id, seq: 0}}
//   delta: {type, assistantMessageEvent, delta: {s, id, seq, n, hdr?, b: [{i, blk?, v, x?}]}}
// `s` names the encoder (one per core-side subscription): several encoders
// may feed one host, so each keeps its own chain, and chain ids are unique
// across encoders, so a delta can never apply to another encoder's message.
// `rebuildMessageUpdate` turns a delta back into the full event before any
// handler runs, so handlers see exactly the shape of a full frame. The chain
// keeps its own copy of the message and never hands it out: every update gets
// a fresh deep copy (strings shared), as a full frame fresh off the wire
// would, so a handler that mutates `event.message` can neither corrupt the
// next rebuild nor leak its edits into later updates. Frames without `delta`
// (older cores, non-assistant messages) pass through untouched.
type UpdateChain = { id: number; seq: number; message: Json };
const updateChains = new Map<string, UpdateChain>();
// Encoders that went away (dropped subscriptions) leave their last chain
// behind; evicting the oldest only costs that encoder a resync.
const MAX_UPDATE_CHAINS = 16;
function setUpdateChain(stream: string, chain: UpdateChain): void {
  updateChains.delete(stream);
  updateChains.set(stream, chain);
  if (updateChains.size > MAX_UPDATE_CHAINS) updateChains.delete(updateChains.keys().next().value as string);
}

class DeltaError extends Error {}
const STREAM_KEY: Record<string, string> = { text: "text", thinking: "thinking", toolCall: "arguments" };

/** Set `key` on `o` as an own data property (also for `__proto__`, as JSON.parse does). */
function setOwn(o: Json, key: string, v: unknown): void {
  if (key === "__proto__") Object.defineProperty(o, key, { value: v, writable: true, enumerable: true, configurable: true });
  else o[key] = v;
}

/** Deep copy of a JSON value: fresh objects and arrays, strings shared. */
function copyJson(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(copyJson);
  if (v === null || typeof v !== "object") return v;
  const out: Json = {};
  for (const [k, x] of Object.entries(v)) setOwn(out, k, copyJson(x));
  return out;
}

/** Inverse of events.rs `value_patch`. */
function applyValuePatch(prev: unknown, p: Json): unknown {
  if ("=" in p) return p["="];
  if ("+" in p) {
    if (typeof prev !== "string" || typeof p["+"] !== "string") throw new DeltaError("string append on a non-string");
    return prev + p["+"];
  }
  if ("o" in p) {
    if (!prev || typeof prev !== "object" || Array.isArray(prev)) throw new DeltaError("object patch on a non-object");
    const out: Json = { ...(prev as Json) };
    for (const [k, sp] of Object.entries(p.o as Json)) setOwn(out, k, applyValuePatch((prev as Json)[k], sp as Json));
    return out;
  }
  if ("a" in p) {
    if (!Array.isArray(prev) || typeof p.n !== "number") throw new DeltaError("array patch on a non-array");
    const out = prev.slice();
    out.length = p.n;
    for (const [i, sp] of p.a as [number, Json][]) out[i] = applyValuePatch(prev[i], sp);
    return out;
  }
  throw new DeltaError("unknown patch");
}

/**
 * Restore `event.message` from a delta frame (and drop the `delta` field).
 * `false` when the frame does not apply to the chain this host holds (gap,
 * unknown id, malformed entry): the chain is dropped and the core resends full.
 */
function rebuildMessageUpdate(event: Json): boolean {
  const d = event.delta as Json | undefined;
  if (d === undefined) return true;
  delete event.delta;
  const stream = String(d.s);
  if (event.message !== undefined) {
    if (typeof d.id !== "number") {
      updateChains.delete(stream);
      return true;
    }
    setUpdateChain(stream, { id: d.id, seq: 0, message: event.message as Json });
    event.message = copyJson(event.message);
    return true;
  }
  const chain = updateChains.get(stream);
  updateChains.delete(stream);
  if (!chain || chain.id !== d.id || chain.seq + 1 !== d.seq) return false;
  try {
    const ame = event.assistantMessageEvent as Json | undefined;
    const prev = (chain.message.content ?? []) as Json[];
    const content = prev.slice();
    for (const e of (d.b ?? []) as Json[]) {
      const i = e.i as number;
      if (!Number.isInteger(i) || i < 0 || i > content.length) throw new DeltaError(`block index ${i}`);
      const old = prev[i] as Json | undefined;
      const base = (e.blk as Json | undefined) ?? old;
      const key = base ? STREAM_KEY[String(base.type)] : undefined;
      if (!base || !key) throw new DeltaError(`block ${i}: unknown base`);
      const oldValue = old && old.type === base.type ? old[key] : undefined;
      let v: unknown;
      switch (e.v) {
        case "k":
          if (oldValue === undefined) throw new DeltaError(`block ${i}: keep without a previous value`);
          v = oldValue;
          break;
        case "a":
          if (typeof oldValue !== "string" || typeof ame?.delta !== "string") throw new DeltaError(`block ${i}: bad append`);
          v = oldValue + ame.delta;
          break;
        case "+":
          if (typeof oldValue !== "string" || typeof e.x !== "string") throw new DeltaError(`block ${i}: bad append`);
          v = oldValue + e.x;
          break;
        case "=":
          v = e.x;
          break;
        case "p":
          v = applyValuePatch(oldValue, e.x as Json);
          break;
        default:
          throw new DeltaError(`block ${i}: unknown op ${String(e.v)}`);
      }
      // `base` carries the streamed key (skeleton or previous block), so the
      // assignment keeps a full frame's key order.
      const block: Json = { ...base };
      block[key] = v;
      content[i] = block;
    }
    if (content.length !== d.n) throw new DeltaError(`content length ${content.length} != ${String(d.n)}`);
    const message: Json = d.hdr !== undefined ? { ...(d.hdr as Json), content } : { ...chain.message, content };
    // Key order of a full frame / omp's event: type, message, assistantMessageEvent.
    const a = event.assistantMessageEvent;
    delete event.assistantMessageEvent;
    event.message = copyJson(message);
    event.assistantMessageEvent = a;
    setUpdateChain(stream, { id: chain.id, seq: d.seq as number, message });
    return true;
  } catch (e) {
    if (!(e instanceof DeltaError)) throw e;
    log("warn", `message_update delta rejected (${e.message}); core will resend the full message`);
    return false;
  }
}

// ---------------------------------------------------------------- link_activate
/**
 * Deliver a click on `url` as `link_activate` to the extension that claimed its scheme (the first
 * loaded one when several did). Returns whether an extension owns the link.
 *
 * Like a command or shortcut, a click is the user's own action and its handler may open a dialog:
 * it runs without the event-handler timeout. A failure is reported, not thrown.
 */
async function runLinkActivate(url: string, rec: HandlerRecord[]): Promise<boolean> {
  const colon = url.indexOf(":");
  if (colon <= 0) return false;
  const scheme = url.slice(0, colon).toLowerCase();
  const owner = exts.find((e) => e.linkSchemes.includes(scheme));
  if (!owner) return false;
  const event: Json = { type: "link_activate", url, scheme };
  for (const h of owner.handlers.get("link_activate") ?? []) await runHandler(owner, h, event, null, rec);
  return true;
}

async function emit(p: Json) {
  if (p.ctx) state = { ...state, ...(p.ctx as Json) };
  // No structuredClone: like the TS runner, every handler for this emit sees
  // this same object (handler-side mutations are visible to later handlers,
  // and the core's copy is independent — it came off the wire).
  const event = p.event as Json;
  const type = String(event.type);
  // Releasing-signal gate (`session_before_shutdown`): the event's `signal` is made here and the core
  // aborts it with `abort_emit` once the close stops waiting; handlers not yet run are skipped and a
  // late result is dropped (ExtensionRunner RELEASING_SIGNAL_EVENTS).
  const abortKey = p.abortId === undefined ? undefined : String(p.abortId);
  const gate = abortKey === undefined ? undefined : new AbortController();
  if (abortKey !== undefined && gate) {
    if (abortedEmits.delete(abortKey)) gate.abort();
    else emitAborts.set(abortKey, gate);
    event.signal = gate.signal;
  }
  if (type === "message_update" && !rebuildMessageUpdate(event)) {
    // Unappliable delta: no handler can see a correct message. Ask the core
    // for a full frame (the next update) instead of running handlers on a
    // corrupt one.
    return { result: { resync: true }, handlers: [], errors: [] };
  }
  // Message boundaries carry no encoder id, and the core starts every message
  // with a full frame (which replaces that encoder's chain), so only the end
  // of the session drops chains.
  if (type === "session_shutdown") updateChains.clear();
  const ame = type === "message_update" ? (event.assistantMessageEvent as Json | undefined) : undefined;
  if (ame?.partialIsMessage === true) {
    // The core did not send `partial` a second time: it equals `message` (same object, as in omp).
    delete ame.partialIsMessage;
    ame.partial = event.message;
  }
  const timeoutMs = Number(p.timeoutMs) || TIMEOUT_MS;
  const rec: HandlerRecord[] = [];
  // `skipExtensions`: extensions (by `ext.name`) whose handlers sit out this one emit
  // (e.g. `context-slim` while the core runs its own CLM context strategy).
  const skip = Array.isArray(p.skipExtensions) ? new Set((p.skipExtensions as unknown[]).map(String)) : undefined;
  const handlersFor = (e: Ext) => (skip?.has(e.name) ? [] : (e.handlers.get(type) ?? []));
  let result: unknown;

  if (type === "session_shutdown") {
    await Promise.all(exts.flatMap((e) => handlersFor(e).map((h) => runHandler(e, h, event, SHUTDOWN_TIMEOUT_MS, rec))));
    for (const t of timers) clearTimeout(t);
    timers.clear();
  } else if (type === "tool_call") {
    const agg = { input: undefined as Json | undefined, ctxs: [] as string[] };
    let control: Json | undefined;
    outer: for (const e of exts)
      for (const h of handlersFor(e)) {
        const r = await runHandler(e, h, event, timeoutMs, rec);
        let hr = r.value as Json | undefined;
        if (r.failed) hr = { block: true, reason: r.failed === "timeout" ? `Extension ${e.path} timed out after ${timeoutMs}ms` : `Extension ${e.path} failed: ${r.message}` };
        if (!hr) continue;
        if (hr.block) {
          result = hr;
          control = undefined;
          agg.input = undefined;
          agg.ctxs = [];
          break outer;
        }
        const { additionalContext, input, ...rest } = hr;
        if (isNonBlank(additionalContext)) agg.ctxs.push(additionalContext);
        if (input !== undefined) agg.input = input as Json;
        control = rest;
      }
    if (result === undefined) {
      const c = joinCtx(agg.ctxs);
      result = c === undefined && agg.input === undefined ? control : { ...control, ...(agg.input ? { input: agg.input } : {}), ...(c ? { additionalContext: c } : {}) };
    }
  } else if (type === "tool_result") {
    const cur = { ...event };
    let modified = false;
    const ctxs: string[] = [];
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, cur, timeoutMs, rec)).value as Json | undefined;
        if (!hr) continue;
        for (const k of ["content", "details", "isError"]) if (hr[k] !== undefined) ((cur[k] = hr[k]), (modified = true));
        if (isNonBlank(hr.additionalContext)) ctxs.push(hr.additionalContext);
      }
    const c = joinCtx(ctxs);
    result = !modified ? (c === undefined ? undefined : { additionalContext: c }) : { content: cur.content, details: cur.details, isError: cur.isError, ...(c ? { additionalContext: c } : {}) };
  } else if (type === "context") {
    let messages = event.messages as unknown[];
    // Only a handler replacement travels back: the core keeps its own list otherwise.
    let modified = false;
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, { type, messages }, timeoutMs, rec)).value as Json | undefined;
        if (hr && Array.isArray(hr.messages)) ((messages = hr.messages), (modified = true));
      }
    result = modified ? { messages } : undefined;
  } else if (type === "before_agent_start") {
    const messages: unknown[] = [];
    let sp = event.systemPrompt as string[];
    let spMod = false;
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, { ...event, systemPrompt: sp }, timeoutMs, rec)).value as Json | undefined;
        if (!hr) continue;
        if (hr.message) messages.push(hr.message);
        if (hr.systemPrompt !== undefined) ((sp = typeof hr.systemPrompt === "string" ? [hr.systemPrompt] : (hr.systemPrompt as string[])), (spMod = true));
      }
    if (messages.length || spMod) result = { messages: messages.length ? messages : undefined, systemPrompt: spMod ? sp : undefined };
  } else if (type === "input") {
    let text = String(event.text);
    let images = event.images;
    const orig = { text, images };
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, { ...event, text, images }, timeoutMs, rec)).value as Json | undefined;
        if (hr?.handled) {
          result = hr;
          break;
        }
        if (hr?.text !== undefined) text = String(hr.text);
        if (hr?.images !== undefined) images = hr.images;
      }
    if (result === undefined) {
      const t: Json = {};
      if (text !== orig.text) t.text = text;
      if (images !== orig.images) t.images = images;
      result = t;
    }
  } else if (type === "assistant_message") {
    // Chained text-only rewrites of the finalized assistant message (ExtensionRunner.emitAssistantMessage).
    const original = ((event.message as Json).content ?? []) as Json[];
    const texts = original.map((b) => (b.type === "text" ? b.text : undefined));
    const rewritten = (i: number) => original[i]?.type === "text" && texts[i] !== original[i].text;
    const current = (): Json[] =>
      original.map((b, i) => {
        if (b.type !== "text" || !rewritten(i)) return b;
        const { textSignature: _stale, ...rest } = b;
        return { ...rest, text: texts[i] };
      });
    const metadata = (b: Json) => {
      const { text: _t, textSignature: _s, ...rest } = b;
      return rest;
    };
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const presented = structuredClone({ ...(event.message as Json), content: current() });
        const hr = (await runHandler(e, h, { type, message: presented }, timeoutMs, rec)).value as Json | undefined;
        if (hr?.content === undefined) continue;
        const expected = structuredClone(current());
        const rep = hr.content as Json[];
        const invalid =
          !Array.isArray(rep) ||
          rep.length !== expected.length ||
          rep.some((b, i) => {
            const prev = expected[i];
            if (!b || typeof b !== "object" || b.type !== prev?.type) return true;
            if (b.type !== "text") return !Bun.deepEquals(b, prev);
            return typeof b.text !== "string" || !Bun.deepEquals(metadata(b), metadata(prev));
          });
        if (invalid) {
          log("warn", `${e.path} assistant_message: content replacement may only change text in existing blocks`);
          continue;
        }
        for (const [i, b] of rep.entries()) if (b.type === "text") texts[i] = b.text;
      }
    if (original.some((_b, i) => rewritten(i))) result = { content: current() };
  } else if (type === "resources_discover") {
    // Every handler contributes; each path is tagged with its extension (ExtensionRunner.emitResourcesDiscover).
    const found = { skillPaths: [] as Json[], promptPaths: [] as Json[], themePaths: [] as Json[] };
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, { type, cwd: event.cwd, reason: event.reason }, timeoutMs, rec)).value as Json | undefined;
        for (const k of ["skillPaths", "promptPaths", "themePaths"] as const) {
          const paths = hr?.[k];
          if (Array.isArray(paths)) found[k].push(...paths.map((path) => ({ path, extensionPath: e.path })));
        }
      }
    result = found;
  } else if (type === "before_provider_request") {
    // Chained payload replacement: each handler sees the previous handler's payload.
    let payload = event.payload;
    let modified = false;
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, { type, payload }, timeoutMs, rec)).value;
        if (hr !== undefined) ((payload = hr), (modified = true));
      }
    result = modified ? { payload } : undefined;
  } else if (type === "cache_warming_decision") {
    // Last handler returning an action wins, starting from the core's proposal.
    let action = event.action;
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, event, timeoutMs, rec)).value as Json | undefined;
        if (hr?.action !== undefined) action = hr.action;
      }
    result = { action };
  } else if (type === "before_subagent_spawn") {
    // `block` short-circuits; otherwise the last model choice wins (with its note).
    let chosen: Json | undefined;
    outer4: for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, event, timeoutMs, rec)).value as Json | undefined;
        if (!hr) continue;
        if (hr.block) {
          chosen = hr;
          break outer4;
        }
        if (hr.model !== undefined) chosen = { model: hr.model, note: hr.note };
      }
    result = chosen;
  } else if (type === "session_move_export" || type === "session_move_import") {
    // senpi `SessionMove*Event.signal`: every handler gets its own AbortSignal, aborted once the
    // handler failed or ran out of budget, so one still writing into `dir` stops instead of racing
    // the move. Failures are reported per handler (the core refuses the move on any).
    for (const e of exts)
      for (const h of handlersFor(e)) {
        const ac = new AbortController();
        const r = await runHandler(e, h, { ...event, signal: ac.signal }, timeoutMs, rec);
        if (r.failed) ac.abort(new Error(r.message ?? r.failed));
      }
  } else if (type === "user_bash" || type === "user_python") {
    // First handler returning a result wins.
    outer3: for (const e of exts)
      for (const h of handlersFor(e)) {
        const hr = (await runHandler(e, h, event, timeoutMs, rec)).value as Json | undefined;
        if (hr) {
          result = hr;
          break outer3;
        }
      }
  } else if (type === "link_activate") {
    // Never a broadcast: a click belongs to the extension that claimed the URL's scheme only (pi
    // `emitLinkActivate`). Clicks normally arrive as the `link_activate` request; this keeps a core
    // that emits it from reaching every extension.
    result = { handled: await runLinkActivate(String(event.url ?? ""), rec) };
  } else {
    // session_before_* (cancel short-circuits, last result wins), session_stop (block short-circuits), everything else: observe.
    const isBefore = type.startsWith("session_before_");
    outer2: for (const e of exts)
      for (const h of handlersFor(e)) {
        if (gate?.signal.aborted) break outer2;
        const hr = (await runHandler(e, h, event, timeoutMs, rec)).value as Json | undefined;
        if (!hr) continue;
        if (isBefore) {
          result = hr;
          if (hr.cancel) break outer2;
        } else if (type === "session_stop") {
          if (hr.decision === "block") {
            result = hr;
            break outer2;
          }
          if (hr.continue === true && (isNonBlank(hr.additionalContext) || isNonBlank(hr.reason))) result ??= hr;
        } else if (type === "session.compacting") result = hr;
      }
  }

  if (abortKey !== undefined) {
    emitAborts.delete(abortKey);
    if (gate?.signal.aborted) result = undefined;
  }
  if (TRACE) {
    try {
      appendFileSync(TRACE, `${JSON.stringify({ ts: Date.now(), event: type, tool: typeof event.toolName === "string" ? event.toolName : undefined, handlers: rec, result: summarize(result) })}\n`);
    } catch {}
  }
  return { result: result ?? null, handlers: rec, errors: rec.filter((r) => r.action === "error").map((r) => ({ extension: r.ext, event: r.event, error: r.error })) };
}

function summarize(v: unknown): unknown {
  if (v === undefined || v === null) return null;
  const o = v as Json;
  const s: Json = {};
  for (const [k, x] of Object.entries(o)) {
    if (Array.isArray(x)) s[k] = `array(${x.length})`;
    else if (typeof x === "string") s[k] = x.length > 120 ? `${x.slice(0, 120)}…(${x.length})` : x;
    else if (x && typeof x === "object") s[k] = "object";
    else s[k] = x;
  }
  return s;
}

// ---------------------------------------------------------------- tools / commands
// omp tool schemas are arktype-backed callables; the wire form is their JSON Schema.
function jsonSchema(schema: unknown): unknown {
  const s = schema as { toJsonSchema?: () => unknown } | undefined;
  return typeof s?.toJsonSchema === "function" ? s.toJsonSchema() : schema;
}

function allTools(): { ext: Ext; tool: Json }[] {
  return exts.flatMap((ext) => [...ext.tools.values()].map((tool) => ({ ext, tool })));
}

// Live tool calls by toolCallId, so a core-side `cancel_tool` can abort them.
const toolAcs = new Map<string, AbortController>();
// Live releasing-signal emits by core `abortId`, and aborts that arrived before their emit started.
const emitAborts = new Map<string, AbortController>();
const abortedEmits = new Set<string>();

async function callTool(p: Json) {
  if (p.ctx) state = { ...state, ...(p.ctx as Json) };
  const found = allTools().findLast((t) => t.tool.name === p.name);
  if (!found) throw rpcError(-32004, `unknown tool: ${p.name}`);
  const ac = new AbortController();
  const id = String(p.toolCallId ?? "");
  toolAcs.set(id, ac);
  try {
    const execute = found.tool.execute as (...a: unknown[]) => Promise<unknown>;
    return (await execute.call(found.tool, id, p.params ?? {}, ac.signal, (partial: unknown) => notify("tool_update", { toolCallId: p.toolCallId, partial }), ctx())) ?? null;
  } finally {
    toolAcs.delete(id);
  }
}

async function callCommand(p: Json) {
  for (const e of exts) {
    const c = e.commands.get(String(p.name));
    if (c) return ((await (c.handler as (a: string, c: unknown) => Promise<unknown>)(String(p.args ?? ""), ctx())) ?? null);
  }
  throw rpcError(-32004, `unknown command: ${p.name}`);
}

// ---------------------------------------------------------------- dispatch
class RpcError extends Error {
  constructor(public code: number, message: string) {
    super(message);
  }
}
const rpcError = (code: number, message: string) => new RpcError(code, message);

let loading: Promise<unknown> = Promise.resolve();

let nowaitTail: Promise<unknown> = Promise.resolve();

/** Wait for `barrier`, unless a fire-and-forget handler is blocked on a core request (see `inNowait`). */
async function awaitBarrier(barrier: Promise<unknown>): Promise<void> {
  let done = false;
  void barrier.then(() => void (done = true));
  while (!done && nowaitBlocked === 0) {
    const { promise: woken, resolve: wake } = Promise.withResolvers<void>();
    nowaitWakers.add(wake);
    try {
      await Promise.race([barrier, woken]);
    } finally {
      nowaitWakers.delete(wake);
    }
  }
}

async function dispatch(method: string, p: Json): Promise<unknown> {
  if (method === "emit" && p.nowait === true) {
    // Reserved synchronously (before any await) so arrival order == execution order.
    const prev = nowaitTail;
    const run = (async () => {
      await prev;
      await loading;
      return inNowait.run(true, () => emit(p));
    })();
    nowaitTail = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }
  if (method === "load") {
    // Loads are serialized, and every other request waits for them (registration order is deterministic).
    const run = loading.then(() => load(p));
    loading = run.catch(() => undefined);
    return run;
  }
  // Awaited emit / call_tool: run after every queued fire-and-forget emit.
  const barrier = method === "emit" || method === "call_tool" ? nowaitTail : undefined;
  await loading;
  if (barrier) await awaitBarrier(barrier);
  switch (method) {
    case "emit":
      return emit(p);
    case "list_tools":
      return allTools().map(({ ext, tool }) => ({ name: tool.name, label: tool.label, description: tool.description, parameters: jsonSchema(tool.parameters), approval: tool.approval, extension: ext.name }));
    case "call_tool":
      return callTool(p);
    case "cancel_tool": {
      // Additive: cores that predate `cancel_tool` never call this.
      const ac = toolAcs.get(String(p.toolCallId ?? ""));
      if (ac) ac.abort();
      return true;
    }
    case "abort_emit": {
      // Additive: cores that predate `abort_emit` never call this.
      const key = String(p.abortId ?? "");
      const ac = emitAborts.get(key);
      if (ac) ac.abort();
      else abortedEmits.add(key);
      return true;
    }
    case "list_commands":
      return exts.flatMap((e) => [...e.commands.values()].map((c) => ({ name: c.name, description: c.description, extension: e.name })));
    case "call_command":
      return callCommand(p);
    // A click on a link whose scheme an extension claimed (`registerLinkScheme`) goes to that
    // extension alone (pi `emitLinkActivate`): never to a broadcast emit.
    case "link_activate": {
      const rec: HandlerRecord[] = [];
      return { handled: await runLinkActivate(String(p.url ?? ""), rec) };
    }
    // `extension_request`: answer a named query an extension registered via `api.rpc.handle`.
    case "rpc_request": {
      const name = String(p.name ?? "").trim();
      if (!name) throw rpcError(-32602, "Extension RPC request name cannot be empty");
      const h = rpcHandlers.get(name);
      if (!h) throw rpcError(-32601, `Unknown extension RPC request: ${name}`);
      return (await h.fn(p.data)) ?? null;
    }
    case "list_rpc":
      return [...rpcHandlers].map(([name, h]) => ({ name, extension: h.ext }));
    case "set_ctx":
      state = { ...state, ...(p.ctx as Json) };
      return true;
    // Additive introspection/update methods; cores that predate them never call these.
    case "list_flags":
      return exts.flatMap((e) => [...e.flags.values()].map((f) => ({ ...f, extension: e.name, value: flagValues.get(f.name as string) })));
    case "set_flag_value":
      flagValues.set(String(p.name), p.value as boolean | string);
      // Owns the name now: a load that fails afterwards must not undo this.
      flagWrites.set(String(p.name), ++writeRev);
      return true;
    case "list_providers":
      return [...providers.values()].map((pr) => ({ ...pr }));
    case "bus_emit":
      events.emit(String(p.channel), p.data);
      return true;
    case "ephemeral_delta": {
      // A running side turn's next text delta; the core awaits this, so delivery order is the
      // provider's order and a throwing `onTextDelta` rejects the turn.
      const deliver = ephemeralDeltas.get(Number(p.streamId));
      if (!deliver) throw rpcError(-32602, `ephemeral_delta: no running side turn ${p.streamId}`);
      await deliver(String(p.delta ?? ""));
      return true;
    }
    case "shutdown":
      // In a shared-daemon Worker, process.exit would kill every session;
      // close the session transport instead (daemon terminates this Worker).
      setTimeout(() => (closeSession ? closeSession() : process.exit(0)), 20);
      return true;
    default:
      throw rpcError(-32601, `method not found: ${method}`);
  }
}

async function onMessage(line: string) {
  let msg: Json;
  try {
    msg = JSON.parse(line) as Json;
  } catch {
    out({ id: null, error: { code: -32700, message: "parse error" } });
    return;
  }
  if (msg.method === undefined) {
    const w = pending.get(msg.id as number);
    if (!w) return;
    pending.delete(msg.id as number);
    if (msg.error) w.reject(new Error(String((msg.error as Json).message)));
    else w.resolve(msg.result);
    return;
  }
  const id = msg.id as number | string | undefined;
  try {
    const result = await dispatch(String(msg.method), (msg.params as Json) ?? {});
    if (id !== undefined) out({ id, result });
  } catch (e) {
    if (id === undefined) return;
    const code = e instanceof RpcError ? e.code : -32000;
    out({ id, error: { code, message: e instanceof Error ? e.message : String(e) } });
  }
}

process.on("uncaughtException", (e) => log("error", `uncaughtException: ${e instanceof Error ? (e.stack ?? e.message) : e}`));
process.on("unhandledRejection", (e) => log("error", `unhandledRejection: ${e instanceof Error ? (e.stack ?? e.message) : e}`));

/** Byte-stream -> line-message transport used by both entry points. */
export interface SessionTransport {
  /** Send one newline-terminated JSON-RPC line to the core. */
  send(line: string): void;
  /** Called with each raw incoming chunk (newline framing happens here). */
  onData(cb: (chunk: string) => void): void;
  /** Input ended / core disconnected: the session is over. */
  onClose?(cb: () => void): void;
  /** Host-initiated session end (shutdown RPC): close this session only. */
  close?(): void;
}

/** The session's cwd as last pushed by the core (shared-daemon Workers route
 *  `process.cwd()` and child-process defaults through it). */
export function sessionCwd(): string {
  return String(state.cwd);
}

/**
 * Start serving one session on `t`. Each call creates no explicit state -- the
 * module-level state above belongs to the importing isolate, so a fresh Worker
 * per session gets isolation equivalent to a separate process.
 */
export function startSession(t: SessionTransport): void {
  emitLine = t.send;
  closeSession = t.close ?? null;
  let buf = "";
  t.onData((chunk: string) => {
    buf += chunk;
    let i: number;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (line) void onMessage(line);
    }
  });
  t.onClose?.(() => process.exit(0));
}

if (import.meta.main) {
  // stdio entry point (one bun process per session; the default).
  process.stdin.setEncoding("utf8");
  startSession({
    send: (line) => process.stdout.write(line),
    onData: (cb) => process.stdin.on("data", cb),
    onClose: (cb) => process.stdin.on("end", cb),
  });
}
