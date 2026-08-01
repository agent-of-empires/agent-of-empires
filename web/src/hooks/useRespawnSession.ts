import { useState } from "react";

export type RespawnState = "idle" | "retrying" | "ok" | "failed";

interface RespawnSnapshot {
  resetKey: string | null;
  /** Bumped every time an incident ends, so a request that outlives its
   *  incident can be told apart from the next one even when both share a
   *  resetKey. */
  incident: number;
  state: RespawnState;
  error: string | null;
}

/** Shared respawn machine for the structured-view recovery banners.
 *  POSTs to `/acp/spawn` (which re-runs the ACP handshake) and tracks the
 *  idle/retrying/ok/failed lifecycle. A fresh `AcpSessionAssigned` or a
 *  `UserPromptSent` clears the recovery banners on the reducer side
 *  (`applyNewTurnResets` and the `AcpSessionAssigned` arm null the
 *  worker-stopped, startup-error, and rate-limit state), so callers only
 *  need to fire `respawn` and reflect `state`/`error`. Extracted so the
 *  WorkerStopped, StartupError, and compat-failure screens share one
 *  implementation instead of three copies. `resetKey` lets callers scope
 *  status to one recovery incident, e.g. a specific rate-limit reset time,
 *  without one render of stale ok or failed state. See #2109.
 *
 *  Any `resetKey` change ends the incident and drops its stored status; null
 *  additionally means no incident is active. Callers whose key is not unique
 *  per incident (a rate limit with no reported reset keys on a literal,
 *  #3152) rely on that to keep one incident's ok/failed out of the next. */
export function useRespawnSession(sessionId: string, resetKey: string | null = null) {
  const [snapshot, setSnapshot] = useState<RespawnSnapshot>({
    resetKey,
    incident: 0,
    state: "idle",
    error: null,
  });
  // Drop a stored status once the incident it belongs to is over. Any key
  // change ends an incident, not just the null the reducer sets on the next
  // prompt: a reported reset can be replaced by a later one, or by the
  // "unknown" literal, with no null in between. Adjusting during render
  // (React's documented pattern for resetting state on a prop change) rather
  // than in an effect: no extra commit, and the next line reads the cleared
  // value immediately.
  if (resetKey !== snapshot.resetKey) {
    setSnapshot({ resetKey, incident: snapshot.incident + 1, state: "idle", error: null });
  }
  const isCurrent = snapshot.resetKey === resetKey;
  const state = isCurrent ? snapshot.state : "idle";
  const error = isCurrent ? snapshot.error : null;

  const respawn = async (): Promise<boolean> => {
    const activeResetKey = resetKey;
    const activeIncident = snapshot.incident;
    // A request can outlive the incident it was started for (the user leaves
    // the banner open, the limit clears, the next one arrives). Its late
    // completion must not write into the new incident, which the resetKey
    // alone cannot rule out because an unreported reset keys on a literal.
    const settle = (state: RespawnState, error: string | null) =>
      setSnapshot((prev) =>
        prev.incident === activeIncident ? { resetKey: activeResetKey, incident: activeIncident, state, error } : prev,
      );
    settle("retrying", null);
    try {
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/spawn`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({}),
      });
      if (res.ok) {
        settle("ok", null);
        return true;
      }
      const detail = (await res.text().catch(() => "")).slice(0, 200);
      settle("failed", `Server returned ${res.status}. ${detail}`.trim());
      return false;
    } catch (e) {
      settle("failed", e instanceof Error ? e.message : String(e));
      return false;
    }
  };

  return { state, error, respawn };
}
