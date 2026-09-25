import { useCallback, useMemo, useRef, useState } from "react";
import { usePluginUiEntries } from "../lib/pluginUiContext";
import { pluginSortSpecs } from "../lib/pluginUi";
import type { ProjectInfo, RepoGroup, Workspace } from "../lib/types";
import { SidebarSystemHealth } from "./SystemHealthStrip";
import type { SidebarAxis } from "../lib/sidebarAxis";
import {
  archivableWorkspaces,
  type NestedSidebarGroup,
  type OrgNestedGroup,
  type SidebarGroup,
  type SidebarWorkspaceView,
} from "../lib/sidebarGroups";
import type { RepoAppearanceUpdate } from "../lib/repoAppearance";
import { useWebSettings } from "../hooks/useWebSettings";
import { SidebarCompactContext } from "../lib/sidebarCompact";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import { useServerDown } from "../lib/connectionState";
import type { SidebarSortMode } from "../lib/sidebarSort";
import { useSidebarTriage } from "../hooks/useSidebarTriage";
import { ProjectsSection } from "./ProjectsSection";
import { FoldChevron, PlusIcon, StrokeIcon } from "./icons";
import { usePersistedFlag } from "./usePersistedFlag";
import { useSuppressClickAfterDrag } from "./sidebar/dnd";
import {
  filterFlat,
  filterNested,
  filterOrg,
  makeRowFilter,
  renderedOrder,
  sunkViews,
  uniqueWorkspaces,
} from "./sidebar/filterGroups";
import { FlatGroupList, NestedGroupList, OrgGroupList, type ListContext } from "./sidebar/GroupLists";
import { SessionRow } from "./sidebar/SessionRow";
import { FacetPanel, SidebarToolbar } from "./sidebar/SidebarToolbar";
import { TrashMenu } from "./sidebar/TrashMenu";
import { useFacetFilter } from "./sidebar/useFacetFilter";
import { useSidebarSelection } from "./sidebar/useSidebarSelection";
import { useSidebarWidth } from "./sidebar/useSidebarWidth";

interface Props {
  groups: SidebarGroup[];
  /** Used only when `axis === "repo+group"`. */
  nestedGroups: NestedSidebarGroup[];
  /** Used only when `axis === "org"`. */
  orgGroups: OrgNestedGroup[];
  /** Workspaces whose every session is trashed, computed by the parent from the unsliced list. */
  trashedWorkspaces?: Workspace[];
  onToggleSubgroup: (repoId: string, groupPath: string) => void;
  onToggleOrg: (orgId: string) => void;
  onToggleOrgRepo: (orgId: string, repoId: string) => void;
  onReorderWorkspaces: (newOrder: string[]) => void;
  onReorderGroups: (orderedGroupIds: string[]) => void;
  activeId: string | null;
  open: boolean;
  onToggle: () => void;
  onSelect: (workspaceId: string, sessionId: string | null) => void;
  onToggleGroup: (groupId: string) => void;
  onUpdateRepoAppearance: (repoId: string, update: RepoAppearanceUpdate) => void;
  onNew: () => void;
  onCreateSession: (repoPath: string) => void;
  onPinProject?: (repoPath: string) => void;
  onUnpinProject?: (group: SidebarGroup) => void;
  /** Opens project settings, registering the repo first if needed. */
  onEditProjectSettings?: (group: SidebarGroup) => void;
  /** Saved projects with no live session. */
  savedProjects: RepoGroup[];
  onAddProject: () => void;
  onEditProject: (project: ProjectInfo) => void;
  onRemoveProject: (group: RepoGroup) => void;
  onSettings: () => void;
  onDeleteSession?: (sessionIds: string[]) => void;
  /** Receives every session id of the trashed workspace. */
  onRestoreSession?: (sessionIds: string[]) => void;
  onEmptyTrash?: () => void;
  onStopSession?: (sessionId: string) => void;
  onStartSession?: (sessionId: string) => void;
  onSwitchView?: (sessionId: string, toStructured: boolean) => void;
  readOnly?: boolean;
  /** False in CityHall client mode, which hides project management. */
  canManageProjects?: boolean;
  sortMode: SidebarSortMode;
  onSortModeChange: (mode: SidebarSortMode) => void;
  pluginSortRef: { pluginId: string; entryId: string } | null;
  onPluginSortChange: (ref: { pluginId: string; entryId: string }) => void;
  axis: SidebarAxis;
  onAxisChange: (axis: SidebarAxis) => void;
}

