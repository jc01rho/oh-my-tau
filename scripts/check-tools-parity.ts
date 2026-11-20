// Parity gate: runs `bun tests-tools/compare.ts` and fails when a tool that
// matched before regresses or the tool-name sets drift unexpectedly.
//
// Intentional / deferred differences after Phase 1:
// - bash: description first line awaits Phase 2 embedded shell
// - ask: keeps legacy `questions[]` alongside `html` (omits required:["html"])
// - todo: keeps tau's `reason` field for blocker tracking (TS has no `reason`)
//   ask/todo parameter shape is NOT blanket-allowed: preciseSchemaDiffs below
//   accepts only those exact properties and fails on any other
//   property/type/required difference.
// - task: keeps `model` schema entry (Phase 3/4 perCallModel gate)
// - edit, eval, glob, web_search: wording-only first-line description deltas (params match)
//
// Run: bun scripts/check-tools-parity.ts
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

/**
 * Per-tool documented deltas post-Phase 1, as regexes matched against one
 * `- <issue>` line of the comparison output. A tool is allowed to differ only
 * in the ways listed here: any new missing/extra/type/required difference in
 * these tools is a regression and fails the gate.
 */
const ALLOWED_DELTAS: Record<string, RegExp[]> = {
	// Phase 2 embedded shell: description wording only.
	bash: [/^description first line differs:$/],
	// Wording-only first-line description deltas; parameters match exactly.
	edit: [/^description first line differs:$/],
	eval: [/^description first line differs:$/],
	glob: [/^description first line differs:$/],
	web_search: [/^description first line differs:$/],
	// Legacy `questions[]` kept alongside `html`, so `html` is not required.
	// The parameter shape itself is verified by preciseSchemaDiffs below.
	ask: [
		/^full description differs from TS$/,
		/^extra in rust: questions$/,
		/^required html: ts=true rust=false$/,
		/^description first line differs:$/,
	],
	// tau keeps `reason` on todo for blocker tracking.
	todo: [
		/^full description differs from TS$/,
		/^extra in rust: reason$/,
	],
	// Phase 3/4 perCallModel gate keeps `model` on task.
	task: [/^extra in rust: model$/],
};

interface CompareReport {
	exitCode: number;
	/** Per differing tool, the `- <issue>` lines the comparison reported. */
	toolIssues: Record<string, string[]>;
	missingInRust: string[];
	extraInRust: string[];
	staleNotPorted: string[];
	/** NOT_PORTED_YET entries the Rust registry now registers. */
	driftedBack: string[];
	raw: string;
}

function parseList(line: string): string[] {
	const rest = line.slice(line.indexOf(":") + 1).trim();
	if (!rest || rest === "(none)") return [];
	return rest.split(",").map(s => s.trim()).filter(Boolean);
}

function parseCompare(output: string, exitCode: number): CompareReport {
	const toolIssues: Record<string, string[]> = {};
	let missingInRust: string[] = [];
	let extraInRust: string[] = [];
	let staleNotPorted: string[] = [];
	let driftedBack: string[] = [];
	let current: string | null = null;

	for (const raw of output.split("\n")) {
		const line = raw.trimEnd();
		const trimmed = line.trim();
		const nameDiff = /^(missing in omp-rs|extra in omp-rs):\s*(.*)$/.exec(trimmed);
		if (nameDiff) {
			if (nameDiff[1] === "missing in omp-rs") missingInRust = parseList(trimmed);
			else extraInRust = parseList(trimmed);
			current = null;
			continue;
		}
		const drifted = /^drifted-back:\s*(.*)$/.exec(trimmed);
		if (drifted) {
			driftedBack = parseList(trimmed);
			current = null;
			continue;
		}
		const stale = /^stale not-ported:\s*(.*)$/.exec(trimmed);
		if (stale) {
			staleNotPorted = parseList(trimmed);
			current = null;
			continue;
		}
		const ok = /^\[([a-z_][a-z0-9_]*)\] OK$/.exec(trimmed);
		if (ok) {
			current = null;
			continue;
		}
		const bad = /^\[([a-z_][a-z0-9_]*)\]$/.exec(trimmed);
		if (bad) {
			current = bad[1];
			toolIssues[current] = [];
			continue;
		}
		const issue = /^\s+- (.*)$/.exec(line);
		if (issue && current) toolIssues[current].push(issue[1].trim());
	}

	return { exitCode, toolIssues, missingInRust, extraInRust, staleNotPorted, driftedBack, raw: output };
}

function runCompare(): CompareReport {
	const res = spawnSync(process.execPath, [join(ROOT, "tests-tools/compare.ts")], {
		cwd: ROOT,
		encoding: "utf8",
	});
	const stdout = res.stdout ?? "";
	const stderr = res.stderr ?? "";
	if (res.error) {
		console.error(`failed to run tests-tools/compare.ts: ${res.error.message}`);
		process.exit(1);
	}
	const output = stderr ? `${stdout}\n${stderr}` : stdout;
	const report = parseCompare(output, res.status ?? 1);
	if (res.status !== 0) {
		console.error(stderr.trim() || stdout.trim());
		console.error(`tests-tools/compare.ts exited with status ${res.status}`);
	}
	return report;
}

interface SchemaAllowance {
	/** Properties omp-rs may add on top of the TS reference (they must stay optional). */
	extra_props: string[];
	/** Properties the TS reference requires that omp-rs may leave optional. */
	optional_props: string[];
}

/**
 * The only structural Phase 1 deltas allowed on `ask` and `todo`. Anything else
 * — an extra property, a lost property, a type change, or a required-flag change
 * — is a regression and fails the gate.
 */
