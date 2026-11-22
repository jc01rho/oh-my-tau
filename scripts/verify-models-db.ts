/**
 * TS-readability check for omp-rs-written models.db rows.
 *
 * With a vendor-src checkout: reads a provider's `model_cache` row through the
 * *real* TS reader (`catalog/src/model-cache.ts readModelCache`), which enforces
 * CACHE_SCHEMA_VERSION, the exact `materialization_policy` string (app
 * version + builder version + rules hash) and `isMaterializedModel` on every
 * payload row — the same gates a live TS omp applies on startup.
 *
 * Clean clone (no `vendor-src/oh-my-pi`): falls back to a reduced structural
 * check — the row is read with bun:sqlite, its payload parsed, and each model
 * cross-checked against the committed `crates/omp-cli/assets/tui-model-catalog.json`.
 * The policy string is printed but not validated (the TS gate needs the checkout).
 *
 * Run (from the repo root): bun scripts/verify-models-db.ts <models.db> [provider]
 */
import { Database } from "bun:sqlite";
import { existsSync } from "node:fs";
import { join } from "node:path";

const ROOT = join(import.meta.dir, "..");
const TS_MODEL_CACHE = join(ROOT, "vendor-src/oh-my-pi/packages/catalog/src/model-cache.ts");
const TUI_CATALOG = join(ROOT, "crates/omp-cli/assets/tui-model-catalog.json");

const [dbPath, provider = "mockprov"] = process.argv.slice(2);
if (!dbPath) {
	console.error("usage: bun verify-models-db.ts <models.db> [provider]");
	process.exit(2);
}

if (existsSync(TS_MODEL_CACHE)) {
	const { readModelCache } = await import(TS_MODEL_CACHE);
	const ttlMs = 2 * 60 * 60 * 1000;
	const entry = readModelCache(provider, ttlMs, () => Date.now(), dbPath);
	if (!entry) {
		console.error(`REJECTED: TS readModelCache('${provider}') returned null — policy/schema/payload gate failed`);
		process.exit(1);
	}
	console.log(
		`TS readModelCache OK: provider=${provider} models=${entry.models.length} fresh=${entry.fresh} authoritative=${entry.authoritative}`,
	);
	console.log(`  ids: ${entry.models.map((m: { id: string }) => m.id).join(", ")}`);
	for (const model of entry.models) {
		console.log(`  ${model.id}: api=${model.api} ctx=${model.contextWindow} max=${model.maxTokens} reasoning=${model.reasoning}`);
	}
	process.exit(0);
}

console.error(`verify-models-db: ${TS_MODEL_CACHE} not found; falling back to ${TUI_CATALOG} (structural check only)`);
type CatalogRow = [string, string, number, number, number, number, number, number, number, string, number | null];
const catalog: Record<string, CatalogRow[]> = JSON.parse(await Bun.file(TUI_CATALOG).text());

const db = new Database(dbPath, { readonly: true });
const row = db
	.query("SELECT version, materialization_policy, authoritative, models FROM model_cache WHERE provider_id = ?1")
	.get(provider) as { version: number; materialization_policy: string; authoritative: number; models: string } | null;
if (!row) {
	console.error(`REJECTED: no model_cache row for provider '${provider}'`);
	process.exit(1);
}
let models: Array<{ id?: unknown; api?: unknown; contextWindow?: unknown; maxTokens?: unknown; reasoning?: unknown }>;
try {
	models = JSON.parse(row.models);
} catch (err) {
	console.error(`REJECTED: model_cache.models for '${provider}' is not JSON: ${err}`);
	process.exit(1);
}
if (!Array.isArray(models) || models.some(m => typeof m?.id !== "string" || typeof m?.api !== "string")) {
	console.error(`REJECTED: model_cache.models for '${provider}' is not an array of {id, api, ...} rows`);
	process.exit(1);
}
const known = new Set((catalog[provider] ?? []).map(r => r[0]));
console.log(
	`fallback check OK: provider=${provider} models=${models.length} version=${row.version} authoritative=${row.authoritative}`,
);
console.log(`  materialization_policy: ${row.materialization_policy} (not validated without vendor-src)`);
for (const model of models) {
	const tag = known.has(model.id as string) ? "catalog" : "not in tui catalog";
	console.log(`  ${model.id}: api=${model.api} ctx=${model.contextWindow} max=${model.maxTokens} reasoning=${model.reasoning} [${tag}]`);
}
