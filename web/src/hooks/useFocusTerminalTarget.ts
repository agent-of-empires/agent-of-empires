import { useEffect } from "react";
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  setPendingTerminalFocus,
  type FocusTerminalDetail,
  type TerminalFocusTarget,
} from "../lib/terminalFocus";
import { listen } from "./domEvents";

/** `active` false (a hidden retained view) ignores focus events and keeps the latch for its own activation. */
export function useFocusTerminalTarget(
  target: TerminalFocusTarget,
  ref: React.RefObject<HTMLElement | null>,
  active = true,
): void {
  useEffect(() => {
    if (!active) return;
    const onFocusEvent = (e: Event) => {
      const detail = (e as CustomEvent<FocusTerminalDetail>).detail;
      if (detail?.target !== target) return;
      const el = ref.current;
      if (el) el.focus();
      else setPendingTerminalFocus(target);
    };
    return listen(onFocusEvent, [window, FOCUS_TERMINAL_EVENT]);
  }, [target, ref, active]);

  // Spends the latch on activation, which no event handler sees.
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (active && consumePendingTerminalFocus(target)) ref.current?.focus();
  }, [target, ref, active]);
}
