#!/usr/bin/env bun
// Golden gate. Runs tests-golden/compare.ts (which always exits 0) and fails when the
// number of differing items grows past the recorded baseline, or when a new failure
// (missing capture / missing response / non-empty stderr) appears.
// Refresh the baseline deliberately with:
//   bun tests-golden/compare.ts > tests-golden/compare-baseline.txt
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";

const here = dirname(new URL(import.meta.url).pathname);
const root = join(here, "..");
const comparePath = join(root, "tests-golden", "compare.ts");
const baselinePath = join(root, "tests-golden", "compare-baseline.txt");

const COUNT_RE = /(\d+) differing item\(s\)/g;
// Hard failures, as opposed to shape diffs that the baseline already accounts for.
const ERROR_RE = /response missing|missing (ts|rs) capture/;

function finalCount(text: string, label: string): number {
	const all = [...text.matchAll(COUNT_RE)];
	if (!all.length) {
		console.error(`check-golden: ${label} produced no "N differing item(s)" line`);
		process.exit(1);
	}
	return Number(all[all.length - 1][1]);
}

const errorLines = (text: string) =>
	text
		.split("\n")
		.filter(l => ERROR_RE.test(l))
		.map(l => l.trim());

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

const baselineLines = new Set(baseline.split("\n").map(l => l.trim()));
const newErrors = [
	...errorLines(stdout),
	...stderr
		.split("\n")
		.map(l => l.trim())
		.filter(Boolean),
].filter(l => !baselineLines.has(l));

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
