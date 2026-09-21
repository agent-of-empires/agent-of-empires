import { useRef, useState } from "react";
import { Check, Clock, ListOrdered, SlidersHorizontal, Siren } from "lucide-react";
import type { SidebarSortMode } from "../lib/sidebarSort";
import type { PluginSortSpec } from "../lib/pluginUi";
import { Tooltip } from "./Tooltip";
import { useOutsideDismiss } from "./useOutsideDismiss";

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

/** Sort mode dropdown; plugin `sort-key` entries follow the built-ins as computed modes. */
export function SidebarSortPicker({
  sortMode,
  onSortModeChange,
  pluginSorts = [],
  pluginSortRef = null,
  onPluginSortChange,
}: Props) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useOutsideDismiss(open, [ref], () => setOpen(false));

  // A stale ref (for example after a daemon restart) falls back to the built-in trigger.
  const activePlugin = pluginSortRef
    ? pluginSorts.find((s) => s.pluginId === pluginSortRef.pluginId && s.entryId === pluginSortRef.entryId)
    : undefined;

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
          {MODES.map(({ mode, label, Icon }) => (
            <SortOption
              key={mode}
              testId={`sidebar-sort-option-${mode}`}
              Icon={Icon}
              label={label}
              selected={!activePlugin && mode === sortMode}
              onSelect={(selected) => {
                setOpen(false);
                if (!selected) onSortModeChange(mode);
              }}
            />
          ))}
          {pluginSorts.length > 0 && <div className="my-1 border-t border-surface-700/50" role="separator" />}
          {pluginSorts.map((spec) => (
            <SortOption
              key={`${spec.pluginId}:${spec.entryId}`}
              testId={`sidebar-sort-option-plugin-${spec.entryId}`}
              Icon={SlidersHorizontal}
              label={spec.label}
              truncate
              selected={activePlugin?.pluginId === spec.pluginId && activePlugin.entryId === spec.entryId}
              onSelect={(selected) => {
                setOpen(false);
                if (!selected) onPluginSortChange?.({ pluginId: spec.pluginId, entryId: spec.entryId });
              }}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function SortOption({
  testId,
  Icon,
  label,
  truncate = false,
  selected,
  onSelect,
}: {
  testId: string;
  Icon: typeof Clock;
  label: string;
  truncate?: boolean;
  selected: boolean;
  onSelect: (selected: boolean) => void;
}) {
  return (
    <button
      role="menuitemradio"
      aria-checked={selected}
      data-testid={testId}
      onClick={() => onSelect(selected)}
      className={`w-full flex items-center gap-2 px-3 py-1.5 text-sm cursor-pointer hover:bg-surface-700/60 ${
        selected ? "text-brand-500" : "text-text-secondary hover:text-text-primary"
      }`}
    >
      <Icon className="h-3.5 w-3.5 shrink-0" />
      <span className={`flex-1 text-left${truncate ? " truncate" : ""}`}>{label}</span>
      {selected && <Check className="h-3.5 w-3.5 shrink-0" />}
    </button>
  );
}
