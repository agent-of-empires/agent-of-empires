import { useState } from "react";

export type RespawnState = "idle" | "retrying" | "ok" | "failed";

/** A refusal's JSON `{ "error", "message" }` body by its message; any other body as sent. */
function failureText(body: string): string {
  try {
    const message: unknown = JSON.parse(body)?.message;
    if (typeof message === "string" && message) return message;
  } catch {
    // Plain text.
  }
  return body;
}

interface RespawnSnapshot {
  resetKey: string | null;
  incident: number;
  state: RespawnState;
  error: string | null;
  /** The failure's full body, untruncated, for comparing with the error it retried. */
  detail: string | null;
}

export function useRespawnSession(sessionId: string, resetKey: string | null = null) {
  const [snapshot, setSnapshot] = useState<RespawnSnapshot>({
    resetKey,
    incident: 0,
    state: "idle",
    error: null,
    detail: null,
  });
  if (resetKey !== snapshot.resetKey) {
    setSnapshot({ resetKey, incident: snapshot.incident + 1, state: "idle", error: null, detail: null });
  }
  const isCurrent = snapshot.resetKey === resetKey;
  const state = isCurrent ? snapshot.state : "idle";
  const error = isCurrent ? snapshot.error : null;
  const detail = isCurrent ? snapshot.detail : null;

  const respawn = async (): Promise<boolean> => {
    const activeResetKey = resetKey;
    const activeIncident = snapshot.incident;
    // A request can outlive its incident; don't let it write into the next one.
    const settle = (state: RespawnState, error: string | null, detail: string | null = error) =>
      setSnapshot((prev) =>
        prev.incident === activeIncident
          ? { resetKey: activeResetKey, incident: activeIncident, state, error, detail }
          : prev,
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
      const body = failureText(await res.text().catch(() => ""));
      settle("failed", `Server returned ${res.status}. ${body.slice(0, 200)}`.trim(), body);
      return false;
    } catch (e) {
      settle("failed", e instanceof Error ? e.message : String(e));
      return false;
    }
  };

  return { state, error, detail, respawn };
}
