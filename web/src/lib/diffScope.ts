import { createContext, useContext } from "react";

import { safeGetItem, safeSetItem } from "./safeStorage";

/** What the Diff pane compares the working tree against; the server's `?scope=`. */
export type DiffScope = "branch" | "uncommitted" | "last_turn";

export const DIFF_SCOPES: readonly { scope: DiffScope; label: string; title: string }[] = [
  { scope: "branch", label: "Branch", title: "Changes on this branch since it left its base" },
  { scope: "uncommitted", label: "Uncommitted", title: "Changes not yet committed" },
  { scope: "last_turn", label: "Last turn", title: "Changes since the agent's latest turn started" },
];

export const NO_LAST_TURN = "No turn has started since the daemon started";

// Per-browser view state, like the open file: which comparison this browser is looking at for each
// Session. The base branch it compares against stays a Daemon setting.
const KEY = "aoe-diff-scope";

function read(): Record<string, DiffScope> {
  try {
    const parsed = JSON.parse(safeGetItem(KEY) ?? "{}") as Record<string, unknown>;
    return Object.fromEntries(
      Object.entries(parsed).filter((e): e is [string, DiffScope] => DIFF_SCOPES.some((s) => s.scope === e[1])),
    );
  } catch {
    return {};
  }
}

export function loadDiffScope(sessionId: string | null): DiffScope {
  return (sessionId && read()[sessionId]) || "branch";
}

/** Branch is the default, so choosing it forgets the Session. */
export function saveDiffScope(sessionId: string, scope: DiffScope): void {
  const all = read();
  if (scope === "branch") delete all[sessionId];
  else all[sessionId] = scope;
  safeSetItem(KEY, JSON.stringify(all));
}

export function sweepOrphanDiffScopes(live: ReadonlySet<string>): void {
  const all = read();
  const kept = Object.entries(all).filter(([id]) => live.has(id));
  if (kept.length !== Object.keys(all).length) safeSetItem(KEY, JSON.stringify(Object.fromEntries(kept)));
}

/** The scope the diff consumers show; `onChange` is absent where it can't be changed. */
export interface DiffScopeState {
  /** The scope the file list and viewer show now. */
  scope: DiffScope;
  /** The scope the user picked, which falls back to Branch while Last turn has no snapshot. */
  requested: DiffScope;
  /** When the latest turn's snapshot was taken; null hides Last turn behind {@link NO_LAST_TURN}. */
  lastTurnAt: string | null;
  onChange?: (scope: DiffScope) => void;
}

export const DiffScopeContext = createContext<DiffScopeState>({
  scope: "branch",
  requested: "branch",
  lastTurnAt: null,
});

export function useDiffScopeContext(): DiffScopeState {
  return useContext(DiffScopeContext);
}

/** The `?scope=` a request carries; Branch is the server's default, so it sends none. */
export function scopeParam(scope: DiffScope): string | null {
  return scope === "branch" ? null : scope;
}
