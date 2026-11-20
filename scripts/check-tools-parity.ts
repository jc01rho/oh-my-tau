// Parity gate for Phase 0: runs `bun tests-tools/compare.ts` and fails when a tool
// that was clean before starts diverging (or the tool-name sets drift).
//
// Known gaps carried into Phase 0 (fixed in Phase 1); anything outside this list is
// treated as a NEW regression and exits 1.
//
// Run: bun scripts/check-tools-parity.ts
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

/** Tools already known to differ before Phase 1 fixes. */
const KNOWN_DIFFERING = new Set(["edit", "eval", "todo", "web_search"]);

interface CompareReport {
	exitCode: number;
	differing: string[];
	missingInRust: string[];
	extraInRust: string[];
	raw: string;
}

function parseList(line: string): string[] {
	const rest = line.slice(line.indexOf(":") + 1).trim();
	if (!rest || rest === "(none)") return [];
	return rest.split(",").map(s => s.trim()).filter(Boolean);
}

function parseCompare(output: string, exitCode: number): CompareReport {
	const differing: string[] = [];
	let missingInRust: string[] = [];
	let extraInRust: string[] = [];

	for (const line of output.split("\n")) {
		const nameDiff = /^(missing in omp-rs|extra in omp-rs):\s*(.*)$/.exec(line.trim());
		if (nameDiff) {
			if (nameDiff[1] === "missing in omp-rs") missingInRust = parseList(line.trim());
			else extraInRust = parseList(line.trim());
			continue;
		}
		const ok = /^\[([a-z_][a-z0-9_]*)\] OK$/.exec(line.trim());
		if (ok) continue;
		const bad = /^\[([a-z_][a-z0-9_]*)\]$/.exec(line.trim());
		if (bad) differing.push(bad[1]);
	}

	return { exitCode, differing, missingInRust, extraInRust, raw: output };
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

const regressed = report.differing.filter(name => !KNOWN_DIFFERING.has(name));
for (const name of regressed) failures.push(`tool "${name}" no longer matches its TS reference`);

const remaining = report.differing.filter(name => KNOWN_DIFFERING.has(name));

if (failures.length > 0) {
	console.error("parity gate: FAIL");
	for (const failure of failures) console.error(`  - ${failure}`);
	console.error("\nfull comparison:\n" + report.raw.trimEnd());
	process.exit(1);
}

console.log(`parity gate: PASS (${report.differing.length} known difference(s) pending Phase 1)`);
if (remaining.length > 0) console.log(`  known: ${remaining.join(", ")}`);
const fixed = [...KNOWN_DIFFERING].filter(name => !report.differing.includes(name));
if (fixed.length > 0) console.log(`  already matching: ${fixed.join(", ")}`);
