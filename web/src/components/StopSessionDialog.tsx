import { useCallback, useEffect, useRef, useState } from "react";

interface Props {
  sessionTitle: string;
  onConfirm: () => Promise<void>;
  onCancel: () => void;
}

/** Confirmation for stopping a session, matching the TUI's `x` keybind which
 *  prompts "Are you sure you want to stop '<title>'?". Stop kills the running
 *  agent but preserves the session record (it can be resumed), so this is a
 *  lighter modal than DeleteSessionDialog with no cleanup options. */
export function StopSessionDialog({ sessionTitle, onConfirm, onCancel }: Props) {
  const [stopping, setStopping] = useState(false);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  const handleConfirm = useCallback(async () => {
    setStopping(true);
    try {
      await onConfirm();
    } catch {
      setStopping(false);
    }
  }, [onConfirm]);

  // Capture the previously focused element on mount and restore focus on
  // unmount so keyboard users return to the trigger (the context-menu item)
  // instead of losing focus to document.body.
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
        // The focused confirm button already activates on Enter; only handle
        // Enter when focus is elsewhere so we don't double-fire.
        const target = e.target as HTMLElement | null;
        if (target && target.tagName === "BUTTON") return;
        if (stopping) return;
        e.preventDefault();
        void handleConfirm();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, handleConfirm, stopping]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="stop-session-dialog-title"
      data-testid="stop-session-dialog"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onCancel}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[420px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id="stop-session-dialog-title" className="text-sm font-semibold text-text-primary">
            Stop Session
          </h2>
        </div>

        <div className="px-5 py-4">
          <p className="text-[13px] text-text-secondary">
            Are you sure you want to stop <span className="font-mono text-text-primary">{sessionTitle}</span>? The agent
            stops, but the session is kept and can be resumed later.
          </p>
        </div>

        <div className="flex justify-end gap-3 px-5 py-3 border-t border-surface-700">
          <button
            onClick={onCancel}
            disabled={stopping}
            className="px-3 py-1.5 text-sm text-text-secondary hover:text-text-primary rounded-md hover:bg-surface-700/50 cursor-pointer transition-colors disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            ref={confirmButtonRef}
            onClick={handleConfirm}
            disabled={stopping}
            className="px-3 py-1.5 text-sm text-surface-950 bg-brand-600 hover:bg-brand-500 rounded-md cursor-pointer transition-colors disabled:opacity-50 flex items-center gap-2"
          >
            {stopping && (
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
            {stopping ? "Stopping..." : "Stop"}
          </button>
        </div>
      </div>
    </div>
  );
}
