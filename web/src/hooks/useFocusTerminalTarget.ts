import { useEffect } from "react";
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  setPendingTerminalFocus,
  type FocusTerminalDetail,
  type TerminalFocusTarget,
} from "../lib/terminalFocus";

/**
 * Wire a focusable element to the terminalFocus bus for a given target.
 *
 * Registers a {@link FOCUS_TERMINAL_EVENT} listener that focuses `ref` when a
 * matching-target focus is dispatched (handling the already-mounted case, for
 * example re-selecting the active session), and consumes the pending-focus
 * latch on mount (handling the first-open race where this component mounts
 * after the dispatch). If the element is not present when the event fires, the
 * intent is stashed back on the latch.
 *
 * Mirrors the inline wiring TerminalView uses for the "agent" target; the
 * structured view Composer uses it for "composer".
 */
export function useFocusTerminalTarget(
  target: TerminalFocusTarget,
  ref: React.RefObject<HTMLElement | null>,
): void {
  useEffect(() => {
    const onFocusEvent = (e: Event) => {
      const detail = (e as CustomEvent<FocusTerminalDetail>).detail;
      if (detail?.target !== target) return;
      const el = ref.current;
      if (el) el.focus();
      else setPendingTerminalFocus(target);
    };
    window.addEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
    return () => window.removeEventListener(FOCUS_TERMINAL_EVENT, onFocusEvent);
  }, [target, ref]);

  useEffect(() => {
    if (consumePendingTerminalFocus(target)) ref.current?.focus();
  }, [target, ref]);
}
