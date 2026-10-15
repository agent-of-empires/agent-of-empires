import { useEffect } from "react";
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  setPendingTerminalFocus,
  type FocusTerminalDetail,
  type TerminalFocusTarget,
} from "../lib/terminalFocus";
import { listen } from "./domEvents";

export function useFocusTerminalTarget(target: TerminalFocusTarget, ref: React.RefObject<HTMLElement | null>): void {
  useEffect(() => {
    const onFocusEvent = (e: Event) => {
      const detail = (e as CustomEvent<FocusTerminalDetail>).detail;
      if (detail?.target !== target) return;
      const el = ref.current;
      if (el) el.focus();
      else setPendingTerminalFocus(target);
    };
    return listen(onFocusEvent, [window, FOCUS_TERMINAL_EVENT]);
  }, [target, ref]);

  useEffect(() => {
    if (consumePendingTerminalFocus(target)) ref.current?.focus();
  }, [target, ref]);
}
