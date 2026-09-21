import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";

interface Props {
  /** Prefix for the `-dialog` and `-proceed` test ids. */
  id: string;
  title: string;
  confirmLabel: string;
  onConfirm: () => Promise<void> | void;
  onCancel: () => void;
  children: ReactNode;
}

/** Shared modal shell for confirmations that pause a session create. */
export function ConfirmCreateDialog({ id, title, confirmLabel, onConfirm, onCancel, children }: Props) {
  const [confirming, setConfirming] = useState(false);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  const handleConfirm = useCallback(async () => {
    setConfirming(true);
    try {
      await onConfirm();
    } catch {
      setConfirming(false);
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
        const tag = (e.target as HTMLElement | null)?.tagName;
        if (tag === "INPUT" || tag === "TEXTAREA" || tag === "BUTTON") return;
        if (confirming) return;
        e.preventDefault();
        void handleConfirm();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, handleConfirm, confirming]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby={`${id}-dialog-title`}
      data-testid={`${id}-dialog`}
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onCancel}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[460px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id={`${id}-dialog-title`} className="text-sm font-semibold text-status-warning">
            {title}
          </h2>
        </div>

        <div className="px-5 py-4 space-y-3">{children}</div>

        <div className="flex justify-end gap-3 px-5 py-3 border-t border-surface-700">
          <button
            onClick={onCancel}
            disabled={confirming}
            className="px-3 py-1.5 text-sm text-text-secondary hover:text-text-primary rounded-md hover:bg-surface-700/50 cursor-pointer transition-colors disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            ref={confirmButtonRef}
            onClick={handleConfirm}
            disabled={confirming}
            data-testid={`${id}-proceed`}
            className="px-3 py-1.5 text-sm text-surface-900 bg-green-500 hover:bg-green-600 active:bg-green-700 rounded-md cursor-pointer transition-colors disabled:opacity-50 flex items-center gap-2"
          >
            {confirming && (
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
            {confirming ? "Creating..." : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
