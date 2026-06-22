import { useEffect, useRef, useState } from "react";
import type { TipDto } from "../lib/api";

interface Props {
  tips: TipDto[];
  /** Mark one tip seen (called when an unseen tip is expanded into view). */
  onMarkSeen: (id: string) => void;
  /** "Don't show again": turns tips off. */
  onDisable: () => void;
  onClose: () => void;
}

/// Browsable tips panel for the web dashboard, mirroring the TUI overlay:
/// unseen tips lead, already-seen tips collapse into an expandable section so
/// what's new is foregrounded. Expanding an unseen tip marks it seen (mark-seen-
/// on-view), and "Don't show again" turns tips off. Modeled on
/// TelemetryConsentModal's fixed-overlay styling.
export function TipsModal({ tips, onMarkSeen, onDisable, onClose }: Props) {
  const closeRef = useRef<HTMLButtonElement>(null);
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [seenOpen, setSeenOpen] = useState(false);

  const unseen = tips.filter((t) => !t.seen);
  const seen = tips.filter((t) => t.seen);

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

  const toggle = (tip: TipDto) => {
    const opening = expandedId !== tip.id;
    setExpandedId(opening ? tip.id : null);
    // Mark seen when an unseen tip is opened into view.
    if (opening && !tip.seen) onMarkSeen(tip.id);
  };

  const row = (tip: TipDto) => (
    <li key={tip.id} className="border border-surface-700/50 rounded-md overflow-hidden">
      <button
        onClick={() => toggle(tip)}
        aria-expanded={expandedId === tip.id}
        className="w-full px-3 py-2 flex items-center justify-between text-left text-sm text-text-primary hover:bg-surface-850 transition-colors cursor-pointer"
      >
        <span className="flex items-center gap-2">
          {!tip.seen && <span className="w-1.5 h-1.5 rounded-full bg-brand-500 shrink-0" aria-hidden="true" />}
          {tip.title}
        </span>
        <span className="text-text-dim text-xs">{expandedId === tip.id ? "Hide" : "Read"}</span>
      </button>
      {expandedId === tip.id && (
        <p className="px-3 pb-3 pt-1 text-sm text-text-secondary border-t border-surface-700/50">{tip.body}</p>
      )}
    </li>
  );

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="tips-modal-title"
      className="fixed inset-0 bg-black/60 flex items-center justify-center z-50 animate-fade-in"
      onClick={onClose}
    >
      <div
        className="bg-surface-800 border border-surface-700/50 rounded-lg w-[460px] max-w-[90vw] max-h-[80vh] flex flex-col shadow-2xl animate-slide-up"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-surface-700 flex items-center gap-2">
          <span aria-hidden="true">💡</span>
          <h2 id="tips-modal-title" className="text-sm font-semibold text-text-bright">
            Tips
          </h2>
        </div>

        <div className="p-5 space-y-2 overflow-y-auto">
          {tips.length === 0 ? (
            <p className="text-sm text-text-dim">No tips right now. Check back after a new release.</p>
          ) : (
            <>
              {unseen.length > 0 && <ul className="space-y-2">{unseen.map(row)}</ul>}
              {seen.length > 0 && (
                <div className={unseen.length > 0 ? "pt-1" : ""}>
                  <button
                    onClick={() => setSeenOpen((o) => !o)}
                    aria-expanded={seenOpen}
                    className="text-xs text-text-dim hover:text-text-secondary transition-colors cursor-pointer"
                  >
                    {seenOpen ? "Hide" : "Show"} seen ({seen.length})
                  </button>
                  {seenOpen && <ul className="space-y-2 mt-2">{seen.map(row)}</ul>}
                </div>
              )}
            </>
          )}
        </div>

        <div className="px-5 py-4 border-t border-surface-700 flex justify-between gap-2">
          <button
            onClick={() => {
              onDisable();
              onClose();
            }}
            className="h-8 px-3 rounded-md border border-surface-700/50 text-sm text-text-secondary hover:bg-surface-850 hover:text-text-primary transition-colors duration-150 cursor-pointer"
          >
            Don't show again
          </button>
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
