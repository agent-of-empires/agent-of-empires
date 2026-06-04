import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";

// Persisted manual order of repo groups in the sidebar, client-only and
// per-browser, mirroring the collapsed/appearance state in useRepoGroups
// (the server stores only the flat workspace ordering, not group order).
// A list of real repo-group ids (filesystem repo paths); synthetic
// groups (Multi-repo, Scratch) are never stored here because they are
// hard-pinned to the bottom regardless of manual order. See #1644.
const STORAGE_KEY = "aoe-repo-group-order-v1";

export function loadRepoGroupOrder(): string[] {
  const raw = safeGetItem(STORAGE_KEY);
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((id): id is string => typeof id === "string");
  } catch {
    return [];
  }
}

export function persistRepoGroupOrder(order: readonly string[]): void {
  if (order.length === 0) {
    safeRemoveItem(STORAGE_KEY);
    return;
  }
  safeSetItem(STORAGE_KEY, JSON.stringify(order));
}
