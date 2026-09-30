// Publish the native ID and transcript path without requiring a materialized file,
// and report context resets, prompts, and turns to AoE's usage log.
import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync, renameSync, unlinkSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

export default function (pi) {
  const idTarget = process.env.AOE_SESSION_ID_FILE;
  const usageBin = process.env.AOE_USAGE_BIN;
  const rootOnly = process.env.AOE_SESSION_ROOT_ONLY === "1";
  const source = process.env.AOE_SESSION_SOURCE;
  if (!rootOnly && source !== undefined && !/^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$/.test(source)) return;
  const suffix = !rootOnly && source ? "." + source : "";
  const sessionId = (ctx) => {
    try {
      return ctx?.sessionManager?.getSessionId?.() || undefined;
    } catch {
      return undefined;
    }
  };
  // Fire and forget: the agent never waits on the usage log.
  const report = (payload) => {
    if (!usageBin || !process.env.AOE_INSTANCE_ID) return;
    try {
      const child = spawn(usageBin, ["__usage-event", "--agent", "pi"], {
        stdio: ["pipe", "ignore", "ignore"],
        detached: true,
      });
      child.on("error", () => {});
      child.stdin.on("error", () => {});
      child.stdin.end(JSON.stringify(payload));
      child.unref();
    } catch {
      // never block the agent
    }
  };
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

  pi.on("session_start", async (event, ctx) => {
    report({ hook_event_name: "session_start", reason: event?.reason, session_id: sessionId(ctx) });
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
        writeAtomic(idTarget + suffix, id);
        if (file) writeAtomic(join(dirname(idTarget), "session_path" + suffix), file);
      }
    } catch {
      // never block the agent
    }
  });
  pi.on("session_shutdown", async (event, ctx) => {
    report({ hook_event_name: "session_shutdown", reason: event?.reason, session_id: sessionId(ctx) });
  });
  pi.on("session_compact", async (event, ctx) => {
    report({ hook_event_name: "session_compact", reason: event?.reason, session_id: sessionId(ctx) });
  });
  pi.on("input", async (event, ctx) => {
    report({ hook_event_name: "input", source: event?.source, session_id: sessionId(ctx) });
  });
  pi.on("agent_settled", async (_event, ctx) => {
    report({ hook_event_name: "agent_settled", session_id: sessionId(ctx) });
  });
}
