// Session move handlers the way omo-nyang's session-move extension uses them: check the signal,
// write into the staged folder, and refuse on demand (`moveId: "boom"`).
import { mkdirSync, writeFileSync } from "node:fs";

export default function sessionMove(pi: any): void {
  for (const type of ["session_move_export", "session_move_import"]) {
    pi.on(type, async (e: any) => {
      e.signal.throwIfAborted();
      if (e.moveId === "boom") throw new Error(`${type} refused`);
      mkdirSync(`${e.dir}/fixture`, { recursive: true });
      writeFileSync(`${e.dir}/fixture/${type}.json`, JSON.stringify({ moveId: e.moveId }));
      pi.appendEntry(`seen:${type}`, JSON.stringify({ ...e, signal: e.signal instanceof AbortSignal }));
    });
  }
}
