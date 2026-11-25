import { useRef, useState } from "react";
import { Check, ChevronDown, PanelRight, SquareTerminal } from "lucide-react";

import type { PaneDisplay } from "./Dock";
import { PaneIcon } from "./PaneIcon";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import { useOutsideDismiss } from "./useOutsideDismiss";
import { formatShortcut, IS_MAC, SHORTCUTS_BY_ID, type ShortcutId } from "../lib/shortcuts";

export interface PanelControlProps {
  /** Whether the review panel (the right dock) shows. */
  open: boolean;
  onToggle: () => void;
  /** Changed files, shown while the panel is closed. */
  badge: number;
  /** The panes the menu lists: the review panel's, then the terminal. */
  paneIds: readonly string[];
  descriptorFor: (id: string) => PaneDisplay;
  /** The listed panes open now. */
  openIds: readonly string[];
  onTogglePane: (id: string) => void;
}

// An open pane's control is tinted with the brand accent rather than filled.
const toneFor = (open: boolean) => (open ? "text-brand-400" : "text-text-dim hover:text-text-secondary");

/** "Show review panel (⌘⌥B)", the way desktop apps name a toggle and its shortcut. */
const titled = (label: string, shortcut: ShortcutId) =>
  `${label} (${formatShortcut(SHORTCUTS_BY_ID[shortcut].chord, { mac: IS_MAC, separator: "+" })})`;

const half = "h-7 flex items-center justify-center cursor-pointer transition-colors hover:bg-surface-700/50";

/** The header's panel control: the icon shows or hides the review panel, the chevron opens a menu that opens
 *  or closes each pane, checked while open. */
export function PanelControl({
  open,
  onToggle,
  badge,
  paneIds,
  descriptorFor,
  openIds,
  onTogglePane,
}: PanelControlProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const chevronRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  useOutsideDismiss(menuOpen, [chevronRef, menuRef], (e) => {
    setMenuOpen(false);
    if (e.type === "keydown") chevronRef.current?.focus();
  });
  const shown = open ? 0 : badge;
  const tone = toneFor(open);
  return (
    <div className="relative hidden md:flex items-center rounded-md" data-testid="panel-control">
      <button
        type="button"
        onClick={onToggle}
        aria-pressed={open}
        data-testid="right-dock-toggle"
        {...tourAnchor(TOUR_ANCHORS.diffToggle)}
        className={`${half} w-7 rounded-l-md ${tone}`}
        title={titled(`${open ? "Hide" : "Show"} review panel`, "rightPanel")}
        aria-label={`Toggle review panel${shown > 0 ? `, ${shown} changed` : ""}`}
      >
        <span className="relative">
          <PanelRight className="size-4" strokeWidth={1.5} aria-hidden />
          {shown > 0 && (
            <span
              aria-hidden="true"
              data-testid="right-dock-badge"
              className="absolute -top-1.5 -right-2.5 min-w-[1rem] rounded-full border border-surface-600 bg-surface-800 px-1 text-[10px] font-semibold leading-[0.875rem] tabular-nums text-text-secondary text-center"
            >
              {shown > 99 ? "99+" : shown}
            </span>
          )}
        </span>
      </button>
      <button
        ref={chevronRef}
        type="button"
        onClick={() => setMenuOpen((o) => !o)}
        aria-haspopup="menu"
        aria-expanded={menuOpen}
        title="Panes"
        aria-label="Panes"
        data-testid="pane-menu-toggle"
        className={`${half} w-4 rounded-r-md text-text-dim hover:text-text-secondary`}
      >
        <ChevronDown className="size-3" aria-hidden />
      </button>
      {menuOpen && (
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
                  setMenuOpen(false);
                  onTogglePane(id);
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

/** Shows or hides the active Session's paired terminal. */
export function TerminalToggle({ open, onToggle }: { open: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-pressed={open}
      data-testid="pane-toggle-terminal"
      title={titled(`${open ? "Hide" : "Show"} terminal`, "terminalFocus")}
      aria-label="Toggle terminal pane"
      className={`hidden md:flex w-7 h-7 items-center justify-center cursor-pointer rounded-md transition-colors hover:bg-surface-700/50 ${toneFor(open)}`}
    >
      <SquareTerminal className="size-4" strokeWidth={1.5} aria-hidden />
    </button>
  );
}
