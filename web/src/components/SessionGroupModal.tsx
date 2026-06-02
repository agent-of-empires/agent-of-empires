import { useCallback, useEffect, useRef, useState } from "react";

interface Props {
  sessionTitle: string;
  currentGroup: string;
  /** Persist the new group. Returns false on failure so the modal can
   *  surface an error and stay open instead of silently dropping the
   *  change. */
  onSave: (group: string) => Promise<boolean>;
  onClose: () => void;
}

export function SessionGroupModal({
  sessionTitle,
  currentGroup,
  onSave,
  onClose,
}: Props) {
  const [value, setValue] = useState(currentGroup);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  const handleSave = useCallback(async () => {
    // Trim so a field cleared to blank (or whitespace) ungroups. A
    // non-empty path is sent as-is, matching the wizard group field and
    // the server, which apply no slash normalization.
    const next = value.trim();
    if (next === currentGroup) {
      onClose();
      return;
    }
    setSaving(true);
    setError(null);
    const ok = await onSave(next);
    if (!ok) {
      setSaving(false);
      setError(
        next ? "Failed to update group." : "Failed to clear group.",
      );
      inputRef.current?.focus();
      return;
    }
    onClose();
  }, [value, currentGroup, onSave, onClose]);

  // Capture the previously focused element on mount and restore focus on
  // unmount so keyboard users return to the trigger (the context-menu
  // item) instead of losing focus to document.body. Mirrors
  // DeleteSessionDialog.
  useEffect(() => {
    previousFocusRef.current = document.activeElement as HTMLElement | null;
    inputRef.current?.focus();
    inputRef.current?.select();
    return () => {
      previousFocusRef.current?.focus?.();
    };
  }, []);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="session-group-modal-title"
      data-testid="session-group-modal"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={() => !saving && onClose()}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[420px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="px-5 py-4 border-b border-surface-700">
          <h2
            id="session-group-modal-title"
            className="text-sm font-semibold text-text-primary"
          >
            Edit group
          </h2>
        </div>

        {/* Body */}
        <div className="px-5 py-4 space-y-3">
          <p className="text-[13px] text-text-secondary">
            Move{" "}
            <span className="text-text-primary">{sessionTitle}</span>{" "}
            to a group.
          </p>
          <input
            ref={inputRef}
            type="text"
            value={value}
            onChange={(e) => {
              setValue(e.target.value);
              if (error) setError(null);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                if (!saving) void handleSave();
              }
              if (e.key === "Escape" && !saving) onClose();
            }}
            placeholder="Group (blank to ungroup)"
            data-testid="session-group-modal-input"
            className="w-full bg-surface-900 border border-surface-700 rounded px-2 py-1.5 text-[13px] font-mono text-text-primary focus:outline-none focus:border-brand-600"
          />
          <p className="text-[12px] text-text-dim">
            Leave blank to ungroup. Use <span className="font-mono">/</span> for
            hierarchy, for example <span className="font-mono">work/projects</span>.
          </p>
          {error && (
            <p
              data-testid="session-group-modal-error"
              className="text-[12px] text-status-error"
            >
              {error}
            </p>
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-3 px-5 py-3 border-t border-surface-700">
          <button
            onClick={onClose}
            disabled={saving}
            className="px-3 py-1.5 text-sm text-text-secondary hover:text-text-primary rounded-md hover:bg-surface-700/50 cursor-pointer transition-colors disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            onClick={handleSave}
            disabled={saving}
            data-testid="session-group-modal-save"
            className="px-3 py-1.5 text-sm text-white bg-brand-600/90 hover:bg-brand-600 rounded-md cursor-pointer transition-colors disabled:opacity-50 flex items-center gap-2"
          >
            {saving && (
              <svg className="animate-spin h-3.5 w-3.5" viewBox="0 0 24 24">
                <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
              </svg>
            )}
            {saving ? "Saving..." : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}
