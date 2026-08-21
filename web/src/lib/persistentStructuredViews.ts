export const MIN_PERSISTENT_STRUCTURED_VIEWS = 1;
export const MAX_PERSISTENT_STRUCTURED_VIEWS = 5;
export const DEFAULT_PERSISTENT_STRUCTURED_VIEWS = 2;

export function normalizePersistentStructuredViewLimit(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return DEFAULT_PERSISTENT_STRUCTURED_VIEWS;
  }
  return Math.min(MAX_PERSISTENT_STRUCTURED_VIEWS, Math.max(MIN_PERSISTENT_STRUCTURED_VIEWS, Math.round(value)));
}
