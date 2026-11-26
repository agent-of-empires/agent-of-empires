import { useEffect, useRef, useState } from "react";
import { type TourAnchorId } from "../lib/tourSteps";
import { Check, type LucideIcon } from "lucide-react";
import { StrokeIcon } from "./icons";

export interface OverflowItem {
  label: string;
  onClick: () => void;
  /** A second line under the label, such as the branch Copy branch copies. */
  detail?: string;
  icon?: LucideIcon;
  /** Starts a new group with a rule above it. */
  separated?: boolean;
  /** Makes the item one of a set of choices; true marks the current one. */
  checked?: boolean;
}

interface Props {
  items: OverflowItem[];
  /** Phones show the trigger as a round floating control and the menu as a larger frosted list. */
  phoneFloating?: boolean;
  /** Tour anchor attached to the trigger button (the stable, always-rendered
   *  element), so the tour can point at the menu without it being open. */
  triggerDataTour?: TourAnchorId;
}

export function OverflowMenu({ items, triggerDataTour, phoneFloating = false }: Props) {
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
        className={`w-8 h-8 flex items-center justify-center rounded-md cursor-pointer text-text-muted hover:text-text-secondary hover:bg-surface-700/50 transition-colors ${
          phoneFloating
            ? "max-md:w-10 max-md:h-10 max-md:rounded-full max-md:float-control max-md:text-text-secondary"
            : ""
        }`}
        aria-label="More options"
        aria-haspopup="menu"
        aria-expanded={open}
      >
        <StrokeIcon size={16} strokeWidth="1.5">
          <circle cx="5" cy="12" r="1.5" />
          <circle cx="12" cy="12" r="1.5" />
          <circle cx="19" cy="12" r="1.5" />
        </StrokeIcon>
      </button>

      {open && (
        <div
          role="menu"
          className={`absolute right-0 top-full mt-1 min-w-[160px] bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1 z-50 animate-fade-in ${
            phoneFloating
              ? "max-md:mt-2 max-md:min-w-[240px] max-md:max-h-[calc(100dvh-7rem)] max-md:overflow-y-auto max-md:overscroll-contain max-md:rounded-2xl max-md:glass-menu max-md:py-1.5"
              : ""
          }`}
        >
          {items.map(({ label, onClick, detail, icon: Icon, separated, checked }) => (
            <div key={label}>
              {separated && <div className="my-1 mx-3 border-t border-surface-600/40" role="separator" />}
              <button
                role={checked === undefined ? "menuitem" : "menuitemradio"}
                aria-checked={checked}
                onClick={() => {
                  setOpen(false);
                  onClick();
                }}
                className={`w-full flex items-center gap-3 text-left px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-700/60 hover:text-text-primary cursor-pointer ${
                  phoneFloating ? "max-md:px-4 max-md:py-2.5 max-md:text-[15px] max-md:text-text-primary" : ""
                }`}
              >
                {Icon && <Icon className="size-[18px] shrink-0 text-text-secondary" strokeWidth={1.6} aria-hidden />}
                <span className="min-w-0 flex-1">
                  <span className="block truncate">{label}</span>
                  {detail && <span className="block truncate text-[12px] text-text-dim">{detail}</span>}
                </span>
                {checked && <Check className="size-4 shrink-0 text-text-primary" strokeWidth={2} aria-hidden />}
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
