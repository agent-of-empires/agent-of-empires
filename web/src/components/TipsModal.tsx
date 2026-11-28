import { useEffect, useRef, useState } from "react";
import type { TipDto } from "../lib/api";
import { BUTTON_PRIMARY, BUTTON_SECONDARY } from "../lib/surfaces";
import { Dialog } from "./Dialog";

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
/// with the TUI.
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
    <Dialog
      id="tips-modal"
      describedBy={false}
      panelClassName="md:w-[480px] md:max-h-[80vh]"
      bodyClassName="min-h-[7rem]"
      onDismiss={onClose}
      title={
        <>
          <span aria-hidden="true">💡</span>
          <span className="text-text-bright">Tip of the day</span>
        </>
      }
      headerEnd={
        count > 1 && (
          <span className="shrink-0 text-[13px] md:text-xs text-text-dim">{`Tip ${index + 1} of ${count}`}</span>
        )
      }
      footer={
        <>
          <label className="mr-auto max-md:basis-full max-md:min-h-11 flex items-center gap-2 text-[15px] md:text-xs text-text-secondary cursor-pointer select-none">
            <input
              type="checkbox"
              checked={enabled}
              onChange={(e) => onSetEnabled(e.target.checked)}
              className="accent-brand-500 cursor-pointer"
            />
            Show tips on startup
          </label>
          <button type="button" onClick={() => go(index - 1)} disabled={count <= 1} className={BUTTON_SECONDARY}>
            Previous
          </button>
          <button type="button" onClick={() => go(index + 1)} disabled={count <= 1} className={BUTTON_SECONDARY}>
            Next
          </button>
          <button type="button" ref={closeRef} onClick={onClose} className={BUTTON_PRIMARY}>
            Close
          </button>
        </>
      }
    >
      {current ? (
        <>
          <h3 className="text-[15px] md:text-sm font-semibold text-text-primary mb-2">{current.title}</h3>
          <p className="text-[15px] md:text-sm text-text-secondary">{current.body}</p>
        </>
      ) : (
        <p className="text-[15px] md:text-sm text-text-dim">No tips right now. Check back after a new release.</p>
      )}
    </Dialog>
  );
}
