import type { SidebarAxis } from "../../lib/sidebarAxis";
import { SidebarViewMenu } from "./SidebarViewMenu";
import type { SidebarSortMode } from "../../lib/sidebarSort";
import { toneTextClass, type PluginFacetSpec, type PluginSortSpec } from "../../lib/pluginUi";

type PluginSortRef = { pluginId: string; entryId: string };

/** Heading above the list; the picker labels its axes itself. */
const AXIS_HEADING: Record<SidebarAxis, string> = {
  repo: "Sessions",
  org: "Sessions",
  group: "Groups",
  "repo+group": "Sessions",
  status: "Status",
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
  onClose: () => void;
}) {
  const axisHeading = AXIS_HEADING[axis];
  return (
    <div className={`${compact ? "px-1" : "pl-4 pr-2"} pt-4 pb-1 flex items-center`}>
      {compact ? (
        <span className="flex-1" />
      ) : (
        <>
          <span data-testid="sidebar-axis-heading" className="text-[12px] font-medium text-text-dim flex-1">
            {axisHeading}
          </span>
          <SidebarViewMenu
            axis={axis}
            onAxisChange={onAxisChange}
            sortMode={sortMode}
            onSortModeChange={onSortModeChange}
            pluginSorts={pluginSorts}
            pluginSortRef={pluginSortRef}
            onPluginSortChange={onPluginSortChange}
            filterOpen={filterOpen}
            onToggleFilter={onToggleFilter}
            hasFacets={hasFacets}
            facetsActive={facetsActive}
            facetOpen={facetOpen}
            onToggleFacets={onToggleFacets}
          />
        </>
      )}
      <button
        onClick={onClose}
        aria-label="Close sidebar"
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
