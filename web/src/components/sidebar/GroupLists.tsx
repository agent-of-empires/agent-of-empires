import type { ComponentProps, MutableRefObject, ReactNode } from "react";
import { DndContext, MouseSensor, TouchSensor, useSensor, useSensors, type DragEndEvent } from "@dnd-kit/core";
import { SortableContext, verticalListSortingStrategy } from "@dnd-kit/sortable";
import {
  nestedSidebarGroupShouldRender,
  sidebarGroupHasLiveWorkspace,
  sidebarGroupShouldRender,
  UNGROUPED_GROUP_ID,
  type NestedSidebarGroup,
  type SidebarGroup,
  type SidebarWorkspaceView,
} from "../../lib/sidebarGroups";
import { workspaceIsPinned, workspaceIsSunk } from "../../lib/sidebarSort";
import { dragEnded, dragStarted, typedClosestCenter, useReleaseDragOnUnmount, type DragClickGuard } from "./dnd";
import { SessionRow } from "./SessionRow";
import { SidebarGroupHeader } from "./SidebarGroupHeader";
import { ROWS, rowsId } from "./keyboardNav";
import { SortableGroupBlock, SortableSessionRow } from "./Sortable";
import type { DragHandleProps } from "./types";

type RowProps = Omit<ComponentProps<typeof SessionRow>, "indented">;

export interface ListContext {
  hasFilter: boolean;
  readOnly?: boolean;
  offline: boolean;
  displayedActiveId: string | null;
  displayedActiveSessionId: string | null;
  onNew: (prefill?: { group?: string }) => void;
  onCreateSession: (repoPath: string) => void;
  onPinProject?: (repoPath: string) => void;
  onUnpinProject?: (group: SidebarGroup) => void;
  onEditProjectSettings?: (group: SidebarGroup) => void;
  onArchiveGroup: (group: SidebarGroup) => void;
  rowProps: (v: SidebarWorkspaceView) => RowProps;
}

const liveViews = (group: SidebarGroup) => group.workspaces.filter((v) => !workspaceIsSunk(v.workspace));
// A Group slice holds only that Group's sessions, so the workspace id alone would mark every slice.
const containsActive = (views: SidebarWorkspaceView[], id: string | null, sessionId: string | null) =>
  views.some(
    (v) => v.workspace.id === id && (sessionId == null || v.workspace.sessions.some((s) => s.id === sessionId)),
  );

/** Wraps one Group's rows, collapsed or not, so its header's `aria-controls` always resolves. */
function GroupRows({ id, children }: { id: string; children: ReactNode }) {
  return (
    <div id={id} {...{ [ROWS]: "" }}>
      {children}
    </div>
  );
}

/** Header for any group level. `full` is the unfiltered group so "Archive all" covers hidden members. */
function GroupHeader({
  ctx,
  full,
  visible,
  onToggle,
  pinnable = false,
  createsInRepo = false,
  dragHandle,
  rows,
}: {
  ctx: ListContext;
  full: SidebarGroup;
  visible: SidebarWorkspaceView[];
  onToggle: () => void;
  pinnable?: boolean;
  createsInRepo?: boolean;
  dragHandle?: DragHandleProps;
  /** Id of the `GroupRows` this header controls. */
  rows: string;
}) {
  const expanded = ctx.hasFilter || !full.collapsed;
  const locked = ctx.readOnly || ctx.offline;
  return (
    <SidebarGroupHeader
      group={{ ...full, collapsed: !expanded }}
      hasActiveChild={!expanded && containsActive(visible, ctx.displayedActiveId, ctx.displayedActiveSessionId)}
      onClick={() => !ctx.hasFilter && onToggle()}
      onArchiveAll={locked ? undefined : () => ctx.onArchiveGroup(full)}
      onPin={locked || !pinnable ? undefined : ctx.onPinProject}
      onUnpin={locked || !pinnable ? undefined : ctx.onUnpinProject}
      onEditProject={locked || !pinnable ? undefined : ctx.onEditProjectSettings}
      onNewSession={() =>
        createsInRepo && full.capabilities.create === "repo" && full.repoPath
          ? ctx.onCreateSession(full.repoPath)
          : full.kind === "sessionGroup" && full.groupPath
            ? ctx.onNew({ group: full.groupPath })
            : ctx.onNew()
      }
      offline={ctx.offline}
      dragHandle={dragHandle}
      rowsId={rows}
    />
  );
}

