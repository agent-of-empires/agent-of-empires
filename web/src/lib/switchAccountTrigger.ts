// Open signal for the "Switch account" dialog, with a pending latch (like switchAgentTrigger.ts) for when the target view mounts after navigation.
// Also holds the restarts in flight per Session, so their status outlives an unmounted view.

let pendingSwitchAccount: string | null = null;
const listeners = new Set<() => void>();

function emit(): void {
  listeners.forEach((listener) => listener());
}

export function requestSwitchAccount(sessionId: string): void {
  pendingSwitchAccount = sessionId;
  emit();
}

const switching = new Map<string, string>();

export function setSwitchingAccount(sessionId: string, tool: string | null): void {
  if (tool === null) switching.delete(sessionId);
  else switching.set(sessionId, tool);
  emit();
}

export function getSwitchingAccount(sessionId: string): string | null {
  return switching.get(sessionId) ?? null;
}

export function subscribePendingSwitchAccount(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getPendingSwitchAccount(): string | null {
  return pendingSwitchAccount;
}

export function clearPendingSwitchAccount(): void {
  if (pendingSwitchAccount === null) return;
  pendingSwitchAccount = null;
  emit();
}