export function WorkspaceSidebar(props: Props) {
  const { groups, nestedGroups, orgGroups, trashedWorkspaces = [], activeId, open, readOnly, axis } = props;
  const { settings: webSettings, update: updateWebSettings } = useWebSettings();
  const rightSide = webSettings.sidebarSide === "right";
  const compact = webSettings.sidebarCompact;
  const offline = useServerDown();
  const { effectiveWidth, startResize } = useSidebarWidth(compact);

  const pluginUiEntries = usePluginUiEntries();
  const pluginSorts = useMemo(() => pluginSortSpecs(pluginUiEntries), [pluginUiEntries]);
  const facets = useFacetFilter(pluginUiEntries);
  const pluginSortActive =
    props.pluginSortRef != null &&
    pluginSorts.some((s) => s.pluginId === props.pluginSortRef!.pluginId && s.entryId === props.pluginSortRef!.entryId);

  // Reorder rebuilds order from the full list, so it is off whenever the visible order is computed or filtered,
  // and on axes (the user-group axis) whose groups cannot persist an order.
  const reorderDisabled =
    !!readOnly ||
    props.sortMode === "lastActivity" ||
    pluginSortActive ||
    facets.activeFacets.length > 0 ||
    groups.some((g) => !g.capabilities.reorder);
  const dragSuppressRef = useRef<number>(0);
  useSuppressClickAfterDrag(dragSuppressRef);

  const [filterOpen, setFilterOpen] = useState(false);
  const [filterQuery, setFilterQuery] = useState("");
  const [facetOpen, setFacetOpen] = useState(false);
  const filterRef = useRef<HTMLInputElement>(null);
  const [sunkExpanded, toggleSunkExpanded] = usePersistedFlag("aoe-sidebar-sunk-expanded", false);
  // The compact rail has no filter controls, so a query typed earlier stops applying without being cleared.
  const activeFilterQuery = compact ? "" : filterQuery;
  const q = activeFilterQuery.trim().toLowerCase();
  const hasFilter = !!q || facets.activeFacets.length > 0;

  const { matchesFacets } = facets;
  const [filteredGroups, filteredNested, filteredOrgGroups] = useMemo(() => {
    if (!hasFilter) return [groups, nestedGroups, orgGroups] as const;
    const keep = makeRowFilter(q, matchesFacets);
    return [filterFlat(groups, keep), filterNested(nestedGroups, keep), filterOrg(orgGroups, keep)] as const;
  }, [hasFilter, q, matchesFacets, groups, nestedGroups, orgGroups]);
  const isNested = axis === "repo+group";
  const isOrgAxis = axis === "org";

  const allWorkspaces = useMemo(() => uniqueWorkspaces(groups), [groups]);
  const triage = useSidebarTriage(allWorkspaces);
  const orderedIds = useMemo(
    () => renderedOrder(filteredGroups, hasFilter, sunkExpanded),
    [filteredGroups, hasFilter, sunkExpanded],
  );
  const selection = useSidebarSelection({
    allWorkspaces,
    orderedIds,
    triage,
    readOnly,
    activeId,
    onSelect: props.onSelect,
  });
  const { onBulkArchive } = selection;

  // Archiving a whole group is a bigger hammer than one row, so it confirms first, like the TUI.
  const onArchiveGroup = useCallback(
    (group: SidebarGroup) => {
      const wss = archivableWorkspaces(group);
      if (wss.length === 0) return;
      const noun = wss.length === 1 ? "session" : "sessions";
      if (!window.confirm(`Archive all ${wss.length} ${noun} in "${group.displayName}"?`)) return;
      onBulkArchive(wss, true);
    },
    [onBulkArchive],
  );

  const ctx: ListContext = {
    hasFilter,
    readOnly,
    offline,
    displayedActiveId: selection.displayedActiveId,
    onNew: props.onNew,
    onCreateSession: props.onCreateSession,
    onUpdateAppearance: props.onUpdateRepoAppearance,
    onPinProject: props.onPinProject,
    onUnpinProject: props.onUnpinProject,
    onEditProjectSettings: props.onEditProjectSettings,
    onArchiveGroup,
    rowProps: (v: SidebarWorkspaceView) => ({
      workspace: v.workspace,
      isActive: v.workspace.id === selection.displayedActiveId,
      isSelected: selection.isSelected(v.workspace.id),
      onActivate: selection.handleRowActivate,
      onDelete: props.onDeleteSession,
      onStop: props.onStopSession,
      onStart: props.onStartSession,
      onSwitchView: props.onSwitchView,
      readOnly,
      optimistic: triage.optimisticFor(v.workspace.id),
      onPinToggle: triage.pinToggle,
      onArchiveToggle: triage.archiveToggle,
      onSnooze: triage.snooze,
      onUnreadToggle: triage.unreadToggle,
      bulkApi: selection.rowBulkApi,
    }),
  };

  const savedProjectsMatchQuery =
    !!q &&
    props.savedProjects.some((p) => p.displayName.toLowerCase().includes(q) || p.repoPath.toLowerCase().includes(q));
  const hasResults =
    (isNested ? filteredNested.length : isOrgAxis ? filteredOrgGroups.length : filteredGroups.length) > 0 ||
    savedProjectsMatchQuery;
  const sunk = sunkViews(
    isNested
      ? filteredNested.flatMap((ng) => ng.subgroups)
      : isOrgAxis
        ? filteredOrgGroups.flatMap((og) => og.repos)
        : filteredGroups,
  );

  const toggleFilter = () => {
    setFilterOpen((o) => {
      if (o) setFilterQuery("");
      return !o;
    });
    if (!filterOpen) requestAnimationFrame(() => filterRef.current?.focus());
  };

  return (
    <SidebarCompactContext.Provider value={compact}>
      <div
        className={`fixed top-12 inset-x-0 bottom-0 z-30 md:hidden transition-opacity duration-300 ${
          open ? "bg-black/50" : "opacity-0 pointer-events-none"
        }`}
        onClick={props.onToggle}
      />
      <div
        {...tourAnchor(TOUR_ANCHORS.sidebar)}
        style={{ width: effectiveWidth }}
        data-compact={compact ? "true" : undefined}
        className={`fixed top-12 bottom-0 z-40 md:static md:z-auto bg-surface-800 border-surface-700/60 flex flex-col md:h-full shrink-0 transition-transform duration-300 ease-in-out md:transition-none ${
          rightSide ? "right-0 border-l md:border-l-0 md:border-r" : "left-0 border-r"
        } ${open ? "translate-x-0" : `${rightSide ? "translate-x-full" : "-translate-x-full"} md:hidden`}`}
      >
        <SidebarToolbar
          compact={compact}
          axis={axis}
          onAxisChange={props.onAxisChange}
          sortMode={props.sortMode}
          onSortModeChange={props.onSortModeChange}
          pluginSorts={pluginSorts}
          pluginSortRef={props.pluginSortRef}
          onPluginSortChange={props.onPluginSortChange}
          hasFacets={facets.facetSpecs.length > 0}
          facetsActive={facets.activeFacets.length > 0}
          facetOpen={facetOpen}
          onToggleFacets={() => setFacetOpen((o) => !o)}
          filterOpen={filterOpen}
          onToggleFilter={toggleFilter}
          offline={offline}
          onNew={props.onNew}
          onToggleCompact={() => updateWebSettings({ sidebarCompact: !compact })}
          onClose={props.onToggle}
        />

        {filterOpen && !compact && (
          <div className="px-3 pb-2">
            <input
              ref={filterRef}
              type="text"
              value={filterQuery}
              onChange={(e) => setFilterQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") toggleFilter();
              }}
              placeholder="Filter by name, branch, agent..."
              data-testid="sidebar-filter-input"
              className="w-full bg-surface-800 border border-surface-700 rounded-md px-2.5 py-1.5 text-[13px] text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
            />
          </div>
        )}

        {facetOpen && !compact && facets.facetSpecs.length > 0 && (
          <FacetPanel
            facetSpecs={facets.facetSpecs}
            selectedValues={facets.selectedValues}
            onToggle={facets.toggleValue}
          />
        )}

        <div className="flex-1 overflow-y-auto overflow-x-hidden border-t border-surface-700/60">
          {isNested ? (
            <NestedGroupList
              ctx={ctx}
              groups={filteredNested}
              fullGroups={nestedGroups}
              onToggleGroup={props.onToggleGroup}
              onToggleSubgroup={props.onToggleSubgroup}
            />
          ) : isOrgAxis ? (
            <OrgGroupList
              ctx={ctx}
              groups={filteredOrgGroups}
              fullGroups={orgGroups}
              onToggleOrg={props.onToggleOrg}
              onToggleOrgRepo={props.onToggleOrgRepo}
            />
          ) : (
            <FlatGroupList
              ctx={ctx}
              groups={filteredGroups}
              fullGroups={groups}
              reorderDisabled={reorderDisabled}
              groupDragDisabled={reorderDisabled || q.length > 0}
              onToggleGroup={props.onToggleGroup}
              onReorderGroups={props.onReorderGroups}
              onReorderWorkspaces={props.onReorderWorkspaces}
              dragSuppressRef={dragSuppressRef}
            />
          )}

          {sunk.length > 0 && (
            <div data-testid="sidebar-sunk-section">
              <button
                onClick={toggleSunkExpanded}
                data-testid="sidebar-sunk-toggle"
                aria-expanded={sunkExpanded}
                className={`w-full flex items-center gap-2 py-1.5 text-[11px] font-mono uppercase text-text-muted hover:text-text-secondary hover:bg-surface-800/40 cursor-pointer transition-colors border-t border-surface-800/60 ${
                  compact ? "px-2" : "px-3 tracking-widest"
                }`}
              >
                <FoldChevron
                  className={`shrink-0 transition-transform duration-75 ${sunkExpanded ? "" : "-rotate-90"}`}
                />
                {/* The count and wide tracking do not fit the compact rail. */}
                <span className="truncate">Snoozed &amp; archived{compact ? "" : ` (${sunk.length})`}</span>
              </button>
              {sunkExpanded && sunk.map((v) => <SessionRow key={v.key} {...ctx.rowProps(v)} indented />)}
            </div>
          )}

          {(props.canManageProjects ?? true) && (
            <ProjectsSection
              projects={props.savedProjects}
              query={q}
              readOnly={readOnly}
              offline={offline}
              onCreateSession={props.onCreateSession}
              onAddProject={props.onAddProject}
              onEditProject={props.onEditProject}
              onRemoveProject={props.onRemoveProject}
            />
          )}

          {!hasResults && hasFilter && (
            <div className="px-4 py-8 text-center">
              <p className="text-sm text-text-muted">No matches for &ldquo;{activeFilterQuery}&rdquo;</p>
            </div>
          )}

          {!hasResults && !hasFilter && (
            <div className="px-4 py-10 text-center" data-testid="sidebar-empty-state">
              <p className="text-sm font-medium text-text-secondary">No sessions yet</p>
              <p className="mt-1 text-[13px] text-text-muted">Create a session to start working in a repo.</p>
              <button
                onClick={props.onNew}
                disabled={offline}
                className="mt-4 inline-flex items-center gap-1.5 rounded-md bg-brand-600 px-3 py-1.5 text-[13px] font-medium text-white hover:bg-brand-500 cursor-pointer transition-colors disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:bg-brand-600"
              >
                <PlusIcon size={14} />
                New session
              </button>
            </div>
          )}
        </div>

        <SidebarSystemHealth />

        <div className="border-t border-surface-700/20 p-2 flex items-center gap-1">
          {trashedWorkspaces.length > 0 && (
            <TrashMenu
              trashedWorkspaces={trashedWorkspaces}
              readOnly={readOnly}
              onOpen={selection.handleRowActivate}
              onRestore={(ids) => props.onRestoreSession?.(ids)}
              onDelete={(ids) => props.onDeleteSession?.(ids)}
              onEmptyTrash={() => props.onEmptyTrash?.()}
            />
          )}
          <button
            onClick={props.onSettings}
            {...tourAnchor(TOUR_ANCHORS.sidebarSettings)}
            className="w-8 h-8 shrink-0 flex items-center justify-center text-text-secondary hover:text-text-primary hover:bg-surface-800/50 cursor-pointer rounded-md transition-colors"
            title="Settings"
            aria-label="Settings"
          >
            <StrokeIcon size={16} strokeWidth="1.5">
              <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
              <circle cx="12" cy="12" r="3" />
            </StrokeIcon>
          </button>
        </div>
      </div>
      {/* Hidden with the panel so no dead drag bar lingers when the sidebar is closed. */}
      <div
        data-testid="sidebar-resize-handle"
        onMouseDown={startResize}
        className={`${open && !compact ? "hidden md:block" : "hidden"} w-1 cursor-col-resize shrink-0 bg-surface-800 hover:bg-brand-600/50 transition-colors duration-75`}
      />
    </SidebarCompactContext.Provider>
  );
}
