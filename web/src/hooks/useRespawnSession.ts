import { useState } from "react";

export type RespawnState = "idle" | "retrying" | "ok" | "failed";

/** Shared respawn machine for the structured-view recovery banners.
 *  POSTs to `/acp/spawn` (which re-runs the ACP handshake) and tracks the
 *  idle/retrying/ok/failed lifecycle. The next `AcpSessionAssigned` (or
 *  user prompt) clears the banner on the reducer side, so callers only need
 *  to fire `respawn` and reflect `state`/`error`. Extracted so the
 *  WorkerStopped, StartupError, and compat-failure screens share one
 *  implementation instead of three copies. See #2109. */
export function useRespawnSession(sessionId: string) {
  const [state, setState] = useState<RespawnState>("idle");
  const [error, setError] = useState<string | null>(null);

  const respawn = async (): Promise<boolean> => {
    setState("retrying");
    setError(null);
    try {
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/spawn`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({}),
      });
      if (res.ok) {
        setState("ok");
        return true;
      }
      const detail = (await res.text().catch(() => "")).slice(0, 200);
      setState("failed");
      setError(`Server returned ${res.status}. ${detail}`.trim());
      return false;
    } catch (e) {
      setState("failed");
      setError(e instanceof Error ? e.message : String(e));
      return false;
    }
  };

  return { state, error, respawn };
}
