import { useEffect, useRef, useState } from "react";
import type { TipDto } from "../lib/api";

interface Props {
  tips: TipDto[];
  /** Tip to open on (the first unseen one, so new content leads). */
  startIndex: number;
  /** "Show tips on startup" checkbox state (session.show_tips). */
  enabled: boolean;
  /** Mark one tip seen (called as each tip is shown). */
  onMarkSeen: (id: string) => void;
  /** Toggle "Show tips on startup". */
  onSetEnabled: (enabled: boolean) => void;
  onClose: () => void;
}

/// Tip-of-the-day modal for the web dashboard, in the style of GIMP / DBeaver:
/// one tip at a time with Previous / Next, a "Show tips on startup" checkbox,
/// and Close. Auto-pops on startup (from App) and is reopenable from the top-bar
/// menu. Each tip is marked seen as it is shown, so the seen state stays in sync
/// with the TUI. Modeled on TelemetryConsentModal's fixed-overlay styling.
export function TipsModal({ tips, startIndex, enabled, onMarkSeen, onSetEnabled, onClose }: Props) {
  const count = tips.length;
  const clampedStart = count === 0 ? 0 : Math.min(Math.max(startIndex, 0), count - 1);
  const [index, setIndex] = useState(clampedStart);
  const closeRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    closeRef.current?.focus();
  }, []);

  // Esc closes, matching the other dashboard modals.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // The initially shown tip is marked seen by the opener (App); navigation
  // marks the rest, so mark-on-view never runs from an effect.
  const go = (next: number) => {
    if (count === 0) return;
    const wrapped = (next + count) % count;
    setIndex(wrapped);
    const t = tips[wrapped];
    if (t && !t.seen) onMarkSeen(t.id);
  };

  const current = tips[index];

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="tips-modal-title"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onClose}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[480px] max-w-[90vw] max-h-[80vh] flex flex-col shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700 flex items-center gap-2">
          <span aria-hidden="true">💡</span>
          <h2 id="tips-modal-title" className="text-sm font-semibold text-text-bright">
            Tip of the day
          </h2>
        </div>

        <div className="p-5 overflow-y-auto min-h-[7rem]">
          {current ? (
            <>
              <h3 className="text-sm font-semibold text-text-primary mb-2">{current.title}</h3>
              <p className="text-sm text-text-secondary">{current.body}</p>
            </>
          ) : (
            <p className="text-sm text-text-dim">No tips right now. Check back after a new release.</p>
          )}
        </div>

        <div className="px-5 py-3 border-t border-surface-700 flex items-center justify-between gap-2">
          <label className="flex items-center gap-2 text-xs text-text-secondary cursor-pointer select-none">
            <input
              type="checkbox"
              checked={enabled}
              onChange={(e) => onSetEnabled(e.target.checked)}
              className="accent-brand-500 cursor-pointer"
            />
            Show tips on startup
          </label>
          {count > 1 && <span className="text-xs text-text-dim">{`Tip ${index + 1} of ${count}`}</span>}
        </div>

        <div className="px-5 py-3 border-t border-surface-700 flex items-center justify-between gap-2">
          <div className="flex gap-2">
            <button
              onClick={() => go(index - 1)}
              disabled={count <= 1}
              className="h-8 px-3 rounded-md border border-surface-700/50 text-sm text-text-secondary hover:bg-surface-850 hover:text-text-primary transition-colors duration-150 cursor-pointer disabled:opacity-40 disabled:cursor-default"
            >
              Previous
            </button>
            <button
              onClick={() => go(index + 1)}
              disabled={count <= 1}
              className="h-8 px-3 rounded-md border border-surface-700/50 text-sm text-text-secondary hover:bg-surface-850 hover:text-text-primary transition-colors duration-150 cursor-pointer disabled:opacity-40 disabled:cursor-default"
            >
              Next
            </button>
          </div>
          <button
            ref={closeRef}
            onClick={onClose}
            className="h-8 px-3 rounded-md bg-brand-600 text-sm text-white hover:bg-brand-500 transition-colors duration-150 cursor-pointer"
          >
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
