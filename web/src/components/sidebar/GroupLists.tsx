import type { ComponentProps, ReactNode } from "react";
import { DndContext, MouseSensor, TouchSensor, useSensor, useSensors, type DragEndEvent } from "@dnd-kit/core";
import { SortableContext, arrayMove, verticalListSortingStrategy } from "@dnd-kit/sortable";
import type { RepoAppearanceUpdate } from "../../lib/repoAppearance";
import {
  nestedSidebarGroupShouldRender,
  orgNestedGroupShouldRender,
  sidebarGroupHasLiveWorkspace,
  sidebarGroupShouldRender,
  type NestedSidebarGroup,
  type OrgNestedGroup,
  type SidebarGroup,
  type SidebarWorkspaceView,
} from "../../lib/sidebarGroups";
import { workspaceIsPinned, workspaceIsSunk } from "../../lib/sidebarSort";
import { DragSuppressContext, typedClosestCenter } from "./dnd";
import { SessionRow } from "./SessionRow";
import { SidebarGroupHeader } from "./SidebarGroupHeader";
import { SortableRepoGroup, SortableSessionRow } from "./Sortable";
import type { DragHandleProps } from "./types";

type RowProps = Omit<ComponentProps<typeof SessionRow>, "indented">;

export interface ListContext {
  hasFilter: boolean;
  readOnly?: boolean;
  offline: boolean;
  displayedActiveId: string | null;
  onNew: () => void;
  onCreateSession: (repoPath: string) => void;
  onUpdateAppearance: (repoId: string, update: RepoAppearanceUpdate) => void;
  onPinProject?: (repoPath: string) => void;
  onUnpinProject?: (group: SidebarGroup) => void;
  onEditProjectSettings?: (group: SidebarGroup) => void;
  onArchiveGroup: (group: SidebarGroup) => void;
  rowProps: (v: SidebarWorkspaceView) => RowProps;
}

const liveViews = (group: SidebarGroup) => group.workspaces.filter((v) => !workspaceIsSunk(v.workspace));
const containsActive = (views: SidebarWorkspaceView[], id: string | null) => views.some((v) => v.workspace.id === id);

/** Header for any group level. `full` is the unfiltered group so "Archive all" covers hidden members. */
function GroupHeader({
  ctx,
  full,
  visible,
  onToggle,
  pinnable = false,
  createsInRepo = false,
  dragHandle,
}: {
  ctx: ListContext;
  full: SidebarGroup;
  visible: SidebarWorkspaceView[];
  onToggle: () => void;
  pinnable?: boolean;
  createsInRepo?: boolean;
  dragHandle?: DragHandleProps;
}) {
  const expanded = ctx.hasFilter || !full.collapsed;
  const locked = ctx.readOnly || ctx.offline;
  return (
    <SidebarGroupHeader
      group={{ ...full, collapsed: !expanded }}
      hasActiveChild={!expanded && containsActive(visible, ctx.displayedActiveId)}
      onClick={() => !ctx.hasFilter && onToggle()}
      onUpdateAppearance={ctx.onUpdateAppearance}
      onArchiveAll={locked ? undefined : () => ctx.onArchiveGroup(full)}
      onPin={locked || !pinnable ? undefined : ctx.onPinProject}
      onUnpin={locked || !pinnable ? undefined : ctx.onUnpinProject}
      onEditProject={locked || !pinnable ? undefined : ctx.onEditProjectSettings}
      onNewSession={() =>
        createsInRepo && full.capabilities.create === "repo" && full.repoPath
          ? ctx.onCreateSession(full.repoPath)
          : ctx.onNew()
      }
      offline={ctx.offline}
      dragHandle={dragHandle}
    />
  );
}

export function FlatGroupList({
  ctx,
  groups,
  fullGroups,
  reorderDisabled,
  groupDragDisabled,
  onToggleGroup,
  onReorderGroups,
  onReorderWorkspaces,
  dragSuppressRef,
}: {
  ctx: ListContext;
  groups: SidebarGroup[];
  fullGroups: SidebarGroup[];
  reorderDisabled: boolean;
  groupDragDisabled: boolean;
  onToggleGroup: (groupId: string) => void;
  onReorderGroups: (orderedGroupIds: string[]) => void;
  onReorderWorkspaces: (newOrder: string[]) => void;
  dragSuppressRef: React.MutableRefObject<number>;
}) {
  // Desktop activates on distance so a still click navigates; touch needs a hold so flicks scroll.
  const sensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 8 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 150, tolerance: 8 } }),
  );
  const byId = new Map(fullGroups.map((g) => [g.id, g]));

  const handleDragEnd = ({ active, over }: DragEndEvent) => {
    if (!over || active.id === over.id) return;
    if (active.data.current?.type === "group") {
      if (over.data.current?.type !== "group") return;
      const ids = fullGroups.map((g) => g.id);
      const from = ids.indexOf(String(active.id));
      const to = ids.indexOf(String(over.id));
      if (from >= 0 && to >= 0) onReorderGroups(arrayMove(ids, from, to));
      return;
    }
    // Rows reorder within their own group; the full flat id list is persisted so every client agrees.
    const groupIndex = fullGroups.findIndex((g) => g.workspaces.some((v) => v.key === active.id));
    const group = fullGroups[groupIndex];
    if (!group) return;
    const from = group.workspaces.findIndex((v) => v.key === active.id);
    const to = group.workspaces.findIndex((v) => v.key === over.id);
    if (from < 0 || to < 0) return;
    const reordered = arrayMove(group.workspaces, from, to);
    onReorderWorkspaces(
      fullGroups.flatMap((g, i) => (i === groupIndex ? reordered : g.workspaces).map((v) => v.workspace.id)),
    );
  };

  const liveGroups = groups.filter(sidebarGroupShouldRender);
  const body = (group: SidebarGroup, dragHandle?: DragHandleProps) => {
    const live = liveViews(group);
    return (
      <>
        <GroupHeader
          ctx={ctx}
          full={{ ...(byId.get(group.id) ?? group), collapsed: group.collapsed }}
          visible={group.workspaces}
          onToggle={() => onToggleGroup(group.id)}
          pinnable
          createsInRepo
          dragHandle={dragHandle}
        />
        {(ctx.hasFilter || !group.collapsed) && (
          <SortableContext items={live.map((v) => v.key)} strategy={verticalListSortingStrategy}>
            {live.map((v) => (
              <SortableSessionRow
                key={v.key}
                rowKey={v.key}
                {...ctx.rowProps(v)}
                onCreateSession={ctx.onCreateSession}
                // Pinned rows float to the top of their group, so dragging them is meaningless.
                dragDisabled={reorderDisabled || workspaceIsPinned(v.workspace)}
              />
            ))}
          </SortableContext>
        )}
      </>
    );
  };

  return (
    <DragSuppressContext.Provider value={dragSuppressRef}>
      <DndContext
        sensors={sensors}
        collisionDetection={typedClosestCenter}
        onDragEnd={reorderDisabled ? undefined : handleDragEnd}
      >
        <SortableContext items={liveGroups.map((g) => g.id)} strategy={verticalListSortingStrategy}>
          {liveGroups.map((group) => (
            <SortableRepoGroup key={group.id} groupId={group.id} disabled={groupDragDisabled}>
              {(handle) => body(group, groupDragDisabled ? undefined : handle)}
            </SortableRepoGroup>
          ))}
        </SortableContext>
      </DndContext>
    </DragSuppressContext.Provider>
  );
}

