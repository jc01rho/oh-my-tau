import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

// Registers `fx-prov` against the harness's mock provider, the way the gateway's
// model-provider.ts registers `omomeow-opus`. The mock URL is the one the harness
// wrote into the temp home's models.yml for the `mock` provider.
export default function providerExt(pi: any): void {
  const models = readFileSync(join(homedir(), ".omp/agent/models.yml"), "utf8");
  const baseUrl = /baseUrl:\s*(\S+)/.exec(models)?.[1];
  if (!baseUrl) throw new Error("mock baseUrl not found in models.yml");
  pi.registerProvider("fx-prov", {
    baseUrl,
    apiKey: "fx-not-a-secret",
    api: "openai-completions",
    authHeader: true,
    models: [
      { id: "m1", name: "Fx One", reasoning: false, input: ["text"], maxTokens: 4096, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } },
      { id: "m2", name: "Fx Two", reasoning: false, input: ["text"], maxTokens: 4096, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } },
    ],
  });
  pi.registerProvider("fx-gone", {
    baseUrl,
    apiKey: "fx-not-a-secret",
    api: "openai-completions",
    models: [{ id: "g1", name: "Fx Gone" }],
  });
  pi.unregisterProvider("fx-gone");
}
