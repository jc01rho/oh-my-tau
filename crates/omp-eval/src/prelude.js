// I/O and tool prelude for the JS eval runner. Port of the I/O and tool part of
// omp's eval/js/shared/prelude.txt. Every helper talks to the host only through
// `globalThis.__ompHostCall(op, args)` (waits for the reply) and
// `globalThis.__ompHostNotify(op, args)` (fire-and-forget status). Agent,
// completion, judge, workpool and budget helpers live in prelude_agents.js.
//
// The whole file runs inside a function so top-level bindings never collide
// with user cells, and it is safe to re-run: globals are simply reassigned.
(() => {
  const isNil = value => value === undefined || value === null;
  const isPlainObject = value => value !== null && typeof value === "object" && !Array.isArray(value);

  // Accepts either one trailing options object or positional optional args,
  // mirroring the TS prelude so `read(path, { offset })` and `read(path, 1, 10)`
  // behave the same way.
  const positionalOptions = (name, args, keys, example) => {
    for (let index = keys.length; index < args.length; index++) {
      if (!isNil(args[index])) {
        throw new TypeError(
          `${name}() accepts at most ${keys.length} positional optional args; got ${args.length}. Pass ${name}(..., ${example}) for named options.`,
        );
      }
    }
    const options = {};
    for (let index = 0; index < keys.length && index < args.length; index++) {
      const value = args[index];
      if (!isNil(value)) options[keys[index]] = value;
    }
    return options;
  };
  const optionsArg = (name, value, rest, keys, example) => {
    if (isNil(value)) return positionalOptions(name, [value, ...rest], keys, example);
    if (isPlainObject(value)) {
      if (rest.some(arg => !isNil(arg))) {
        throw new TypeError(
          `${name}() takes either a single trailing options object like ${example} or positional optional args; do not mix both forms.`,
        );
      }
      return value;
    }
    if (typeof value === "object") {
      const kind = Array.isArray(value) ? "an array" : (value.constructor?.name ?? "object");
      throw new TypeError(
        `${name}() options must be a plain object like ${example}, null/undefined, or positional optional args, not ${kind}.`,
      );
    }
    return positionalOptions(name, [value, ...rest], keys, example);
  };

  const hostCall = (op, args) => globalThis.__ompHostCall(op, args);
  const hostNotify = (op, args) => globalThis.__ompHostNotify(op, args);

  // Structured-clone-free projection for display(): values that JSON cannot
  // carry are rendered as strings so the notify frame always serializes.

  // TS split (prelude.txt): plain paths are read/written in the worker itself;
  // only URL paths (`scheme://…`: artifact://, agent://, omp://, local://…) go
  // through the host `read` tool, with the line window encoded as a selector.
  const hasScheme = path => typeof path === "string" && /^[A-Za-z][A-Za-z0-9+.-]*:\/\//.test(path);
  const withReadLineSelector = (path, options) => {
    const offset = typeof options.offset === "number" ? options.offset : 1;
    const limit = typeof options.limit === "number" ? options.limit : undefined;
    if (offset <= 1 && limit === undefined) return path;
    if (limit !== undefined && limit <= 0) return null;
    const start = Math.max(1, offset);
    if (limit === undefined) return `${path}:${start}-`;
    return `${path}:${start}-${start + limit - 1}`;
  };
  const resolveCellPath = rawPath => {
    const p = String(rawPath);
    return p.startsWith("/") ? p : `${process.cwd()}/${p}`;
  };
  const readToolText = async path => {
    const res = await hostCall("read", { path });
    return res && typeof res === "object" && "text" in res ? res.text : res;
  };

  const read = async (path, opts, ...rest) => {
    const options = optionsArg("read", opts, rest, ["offset", "limit"], "{ offset, limit }");
    if (hasScheme(path)) {
      const toolPath = withReadLineSelector(path, options);
      return toolPath === null ? "" : readToolText(toolPath);
    }
    let text = await Bun.file(resolveCellPath(path)).text();
    const offset = typeof options.offset === "number" ? options.offset : 1;
    const limit = typeof options.limit === "number" ? options.limit : undefined;
    if (limit !== undefined && limit <= 0) return "";
    if (offset > 1 || limit !== undefined) {
      const lines = text.split(/\r?\n/);
      const start = Math.max(0, offset - 1);
      const end = limit !== undefined ? start + limit : lines.length;
      text = lines.slice(start, end).join("\n");
    }
    return text;
  };

  const write = async (path, data) => {
    const isBlob = typeof Blob !== "undefined" && data instanceof Blob;
    if (typeof data !== "string" && !isBlob && !(data instanceof ArrayBuffer) && !ArrayBuffer.isView(data)) {
      throw new TypeError("write() expects string, Blob, ArrayBuffer, or TypedArray data");
    }
    const filePath = resolveCellPath(path);
    await Bun.write(filePath, data);
    return filePath;
  };

  // env(), env(key), env(key, value): the worker's own process environment.
  const env = (key, value) => {
    if (isNil(key)) {
      return Object.fromEntries(Object.entries(process.env).sort(([a], [b]) => a.localeCompare(b)));
    }
    if (value !== undefined) {
      process.env[String(key)] = String(value);
      return value;
    }
    return process.env[String(key)];
  };

  // tool(fn, { name, description, parameters }) registers a JS-side tool;
  // tool.<name>(args) calls a host tool through the tool bridge.
  const toolRegistry = (globalThis.__omp_tools__ ??= new Map());
  const isValidToolName = name => typeof name === "string" && /^[A-Za-z_][A-Za-z0-9_]{0,63}$/.test(name);
  const registerTool = (fn, opts = {}) => {
    if (typeof fn !== "function") throw new TypeError("tool() expects a function");
    if (!isPlainObject(opts)) throw new TypeError("tool() options must be a plain object");
    const name = opts.name ?? fn.name;
    if (!isValidToolName(name)) {
      throw new TypeError(`invalid tool name ${JSON.stringify(name)}`);
    }
    const description = opts.description ?? `JS tool ${name}`;
    const parameters = opts.parameters ?? { type: "object", properties: {}, additionalProperties: true };
    toolRegistry.set(name, { name, fn, description, parameters });
    hostNotify("tool_define", { name, params: Object.keys(parameters.properties ?? {}) });
    return fn;
  };
  const tool = new Proxy(registerTool, {
    apply(_target, _thisArg, args) {
      return registerTool(args[0], args[1]);
    },
    get(_target, prop) {
      if (prop === "defined") return () => [...toolRegistry.keys()];
      if (prop === "undefine") return name => toolRegistry.delete(name);
      if (typeof prop !== "string") return undefined;
      return async args => hostCall(`tool.${prop}`, args ?? {});
    },
  });

  // output(id..., { offset, limit }): read agent:// results. One id yields the
  // value itself, several ids yield an array in input order.
  const output = async (...args) => {
    let opts = {};
    let ids = args;
    if (args.length > 0) {
      const last = args.at(-1);
      if (last && typeof last === "object" && !Array.isArray(last)) {
        opts = last;
        ids = args.slice(0, -1);
      }
    }
    const values = await Promise.all(ids.map(id => tool.read({ path: `agent://${id}`, ...opts })));
    return values.length === 1 ? values[0] : values;
  };

  const log = message => hostNotify("log", { message: String(message) });

  const phase = title => {
    globalThis.__omp_phase__ = String(title);
    hostNotify("phase", { title: String(title) });
  };

  const display = (...values) => globalThis.__ompDisplay(...values);

  // -------------------------------------------------------------------------
  // Shell, agent, completion, judgment, workpool and budget host-call
  // families. Op names and payload shapes are verbatim from shell-bridge.ts,
  // agent-bridge.ts, completion-bridge.ts, judgment-bridge.ts,
  // judgment-batch-bridge.ts, workpool-bridge.ts, handle-bridge.ts and
  // budget-bridge.ts; every round-trip goes through `__ompHostCall`.
  // -------------------------------------------------------------------------

  class ShellError extends Error {
    constructor(message, output, exitCode) {
      super(message);
      this.name = "ShellError";
      this.output = output;
      this.exitCode = exitCode;
    }
  }

  // Run one command on the embedded bash engine; rejects with ShellError on a
  // non-zero exit or timeout.
  const sh = async (command, opts, ...rest) => {
    if (typeof command !== "string") {
      throw new TypeError(`sh() expects a command string (got ${typeof command})`);
    }
    const options = optionsArg("sh", opts, rest, ["cwd", "timeout"], "{ cwd, timeout }");
    const args = { command };
    if (!isNil(options.cwd)) args.cwd = String(options.cwd);
    if (!isNil(options.timeout)) args.timeout = Number(options.timeout);
    const result = await hostCall("__shell__", args);
    const output = typeof result?.output === "string" ? result.output : "";
    const exitCode = result?.exitCode;
    if (exitCode === 0) return output;
    const status = result?.timedOut
      ? `timed out after ${options.timeout}s`
      : `exited with code ${exitCode}`;
    const detail = output ? `\n${output.replace(/\s+$/, "")}` : "";
    throw new ShellError(`$ ${command}: ${status}${detail}`, output, exitCode ?? null);
  };

  const HANDLE_UNSET = Symbol("handle-unset");
  const timeoutError = message => {
    const error = new Error(message);
    error.name = "TimeoutError";
    return error;
  };

  class Handle {
    // Duck-typed so handles built by the agent prelude (which loads after this
    // one and rebinds the concrete classes) still satisfy `x instanceof Handle`.
    static [Symbol.hasInstance](value) {
      return (
        value !== null &&
        typeof value === "object" &&
        typeof value.kind === "string" &&
        "id" in value &&
        typeof value.wait === "function"
      );
    }

    constructor(kind, id, schema) {
      this.kind = kind;
      this.id = id;
      this._schema = schema;
      this._result = HANDLE_UNSET;
    }

    async status() {
      const snapshot = await hostCall("__status__", {
        item: { kind: this.kind, id: this.id },
      });
      return snapshot?.status ?? "failed";
    }

    async done() {
      return (await this.status()) !== "running";
    }

    async wait(opts, ...rest) {
      if (this._result !== HANDLE_UNSET) return this._result;
      const options = optionsArg("Handle.wait", opts, rest, ["timeout"], "{ timeout }");
      return (await wait([this], options))[0];
    }

    async cancel() {
      const result = await hostCall("__cancel__", {
        item: { kind: this.kind, id: this.id },
      });
      return Boolean(result?.cancelled);
    }
  }

  class AgentHandle extends Handle {
    constructor(id, agentName, schema) {
      super("agent", id, schema);
      this.agent = agentName;
      this.handle = `agent://${id}`;
    }

    toString() {
      return `<agent ${this.id} (${this.agent})>`;
    }

    async send(message) {
      return await hostCall("write", {
        path: this.handle,
        content: String(message),
        i: "Messaging agent",
      });
    }

    async output(opts = {}) {
      return await globalThis.output(this.id, opts);
    }
  }

  class CompletionHandle extends Handle {
    constructor(id, schema) {
      super("completion", id, schema);
    }

    toString() {
      return `<completion ${this.id}>`;
    }
  }

  // Immediate-return wrapper for the async factories: await it for the handle,
  // or call handle methods directly and they await resolution internally.
  class PendingHandle {
    constructor(promise) {
      this._promise = promise;
    }

    then(onFulfilled, onRejected) {
      return this._promise.then(onFulfilled, onRejected);
    }

    catch(onRejected) {
      return this._promise.catch(onRejected);
    }

    finally(onFinally) {
      return this._promise.finally(onFinally);
    }

    async status() {
      return (await this._promise).status();
    }

    async done() {
      return (await this._promise).done();
    }

    async wait(...args) {
      return (await this._promise).wait(...args);
    }

    async cancel() {
      return (await this._promise).cancel();
    }

    async send(message) {
      return (await this._promise).send(message);
    }

    async output(options) {
      return (await this._promise).output(options);
    }
  }

  const resolveHandleSnapshot = (handle, snapshot) => {
    const status = snapshot?.status ?? "failed";
    if (status === "running") throw timeoutError(`${handle.kind} handle ${handle.id} is still running`);
    if (status === "failed" || status === "cancelled") {
      throw new Error(snapshot?.error || `${handle.kind} handle ${handle.id} failed`);
    }
    const value = Object.prototype.hasOwnProperty.call(snapshot ?? {}, "data")
      ? snapshot.data
      : handle._schema !== undefined
        ? JSON.parse(snapshot?.text ?? "")
        : (snapshot?.text ?? "");
    handle._result = value;
    return value;
  };

  const wait = async (handles, opts, ...rest) => {
    const { timeout, raiseErrors = true } = optionsArg(
      "wait",
      opts,
      rest,
      ["timeout", "raiseErrors"],
      "{ timeout, raiseErrors }",
    );
    const isHandleLike = value => value instanceof Handle || value instanceof PendingHandle;
    const raw = isHandleLike(handles) ? [handles] : Array.from(handles ?? []);
    const items = await Promise.all(raw);
    for (const handle of items) {
      if (!(handle instanceof Handle)) throw new TypeError("wait() expects agent or completion handles");
    }
    const results = new Array(items.length);
    const pending = [];
    const pendingIndexes = [];
    for (let index = 0; index < items.length; index++) {
      const handle = items[index];
      if (handle._result === HANDLE_UNSET) {
        pending.push({ kind: handle.kind, id: handle.id });
        pendingIndexes.push(index);
      } else {
        results[index] = handle._result;
      }
    }
    if (pending.length > 0) {
      const args = { items: pending };
      if (timeout !== undefined) args.timeoutMs = Math.max(0, Number(timeout) * 1000);
      const response = await hostCall("__wait__", args);
      const snapshots = Array.isArray(response?.items) ? response.items : [];
      if (snapshots.length !== pending.length) throw new Error("wait() returned an incomplete handle result");
      for (let offset = 0; offset < pendingIndexes.length; offset++) {
        const index = pendingIndexes[offset];
        try {
          results[index] = resolveHandleSnapshot(items[index], snapshots[offset]);
        } catch (error) {
          if (error?.name === "TimeoutError") throw error;
          results[index] = error instanceof Error ? error : new Error(String(error));
        }
      }
    }
    if (raiseErrors) {
      const failure = results.find(value => value instanceof Error);
      if (failure) throw failure;
    }
    return results;
  };

  const completion = (prompt, opts, ...rest) => {
    const promise = (async () => {
      const options = optionsArg(
        "completion",
        opts,
        rest,
        ["model", "system", "schema"],
        "{ model, system, schema }",
      );
      const result = await hostCall("__completion__", { prompt, ...options });
      if (!result || typeof result.id !== "string") throw new Error("completion() did not return a handle");
      return new CompletionHandle(result.id, options.schema);
    })();
    return new PendingHandle(promise);
  };

  const checkQuestions = questions => {
    if (questions === null || typeof questions !== "object" || Array.isArray(questions)) {
      throw new TypeError("judge(state, questions) expects questions as an object keyed by id");
    }
  };

  const judge = async (state, questions, opts, ...rest) => {
    checkQuestions(questions);
    const options = optionsArg("judge", opts, rest, ["images"], "{ images }");
    const args = { state, questions };
    if (!isNil(options.images)) args.images = options.images;
    const result = await hostCall("__judge__", args);
    if (!result || !isPlainObject(result.answers)) throw new Error("judge() did not return answers");
    return result.answers;
  };

  class JudgmentItem {
    constructor(item) {
      this.key = item.key;
      this.answers = item.answers;
      this.error = item.error;
      this.model = item.model;
    }

    get ok() {
      return this.error === undefined;
    }

    toString() {
      return this.error === undefined
        ? `<judgment ${JSON.stringify(this.key)} ${JSON.stringify(this.answers)}>`
        : `<judgment ${JSON.stringify(this.key)} error=${JSON.stringify(this.error)}>`;
    }
  }

  class JudgmentBatch {
    constructor(id, total, intent) {
      this.id = id;
      this.total = total;
      this.intent = intent;
    }

    async _call(op, args = {}) {
      return await hostCall("__judge_batch__", { op, id: this.id, ...args });
    }

    async status() {
      return await this._call("status");
    }

    async drain(opts, ...rest) {
      const { timeout } = optionsArg("JudgmentBatch.drain", opts, rest, ["timeout"], "{ timeout }");
      const args = timeout === undefined ? {} : { timeoutMs: Math.max(0, Number(timeout) * 1000) };
      const result = await this._call("drain", args);
      const items = Array.isArray(result?.items) ? result.items : [];
      return items.map(item => [item.key, new JudgmentItem(item)]);
    }

    async *drainIter(opts, ...rest) {
      const { timeout } = optionsArg("JudgmentBatch.drainIter", opts, rest, ["timeout"], "{ timeout }");
      const deadline = timeout === undefined ? undefined : Date.now() + Number(timeout) * 1000;
      while (true) {
        const remaining =
          deadline === undefined ? undefined : Math.max(0, (deadline - Date.now()) / 1000);
        const items = await this.drain({ timeout: remaining });
        if (items.length === 0) return;
        yield* items;
      }
    }

    async results() {
      return (await this._call("results"))?.results ?? {};
    }

    async failed() {
      return (await this._call("failed"))?.failed ?? {};
    }

    async cancel() {
      return Boolean((await this._call("cancel"))?.cancelled);
    }

    async close() {
      await this._call("close");
    }

    toString() {
      return `<judgeBatch ${this.id} total=${this.total}>`;
    }
  }

  const judgeBatchFrom = result => {
    if (!result || typeof result.id !== "string") throw new Error("judge_batch() did not return a batch");
    return new JudgmentBatch(result.id, Number(result.total) || 0, result.intent ?? "Judging");
  };

  const judge_batch = async (states, questions, opts, ...rest) => {
    checkQuestions(questions);
    const options = optionsArg(
      "judge_batch",
      opts,
      rest,
      ["images", "concurrency", "retries", "minOk", "intent"],
      "{ intent, concurrency, retries, minOk, images }",
    );
    if (
      options.intent !== undefined &&
      (typeof options.intent !== "string" || options.intent.trim().length === 0)
    ) {
      throw new TypeError("judge_batch() intent must be a non-empty string");
    }
    let items;
    if (Array.isArray(states)) items = states.map((state, key) => ({ key, state }));
    else if (isPlainObject(states)) items = Object.entries(states).map(([key, state]) => ({ key, state }));
    else throw new TypeError("judge_batch(states, questions) expects an object or array");
    const { images, ...rest2 } = options;
    if (images !== undefined) {
      if (!isPlainObject(images)) {
        throw new TypeError("judge_batch() images must be an object keyed by input key");
      }
      const keys = new Set(items.map(item => item.key));
      for (const key of Object.keys(images)) {
        const typed = /^-?\d+$/.test(key) ? Number(key) : key;
        if (!keys.has(typed) && !keys.has(key)) {
          throw new Error(`images contains unknown input key ${JSON.stringify(key)}`);
        }
      }
      items = items.map(item =>
        Object.prototype.hasOwnProperty.call(images, String(item.key))
          ? { ...item, images: images[String(item.key)] }
          : item,
      );
    }
    return judgeBatchFrom(
      await hostCall("__judge_batch__", { op: "create", items, questions, ...rest2 }),
    );
  };

  judge_batch.attach = async id =>
    judgeBatchFrom(await hostCall("__judge_batch__", { op: "attach", id: String(id) }));

  const agent = (prompt, opts, ...rest) => {
    const promise = (async () => {
      const options = optionsArg(
        "agent",
        opts,
        rest,
        ["agent", "label", "schema", "isolated", "apply", "merge", "schemaMode", "tools", "model"],
        "{ agent, label, schema, schemaMode, isolated, apply, merge, tools, model }",
      );
      const result = await hostCall("__agent__", { prompt, ...options });
      if (!result || typeof result.id !== "string") throw new Error("agent() did not return a handle");
      return new AgentHandle(result.id, result.agent, options.schema);
    })();
    return new PendingHandle(promise);
  };

  class WorkPool {
    constructor(name, agentName, limit) {
      this.name = name;
      this.agent = agentName;
      this.limit = limit;
    }

    async push(...items) {
      if (!items.every(item => typeof item === "string")) {
        throw new TypeError("WorkPool.push() expects string items");
      }
      const result = await hostCall("__workpool__", { op: "push", name: this.name, items });
      return result?.ids ?? [];
    }

    async status() {
      return await hostCall("__workpool__", { op: "status", name: this.name });
    }

    async peek() {
      return await hostCall("__workpool__", { op: "peek", name: this.name });
    }

    async close() {
      return await hostCall("__workpool__", { op: "close", name: this.name });
    }

    toString() {
      return `<workpool ${this.name} (${this.agent}) ${this.limit} agents>`;
    }
  }

  const workpool = async (agentName, opts, ...rest) => {
    if (isPlainObject(agentName) && isNil(opts) && rest.every(isNil)) {
      opts = agentName;
      agentName = undefined;
    }
    const options = optionsArg(
      "workpool",
      opts,
      rest,
      ["name", "context", "tools", "model"],
      "{ name, context, tools, model }",
    );
    const result = await hostCall("__workpool__", {
      op: "create",
      ...(agentName === undefined ? {} : { agent: agentName }),
      ...options,
    });
    if (!result || typeof result.name !== "string") throw new Error("workpool() did not return a pool");
    return new WorkPool(result.name, result.agent, result.limit);
  };

  const budgetSnap = async () => {
    const snapshot = await hostCall("__budget__", {});
    return snapshot && typeof snapshot === "object" ? snapshot : {};
  };

  const budget = {
    total: async () => (await budgetSnap()).total ?? null,
    spent: async () => Number((await budgetSnap()).spent ?? 0),
    remaining: async () => {
      const snapshot = await budgetSnap();
      return snapshot.total == null
        ? Infinity
        : Math.max(0, Number(snapshot.total) - Number(snapshot.spent ?? 0));
    },
    hard: async () => Boolean((await budgetSnap()).hard),
  };

  Object.assign(globalThis, {
    display,
    tool,
    output,
    log,
    phase,
    read,
    write,
    env,
    sh,
    ShellError,
    Handle,
    AgentHandle,
    CompletionHandle,
    PendingHandle,
    wait,
    completion,
    judge,
    judge_batch,
    judgeBatch: judge_batch,
    JudgmentItem,
    JudgmentBatch,
    agent,
    workpool,
    WorkPool,
    budget,
  });
})();
