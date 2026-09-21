import { useCallback, useEffect, useRef, useState, type RefObject } from "react";

/** Focuses `target` on mount and returns focus to the previously focused element on unmount. */
export function useDialogFocus(target: RefObject<HTMLElement | null>, select = false) {
  const previous = useRef<HTMLElement | null>(null);
  useEffect(() => {
    previous.current = document.activeElement as HTMLElement | null;
    target.current?.focus();
    if (select) (target.current as HTMLInputElement | null)?.select();
    return () => previous.current?.focus?.();
    // Mount-only: focus moves once when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}

/** Runs `action` with a busy flag that stays set on success (the dialog unmounts) and clears on failure. */
export function useBusyAction(action: () => Promise<void>) {
  const [busy, setBusy] = useState(false);
  const run = useCallback(async () => {
    setBusy(true);
    try {
      await action();
    } catch {
      setBusy(false);
    }
  }, [action]);
  return [busy, run] as const;
}

const OWNS_ENTER = ["INPUT", "TEXTAREA", "BUTTON"];

/** Escape cancels; Enter confirms unless a focused control already handles Enter. */
export function useConfirmKeys(onCancel: () => void, onConfirm: () => void, busy = false) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") return onCancel();
      if (e.key !== "Enter" || busy || OWNS_ENTER.includes((e.target as HTMLElement).tagName)) return;
      e.preventDefault();
      onConfirm();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, onConfirm, busy]);
}
