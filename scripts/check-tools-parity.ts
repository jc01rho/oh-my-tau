// Parity gate: runs `bun tests-tools/compare.ts` and fails when a tool that
// matched before regresses or the tool-name sets drift unexpectedly.
//
// Intentional / deferred differences after Phase 1:
// - bash: description first line awaits Phase 2 embedded shell
// - edit, eval, glob, web_search: wording-only first-line description deltas (params match)
//
// Closed in Phase 3 (allowances deleted, the deltas are gone):
// - ask: schema is omp's `askSchema` exactly (`{html}` required); the legacy
//   `questions[]` shape is recovery-only at runtime, no longer advertised
// - todo: schema is omp's `todoSchema` exactly (7-value op, no `reason`);
//   `block`/`unblock` + `reason` are runtime-only extensions
// - task: top-level `model` removed (model selectors are per-item only)
// - glob: `additionalProperties:false` dropped; the reference's `pattern` key is
//   a runtime-only extension (`globSearchPath` scoping in `search::glob`)
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
	web_search: [/^description first line differs:$/],
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

/** Documented data contract of one property omp-rs adds on top of the TS reference. */
interface DocumentedSchema {
	type: "string" | "number" | "boolean" | "array" | "object";
	/** Documented fields of an object schema; any other field is undocumented. */
	props?: Record<string, DocumentedSchema>;
	/** Fields that must appear in the schema's `required` array. */
	required?: string[];
	/** Documented item schema of an array. */
	items?: DocumentedSchema;
}

interface SchemaAllowance {
	/** Properties omp-rs may add on top of the TS reference, with their documented schema. */
	extra_props: Record<string, DocumentedSchema>;
	/** Properties the TS reference requires that omp-rs may leave optional. */
	optional_props: string[];
	/** Extra enum values omp-rs may accept, keyed by dotted property path (e.g. `op`). */
	extra_enum_values?: Record<string, string[]>;
}

/**
 * Structural deltas tolerated between the Rust schema and the TS reference, per
 * tool. Phase 3 closed the last two (`ask`, `todo`): both schemas now match the
 * golden byte-for-byte and their runtime-only extensions are out-of-schema, so
 * the table is empty. A new entry here must be justified in
 * `docs/parity/tools.md` §4, not added to silence a diff.
 */
const PRECISE_SCHEMA_ALLOWANCES: Record<string, SchemaAllowance> = {};

type JsonSchema = Record<string, unknown>;

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

