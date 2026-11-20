// Extract name / first description line / JSON parameter schema for every upstream
// omp built-in tool (29 visible + 3 hidden = 32, per src/tools/builtin-names.ts) from
// pi-coding-agent and write them to tests-tools/ts_tools.json.
//
// Run: bun tests-tools/ts_schemas.ts
//
// The omp TS sources live outside this repo: set OMP_TS_SRC to the
// pi-coding-agent package root (default: vendor-src/oh-my-pi/packages/coding-agent).
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const TS_SRC = process.env.OMP_TS_SRC ?? join(dirname(fileURLToPath(import.meta.url)), "../vendor-src/oh-my-pi/packages/coding-agent");
const load = (rel: string) => import(join(TS_SRC, rel));
const { BUILTIN_TOOL_NAMES, HIDDEN_TOOL_NAMES } = await load("src/tools/builtin-names.ts");
const { ReadTool } = await load("src/tools/read.ts");
const { BashTool } = await load("src/tools/bash.ts");
const { EditTool } = await load("src/edit/index.ts");
const { GlobTool } = await load("src/tools/glob.ts");
const { GrepTool } = await load("src/tools/grep.ts");
const { FindTool } = await load("src/tools/jfind/index.ts");
const { WriteTool } = await load("src/tools/write.ts");
const { TaskTool } = await load("src/task/index.ts");
const { EvalTool } = await load("src/tools/eval.ts");
const { AstGrepTool } = await load("src/tools/ast-grep.ts");
const { AstEditTool } = await load("src/tools/ast-edit.ts");
const { TodoTool } = await load("src/tools/todo.ts");
const { AskTool } = await load("src/tools/ask.ts");
const { WebSearchTool } = await load("src/web/search");
const { WaitTool } = await load("src/tools/wait");
const { ThinkTool } = await load("src/tools/think");
const { YieldTool } = await load("src/tools/yield");
const { GoalTool } = await load("src/goals/tools/goal-tool");
const { SecurityScanTool } = await load("src/tools/security-scan.ts");
const { DebugTool } = await load("src/tools/debug.ts");
const { GithubTool } = await load("src/tools/gh.ts");
const { LspTool } = await load("src/lsp/index.ts");
const { CheckpointTool, RewindTool } = await load("src/tools/checkpoint.ts");
const { ContextNotesTool, NewContextTool } = await load("src/tools/context-notes.ts");
const { MemoryEditTool } = await load("src/tools/memory-edit.ts");
const { MemoryRetainTool } = await load("src/tools/memory-retain.ts");
const { MemoryRecallTool } = await load("src/tools/memory-recall.ts");
const { MemoryReflectTool } = await load("src/tools/memory-reflect.ts");
const { LearnTool } = await load("src/tools/learn.ts");
const { ManageSkillTool } = await load("src/tools/manage-skill.ts");

// Minimal ToolSession stub: tools only read config/description state here, never
// execute. Declared structurally so no omp types need to be imported.
interface StubSession {
	// Must survive `cfg.get(scope)` in src/config/registry.ts (reads revision/valueCache).
	settings: { revision: number; valueCache: Record<string, unknown> };
	isToolActive: () => boolean;
	getActiveModel: () => undefined;
	rawValue: (key: unknown) => undefined;
	getSessionSpawns: () => string;
	deviceOnlyWrite: boolean;
	pendingFullWriteDescription: boolean;
}

// Operation-layer settings view. `grantAuthority()` is read by perCallModelEnabled()
// (task/eval descriptions) and must return a layers object, so it returns itself.
function stubSettings(): Record<string, unknown> {
	const settings: Record<string, unknown> = {
		revision: 0,
		valueCache: {},
		rawValue: () => undefined,
		warnState: { invalid: new Map(), items: new Map() },
		getProvenance: () => undefined,
		getShellConfig: () => ({ env: {} }),
	};
	settings.grantAuthority = () => settings;
	return settings;
}

function stubSession(): StubSession {
	return {
		settings: stubSettings() as StubSession["settings"],
		isToolActive: () => true,
		getActiveModel: () => undefined,
		rawValue: () => undefined,
		getSessionSpawns: () => "*",
		deviceOnlyWrite: false,
		pendingFullWriteDescription: false,
	};
}

interface ExtractedTool {
	name: string;
	description_first_line: string;
	description: string;
	parameters: unknown;
}

interface ToolLike {
	description: string;
	parameters: unknown;
}

