import { useState } from "react";

export type RespawnState = "idle" | "retrying" | "ok" | "failed";

interface RespawnSnapshot {
  resetKey: string | null;
  state: RespawnState;
  error: string | null;
}

/** Shared respawn machine for the structured-view recovery banners.
 *  POSTs to `/acp/spawn` (which re-runs the ACP handshake) and tracks the
 *  idle/retrying/ok/failed lifecycle. The next `AcpSessionAssigned` (or
 *  user prompt) clears the banner on the reducer side, so callers only need
 *  to fire `respawn` and reflect `state`/`error`. Extracted so the
 *  WorkerStopped, StartupError, and compat-failure screens share one
 *  implementation instead of three copies. `resetKey` lets callers scope
 *  status to one recovery incident, e.g. a specific rate-limit reset time,
 *  without one render of stale ok or failed state. See #2109. */
export function useRespawnSession(sessionId: string, resetKey: string | null = null) {
  const [snapshot, setSnapshot] = useState<RespawnSnapshot>({
    resetKey,
    state: "idle",
    error: null,
  });
  const isCurrent = snapshot.resetKey === resetKey;
  const state = isCurrent ? snapshot.state : "idle";
  const error = isCurrent ? snapshot.error : null;

  const respawn = async (): Promise<boolean> => {
    const activeResetKey = resetKey;
    setSnapshot({ resetKey: activeResetKey, state: "retrying", error: null });
    try {
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/spawn`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({}),
      });
      if (res.ok) {
        setSnapshot({ resetKey: activeResetKey, state: "ok", error: null });
        return true;
      }
      const detail = (await res.text().catch(() => "")).slice(0, 200);
      setSnapshot({
        resetKey: activeResetKey,
        state: "failed",
        error: `Server returned ${res.status}. ${detail}`.trim(),
      });
      return false;
    } catch (e) {
      setSnapshot({
        resetKey: activeResetKey,
        state: "failed",
        error: e instanceof Error ? e.message : String(e),
      });
      return false;
    }
  };

  return { state, error, respawn };
}
