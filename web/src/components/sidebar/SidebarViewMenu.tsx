import { useId, useRef, useState } from "react";
import { SlidersHorizontal } from "lucide-react";

import type { PluginSortSpec } from "../../lib/pluginUi";
import {
  DEFAULT_SIDEBAR_VIEW,
  GROUP_BY_LABEL,
  SORT_LABEL,
  type SidebarGroupBy,
  type SidebarSort,
} from "../../lib/sidebarView";
import { DropdownMenu, MenuHeading, MenuItem, MenuSeparator } from "../ContextMenu";
import { moveMenuFocus } from "../menuFocus";
import { Tooltip } from "../Tooltip";
import { useOutsideDismiss } from "../useOutsideDismiss";
import { TOOLBAR_BUTTON, TOOLBAR_TINT } from "./styles";

type PluginSortRef = { pluginId: string; entryId: string };

const GROUPINGS: readonly SidebarGroupBy[] = ["project", "group"];
const SORTS: readonly SidebarSort[] = ["custom", "newest", "name"];

function Option({
  role,
  checked,
  testId,
  label,
  hint,
  disabled,
  onSelect,
}: {
  role: "menuitemradio" | "menuitemcheckbox";
  checked: boolean;
  testId: string;
  label: string;
  hint?: string;
  disabled?: boolean;
  /** `keyboard` when Enter or Space picked it, not a pointer. */
  onSelect: (keyboard: boolean) => void;
}) {
  return (
    <MenuItem
      role={role}
      checked={checked}
      disabled={disabled}
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

/** The sidebar's one view control, as Claude Code desktop keeps a single icon by its list: the grouping and sort
 *  every Client shares, this browser's plugin sorts, and whether the filter and plugin facets show. A dot marks a
 *  view other than the default. */
export function SidebarViewMenu({
  groupBy,
  onGroupByChange,
  sort,
  onSortChange,
  pluginSorts,
  pluginSortRef,
  onPluginSortChange,
  lockReason = null,
  onNewGroup,
  newGroupDisabled = false,
  filterOpen,
  filterActive = false,
  onToggleFilter,
  hasFacets,
  facetsActive,
  facetOpen,
  onToggleFacets,
}: {
  groupBy: SidebarGroupBy;
  onGroupByChange: (groupBy: SidebarGroupBy) => void;
  sort: SidebarSort;
  onSortChange: (sort: SidebarSort) => void;
  pluginSorts: PluginSortSpec[];
  pluginSortRef: PluginSortRef | null;
  onPluginSortChange: (ref: PluginSortRef) => void;
  /** Why this viewer cannot change the shared view, which is the Daemon's: a read-only Daemon or CityHall. */
  lockReason?: string | null;
  /** Opens New group, which asks for a name and the Sessions to put in it. */
  onNewGroup: () => void;
  newGroupDisabled?: boolean;
  filterOpen: boolean;
  /** A filter applies while its panel is closed, such as Needs you. */
  filterActive?: boolean;
  onToggleFilter: () => void;
  hasFacets: boolean;
  facetsActive: boolean;
  facetOpen: boolean;
  onToggleFacets: () => void;
}) {
  const [open, setOpen] = useState(false);
  const locked = lockReason !== null;
  const reasonId = useId();
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
  const customized =
    groupBy !== DEFAULT_SIDEBAR_VIEW.group_by ||
    sort !== DEFAULT_SIDEBAR_VIEW.sort ||
    !!activePlugin ||
    filterOpen ||
    filterActive ||
    facetsActive;
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
          data-group-by={groupBy}
          data-sort={activePlugin ? "plugin" : sort}
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
          describedBy={lockReason ? reasonId : undefined}
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
          {lockReason && (
            <div
              id={reasonId}
              className="px-3 pt-1 pb-1.5 text-[12px] text-text-muted"
              data-testid="sidebar-view-lock-reason"
            >
              {lockReason}
            </div>
          )}
          <MenuHeading decorative>Group by</MenuHeading>
          <div role="group" aria-label="Group by">
            {GROUPINGS.map((value) => (
              <Option
                key={value}
                role="menuitemradio"
                checked={value === groupBy}
                testId={`sidebar-view-group-${value}`}
                label={GROUP_BY_LABEL[value]}
                disabled={locked}
                onSelect={(k) => choose(k, () => value !== groupBy && onGroupByChange(value))}
              />
            ))}
          </div>
          {/* Under Group by, past a divider, so it does not read as a third grouping. */}
          <MenuSeparator />
          <MenuItem
            role="menuitem"
            tabIndex={-1}
            testId="sidebar-view-new-group"
            disabled={newGroupDisabled}
            onClick={() => {
              setOpen(false);
              onNewGroup();
            }}
          >
            + New group…
          </MenuItem>
          <MenuSeparator />
          <MenuHeading decorative>Sort by</MenuHeading>
          <div role="group" aria-label="Sort by">
            {SORTS.map((value) => (
              <Option
                key={value}
                role="menuitemradio"
                checked={!activePlugin && value === sort}
                testId={`sidebar-view-sort-${value}`}
                label={SORT_LABEL[value]}
                hint={value === "custom" ? "drag to reorder" : undefined}
                // Picking the shared sort back only drops this browser's plugin sort, which writes nothing.
                disabled={locked && !(activePlugin && value === sort)}
                onSelect={(k) => choose(k, () => (activePlugin || value !== sort) && onSortChange(value))}
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
                  hint="this browser"
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
            label={filterActive && !filterOpen ? "Filter sessions · active" : "Filter sessions"}
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