// One entry per upstream built-in. `name` is the canonical (wire) name from
// builtin-names.ts; `make` constructs the tool without going through createIf(),
// whose settings/availability gates would drop most gated tools.
const EXTRACTORS: Record<string, (s: StubSession) => ToolLike | Promise<ToolLike>> = {
	read: s => new ReadTool(s),
	bash: s => new BashTool(s),
	edit: s => new EditTool(s, "replace"), // pin Replace: omp-rs default mode for glm-style models
	ast_grep: s => new AstGrepTool(s as any),
	ast_edit: s => new AstEditTool(s as any),
	ask: s => new AskTool(s),
	debug: s => new DebugTool(s as any),
	eval: s => new EvalTool(permissiveStub()),
	github: s => new GithubTool(s as any),
	glob: s => new GlobTool(s, { rootPathAlias: true }),
	grep: s => new GrepTool(s),
	find: s => new FindTool(s),
	lsp: s => new LspTool(s as any),
	checkpoint: s => new CheckpointTool(s as any),
	rewind: s => new RewindTool(s as any),
	context_notes: s => new ContextNotesTool(s as any),
	new_context: s => new NewContextTool(s as any),
	security_scan: s => new SecurityScanTool(s as any),
	// task needs a session with many optional hooks (task discovery, spawn
	// policy, eval backend config); a permissive proxy stub satisfies every
	// optional lookup and yields the protocol-default render.
	task: () => (TaskTool as any).create(permissiveStub()),
	wait: s => new WaitTool(s as any),
	todo: s => new TodoTool(s),
	web_search: s => new WebSearchTool(s),
	write: s => new WriteTool(s),
	memory_edit: s => new MemoryEditTool(s as any),
	retain: s => new MemoryRetainTool(s as any),
	recall: s => new MemoryRecallTool(s as any),
	reflect: s => new MemoryReflectTool(s as any),
	learn: s => new LearnTool(s as any),
	manage_skill: () => new ManageSkillTool(),
	// hidden tools: registered but not advertised in the default visible set
	yield: s => new YieldTool({ ...s, outputSchema: undefined } as any),
	goal: s => new GoalTool(s as any),
	think: () => new ThinkTool(),
};

function permissiveStub(): any {
	return new Proxy({}, {
		get(_t, key) {
			if (key === "settings") return stubSettings();
			if (key === "cwd") return process.cwd();
			if (key === "taskDepth") return 0;
			if (typeof key === "symbol") return undefined;
			return (..._a: unknown[]) => undefined;
		},
	});
}

// omp tool schemas are ArkType Type objects: `toJsonSchema()` yields the canonical
// JSON Schema. Older TypeBox-style schemas (plain objects) are round-tripped as-is.
function plainify(value: unknown, seen: Set<object> = new Set()): unknown {
	if (Array.isArray(value)) return value.map(v => plainify(v, seen));
	if (value !== null && (typeof value === "object" || typeof value === "function")) {
		if (seen.has(value)) return undefined; // break schema self/cyclic references
		seen.add(value);
		const source = value as Record<string, unknown>;
		const out: Record<string, unknown> = {};
		for (const key of Object.keys(source)) out[key] = plainify(source[key], seen);
		return out;
	}
	return value;
}

function toSchema(parameters: unknown): unknown {
	// ArkType Type objects are functions carrying `.json` / `.toJsonSchema()`.
	if (typeof parameters === "function" || parameters !== null && typeof parameters === "object") {
		const candidate = parameters as { toJsonSchema?: unknown; json?: unknown };
		if (typeof candidate.toJsonSchema === "function") {
			return JSON.parse(JSON.stringify(candidate.toJsonSchema())) as unknown;
		}
		if (candidate.json !== undefined) return JSON.parse(JSON.stringify(candidate.json)) as unknown;
	}
	if (parameters !== null && typeof parameters === "object") {
		return JSON.parse(JSON.stringify(parameters)) as unknown;
	}
	throw new Error(`unsupported schema object: ${typeof parameters}`);
}

// Upstream declaration order: visible built-ins first, hidden ones last. Extracting
// in this order keeps ts_tools.json diffable against builtin-names.ts.
const NAMES: string[] = [...(BUILTIN_TOOL_NAMES as readonly string[]), ...(HIDDEN_TOOL_NAMES as readonly string[])];

const session = stubSession();
const tools: ExtractedTool[] = [];
const failures: string[] = [];

for (const name of NAMES) {
	const make = EXTRACTORS[name];
	if (!make) {
		failures.push(`${name}: no extractor (upstream built-in not covered by tests-tools/ts_schemas.ts)`);
		continue;
	}
	try {
		const tool = await make(session);
		tools.push({
			name,
			description_first_line: tool.description.split("\n", 1)[0] ?? "",
			description: tool.description,
			parameters: toSchema(tool.parameters),
		});
	} catch (e) {
		failures.push(`${name}: ${e instanceof Error ? (e.stack ?? e.message) : String(e)}`);
	}
}

// Drop extractors upstream no longer declares, so a removed tool cannot linger.
for (const name of Object.keys(EXTRACTORS)) {
	if (!NAMES.includes(name)) failures.push(`${name}: extractor declared but not in upstream builtin-names.ts`);
}

const outDir = dirname(fileURLToPath(import.meta.url));
const outPath = join(outDir, "ts_tools.json");

if (failures.length > 0) {
	console.error(`FAILED tools (${failures.length}):\n${failures.join("\n\n")}`);
	process.exit(1);
}
writeFileSync(outPath, `${JSON.stringify(tools, null, "\t")}\n`);
console.log(`wrote ${outPath} with ${tools.length} tools: ${tools.map(t => t.name).join(", ")}`);
