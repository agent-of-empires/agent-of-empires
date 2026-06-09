import { useEffect, useRef, useState } from "react";
import { Check, Clock, ListOrdered, Siren } from "lucide-react";
import type { SidebarSortMode } from "../lib/sidebarSort";
import { Tooltip } from "./Tooltip";

// Sidebar sort picker (#1640). Replaces the former two-state Clock /
// ListOrdered toggle now that there are three modes; a cycle button gets
// opaque past two states, so this is an explicit labeled dropdown. The
// trigger shows the active mode's icon and tints brand when any non-manual
// (computed) mode is active, matching the axis toggle's affordance. Outside
// click and Escape close it, mirroring OverflowMenu.

interface ModeSpec {
  mode: SidebarSortMode;
  label: string;
  Icon: typeof Clock;
}

const MODES: readonly ModeSpec[] = [
  { mode: "manual", label: "Manual", Icon: ListOrdered },
  { mode: "lastActivity", label: "Last activity", Icon: Clock },
  { mode: "attention", label: "Attention", Icon: Siren },
];

const TRIGGER_TOOLTIP: Record<SidebarSortMode, string> = {
  manual: "Sort: manual, drag enabled",
  lastActivity: "Sort: last activity, drag disabled",
  attention: "Sort: attention, drag disabled",
};

interface Props {
  sortMode: SidebarSortMode;
  onSortModeChange: (mode: SidebarSortMode) => void;
}

export function SidebarSortPicker({ sortMode, onSortModeChange }: Props) {
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

  // MODES is non-empty by construction, so the fallback is always defined.
  const active = MODES.find((m) => m.mode === sortMode) ?? MODES[0]!;
  const ActiveIcon = active.Icon;

  return (
    <div ref={ref} className="relative">
      <Tooltip text={TRIGGER_TOOLTIP[sortMode]}>
        <button
          onClick={() => setOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label={`Sort sessions, current: ${active.label}`}
          data-testid="sidebar-sort-toggle"
          data-sort-mode={sortMode}
          className={`w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors ${
            sortMode !== "manual" ? "text-brand-500" : "text-text-dim hover:text-text-secondary"
          }`}
        >
          <ActiveIcon className="h-3.5 w-3.5" />
        </button>
      </Tooltip>

      {open && (
        <div
          role="menu"
          data-testid="sidebar-sort-menu"
          className="absolute right-0 top-full mt-1 min-w-[160px] bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1 z-50 animate-fade-in"
        >
          {MODES.map(({ mode, label, Icon }) => {
            const selected = mode === sortMode;
            return (
              <button
                key={mode}
                role="menuitemradio"
                aria-checked={selected}
                data-testid={`sidebar-sort-option-${mode}`}
                onClick={() => {
                  setOpen(false);
                  if (mode !== sortMode) onSortModeChange(mode);
                }}
                className={`w-full flex items-center gap-2 px-3 py-1.5 text-sm cursor-pointer hover:bg-surface-700/60 ${
                  selected ? "text-brand-500" : "text-text-secondary hover:text-text-primary"
                }`}
              >
                <Icon className="h-3.5 w-3.5 shrink-0" />
                <span className="flex-1 text-left">{label}</span>
                {selected && <Check className="h-3.5 w-3.5 shrink-0" />}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
