import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { VolumeIgnoresGlobPreview } from "../../lib/api";

interface Props {
  globs: VolumeIgnoresGlobPreview[];
  /** Resolves with whether the user ticked "Don't show this again". */
  onConfirm: (dontShowAgain: boolean) => Promise<void> | void;
  onCancel: () => void;
}

/**
 * One-time confirmation shown before creating a sandbox session whose resolved
 * config has glob `volume_ignores` (recursive `**` patterns). Those are expanded
 * against the workspace now, a point-in-time snapshot that won't shadow
 * directories a build creates later inside the container (#2045). Mirrors the
 * native TUI confirm gate; the "Don't show again" checkbox writes the same
 * server-side app_state flag.
 */
export function VolumeIgnoresGlobDialog({ globs, onConfirm, onCancel }: Props) {
  const [dontShowAgain, setDontShowAgain] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const confirmButtonRef = useRef<HTMLButtonElement | null>(null);
  const previousFocusRef = useRef<HTMLElement | null>(null);

  const matchCount = useMemo(() => globs.reduce((sum, g) => sum + g.matched_paths.length, 0), [globs]);

  const handleConfirm = useCallback(async () => {
    setConfirming(true);
    try {
      await onConfirm(dontShowAgain);
    } catch {
      setConfirming(false);
    }
  }, [onConfirm, dontShowAgain]);

  // Restore focus to the trigger on unmount, matching DeleteSessionDialog.
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
        if (target) {
          const tag = target.tagName;
          if (tag === "INPUT" || tag === "TEXTAREA" || tag === "BUTTON") return;
        }
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
      aria-labelledby="volume-ignores-glob-dialog-title"
      data-testid="volume-ignores-glob-dialog"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onCancel}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[460px] max-w-[90vw] shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="px-5 py-4 border-b border-surface-700">
          <h2 id="volume-ignores-glob-dialog-title" className="text-sm font-semibold text-status-warning">
            Glob volume_ignores
          </h2>
        </div>

        {/* Body */}
        <div className="px-5 py-4 space-y-3">
          <p className="text-[13px] text-text-secondary">
            This session's <span className="font-mono text-text-primary">volume_ignores</span> include glob patterns.
            They are expanded against the workspace now, matching{" "}
            <span className="text-text-primary">{matchCount}</span> director{matchCount === 1 ? "y" : "ies"}, and one
            ignore mount is created per match.
          </p>

          <ul className="space-y-1 max-h-40 overflow-y-auto" data-testid="volume-ignores-glob-list">
            {globs.map((g) => (
              <li key={g.pattern} className="text-[13px] text-text-secondary flex items-baseline justify-between gap-3">
                <span className="font-mono text-text-primary truncate">{g.pattern}</span>
                <span className="text-text-dim whitespace-nowrap">{g.matched_paths.length} matched</span>
              </li>
            ))}
          </ul>

          <p className="text-[12px] text-text-dim">
            This is a point-in-time snapshot. Directories a build creates later inside the container are not hidden;
            re-create the session to pick up new matches.
          </p>

          <Checkbox
            checked={dontShowAgain}
            onChange={setDontShowAgain}
            label="Don't show this again"
            testId="volume-ignores-glob-dont-show-again"
          />
        </div>

        {/* Footer */}
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
            data-testid="volume-ignores-glob-proceed"
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
            {confirming ? "Creating..." : "Proceed"}
          </button>
        </div>
      </div>
    </div>
  );
}

function Checkbox({
  checked,
  onChange,
  label,
  testId,
}: {
  checked: boolean;
  onChange: (val: boolean) => void;
  label: string;
  testId?: string;
}) {
  return (
    <label
      className="flex items-start gap-2.5 cursor-pointer group"
      data-testid={testId}
      data-checked={checked ? "true" : "false"}
    >
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        aria-label={label}
        className="peer sr-only"
      />
      <span
        aria-hidden="true"
        className={`mt-0.5 w-4 h-4 rounded border flex items-center justify-center shrink-0 transition-colors peer-focus-visible:outline peer-focus-visible:outline-2 peer-focus-visible:outline-offset-2 peer-focus-visible:outline-green-500 ${
          checked ? "bg-green-500 border-green-500" : "border-surface-600 group-hover:border-surface-500"
        }`}
      >
        {checked && (
          <svg width="10" height="10" viewBox="0 0 10 10" fill="none">
            <path d="M2 5L4 7L8 3" stroke="white" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        )}
      </span>
      <span className="text-[13px] text-text-secondary group-hover:text-text-primary transition-colors">{label}</span>
    </label>
  );
}
