import { Fragment, useEffect, useRef, useState } from "react";
import { type TourAnchorId } from "../lib/tourSteps";
import { type LucideIcon } from "lucide-react";
import { DropdownMenu, MenuItem, MenuSeparator } from "./ContextMenu";
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
        <DropdownMenu
          phoneGlass={phoneFloating}
          className={`absolute right-0 top-full mt-1 min-w-[180px] z-50 ${
            phoneFloating ? "max-md:mt-2 max-md:min-w-[240px] max-md:max-h-[calc(100dvh-7rem)]" : ""
          }`}
        >
          {items.map(({ label, onClick, detail, icon: Icon, separated, checked }) => (
            <Fragment key={label}>
              {separated && <MenuSeparator />}
              <MenuItem
                role={checked === undefined ? "menuitem" : "menuitemradio"}
                checked={checked}
                icon={Icon && <Icon strokeWidth={1.6} />}
                detail={detail}
                onClick={() => {
                  setOpen(false);
                  onClick();
                }}
              >
                {label}
              </MenuItem>
            </Fragment>
          ))}
        </DropdownMenu>
      )}
    </div>
  );
}
