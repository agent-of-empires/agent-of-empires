export const FOCUS_TERMINAL_EVENT = "aoe:focus-terminal";

export type TerminalFocusTarget = "agent" | "paired" | "composer";

export interface FocusTerminalDetail {
  target: TerminalFocusTarget;
}

export function dispatchFocusTerminal(target: TerminalFocusTarget) {
  window.dispatchEvent(
    new CustomEvent<FocusTerminalDetail>(FOCUS_TERMINAL_EVENT, {
      detail: { target },
    }),
  );
}

// When the target component is not mounted yet (the right panel is
// collapsed so the paired terminal is gone, or a freshly selected session's
// terminal/composer is still resolving), dispatching a focus event has no
// listener to receive it. The caller stashes the intent here, and the target
// (PairedTerminal, TerminalView, or the structured view Composer) consumes it once it
// mounts and is ready.
let pendingFocus: TerminalFocusTarget | null = null;

export function setPendingTerminalFocus(target: TerminalFocusTarget) {
  pendingFocus = target;
}

export function consumePendingTerminalFocus(
  target: TerminalFocusTarget,
): boolean {
  if (pendingFocus === target) {
    pendingFocus = null;
    return true;
  }
  return false;
}

// Focus the canonical input for a freshly selected session: the structured view
// composer in structured view mode, the xterm textarea otherwise. Sets the pending
// latch (consumed on mount when the target is still resolving) and dispatches
// (handled immediately when the target is already mounted, e.g. re-selecting
// the active session). No-ops when there is no session or on coarse pointers,
// so a session swap never pops the soft keyboard (#1178).
export function requestSessionInputFocus(
  session: { view?: "structured" | "terminal" } | undefined,
  isCoarse: boolean,
): void {
  if (!session || isCoarse) return;
  const target: TerminalFocusTarget =
    session.view === "structured" ? "composer" : "agent";
  setPendingTerminalFocus(target);
  dispatchFocusTerminal(target);
}
