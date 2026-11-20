#!/usr/bin/env bun
// Golden gate. Runs tests-golden/compare.ts (which always exits 0) and fails when the
// number of differing items grows past the recorded baseline, or when a new failure
// (missing capture / missing response / non-empty stderr) appears.
// Refresh the baseline deliberately with:
//   bun tests-golden/compare.ts > tests-golden/compare-baseline.txt
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const comparePath = join(root, "tests-golden", "compare.ts");
const baselinePath = join(root, "tests-golden", "compare-baseline.txt");

const COUNT_RE = /(\d+) differing item\(s\)/g;
// Hard failures, as opposed to shape diffs that the baseline already accounts for.
const ERROR_RE = /response missing|missing (ts|rs) capture/;
const SCENARIO_RE = /^=== (.+?) ===$/;
const STDERR_SCOPE = "<stderr>";

function finalCount(text: string, label: string): number {
	const all = [...text.matchAll(COUNT_RE)];
	if (!all.length) {
		console.error(`check-golden: ${label} produced no "N differing item(s)" line`);
		process.exit(1);
	}
	return Number(all[all.length - 1][1]);
}

// Hard-error occurrences keyed by the `=== <scenario> ===` section they appear under, so an
// error that merely moves to another scenario (or is duplicated) still counts as new.
function errorCounts(text: string): Map<string, number> {
	const counts = new Map<string, number>();
	let scenario = "<global>";
	for (const raw of text.split("\n")) {
		const line = raw.trim();
		if (!line) continue;
		const header = SCENARIO_RE.exec(line);
		if (header) {
			scenario = header[1];
			continue;
		}
		if (!ERROR_RE.test(line)) continue;
		const key = `${scenario}\u0000${line}`;
		counts.set(key, (counts.get(key) ?? 0) + 1);
	}
	return counts;
}

function stderrCounts(text: string): Map<string, number> {
	const counts = new Map<string, number>();
	for (const raw of text.split("\n")) {
		const line = raw.trim();
		if (!line) continue;
		const key = `${STDERR_SCOPE}\u0000${line}`;
		counts.set(key, (counts.get(key) ?? 0) + 1);
	}
	return counts;
}

function newErrorLines(current: Map<string, number>, known: Map<string, number>): string[] {
	const out: string[] = [];
	for (const [key, count] of current) {
		const extra = count - (known.get(key) ?? 0);
		if (extra <= 0) continue;
		const [scope, line] = key.split("\u0000");
		for (let i = 0; i < extra; i++) out.push(`${scope}: ${line}`);
	}
	return out;
}

const proc = Bun.spawn(["bun", comparePath], { cwd: root, stdout: "pipe", stderr: "pipe" });
const [stdout, stderr, exitCode] = await Promise.all([
	new Response(proc.stdout).text(),
	new Response(proc.stderr).text(),
	proc.exited,
]);

if (exitCode !== 0) {
	console.error(`check-golden: compare.ts exited ${exitCode}\n${stderr}`);
	process.exit(1);
}

const baseline = readFileSync(baselinePath, "utf8");
const baselineCount = finalCount(baseline, "baseline");
const currentCount = finalCount(stdout, "compare run");

const currentErrors = errorCounts(stdout);
for (const [key, count] of stderrCounts(stderr)) {
	currentErrors.set(key, (currentErrors.get(key) ?? 0) + count);
}
const newErrors = newErrorLines(currentErrors, errorCounts(baseline));

const failures: string[] = [];
if (currentCount > baselineCount) {
	failures.push(
		`differing items grew: ${currentCount} > baseline ${baselineCount} (+${currentCount - baselineCount})`,
	);
}
if (newErrors.length) {
	failures.push(`new error(s):\n${newErrors.map(l => `  ${l}`).join("\n")}`);
}

if (failures.length) {
	console.error(`check-golden: FAILED\n${failures.map(f => `- ${f}`).join("\n")}`);
	console.error(
		`\nIf the new state is the intended baseline, refresh it:\n  bun tests-golden/compare.ts > tests-golden/compare-baseline.txt`,
	);
	process.exit(1);
}

const delta = currentCount - baselineCount;
console.log(
	`check-golden: ok (${currentCount} differing item(s), baseline ${baselineCount}${delta ? `, ${delta} fewer` : ""})`,
);
