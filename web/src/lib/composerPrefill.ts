const pending = new Map<string, string[]>();
const listeners = new Set<() => void>();

/** Append only after an explicit user action; never submit or replace an existing draft. */
export function requestComposerPrefill(sessionId: string, text: string): void {
  pending.set(sessionId, [...(pending.get(sessionId) ?? []), text]);
  listeners.forEach((listener) => listener());
}

export function consumeComposerPrefill(sessionId: string): string | null {
  const texts = pending.get(sessionId);
  pending.delete(sessionId);
  return texts?.join("\n\n") ?? null;
}

export function subscribeComposerPrefill(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
