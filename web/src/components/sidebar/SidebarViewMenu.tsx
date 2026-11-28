import { useRef, useState } from "react";
import { SlidersHorizontal } from "lucide-react";

import type { SidebarAxis } from "../../lib/sidebarAxis";
import type { SidebarSortMode } from "../../lib/sidebarSort";
import type { PluginSortSpec } from "../../lib/pluginUi";
import { DropdownMenu, MenuHeading, MenuItem, MenuSeparator } from "../ContextMenu";
import { moveMenuFocus } from "../menuFocus";
import { Tooltip } from "../Tooltip";
import { useOutsideDismiss } from "../useOutsideDismiss";
import { TOOLBAR_BUTTON, TOOLBAR_TINT } from "./styles";

type PluginSortRef = { pluginId: string; entryId: string };

const AXES: readonly { axis: SidebarAxis; label: string }[] = [
  { axis: "repo", label: "Repository" },
  { axis: "org", label: "Organization" },
  { axis: "group", label: "Group" },
  { axis: "repo+group", label: "Repository, then group" },
  { axis: "status", label: "Session status" },
];

const SORTS: readonly { mode: SidebarSortMode; label: string; hint?: string }[] = [
  { mode: "manual", label: "Manual", hint: "drag to reorder" },
  { mode: "lastActivity", label: "Last activity" },
  { mode: "attention", label: "Attention" },
];

function Option({
  role,
  checked,
  testId,
  label,
  hint,
  onSelect,
}: {
  role: "menuitemradio" | "menuitemcheckbox";
  checked: boolean;
  testId: string;
  label: string;
  hint?: string;
  /** `keyboard` when Enter or Space picked it, not a pointer. */
  onSelect: (keyboard: boolean) => void;
}) {
  return (
    <MenuItem
      role={role}
      checked={checked}
      tabIndex={-1}
      testId={testId}
      // A keyboard click carries no pointer detail.
      onClick={(e) => onSelect(e.detail === 0)}
    >
      {label}
      {hint && <span className="text-text-dim"> · {hint}</span>}
    </MenuItem>
  );
}

/** The sidebar's one view control, as Claude Code desktop keeps a single icon by its list: how sessions are
 *  grouped and sorted, and whether the filter and plugin facets show. A dot marks a view other than the default. */
export function SidebarViewMenu({
  axis,
  onAxisChange,
  sortMode,
  onSortModeChange,
  pluginSorts,
  pluginSortRef,
  onPluginSortChange,
  filterOpen,
  onToggleFilter,
  hasFacets,
  facetsActive,
  facetOpen,
  onToggleFacets,
}: {
  axis: SidebarAxis;
  onAxisChange: (axis: SidebarAxis) => void;
  sortMode: SidebarSortMode;
  onSortModeChange: (mode: SidebarSortMode) => void;
  pluginSorts: PluginSortSpec[];
  pluginSortRef: PluginSortRef | null;
  onPluginSortChange: (ref: PluginSortRef) => void;
  filterOpen: boolean;
  onToggleFilter: () => void;
  hasFacets: boolean;
  facetsActive: boolean;
  facetOpen: boolean;
  onToggleFacets: () => void;
}) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  useOutsideDismiss(open, [triggerRef, menuRef], (e) => {
    setOpen(false);
    if (e.type === "keydown") triggerRef.current?.focus();
  });
  // A stale ref (for example after a daemon restart) falls back to the built-in sort.
  const activePlugin = pluginSortRef
    ? pluginSorts.find((s) => s.pluginId === pluginSortRef.pluginId && s.entryId === pluginSortRef.entryId)
    : undefined;
  const customized = axis !== "repo" || sortMode !== "manual" || !!activePlugin || filterOpen || facetsActive;
  // A keyboard choice hands focus back to the trigger, where a pointer's would only raise its tooltip; a
  // choice that opens the filter then moves it there.
  const choose = (keyboard: boolean, fn: () => void) => {
    setOpen(false);
    if (keyboard) triggerRef.current?.focus();
    fn();
  };
  return (
    <div className="relative">
      <Tooltip text="View options" open={open ? false : undefined}>
        <button
          ref={triggerRef}
          type="button"
          onClick={() => setOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label="View options"
          data-testid="sidebar-view-toggle"
          data-axis={axis}
          data-sort-mode={activePlugin ? "plugin" : sortMode}
          // A phone shows it as a round button beside the title, as Claude's app does.
          className={`relative ${TOOLBAR_BUTTON} ${TOOLBAR_TINT(open)} max-md:w-10 max-md:h-10 max-md:rounded-full max-md:float-control`}
        >
          <SlidersHorizontal className="size-3.5" aria-hidden />
          {customized && <span className="absolute right-1 top-1 h-1.5 w-1.5 rounded-full bg-brand-500" aria-hidden />}
        </button>
      </Tooltip>
      {open && (
        <DropdownMenu
          menuRef={menuRef}
          initialFocus="checked"
          label="View options"
          testId="sidebar-view-menu"
          onKeyDown={(e) => {
            // Tab leaves the menu rather than walking its options.
            if (e.key === "Tab") {
              e.preventDefault();
              setOpen(false);
              triggerRef.current?.focus();
              return;
            }
            moveMenuFocus(e);
          }}
          className="absolute right-0 top-full mt-1 z-50 w-60 max-h-[calc(100vh-8rem)]"
        >
          <MenuHeading decorative>Group by</MenuHeading>
          <div role="group" aria-label="Group by">
            {AXES.map((a) => (
              <Option
                key={a.axis}
                role="menuitemradio"
                checked={a.axis === axis}
                testId={`sidebar-axis-option-${a.axis}`}
                label={a.label}
                onSelect={(k) => choose(k, () => a.axis !== axis && onAxisChange(a.axis))}
              />
            ))}
          </div>
          <MenuSeparator />
          <MenuHeading decorative>Sort by</MenuHeading>
          <div role="group" aria-label="Sort by">
            {SORTS.map((s) => (
              <Option
                key={s.mode}
                role="menuitemradio"
                checked={!activePlugin && s.mode === sortMode}
                testId={`sidebar-sort-option-${s.mode}`}
                label={s.label}
                hint={s.hint}
                onSelect={(k) => choose(k, () => (activePlugin || s.mode !== sortMode) && onSortModeChange(s.mode))}
              />
            ))}
            {pluginSorts.map((spec) => {
              const checked = activePlugin?.pluginId === spec.pluginId && activePlugin.entryId === spec.entryId;
              return (
                <Option
                  key={`${spec.pluginId}:${spec.entryId}`}
                  role="menuitemradio"
                  checked={checked}
                  testId={`sidebar-sort-option-plugin-${spec.entryId}`}
                  label={spec.label}
                  onSelect={(k) =>
                    choose(k, () => !checked && onPluginSortChange({ pluginId: spec.pluginId, entryId: spec.entryId }))
                  }
                />
              );
            })}
          </div>
          <MenuSeparator />
          <Option
            role="menuitemcheckbox"
            checked={filterOpen}
            testId="sidebar-filter-toggle"
            label="Filter sessions"
            onSelect={(k) => choose(k, onToggleFilter)}
          />
          {hasFacets && (
            <Option
              role="menuitemcheckbox"
              checked={facetOpen}
              testId="sidebar-facet-toggle"
              label={facetsActive ? "Plugin facets · active" : "Plugin facets"}
              onSelect={(k) => choose(k, onToggleFacets)}
            />
          )}
        </DropdownMenu>
      )}
    </div>
  );
}
