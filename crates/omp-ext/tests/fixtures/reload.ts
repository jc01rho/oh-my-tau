// One registration of every kind `unload_all` must release, so a reload that loads it again can be
// checked for a single registration of each (`reload_twice_keeps_one_registration_of_each_kind`).
export default function reloadFixture(pi: any): void {
  pi.rpc.handle("fx.reload-ping", () => ({ pong: true }));
  pi.registerTool({
    name: "fx_reload_tool",
    label: "Reload tool",
    description: "one tool however often the extension reloads",
    parameters: {},
    async execute() {
      return { content: [{ type: "text", text: "ok" }] };
    },
  });
  pi.registerProvider("fx-reload", { baseUrl: "http://127.0.0.1:9", models: [{ id: "m1" }] });
  pi.registerFlag("fx-reload-flag", { type: "string", default: "default" });
  pi.events.on("fx.reload-bus", () => pi.appendEntry("fx-reload-bus", null));
  pi.on("turn_end", () => undefined);
  pi.on("session_shutdown", (event: any) => pi.appendEntry("fx-reload-shutdown", { reason: event.reason ?? null }));
}
