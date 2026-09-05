// Loaded by AoE as an agent extension. Publishes the pane's current
// conversation to the per-instance sidecar AoE reads, so a conversation
// started inside the pane is attributed to that pane rather than guessed from
// a store keyed by cwd alone.
//
// Two files: `session_id` (the uuid) and `session_path` (the absolute
// transcript path). The path survives a worktree move when the agent indexes
// sessions by their starting cwd.
//
// `session_start` covers startup, resume, fork, and new. Failures are
// swallowed: publishing identity must never interfere with the agent.
import { mkdirSync, writeFileSync, renameSync, unlinkSync } from "node:fs";
import { dirname, join } from "node:path";

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
      // Prime writes rlmDepth into every child header; depth zero owns the pane.
      if (rootOnly && ctx?.sessionManager?.getHeader?.()?.rlmDepth > 0) return;
      const id = ctx?.sessionManager?.getSessionId?.();
      if (!id) return;
      writeAtomic(idTarget, id);
      const file = ctx?.sessionManager?.getSessionFile?.();
      if (file) {
        writeAtomic(join(dirname(idTarget), "session_path"), file);
      }
      if (rootOnly) {
        writeAtomic(join(dirname(idTarget), "root_only"), "1");
      }
    } catch {
      // never block the agent
    }
  });
}
