import { useRef, useState } from "react";
import { Check, LayoutGrid } from "lucide-react";

import type { PaneDisplay } from "./Dock";
import { PaneIcon } from "./PaneIcon";
import { useOutsideDismiss } from "./useOutsideDismiss";

export interface PaneMenuControl {
  /** The panes the menu lists, in order: the review panes, then plugin panes. */
  paneIds: string[];
  descriptorFor: (id: string) => PaneDisplay;
  /** The listed panes open now. */
  openIds: readonly string[];
  onToggle: (id: string) => void;
}

/** The right dock's pane picker: each pane with a check while open; picking one opens or closes it. */
export function PaneMenu({
  paneIds,
  descriptorFor,
  openIds,
  onToggle,
  className,
}: PaneMenuControl & { className: string }) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  useOutsideDismiss(open, [triggerRef, menuRef], (e) => {
    setOpen(false);
    if (e.type === "keydown") triggerRef.current?.focus();
  });
  if (paneIds.length === 0) return null;
  return (
    <div className="relative flex items-center">
      <button
        ref={triggerRef}
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-haspopup="menu"
        aria-expanded={open}
        title="Panes"
        aria-label="Panes"
        data-testid="pane-menu-toggle"
        className={className}
      >
        <LayoutGrid className="size-3.5" aria-hidden />
      </button>
      {open && (
        <div
          ref={(el) => {
            menuRef.current = el;
            // Focus the first entry as the menu opens.
            if (el && !el.contains(document.activeElement))
              el.querySelector<HTMLElement>("[role=menuitemcheckbox]")?.focus();
          }}
          role="menu"
          aria-label="Panes"
          data-testid="pane-menu"
          data-owns-escape="open"
          className="absolute right-0 top-full mt-1 z-50 w-52 py-1 bg-surface-800 border border-surface-700/50 rounded-md shadow-xl animate-fade-in"
        >
          {paneIds.map((id) => {
            const desc = descriptorFor(id);
            const checked = openIds.includes(id);
            return (
              <button
                key={id}
                type="button"
                role="menuitemcheckbox"
                aria-checked={checked}
                data-testid={`pane-toggle-${id}`}
                onClick={() => {
                  setOpen(false);
                  onToggle(id);
                }}
                className="w-full flex items-center gap-2 h-8 px-3 text-left text-[12px] text-text-secondary cursor-pointer hover:bg-surface-700/60 hover:text-text-primary focus:outline-none focus-visible:bg-surface-700/60"
              >
                <PaneIcon
                  icon={desc.icon}
                  iconAssetUrl={desc.iconAssetUrl}
                  className="size-3.5 shrink-0 text-text-dim"
                />
                <span className="truncate">{desc.title}</span>
                {checked && <Check className="size-3.5 ml-auto shrink-0" aria-hidden />}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