const PRECISE_SCHEMA_ALLOWANCES: Record<string, SchemaAllowance> = {
	// Legacy `questions[]` still accepted next to `html`; `html` stays optional.
	ask: { extra_props: ["questions"], optional_props: ["html"] },
	// tau keeps `reason` on todo for blocker tracking.
	todo: { extra_props: ["reason"], optional_props: [] },
};

type JsonSchema = Record<string, unknown>;

function schemaProps(schema: JsonSchema): Record<string, JsonSchema> {
	return (schema.properties ?? {}) as Record<string, JsonSchema>;
}

function schemaRequired(schema: JsonSchema): Set<string> {
	const req = schema.required;
	return new Set(Array.isArray(req) ? (req as string[]) : []);
}

/** Parsed Rust schemas via compare.ts's machine-readable dump. */
function dumpRustSchemas(): Record<string, JsonSchema> {
	const res = spawnSync(process.execPath, [join(ROOT, "tests-tools/compare.ts"), "--dump-rust-tools"], {
		cwd: ROOT,
		encoding: "utf8",
	});
	if (res.error || res.status !== 0) {
		console.error(`failed to dump rust schemas: ${res.error?.message ?? (res.stderr ?? "").trim()}`);
		process.exit(1);
	}
	const tools = JSON.parse(res.stdout) as Array<{ name: string; parameters: JsonSchema }>;
	return Object.fromEntries(tools.map(t => [t.name, t.parameters]));
}

/** Structural diff: only the allowed extra properties / optional flags may differ. */
function preciseSchemaDiffs(tsSchema: JsonSchema, rustSchema: JsonSchema, allow: SchemaAllowance): string[] {
	const diffs: string[] = [];
	const tsProps = schemaProps(tsSchema);
	const rustProps = schemaProps(rustSchema);
	for (const key of Object.keys(tsProps)) {
		if (!(key in rustProps)) {
			diffs.push(`property "${key}" missing in omp-rs`);
			continue;
		}
		const tsType = JSON.stringify(tsProps[key].type);
		const rustType = JSON.stringify(rustProps[key].type);
		if (tsType !== rustType) diffs.push(`property "${key}" type: ts=${tsType} rust=${rustType}`);
	}
	for (const key of Object.keys(rustProps)) {
		if (!(key in tsProps) && !allow.extra_props.includes(key)) diffs.push(`unexpected extra property "${key}" in omp-rs`);
	}
	const tsReq = schemaRequired(tsSchema);
	const rustReq = schemaRequired(rustSchema);
	for (const key of new Set([...tsReq, ...rustReq])) {
		if (tsReq.has(key) === rustReq.has(key)) continue;
		if (!tsReq.has(key)) {
			diffs.push(`property "${key}" required only in omp-rs`);
			continue;
		}
		if (!allow.optional_props.includes(key)) diffs.push(`property "${key}" no longer required in omp-rs`);
	}
	for (const key of allow.extra_props) {
		if (rustReq.has(key)) diffs.push(`allowed extra property "${key}" must stay optional`);
	}
	return diffs;
}

const report = runCompare();
const tsReference = JSON.parse(readFileSync(join(ROOT, "tests-tools/ts_tools.json"), "utf8")) as Array<{
	name: string;
	parameters: JsonSchema;
}>;
const rustSchemas = dumpRustSchemas();

if (report.exitCode !== 0) {
	console.error("\nparity gate: FAIL (comparison script itself failed)");
	process.exit(1);
}

const failures: string[] = [];

for (const name of report.missingInRust) failures.push(`tool "${name}" is missing from omp-rs`);
for (const name of report.extraInRust) failures.push(`tool "${name}" exists only in omp-rs`);
if (report.staleNotPorted.length > 0) {
	failures.push(`NOT_PORTED_YET lists tools the TS reference does not have: ${report.staleNotPorted.join(", ")}`);
}
if (report.driftedBack.length > 0) {
	failures.push(`NOT_PORTED_YET lists tools omp-rs now registers (drop them): ${report.driftedBack.join(", ")}`);
}

const differing = Object.keys(report.toolIssues).sort();
for (const name of differing) {
	const allowed = ALLOWED_DELTAS[name];
	if (!allowed) {
		failures.push(`tool "${name}" no longer matches its TS reference`);
		continue;
	}
	const unexpected = report.toolIssues[name].filter(issue => !allowed.some(re => re.test(issue)));
	if (unexpected.length > 0) {
		failures.push(`tool "${name}" has undocumented delta(s): ${unexpected.join("; ")}`);
	}
}

// Structural check for ask/todo: independent of the compare.ts console report.
for (const [name, allow] of Object.entries(PRECISE_SCHEMA_ALLOWANCES)) {
	const tsTool = tsReference.find(t => t.name === name);
	const rustSchema = rustSchemas[name];
	if (!tsTool || !rustSchema) {
		failures.push(`tool "${name}" is not present in both ts_tools.json and the parsed Rust schemas`);
		continue;
	}
	for (const diff of preciseSchemaDiffs(tsTool.parameters, rustSchema, allow)) {
		failures.push(`tool "${name}" schema: ${diff}`);
	}
}

const remaining = differing.filter(name => name in ALLOWED_DELTAS);

if (failures.length > 0) {
	console.error("parity gate: FAIL");
	for (const failure of failures) console.error(`  - ${failure}`);
	console.error("\nfull comparison:\n" + report.raw.trimEnd());
	process.exit(1);
}

console.log(`parity gate: PASS (${differing.length} tool(s) with documented deltas pending Phase 1)`);
if (remaining.length > 0) console.log(`  known: ${remaining.join(", ")}`);
const fixed = Object.keys(ALLOWED_DELTAS).filter(name => !differing.includes(name));
if (fixed.length > 0) console.log(`  already matching: ${fixed.join(", ")}`);
