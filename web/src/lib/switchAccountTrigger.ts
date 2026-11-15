// Open signal for the "Switch account" dialog, with a pending latch (like switchAgentTrigger.ts) for when the target view mounts after navigation.

let pendingSwitchAccount: string | null = null;
const listeners = new Set<() => void>();

function emit(): void {
  listeners.forEach((listener) => listener());
}

export function requestSwitchAccount(sessionId: string): void {
  pendingSwitchAccount = sessionId;
  emit();
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
