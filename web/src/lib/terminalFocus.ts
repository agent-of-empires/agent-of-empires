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

// A focus intent for a target that is not mounted yet, consumed when it mounts.
let pendingFocus: TerminalFocusTarget | null = null;

export function setPendingTerminalFocus(target: TerminalFocusTarget) {
  pendingFocus = target;
}

export function consumePendingTerminalFocus(target: TerminalFocusTarget): boolean {
  if (pendingFocus === target) {
    pendingFocus = null;
    return true;
  }
  return false;
}

// Focus the new session's input (composer or xterm) now or when it mounts. Skipped on coarse pointers so a session swap never pops the keyboard.
export function requestSessionInputFocus(
  session: { view?: "structured" | "terminal" } | undefined,
  isCoarse: boolean,
): void {
  if (!session || isCoarse) return;
  const target: TerminalFocusTarget = session.view === "structured" ? "composer" : "agent";
  setPendingTerminalFocus(target);
  dispatchFocusTerminal(target);
}
