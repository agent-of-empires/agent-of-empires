// Loaded by AoE with `pi -e`. Publishes the pane's current conversation to the
// per-instance sidecar AoE reads, so a conversation started inside the pane
// with `/new` is attributed to that pane rather than guessed from a store
// keyed by cwd alone.
//
// Two files: `session_id` (the uuid) and `session_path` (the absolute
// transcript path). The path is what survives a worktree move, since pi
// indexes sessions by the cwd they started in and `--session-id` only looks in
// the current one.
//
// `session_start` covers startup, resume, fork, and new. Failures are
// swallowed: publishing identity must never interfere with the agent.
import { mkdirSync, writeFileSync, renameSync, unlinkSync } from "node:fs";
import { dirname, join } from "node:path";

export default function (pi) {
  const idTarget = process.env.AOE_PI_SESSION_ID_FILE;

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
      const id = ctx?.sessionManager?.getSessionId?.();
      if (!id) return;
      writeAtomic(idTarget, id);
      const file = ctx?.sessionManager?.getSessionFile?.();
      if (file) {
        writeAtomic(join(dirname(idTarget), "session_path"), file);
      }
    } catch {
      // never block the agent
    }
  });
}
