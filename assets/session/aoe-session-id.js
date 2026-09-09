// Publishes native conversation IDs and transcript paths for AoE.
// Pi keeps its existing behavior; Prime publishes only depth-zero roots.
// Publication does not imply that the transcript has been materialized.
import { mkdirSync, writeFileSync, renameSync, unlinkSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

export default function (pi) {
  const idTarget = process.env.AOE_SESSION_ID_FILE;
  const rootOnly = process.env.AOE_SESSION_ROOT_ONLY === "1";

  const writeAtomic = (target, value) => {
    let tmp;
    try {
      mkdirSync(dirname(target), { recursive: true });
      // Rename so a reader never sees a half-written value.
      tmp = join(dirname(target), `.${target.split("/").pop()}.${process.pid}.tmp`);
      writeFileSync(tmp, `${value}\n`, { mode: 0o600 });
      renameSync(tmp, target);
      tmp = undefined;
    } catch {
      if (tmp) {
        try {
          unlinkSync(tmp);
        } catch {}
      }
    }
  };

  pi.on("session_start", async (_event, ctx) => {
    if (!idTarget) return;
    try {
      const header = rootOnly ? ctx?.sessionManager?.getHeader?.() : undefined;
      if (rootOnly && header?.rlmDepth !== 0) return;
      const id = ctx?.sessionManager?.getSessionId?.();
      if (!id) return;
      const file = ctx?.sessionManager?.getSessionFile?.();
      if (rootOnly) {
        if (!file) return;
        writeAtomic(join(dirname(idTarget), "root_session"), JSON.stringify({
          id, path: resolve(file), cwd: header.cwd, rlmDepth: header.rlmDepth,
        }));
      } else {
        writeAtomic(idTarget, id);
        if (file) writeAtomic(join(dirname(idTarget), "session_path"), file);
      }
    } catch {
      // never block the agent
    }
  });
}