/** What a drag may do; rows reorder within their own header, headers among themselves. */
export interface DragOptions {
  rowsDisabled: boolean;
  headersDisabled: boolean;
  onMoveWorkspace: (activeId: string, overId: string, after: boolean) => void;
  onMoveRepository: (activeId: string, overId: string, after: boolean) => void;
  /** Moves a Group header among its siblings, by path. */
  onMoveGroup: (activePath: string, overPath: string, after: boolean) => void;
  dragClickGuard: MutableRefObject<DragClickGuard>;
}

interface SortableData {
  type?: "group" | "sessionGroup" | "workspace";
  workspaceId?: string;
  groupPath?: string;
  sortable?: { containerId: string | number; index: number };
}

function SidebarDnd({ drag, children }: { drag: DragOptions; children: ReactNode }) {
  useReleaseDragOnUnmount(drag.dragClickGuard);
  // Desktop activates on distance so a still click navigates; touch needs a hold so flicks scroll.
  const sensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 8 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 150, tolerance: 8 } }),
  );
  const handleDragEnd = ({ active, over }: DragEndEvent) => {
    if (!over || active.id === over.id) return;
    const from = active.data.current as SortableData | undefined;
    const to = over.data.current as SortableData | undefined;
    // A row dropped under another header, or a header on a row, is not a move.
    if (!from?.sortable || !to?.sortable || from.sortable.containerId !== to.sortable.containerId) return;
    const after = to.sortable.index > from.sortable.index;
    if (from.type === "group" && !drag.headersDisabled)
      drag.onMoveRepository(String(active.id), String(over.id), after);
    if (from.type === "sessionGroup" && !drag.headersDisabled && from.groupPath && to.groupPath) {
      drag.onMoveGroup(from.groupPath, to.groupPath, after);
    }
    if (from.type === "workspace" && !drag.rowsDisabled && from.workspaceId && to.workspaceId) {
      drag.onMoveWorkspace(from.workspaceId, to.workspaceId, after);
    }
  };
  return (
    <DndContext
      sensors={sensors}
      collisionDetection={typedClosestCenter}
      onDragStart={() => dragStarted(drag.dragClickGuard.current)}
      onDragEnd={(event) => {
        dragEnded(drag.dragClickGuard.current);
        handleDragEnd(event);
      }}
      onDragCancel={() => dragEnded(drag.dragClickGuard.current)}
    >
      {children}
    </DndContext>
  );
}

/** One header's live rows, each its own drag handle. */
function SortableRows({ ctx, group, drag }: { ctx: ListContext; group: SidebarGroup; drag: DragOptions }) {
  const live = liveViews(group);
  return (
    <SortableContext items={live.map((v) => v.key)} strategy={verticalListSortingStrategy}>
      {live.map((v) => (
        <SortableSessionRow
          key={v.key}
          {...ctx.rowProps(v)}
          onCreateSession={ctx.onCreateSession}
          // Pinned rows float to the top of their group, so dragging them is meaningless.
          dragDisabled={drag.rowsDisabled || workspaceIsPinned(v.workspace)}
        />
      ))}
    </SortableContext>
  );
}

/** My groups: one header per Group, in the shared sort; headers reorder among their siblings, rows within their
 *  Group. */
export function FlatGroupList({
  ctx,
  groups,
  fullGroups,
  drag,
  onToggleGroup,
}: {
  ctx: ListContext;
  groups: SidebarGroup[];
  fullGroups: SidebarGroup[];
  drag: DragOptions;
  onToggleGroup: (groupId: string) => void;
}) {
  const byId = new Map(fullGroups.map((g) => [g.id, g]));
  const live = groups.filter(sidebarGroupShouldRender);
  const block = (group: SidebarGroup, dragHandle?: DragHandleProps) => (
    <>
      <GroupHeader
        ctx={ctx}
        full={{ ...(byId.get(group.id) ?? group), collapsed: group.collapsed }}
        visible={group.workspaces}
        onToggle={() => onToggleGroup(group.id)}
        dragHandle={dragHandle}
        rows={rowsId(group.id)}
      />
      <GroupRows id={rowsId(group.id)}>
        {(ctx.hasFilter || !group.collapsed) && <SortableRows ctx={ctx} group={group} drag={drag} />}
      </GroupRows>
    </>
  );
  const named = live.filter((g) => g.id !== UNGROUPED_GROUP_ID);
  return (
    <SidebarDnd drag={drag}>
      <SortableContext items={named.map((g) => g.id)} strategy={verticalListSortingStrategy}>
        {named.map((group) => (
          <SortableGroupBlock
            key={group.id}
            groupId={group.id}
            groupPath={group.groupPath}
            disabled={drag.headersDisabled}
          >
            {(handle) => block(group, drag.headersDisabled ? undefined : handle)}
          </SortableGroupBlock>
        ))}
      </SortableContext>
      {live
        .filter((g) => g.id === UNGROUPED_GROUP_ID)
        .map((group) => (
          <div key={group.id}>{block(group)}</div>
        ))}
    </SidebarDnd>
  );
}

