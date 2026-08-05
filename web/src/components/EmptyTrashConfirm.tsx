import { useCallback, useEffect, useRef } from "react";

/** Destructive confirm for Empty Trash (#3167). The prompt matches the TUI
 *  ("Permanently delete N trashed session(s)? This cannot be undone.") and it
 *  follows the same backdrop / red-confirm / Esc-Enter / focus-restore pattern
 *  as DeleteSessionDialog. That dialog is session-specific (title, branch, and
 *  cleanup checkboxes) and there is no generic ConfirmDialog in the app, so a
 *  bulk purge needs this dedicated confirm. */
export function EmptyTrashConfirm({
  sessionCount,
  onConfirm,
  onCancel,
}: {
  sessionCount: number;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);
  // A ref (not state) so a rapid Enter+click double-fire is blocked
  // synchronously, before onConfirm unmounts the dialog and any re-render
  // could run; two invocations would otherwise issue duplicate deletes.
  const firedRef = useRef(false);
  const confirm = useCallback(() => {
    if (firedRef.current) return;
    firedRef.current = true;
    onConfirm();
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
        if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.tagName === "BUTTON")) {
          return;
        }
        e.preventDefault();
        confirm();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onCancel, confirm]);

  const noun = sessionCount === 1 ? "session" : "sessions";
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="empty-trash-dialog-title"
      data-testid="empty-trash-dialog"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onCancel}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[420px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id="empty-trash-dialog-title" className="text-sm font-semibold text-status-error">
            Empty Trash
          </h2>
        </div>
        <div className="px-5 py-4">
          <p className="text-[13px] text-text-secondary">
            Permanently delete {sessionCount} trashed {noun}? This cannot be undone.
          </p>
        </div>
        <div className="flex justify-end gap-3 px-5 py-3 border-t border-surface-700">
          <button
            type="button"
            onClick={onCancel}
            className="px-3 py-1.5 text-sm text-text-secondary hover:text-text-primary rounded-md hover:bg-surface-700/50 cursor-pointer transition-colors"
          >
            Cancel
          </button>
          <button
            type="button"
            ref={confirmButtonRef}
            onClick={confirm}
            data-testid="empty-trash-confirm"
            className="px-3 py-1.5 text-sm text-white rounded-md cursor-pointer transition-colors bg-status-error/90 hover:bg-status-error"
          >
            Empty Trash
          </button>
        </div>
      </div>
    </div>
  );
}
