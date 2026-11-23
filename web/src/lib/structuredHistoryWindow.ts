import { DEFAULT_HISTORY_WINDOW } from "./acpHistoryWindow";

export const MIN_STRUCTURED_HISTORY_WINDOW = 50;
export const MAX_STRUCTURED_HISTORY_WINDOW = 1000;

export function normalizeStructuredHistoryWindow(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value)) return DEFAULT_HISTORY_WINDOW;
  return Math.min(MAX_STRUCTURED_HISTORY_WINDOW, Math.max(MIN_STRUCTURED_HISTORY_WINDOW, Math.round(value)));
}
