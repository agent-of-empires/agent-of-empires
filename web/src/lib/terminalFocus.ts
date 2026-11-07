export const FOCUS_TERMINAL_EVENT = "aoe:focus-terminal";

export type TerminalFocusTarget = "agent" | "paired" | "composer";

export interface FocusTerminalDetail {
  target: TerminalFocusTarget;
  /** The paired terminal meant; omitted, any view of the target takes it. */
  terminalIndex?: number;
}

export function dispatchFocusTerminal(target: TerminalFocusTarget, terminalIndex?: number) {
  window.dispatchEvent(
    new CustomEvent<FocusTerminalDetail>(FOCUS_TERMINAL_EVENT, {
      detail: { target, terminalIndex },
    }),
  );
}

/** Whether a focus intent is for the view of `target` at `terminalIndex`. */
export function focusIntentMatches(
  intent: FocusTerminalDetail | null | undefined,
  target: TerminalFocusTarget,
  terminalIndex?: number,
): boolean {
  return intent?.target === target && (intent.terminalIndex === undefined || intent.terminalIndex === terminalIndex);
}

// A focus intent for a target that is not mounted yet, consumed when it mounts.
let pendingFocus: FocusTerminalDetail | null = null;

export function setPendingTerminalFocus(target: TerminalFocusTarget, terminalIndex?: number) {
  pendingFocus = { target, terminalIndex };
}

export function consumePendingTerminalFocus(target: TerminalFocusTarget, terminalIndex?: number): boolean {
  if (focusIntentMatches(pendingFocus, target, terminalIndex)) {
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
