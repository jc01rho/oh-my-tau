// Notifies `fx-barrier:<reason>` on every `session_start`. Loaded after another extension, the
// notification reaches the core after every provider notification that load sent, so its
// `extension_ui_request` frame marks the point by which the core has applied them.
export default function sessionStartNotifyExt(pi: any): void {
  pi.on("session_start", (event: any, ctx: any) => ctx.ui.notify(`fx-barrier:${event.reason ?? "startup"}`));
}
