import { useEffect, useRef, useState } from "react";
import { type TourAnchorId } from "../lib/tourSteps";

export interface OverflowItem {
  label: string;
  onClick: () => void;
}

interface Props {
  items: OverflowItem[];
  /** Tour anchor attached to the trigger button (the stable, always-rendered
   *  element), so the tour can point at the menu without it being open. */
  triggerDataTour?: TourAnchorId;
}

export function OverflowMenu({ items, triggerDataTour }: Props) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKeydown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onDocClick);
    document.addEventListener("keydown", onKeydown);
    return () => {
      document.removeEventListener("mousedown", onDocClick);
      document.removeEventListener("keydown", onKeydown);
    };
  }, [open]);

  return (
    <div ref={ref} className="relative">
      <button
        onClick={() => setOpen((o) => !o)}
        data-tour={triggerDataTour}
        className="w-8 h-8 flex items-center justify-center rounded-md cursor-pointer text-text-muted hover:text-text-secondary hover:bg-surface-700/50 transition-colors"
        aria-label="More options"
        aria-haspopup="menu"
        aria-expanded={open}
      >
        <svg
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <circle cx="5" cy="12" r="1.5" />
          <circle cx="12" cy="12" r="1.5" />
          <circle cx="19" cy="12" r="1.5" />
        </svg>
      </button>

      {open && (
        <div
          role="menu"
          className="absolute right-0 top-full mt-1 min-w-[160px] bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1 z-50 animate-fade-in"
        >
          {items.map((item) => (
            <button
              key={item.label}
              role="menuitem"
              onClick={() => {
                setOpen(false);
                item.onClick();
              }}
              className="w-full text-left px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-700/60 hover:text-text-primary cursor-pointer"
            >
              {item.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
