import { useCallback, useEffect, useRef, useState } from "react";
import { switchViewCopy } from "../lib/acpKeepContext";

interface Props {
  sessionTitle: string;
  /** Switch direction: true = terminal -> structured, false = structured -> terminal. */
  toStructured: boolean;
  /** Whether the pairing preserves the conversation (claude). Drives the copy. */
  keepsContext: boolean;
  onConfirm: () => Promise<void>;
  onCancel: () => void;
}

/** Confirmation for switching a session's view, matching the TUI's
 *  `prompt_switch_view_for_selected`. The body is capability-aware: claude keeps
 *  the conversation across the swap (both directions), other agents restart
 *  fresh. Mirrors StopSessionDialog's focus/escape/enter handling. */
export function SwitchViewDialog({ sessionTitle, toStructured, keepsContext, onConfirm, onCancel }: Props) {
  const [switching, setSwitching] = useState(false);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const copy = switchViewCopy(toStructured, keepsContext);

  const handleConfirm = useCallback(async () => {
    setSwitching(true);
    try {
      await onConfirm();
    } catch {
      setSwitching(false);
    }
  }, [onConfirm]);

  useEffect(() => {
    previousFocusRef.current = document.activeElement as HTMLElement | null;
    confirmButtonRef.current?.focus();
    return () => {
      previousFocusRef.current?.focus?.();
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onCancel();
        return;
      }
      if (e.key === "Enter") {
        const target = e.target as HTMLElement | null;
        if (target && target.tagName === "BUTTON") return;
        if (switching) return;
        e.preventDefault();
        void handleConfirm();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, handleConfirm, switching]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="switch-view-dialog-title"
      data-testid="switch-view-dialog"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onCancel}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[420px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id="switch-view-dialog-title" className="text-sm font-semibold text-text-primary">
            {copy.title}
          </h2>
        </div>

        <div className="px-5 py-4">
          <p className="text-[13px] text-text-secondary">
            <span className="font-mono text-text-primary">{sessionTitle}</span>: {copy.body}
          </p>
        </div>

        <div className="flex justify-end gap-3 px-5 py-3 border-t border-surface-700">
          <button
            onClick={onCancel}
            disabled={switching}
            className="px-3 py-1.5 text-sm text-text-secondary hover:text-text-primary rounded-md hover:bg-surface-700/50 cursor-pointer transition-colors disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            ref={confirmButtonRef}
            onClick={handleConfirm}
            disabled={switching}
            data-testid="switch-view-confirm"
            className="px-3 py-1.5 text-sm text-surface-950 bg-brand-600 hover:bg-brand-500 rounded-md cursor-pointer transition-colors disabled:opacity-50 flex items-center gap-2"
          >
            {switching && (
              <svg className="animate-spin h-3.5 w-3.5" viewBox="0 0 24 24">
                <circle
                  className="opacity-25"
                  cx="12"
                  cy="12"
                  r="10"
                  stroke="currentColor"
                  strokeWidth="4"
                  fill="none"
                />
                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
              </svg>
            )}
            {switching ? "Switching..." : copy.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
