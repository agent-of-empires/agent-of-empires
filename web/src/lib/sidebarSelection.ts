// Pure multi-select model for the sidebar (Shift range, Cmd/Ctrl toggle), keyed by workspace id.

export interface SidebarSelectionState {
  selectedIds: ReadonlySet<string>;
  /** Pivot for Shift+click ranges; `null` without a selection. */
  anchorId: string | null;
}

export const EMPTY_SELECTION: SidebarSelectionState = {
  selectedIds: new Set<string>(),
  anchorId: null,
};

/** Mac Cmd and Windows/Linux Ctrl both map to the additive toggle. */
export type ClickIntent = "navigate" | "toggle" | "range" | "additive-range";

export function classifyClick(modifiers: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean }): ClickIntent {
  const mod = modifiers.metaKey || modifiers.ctrlKey;
  if (modifiers.shiftKey) return mod ? "additive-range" : "range";
  if (mod) return "toggle";
  return "navigate";
}

/** Inclusive and direction-agnostic; collapses to the target when either end is not rendered. */
export function rangeBetween(orderedIds: readonly string[], anchorId: string, targetId: string): string[] {
  const a = orderedIds.indexOf(anchorId);
  const b = orderedIds.indexOf(targetId);
  if (a === -1 || b === -1) return [targetId];
  const [start, end] = a <= b ? [a, b] : [b, a];
  return orderedIds.slice(start, end + 1);
}

export type SidebarSelectionAction =
  | { type: "toggle"; id: string }
  | { type: "navigate"; id: string }
  | { type: "select-only"; id: string }
  | {
      type: "range";
      targetId: string;
      orderedIds: readonly string[];
      /** Shift+Cmd/Ctrl adds the range to the selection. */
      additive: boolean;
    }
  | { type: "clear" }
  | { type: "prune"; validIds: ReadonlySet<string> };

export function selectionReducer(state: SidebarSelectionState, action: SidebarSelectionAction): SidebarSelectionState {
  switch (action.type) {
    case "toggle": {
      const next = new Set(state.selectedIds);
      if (next.has(action.id)) next.delete(action.id);
      else next.add(action.id);
      // The anchor follows the toggled row.
      return { selectedIds: next, anchorId: action.id };
    }
    case "range": {
      // Re-anchor on the clicked row when the anchor is missing or no longer rendered.
      const anchor =
        state.anchorId != null && action.orderedIds.includes(state.anchorId) ? state.anchorId : action.targetId;
      const range = rangeBetween(action.orderedIds, anchor, action.targetId);
      const next = action.additive ? new Set([...state.selectedIds, ...range]) : new Set(range);
      // The anchor stays put so repeated Shift+clicks pivot from the same origin.
      return { selectedIds: next, anchorId: anchor };
    }
    case "select-only":
      // Right-clicking outside the selection selects just that row, without navigating.
      return { selectedIds: new Set([action.id]), anchorId: action.id };
    case "navigate":
      // A plain click clears the selection but anchors on the clicked row.
      return { selectedIds: new Set<string>(), anchorId: action.id };
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
      return {
        selectedIds: next,
        anchorId: anchorValid ? state.anchorId : null,
      };
    }
  }
}
