export default function rpcConformanceExt(pi: any): void {
  pi.rpc.handle("omomeow.move-readiness", async () => ({
    activeSources: [],
  }));
  pi.rpc.handle("omo.feedback.prepare", async (data: unknown) => ({
    kind: "unavailable",
    reason: "test-stub",
    echo: data ?? null,
  }));
}