function isPlainObject(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Deterministic JSON with sorted object keys, so equal schemas serialize identically. */
function canonicalJson(value: unknown): string {
	if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
	if (isPlainObject(value)) {
		const body = Object.keys(value)
			.sort()
			.map(key => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
			.join(",");
		return `{${body}}`;
	}
	return JSON.stringify(value) ?? "undefined";
}

/** Enum membership is order-insensitive; each value keeps its canonical form. */
function canonicalValues(values: unknown): string[] {
	return (Array.isArray(values) ? values : [values]).map(canonicalJson).sort();
}

const SCHEMA_UNION_KEYWORDS = new Set(["anyOf", "oneOf", "allOf"]);
const SCHEMA_NODE_KEYWORDS = new Set(["items", "contains", "propertyNames", "not", "if", "then", "else"]);
/** In JSON Schema an absent `required`/`properties` means empty, not "keyword missing". */
const ABSENT_KEYWORD_DEFAULTS: Record<string, unknown> = { required: [], properties: {} };

/**
 * A keyword that is present at all must already have its JSON Schema shape; only
 * an omitted keyword is normalized to the empty default in ABSENT_KEYWORD_DEFAULTS.
 */
function keywordShapeIssue(key: string, value: unknown, side: "ts" | "omp-rs"): string | null {
	if (key === "required" && !Array.isArray(value)) return `required: ${side}=${canonicalJson(value)} is not an array`;
	if (key === "properties" && !isPlainObject(value))
		return `properties: ${side}=${canonicalJson(value)} is not an object`;
	return null;
}

/** Validate one JSON Schema node against the documented contract of an allowed extra property. */
function checkDocumentedSchema(value: unknown, doc: DocumentedSchema, path: string, diffs: string[]): void {
	if (!isPlainObject(value)) {
		diffs.push(`"${path}" must be a schema object, got ${canonicalJson(value)}`);
		return;
	}
	if (value.type !== doc.type) {
		diffs.push(`"${path}" type: documented ${doc.type} omp-rs=${canonicalJson(value.type)}`);
		return;
	}
	if (doc.type === "array") {
		if (value.items !== undefined && doc.items) checkDocumentedSchema(value.items, doc.items, `${path}[]`, diffs);
		return;
	}
	if (doc.type !== "object") return;
	if (value.properties !== undefined && !isPlainObject(value.properties)) {
		diffs.push(`"${path}" properties: ${canonicalJson(value.properties)} is not an object`);
		return;
	}
	const declared = isPlainObject(value.properties) ? value.properties : {};
	for (const name of Object.keys(declared)) {
		const nested = childPath(path, name);
		const docProp = doc.props?.[name];
		if (!docProp) {
			diffs.push(`"${nested}" is not part of the documented schema`);
			continue;
		}
		checkDocumentedSchema(declared[name], docProp, nested, diffs);
	}
	const required = Array.isArray(value.required) ? value.required.map(String) : [];
	for (const name of doc.required ?? []) {
		if (!required.includes(name)) diffs.push(`"${childPath(path, name)}" is documented as required but is not`);
	}
}

function childPath(path: string, segment: string): string {
	return path === "" ? segment : `${path}.${segment}`;
}

/** Recursive canonical comparison of two schema nodes, honouring the allowance only where documented. */
function diffSchemaNode(
	ts: unknown,
	rust: unknown,
	path: string,
	allow: SchemaAllowance,
	diffs: string[],
): void {
	const where = path === "" ? "root schema" : `"${path}"`;
	if (!isPlainObject(ts) || !isPlainObject(rust)) {
		const tsJson = canonicalJson(ts);
		const rustJson = canonicalJson(rust);
		if (tsJson !== rustJson) diffs.push(`${where}: ts=${tsJson} rust=${rustJson}`);
		return;
	}
	const keys = [...new Set([...Object.keys(ts), ...Object.keys(rust)])].sort();
	for (const key of keys) {
		const inTs = Object.hasOwn(ts, key);
		const inRust = Object.hasOwn(rust, key);
		const tsShape = inTs ? keywordShapeIssue(key, ts[key], "ts") : null;
		const rustShape = inRust ? keywordShapeIssue(key, rust[key], "omp-rs") : null;
		if (tsShape !== null || rustShape !== null) {
			diffs.push(`${where} ${tsShape ?? rustShape}`);
			continue;
		}
		if (!inTs || !inRust) {
			const absentDefault = ABSENT_KEYWORD_DEFAULTS[key];
			if (absentDefault !== undefined) {
				diffSchemaKeyword(key, inTs ? ts[key] : absentDefault, inRust ? rust[key] : absentDefault, path, allow, diffs);
				continue;
			}
			diffs.push(`${where} ${key} present only in ${inTs ? "ts" : "omp-rs"}`);
			continue;
		}
		diffSchemaKeyword(key, ts[key], rust[key], path, allow, diffs);
	}
}

function diffSchemaKeyword(
	key: string,
	tsValue: unknown,
	rustValue: unknown,
	path: string,
	allow: SchemaAllowance,
	diffs: string[],
): void {
	const where = path === "" ? "root schema" : `"${path}"`;
	if (key === "enum") {
		const tsValues = canonicalValues(tsValue);
		const rustValues = canonicalValues(rustValue);
		const missing = tsValues.filter(value => !rustValues.includes(value));
		if (missing.length > 0) diffs.push(`${where} enum missing value(s) in omp-rs: ${missing.join(", ")}`);
		const extra = rustValues.filter(value => !tsValues.includes(value));
		if (extra.length > 0) {
			const permitted = (allow.extra_enum_values?.[path] ?? []).map(canonicalJson);
			const undocumented = extra.filter(value => !permitted.includes(value));
			if (undocumented.length > 0)
				diffs.push(`${where} enum has undocumented extra value(s) in omp-rs: ${undocumented.join(", ")}`);
		}
		return;
	}
	if (key === "required") {
		const names = (value: unknown) => (Array.isArray(value) ? value.map(String) : []);
		const tsRequired = new Set(names(tsValue));
		const rustRequired = new Set(names(rustValue));
		for (const name of tsRequired) {
			if (rustRequired.has(name)) continue;
			if (path === "" && allow.optional_props.includes(name)) continue;
			diffs.push(`${where} property ${name} no longer required in omp-rs`);
		}
		for (const name of rustRequired) {
			if (!tsRequired.has(name)) diffs.push(`${where} property ${name} required only in omp-rs`);
		}
		return;
	}
	if (key === "properties") {
		const tsProps = isPlainObject(tsValue) ? tsValue : {};
		const rustProps = isPlainObject(rustValue) ? rustValue : {};
		const names = [...new Set([...Object.keys(tsProps), ...Object.keys(rustProps)])].sort();
		for (const name of names) {
			const nested = childPath(path, name);
			if (!Object.hasOwn(tsProps, name)) {
				if (path === "" && Object.hasOwn(allow.extra_props, name)) {
					// Allowed extra, but its shape must still honour the documented contract.
					checkDocumentedSchema(rustProps[name], allow.extra_props[name], nested, diffs);
					continue;
				}
				diffs.push(`unexpected extra property "${nested}" in omp-rs`);
				continue;
			}
			if (!Object.hasOwn(rustProps, name)) {
				diffs.push(`property "${nested}" missing in omp-rs`);
				continue;
			}
			diffSchemaNode(tsProps[name], rustProps[name], nested, allow, diffs);
		}
		return;
	}
	if (key === "additionalProperties") {
		diffSchemaNode(tsValue, rustValue, childPath(path, key), allow, diffs);
		return;
	}
	if (SCHEMA_UNION_KEYWORDS.has(key)) {
		const tsNodes = Array.isArray(tsValue) ? tsValue : [tsValue];
		const rustNodes = Array.isArray(rustValue) ? rustValue : [rustValue];
		if (tsNodes.length !== rustNodes.length) {
			diffs.push(`${where} ${key} length: ts=${tsNodes.length} rust=${rustNodes.length}`);
			return;
		}
		tsNodes.forEach((node, index) =>
			diffSchemaNode(node, rustNodes[index], childPath(path, `${key}[${index}]`), allow, diffs),
		);
		return;
	}
	if (SCHEMA_NODE_KEYWORDS.has(key)) {
		diffSchemaNode(tsValue, rustValue, childPath(path, key), allow, diffs);
		return;
	}
	const tsJson = canonicalJson(tsValue);
	const rustJson = canonicalJson(rustValue);
	if (tsJson !== rustJson) diffs.push(`${where} ${key}: ts=${tsJson} rust=${rustJson}`);
}

/** Structural diff: only the documented extra properties, optional flags, and enum values may differ. */
function preciseSchemaDiffs(tsSchema: JsonSchema, rustSchema: JsonSchema, allow: SchemaAllowance): string[] {
	const diffs: string[] = [];
	diffSchemaNode(tsSchema, rustSchema, "", allow, diffs);
	const rustRequired = schemaRequired(rustSchema);
	for (const key of Object.keys(allow.extra_props)) {
		if (rustRequired.has(key)) diffs.push(`allowed extra property "${key}" must stay optional`);
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

const remaining = differing.filter(name => Object.hasOwn(ALLOWED_DELTAS, name));

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
