// Parity gate: runs `bun tests-tools/compare.ts` and fails when a tool that
// matched before regresses or the tool-name sets drift unexpectedly.
//
// Intentional / deferred differences after Phase 1:
// - bash: description first line awaits Phase 2 embedded shell
// - ask: keeps legacy `questions[]` alongside `html` (omits required:["html"])
// - todo: keeps tau's `reason` field for blocker tracking (TS has no `reason`)
// - task: keeps `model` schema entry (Phase 3/4 perCallModel gate)
// - edit, eval, glob, web_search: wording-only first-line description deltas (params match)
//
// Run: bun scripts/check-tools-parity.ts
import { spawnSync } from "node:child_process";
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
	ask: [
		/^parameters differ from TS \(canonical JSON\)$/,
		/^full description differs from TS$/,
		/^extra in rust: questions$/,
		/^required html: ts=true rust=false$/,
		/^description first line differs:$/,
	],
	// tau keeps `reason` on todo for blocker tracking.
	todo: [
		/^parameters differ from TS \(canonical JSON\)$/,
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

	return { exitCode, toolIssues, missingInRust, extraInRust, staleNotPorted, raw: output };
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

const report = runCompare();

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
