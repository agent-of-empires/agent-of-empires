// Pure selection model for the sidebar's multi-select (Shift+click range,
// Cmd/Ctrl+click additive toggle). Kept free of React so the gesture
// semantics are unit-testable, mirroring how sidebarSort.ts holds the pure
// sort/triage helpers. Selection is keyed by workspace id (rows are workspace
// rows); resolution to a session id happens at action time. See #1724.

export interface SidebarSelectionState {
  /** Currently selected workspace ids. */
  selectedIds: ReadonlySet<string>;
  /** Pivot for Shift+click range selection: the last row the user toggled or
   *  the start of the most recent range. `null` when there is no selection. */
  anchorId: string | null;
}

export const EMPTY_SELECTION: SidebarSelectionState = {
  selectedIds: new Set<string>(),
  anchorId: null,
};

/** What a row click means, derived purely from its modifier keys. The parent
 *  classifies the event and dispatches the matching reducer action (and, for
 *  `navigate`, also performs route navigation). Mac uses Cmd, Windows/Linux
 *  use Ctrl; both map to the additive toggle. */
export type ClickIntent = "navigate" | "toggle" | "range" | "additive-range";

export function classifyClick(modifiers: {
  metaKey: boolean;
  ctrlKey: boolean;
  shiftKey: boolean;
}): ClickIntent {
  const mod = modifiers.metaKey || modifiers.ctrlKey;
  if (modifiers.shiftKey) return mod ? "additive-range" : "range";
  if (mod) return "toggle";
  return "navigate";
}

/** Inclusive id range between `anchorId` and `targetId` within the rendered
 *  order. Direction-agnostic (anchor may be above or below the target). If
 *  either endpoint is missing from `orderedIds` (e.g. it scrolled into a
 *  collapsed group since the anchor was set) the range collapses to just the
 *  target, which is the least surprising fallback. */
export function rangeBetween(
  orderedIds: readonly string[],
  anchorId: string,
  targetId: string,
): string[] {
  const a = orderedIds.indexOf(anchorId);
  const b = orderedIds.indexOf(targetId);
  if (a === -1 || b === -1) return [targetId];
  const [start, end] = a <= b ? [a, b] : [b, a];
  return orderedIds.slice(start, end + 1);
}

export type SidebarSelectionAction =
  | { type: "toggle"; id: string }
  | {
      type: "range";
      targetId: string;
      orderedIds: readonly string[];
      /** Add the range to the existing selection instead of replacing it
       *  (Shift+Cmd/Ctrl). */
      additive: boolean;
    }
  | { type: "clear" }
  | { type: "prune"; validIds: ReadonlySet<string> };

export function selectionReducer(
  state: SidebarSelectionState,
  action: SidebarSelectionAction,
): SidebarSelectionState {
  switch (action.type) {
    case "toggle": {
      const next = new Set(state.selectedIds);
      if (next.has(action.id)) next.delete(action.id);
      else next.add(action.id);
      // Anchor follows the toggled row so a subsequent Shift+click ranges
      // from here.
      return { selectedIds: next, anchorId: action.id };
    }
    case "range": {
      // Pivot from the existing anchor; if there is none, or it scrolled out
      // of the rendered order (collapsed group, filter), re-anchor on the
      // clicked row so the next Shift+click forms a range from here instead
      // of repeatedly collapsing to a single row.
      const anchor =
        state.anchorId != null && action.orderedIds.includes(state.anchorId)
          ? state.anchorId
          : action.targetId;
      const range = rangeBetween(action.orderedIds, anchor, action.targetId);
      const next = action.additive
        ? new Set([...state.selectedIds, ...range])
        : new Set(range);
      // Anchor stays put so repeated Shift+clicks re-pivot from the same
      // origin, matching Finder / file-manager behavior.
      return { selectedIds: next, anchorId: anchor };
    }
    case "clear":
      return EMPTY_SELECTION;
    case "prune": {
      let changed = false;
      const next = new Set<string>();
      for (const id of state.selectedIds) {
        if (action.validIds.has(id)) next.add(id);
        else changed = true;
      }
      const anchorValid = state.anchorId != null && action.validIds.has(state.anchorId);
      if (!anchorValid && state.anchorId != null) changed = true;
      if (!changed) return state;
      return { selectedIds: next, anchorId: anchorValid ? state.anchorId : null };
    }
  }
}
