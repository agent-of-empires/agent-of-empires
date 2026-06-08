// Centralised localStorage helpers that swallow QuotaExceededError, private-mode
// SecurityError, and other storage-disabled throws. Use these for any non-critical
// write where best-effort persistence is acceptable. Modules that need to know
// whether the write succeeded (e.g. structured view state cache for eviction-on-quota)
// can branch on safeSetItem's boolean return. Modules that must hard-fail on
// quota (token.ts auth secret, deviceBinding.ts) continue to call
// `window.localStorage.setItem` directly with an `eslint-disable-next-line` and
// keep their own rethrow contracts.

function getStorage(): Storage | null {
  const ls = (globalThis as { localStorage?: Storage }).localStorage;
  return ls ?? null;
}

export function safeSetItem(key: string, value: string): boolean {
  const storage = getStorage();
  if (!storage) return false;
  try {
    storage.setItem(key, value);
    return true;
  } catch {
    return false;
  }
}

export function safeRemoveItem(key: string): void {
  const storage = getStorage();
  if (!storage) return;
  try {
    storage.removeItem(key);
  } catch {
    // private mode or storage disabled; non-fatal
  }
}

export function safeGetItem(key: string): string | null {
  const storage = getStorage();
  if (!storage) return null;
  try {
    return storage.getItem(key);
  } catch {
    return null;
  }
}

export function isQuotaExceededError(err: unknown): boolean {
  if (!(err instanceof DOMException)) return false;
  return (
    err.name === "QuotaExceededError" ||
    err.name === "NS_ERROR_DOM_QUOTA_REACHED" ||
    err.code === 22 ||
    err.code === 1014
  );
}
