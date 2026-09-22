import { useState } from "react";

export type RespawnState = "idle" | "retrying" | "ok" | "failed";

interface RespawnSnapshot {
  resetKey: string | null;
  incident: number;
  state: RespawnState;
  error: string | null;
}

export function useRespawnSession(sessionId: string, resetKey: string | null = null) {
  const [snapshot, setSnapshot] = useState<RespawnSnapshot>({
    resetKey,
    incident: 0,
    state: "idle",
    error: null,
  });
  if (resetKey !== snapshot.resetKey) {
    setSnapshot({ resetKey, incident: snapshot.incident + 1, state: "idle", error: null });
  }
  const isCurrent = snapshot.resetKey === resetKey;
  const state = isCurrent ? snapshot.state : "idle";
  const error = isCurrent ? snapshot.error : null;

  const respawn = async (): Promise<boolean> => {
    const activeResetKey = resetKey;
    const activeIncident = snapshot.incident;
    // A request can outlive its incident; don't let it write into the next one.
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
