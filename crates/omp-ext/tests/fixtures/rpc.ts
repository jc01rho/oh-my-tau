// Registers named queries the way omo-nyang's extensions do
// (`api.rpc.handle("omomeow.move-readiness", ...)`).
export default function rpcFixture(pi: any): void {
  pi.rpc.handle("fx.rpc", (data: any) => ({ echo: data }));
  pi.rpc.handle("fx.async", async (data: any) => ({ doubled: Number(data?.n ?? 0) * 2 }));
  pi.rpc.handle("fx.hang", () => new Promise(() => {}));
  pi.rpc.handle("fx.boom", () => {
    throw new Error("handler exploded");
  });
}