function IndentedLevel({ testId, repoId, children }: { testId: string; repoId: string; children: ReactNode }) {
  return (
    <div className="pl-3" data-testid={testId} data-repo-id={repoId}>
      {children}
    </div>
  );
}

/** Repository: a header per repository, its ungrouped rows straight under it, then its Groups. */
export function NestedGroupList({
  ctx,
  groups,
  fullGroups,
  drag,
  onToggleRepo,
  onToggleSubgroup,
}: {
  ctx: ListContext;
  groups: NestedSidebarGroup[];
  fullGroups: NestedSidebarGroup[];
  drag: DragOptions;
  onToggleRepo: (repoId: string) => void;
  onToggleSubgroup: (repoId: string, groupPath: string) => void;
}) {
  const live = groups.filter(nestedSidebarGroupShouldRender);
  const body = ({ repo, subgroups }: NestedSidebarGroup, dragHandle?: DragHandleProps) => {
    const repoRows = rowsId(`repo:${encodeURIComponent(repo.id)}`);
    const fullSubgroups = fullGroups.find((n) => n.repo.id === repo.id)?.subgroups ?? [];
    const shown = subgroups.filter(sidebarGroupHasLiveWorkspace);
    const named = shown.filter((sg) => sg.id !== UNGROUPED_GROUP_ID);
    // Every id part is encoded, top level included, so `::` is only ever the separator.
    const subgroupId = (sg: SidebarGroup) =>
      `repo:${encodeURIComponent(repo.id)}::${encodeURIComponent(sg.groupPath ?? "")}`;
    return (
      <>
        <GroupHeader
          ctx={ctx}
          full={{ ...(fullGroups.find((n) => n.repo.id === repo.id)?.repo ?? repo), collapsed: repo.collapsed }}
          visible={subgroups.flatMap((sg) => sg.workspaces)}
          onToggle={() => onToggleRepo(repo.id)}
          pinnable
          createsInRepo
          dragHandle={dragHandle}
          rows={repoRows}
        />
        <GroupRows id={repoRows}>
          {(ctx.hasFilter || !repo.collapsed) && (
            <>
              {shown
                .filter((sg) => sg.id === UNGROUPED_GROUP_ID)
                .map((sg) => (
                  <SortableRows key={`${repo.id}::`} ctx={ctx} group={sg} drag={drag} />
                ))}
              <SortableContext items={named.map(subgroupId)} strategy={verticalListSortingStrategy}>
                {named.map((sg) => {
                  const groupPath = sg.groupPath ?? "";
                  const full = fullSubgroups.find((s) => (s.groupPath ?? "") === groupPath) ?? sg;
                  const rows = rowsId(subgroupId(sg));
                  return (
                    <SortableGroupBlock
                      key={subgroupId(sg)}
                      groupId={subgroupId(sg)}
                      groupPath={groupPath}
                      disabled={drag.headersDisabled}
                    >
                      {(handle) => (
                        <IndentedLevel testId="sidebar-nested-subgroup" repoId={repo.id}>
                          <GroupHeader
                            ctx={ctx}
                            full={{ ...full, collapsed: sg.collapsed }}
                            visible={sg.workspaces}
                            onToggle={() => onToggleSubgroup(repo.id, groupPath)}
                            dragHandle={drag.headersDisabled ? undefined : handle}
                            rows={rows}
                          />
                          <GroupRows id={rows}>
                            {(ctx.hasFilter || !sg.collapsed) && <SortableRows ctx={ctx} group={sg} drag={drag} />}
                          </GroupRows>
                        </IndentedLevel>
                      )}
                    </SortableGroupBlock>
                  );
                })}
              </SortableContext>
            </>
          )}
        </GroupRows>
      </>
    );
  };
  return (
    <SidebarDnd drag={drag}>
      <SortableContext items={live.map((n) => n.repo.id)} strategy={verticalListSortingStrategy}>
        {live.map((group) => (
          <SortableGroupBlock key={group.repo.id} groupId={group.repo.id} disabled={drag.headersDisabled}>
            {(handle) => (
              <div data-testid="sidebar-nested-repo" data-repo-id={group.repo.id}>
                {body(group, drag.headersDisabled ? undefined : handle)}
              </div>
            )}
          </SortableGroupBlock>
        ))}
      </SortableContext>
    </SidebarDnd>
  );
}
