import { Layers, ListFilter } from "lucide-react";
import type { SidebarAxis } from "../../lib/sidebarAxis";
import type { SidebarSortMode } from "../../lib/sidebarSort";
import { toneTextClass, type PluginFacetSpec, type PluginSortSpec } from "../../lib/pluginUi";
import { OFFLINE_TITLE } from "../../lib/connectionState";
import { SidebarSortPicker } from "../SidebarSortPicker";
import { Tooltip } from "../Tooltip";
import { DISABLED_ICON_BUTTON, TOOLBAR_BUTTON, TOOLBAR_TINT } from "./styles";
import { StrokeIcon } from "../icons";

type PluginSortRef = { pluginId: string; entryId: string };

/** Each click moves to the next grouping axis. */
const AXES: Record<SidebarAxis, { next: SidebarAxis; heading: string; tooltip: string; aria: string }> = {
  repo: { next: "org", heading: "Sessions", tooltip: "Grouping: by repository", aria: "Group sessions by repository" },
  org: {
    next: "group",
    heading: "Sessions",
    tooltip: "Grouping: by organization",
    aria: "Group sessions by organization",
  },
  group: {
    next: "repo+group",
    heading: "Groups",
    tooltip: "Grouping: by user group",
    aria: "Group sessions by user group",
  },
  "repo+group": {
    next: "repo",
    heading: "Sessions",
    tooltip: "Grouping: by repository, then user group",
    aria: "Group sessions by repository, then user group",
  },
};

export function SidebarToolbar({
  compact,
  axis,
  onAxisChange,
  sortMode,
  onSortModeChange,
  pluginSorts,
  pluginSortRef,
  onPluginSortChange,
  hasFacets,
  facetsActive,
  facetOpen,
  onToggleFacets,
  filterOpen,
  onToggleFilter,
  offline,
  onNew,
  onToggleCompact,
  onClose,
}: {
  compact: boolean;
  axis: SidebarAxis;
  onAxisChange: (axis: SidebarAxis) => void;
  sortMode: SidebarSortMode;
  onSortModeChange: (mode: SidebarSortMode) => void;
  pluginSorts: PluginSortSpec[];
  pluginSortRef: PluginSortRef | null;
  onPluginSortChange: (ref: PluginSortRef) => void;
  hasFacets: boolean;
  facetsActive: boolean;
  facetOpen: boolean;
  onToggleFacets: () => void;
  filterOpen: boolean;
  onToggleFilter: () => void;
  offline: boolean;
  onNew: () => void;
  onToggleCompact: () => void;
  onClose: () => void;
}) {
  const axisSpec = AXES[axis];
  return (
    <div className={`${compact ? "px-1" : "px-3"} pt-3 pb-1 flex items-center`}>
      {compact ? (
        <span className="flex-1" />
      ) : (
        <>
          <span data-testid="sidebar-axis-heading" className="text-sm text-text-muted flex-1">
            {axisSpec.heading}
          </span>
          <Tooltip text={axisSpec.tooltip}>
            <button
              onClick={() => onAxisChange(axisSpec.next)}
              aria-pressed={axis !== "repo"}
              aria-label={axis === "repo" ? axisSpec.aria : `${axisSpec.aria}, currently pressed`}
              data-testid="sidebar-axis-toggle"
              data-axis={axis}
              className={`${TOOLBAR_BUTTON} ${TOOLBAR_TINT(axis !== "repo")}`}
            >
              <Layers className="h-3.5 w-3.5" />
            </button>
          </Tooltip>
          <SidebarSortPicker
            sortMode={sortMode}
            onSortModeChange={onSortModeChange}
            pluginSorts={pluginSorts}
            pluginSortRef={pluginSortRef}
            onPluginSortChange={onPluginSortChange}
          />
          {hasFacets && (
            <Tooltip text="Plugin facets">
              <button
                onClick={onToggleFacets}
                aria-haspopup="true"
                aria-expanded={facetOpen}
                aria-label="Plugin facet filters"
                data-testid="sidebar-facet-toggle"
                className={`relative ${TOOLBAR_BUTTON} ${TOOLBAR_TINT(facetsActive || facetOpen)}`}
              >
                <ListFilter className="h-3.5 w-3.5" />
                {facetsActive && (
                  <span className="absolute -right-0.5 -top-0.5 h-2 w-2 rounded-full bg-brand-500" aria-hidden />
                )}
              </button>
            </Tooltip>
          )}
          <Tooltip text="Filter">
            <button
              onClick={onToggleFilter}
              className={`${TOOLBAR_BUTTON} ${TOOLBAR_TINT(filterOpen, "text-text-secondary")}`}
              aria-label="Filter sessions"
            >
              <StrokeIcon size={14} strokeWidth="2">
                <polygon points="22 3 2 3 10 12.46 10 19 14 21 14 12.46 22 3" />
              </StrokeIcon>
            </button>
          </Tooltip>
          <Tooltip text={offline ? OFFLINE_TITLE : "New project session"}>
            <button
              onClick={onNew}
              disabled={offline}
              className={`w-8 h-8 flex items-center justify-center text-text-muted hover:text-text-secondary hover:bg-surface-800 cursor-pointer rounded-md transition-colors ${DISABLED_ICON_BUTTON}`}
              aria-label="New project session"
            >
              <StrokeIcon size={16} strokeWidth="1.5">
                <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
                <line x1="12" y1="11" x2="12" y2="17" />
                <line x1="9" y1="14" x2="15" y2="14" />
              </StrokeIcon>
            </button>
          </Tooltip>
        </>
      )}
      <Tooltip text={compact ? "Expand sidebar" : "Compact sidebar"}>
        <button
          onClick={onToggleCompact}
          aria-pressed={compact}
          aria-label={compact ? "Expand sidebar" : "Compact sidebar"}
          data-testid="sidebar-compact-toggle"
          className={`${TOOLBAR_BUTTON} ${TOOLBAR_TINT(compact)}`}
        >
          <StrokeIcon size={14} strokeWidth="2">
            <rect x="3" y="3" width="18" height="18" rx="2" />
            <line x1="9" y1="3" x2="9" y2="21" />
          </StrokeIcon>
        </button>
      </Tooltip>
      <button
        onClick={onClose}
        className="md:hidden w-8 h-8 flex items-center justify-center text-text-dim hover:text-text-secondary cursor-pointer rounded-md hover:bg-surface-800 ml-1"
      >
        &times;
      </button>
    </div>
  );
}

