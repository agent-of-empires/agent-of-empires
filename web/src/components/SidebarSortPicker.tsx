import { useEffect, useRef, useState } from "react";
import { Check, Clock, ListOrdered, SlidersHorizontal, Siren } from "lucide-react";
import type { SidebarSortMode } from "../lib/sidebarSort";
import type { PluginSortSpec } from "../lib/pluginUi";
import { Tooltip } from "./Tooltip";

// Sidebar sort picker (#1640). Replaces the former two-state Clock /
// ListOrdered toggle now that there are three modes; a cycle button gets
// opaque past two states, so this is an explicit labeled dropdown. The
// trigger shows the active mode's icon and tints brand when any non-manual
// (computed) mode is active, matching the axis toggle's affordance. Outside
// click and Escape close it, mirroring OverflowMenu.
//
// Plugin `sort-key` entries (#2401) appear as extra options below the
// built-ins; selecting one activates an ephemeral plugin sort (a computed
// mode, so drag is disabled while it is active). Selecting a built-in clears
// it via `onSortModeChange`.

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

const BUILTIN_TOOLTIP: Record<SidebarSortMode, string> = {
  manual: "Sort: manual, drag enabled",
  lastActivity: "Sort: last activity, drag disabled",
  attention: "Sort: attention, drag disabled",
};

interface PluginSortRef {
  pluginId: string;
  entryId: string;
}

interface Props {
  sortMode: SidebarSortMode;
  onSortModeChange: (mode: SidebarSortMode) => void;
  pluginSorts?: PluginSortSpec[];
  pluginSortRef?: PluginSortRef | null;
  onPluginSortChange?: (ref: PluginSortRef) => void;
}

export function SidebarSortPicker({
  sortMode,
  onSortModeChange,
  pluginSorts = [],
  pluginSortRef = null,
  onPluginSortChange,
}: Props) {
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

  // A plugin sort is active only when the ref resolves to a live `sort-key`
  // entry; a stale ref (daemon restart) falls back to the built-in trigger.
  const activePlugin = pluginSortRef
    ? pluginSorts.find((s) => s.pluginId === pluginSortRef.pluginId && s.entryId === pluginSortRef.entryId)
    : undefined;

  // MODES is non-empty by construction, so the fallback is always defined.
  const activeBuiltin = MODES.find((m) => m.mode === sortMode) ?? MODES[0]!;
  const ActiveIcon = activePlugin ? SlidersHorizontal : activeBuiltin.Icon;
  const activeLabel = activePlugin ? activePlugin.label : activeBuiltin.label;
  const computed = activePlugin != null || sortMode !== "manual";
  const triggerTooltip = activePlugin ? `Sort: ${activePlugin.label}, drag disabled` : BUILTIN_TOOLTIP[sortMode];

  return (
    <div ref={ref} className="relative">
      <Tooltip text={triggerTooltip}>
        <button
          onClick={() => setOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label={`Sort sessions, current: ${activeLabel}`}
          data-testid="sidebar-sort-toggle"
          data-sort-mode={activePlugin ? "plugin" : sortMode}
          className={`w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors ${
            computed ? "text-brand-500" : "text-text-dim hover:text-text-secondary"
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
            const selected = !activePlugin && mode === sortMode;
            return (
              <button
                key={mode}
                role="menuitemradio"
                aria-checked={selected}
                data-testid={`sidebar-sort-option-${mode}`}
                onClick={() => {
                  setOpen(false);
                  if (activePlugin || mode !== sortMode) onSortModeChange(mode);
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
          {pluginSorts.length > 0 && (
            <>
              <div className="my-1 border-t border-surface-700/50" role="separator" />
              {pluginSorts.map((spec) => {
                const selected =
                  activePlugin != null &&
                  activePlugin.pluginId === spec.pluginId &&
                  activePlugin.entryId === spec.entryId;
                return (
                  <button
                    key={`${spec.pluginId}:${spec.entryId}`}
                    role="menuitemradio"
                    aria-checked={selected}
                    data-testid={`sidebar-sort-option-plugin-${spec.entryId}`}
                    onClick={() => {
                      setOpen(false);
                      if (!selected) onPluginSortChange?.({ pluginId: spec.pluginId, entryId: spec.entryId });
                    }}
                    className={`w-full flex items-center gap-2 px-3 py-1.5 text-sm cursor-pointer hover:bg-surface-700/60 ${
                      selected ? "text-brand-500" : "text-text-secondary hover:text-text-primary"
                    }`}
                  >
                    <SlidersHorizontal className="h-3.5 w-3.5 shrink-0" />
                    <span className="flex-1 text-left truncate">{spec.label}</span>
                    {selected && <Check className="h-3.5 w-3.5 shrink-0" />}
                  </button>
                );
              })}
            </>
          )}
        </div>
      )}
    </div>
  );
}
