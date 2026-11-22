// Records every `mode_changed` event as a `seen:mode_changed` entry so the core's mode reporting can
// be asserted end to end (TS `ModeChangedEvent`).
export default function modeChanged(pi: any): void {
  pi.on("mode_changed", async (e: any) => {
    pi.appendEntry("seen:mode_changed", JSON.stringify(e));
  });
}
