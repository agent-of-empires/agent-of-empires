export const MIN_PERSISTENT_TERMINALS = 1;
export const MAX_PERSISTENT_TERMINALS = 50;
export const DEFAULT_PERSISTENT_TERMINALS = 5;

export function normalizePersistentTerminalLimit(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return DEFAULT_PERSISTENT_TERMINALS;
  }
  return Math.min(
    MAX_PERSISTENT_TERMINALS,
    Math.max(MIN_PERSISTENT_TERMINALS, Math.round(value)),
  );
}