function IndentedLevel({ testId, repoId, children }: { testId: string; repoId: string; children: ReactNode }) {
  return (
    <div className="pl-3" data-testid={testId} data-repo-id={repoId}>
      {children}
    </div>
  );
}

export function NestedGroupList({
  ctx,
  groups,
  fullGroups,
  onToggleGroup,
  onToggleSubgroup,
}: {
  ctx: ListContext;
  groups: NestedSidebarGroup[];
  fullGroups: NestedSidebarGroup[];
  onToggleGroup: (groupId: string) => void;
  onToggleSubgroup: (repoId: string, groupPath: string) => void;
}) {
  return groups.filter(nestedSidebarGroupShouldRender).map(({ repo, subgroups }) => (
    <div key={repo.id} data-testid="sidebar-nested-repo" data-repo-id={repo.id}>
      <GroupHeader
        ctx={ctx}
        full={repo}
        visible={subgroups.flatMap((sg) => sg.workspaces)}
        onToggle={() => onToggleGroup(repo.id)}
        pinnable
        createsInRepo
      />
      {(ctx.hasFilter || !repo.collapsed) &&
        subgroups.filter(sidebarGroupHasLiveWorkspace).map((sg) => {
          const groupPath = sg.groupPath ?? "";
          const full =
            fullGroups.find((n) => n.repo.id === repo.id)?.subgroups.find((s) => (s.groupPath ?? "") === groupPath) ??
            sg;
          return (
            <IndentedLevel key={`${repo.id}::${groupPath}`} testId="sidebar-nested-subgroup" repoId={repo.id}>
              <GroupHeader
                ctx={ctx}
                full={{ ...full, collapsed: sg.collapsed }}
                visible={sg.workspaces}
                onToggle={() => onToggleSubgroup(repo.id, groupPath)}
              />
              {(ctx.hasFilter || !sg.collapsed) &&
                liveViews(sg).map((v) => (
                  <SessionRow key={`${repo.id}::${groupPath}::${v.key}`} {...ctx.rowProps(v)} indented />
                ))}
            </IndentedLevel>
          );
        })}
    </div>
  ));
}

export function OrgGroupList({
  ctx,
  groups,
  fullGroups,
  onToggleOrg,
  onToggleOrgRepo,
}: {
  ctx: ListContext;
  groups: OrgNestedGroup[];
  fullGroups: OrgNestedGroup[];
  onToggleOrg: (orgId: string) => void;
  onToggleOrgRepo: (orgId: string, repoId: string) => void;
}) {
  const fullRepoById = new Map(fullGroups.flatMap((o) => o.repos.map((r) => [`${o.org.id}::${r.id}`, r] as const)));
  return groups.filter(orgNestedGroupShouldRender).map(({ org, repos }) => (
    <div key={org.id} data-testid="sidebar-org-group" data-org-id={org.id}>
      <GroupHeader
        ctx={ctx}
        full={org}
        visible={repos.flatMap((r) => r.workspaces)}
        onToggle={() => onToggleOrg(org.id)}
      />
      {(ctx.hasFilter || !org.collapsed) &&
        repos.filter(sidebarGroupShouldRender).map((repo) => {
          const full = fullRepoById.get(`${org.id}::${repo.id}`) ?? repo;
          return (
            <IndentedLevel key={`${org.id}::${repo.id}`} testId="sidebar-org-repo" repoId={repo.id}>
              <GroupHeader
                ctx={ctx}
                full={{ ...full, collapsed: repo.collapsed }}
                visible={repo.workspaces}
                onToggle={() => onToggleOrgRepo(org.id, repo.id)}
                createsInRepo
              />
              {(ctx.hasFilter || !repo.collapsed) &&
                liveViews(repo).map((v) => (
                  <SessionRow key={`${org.id}::${repo.id}::${v.key}`} {...ctx.rowProps(v)} indented />
                ))}
            </IndentedLevel>
          );
        })}
    </div>
  ));
}
