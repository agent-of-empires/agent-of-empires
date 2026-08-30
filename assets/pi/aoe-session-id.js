// Loaded by AoE with `pi -e`. Publishes the pane's current conversation id to
// the per-instance sidecar AoE reads (`hooks::read_hook_session_id`), so a
// conversation started inside the pane with `/new` is attributed to that pane
// rather than guessed from a store keyed by cwd alone.
//
// `session_start` covers startup, resume, fork, and new. Failures are
// swallowed: publishing identity must never interfere with the agent.
import { mkdirSync, writeFileSync, renameSync, unlinkSync } from "node:fs";
import { dirname, join } from "node:path";

export default function (pi) {
  const target = process.env.AOE_PI_SESSION_ID_FILE;

  const publish = (ctx) => {
    if (!target) return;
    let tmp;
    try {
      const id = ctx?.sessionManager?.getSessionId?.();
      if (!id) return;
      mkdirSync(dirname(target), { recursive: true });
      // Rename so a reader never sees a half-written id.
      tmp = join(dirname(target), `.session_id.${process.pid}.tmp`);
      writeFileSync(tmp, `${id}\n`, { mode: 0o600 });
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

  pi.on("session_start", async (_event, ctx) => publish(ctx));
}