export function FacetPanel({
  facetSpecs,
  selectedValues,
  onToggle,
}: {
  facetSpecs: PluginFacetSpec[];
  selectedValues: (pluginId: string, entryId: string) => Set<string> | undefined;
  onToggle: (pluginId: string, entryId: string, value: string) => void;
}) {
  return (
    <div className="px-3 pb-2 flex flex-col gap-2" data-testid="sidebar-facet-panel">
      {facetSpecs.map((facet) => {
        const selected = selectedValues(facet.pluginId, facet.entryId);
        return (
          <div key={`${facet.pluginId}:${facet.entryId}`} data-plugin-id={facet.pluginId}>
            <div className="text-[11px] font-semibold uppercase tracking-wide text-text-dim mb-1">{facet.label}</div>
            <div className="flex flex-wrap gap-1">
              {facet.options.map((opt) => {
                const on = selected?.has(opt.value) ?? false;
                return (
                  <button
                    key={opt.value}
                    type="button"
                    aria-pressed={on}
                    data-testid={`sidebar-facet-option-${facet.entryId}-${opt.value}`}
                    onClick={() => onToggle(facet.pluginId, facet.entryId, opt.value)}
                    className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-mono cursor-pointer transition-colors ${
                      on
                        ? "bg-brand-500/15 text-brand-500 ring-1 ring-brand-500/40"
                        : `bg-surface-700/40 hover:bg-surface-700/70 ${toneTextClass(opt.tone)}`
                    }`}
                  >
                    {opt.label}
                  </button>
                );
              })}
            </div>
          </div>
        );
      })}
    </div>
  );
}
