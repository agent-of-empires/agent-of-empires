// Open signal for the Composer's "Switch agent" dialog, with a pending latch (like terminalFocus.ts) for when the target Composer mounts after navigation.

export const OPEN_SWITCH_AGENT_EVENT = "aoe:open-switch-agent";

export interface OpenSwitchAgentDetail {
  sessionId: string;
}

let pendingSwitchAgent: string | null = null;
const listeners = new Set<() => void>();

function emitPendingSwitchAgent(): void {
  listeners.forEach((listener) => listener());
}

export function requestSwitchAgent(sessionId: string): void {
  if (typeof window === "undefined") return;
  pendingSwitchAgent = sessionId;
  emitPendingSwitchAgent();
  window.dispatchEvent(
    new CustomEvent<OpenSwitchAgentDetail>(OPEN_SWITCH_AGENT_EVENT, {
      detail: { sessionId },
    }),
  );
}

export function subscribePendingSwitchAgent(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getPendingSwitchAgent(): string | null {
  return pendingSwitchAgent;
}

export function clearPendingSwitchAgent(): void {
  if (pendingSwitchAgent === null) return;
  pendingSwitchAgent = null;
  emitPendingSwitchAgent();
}

// Consumes the latch when it targets this session.
export function consumePendingSwitchAgent(sessionId: string): boolean {
  if (pendingSwitchAgent === sessionId) {
    clearPendingSwitchAgent();
    return true;
  }
  return false;
}
