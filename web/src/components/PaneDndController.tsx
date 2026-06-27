import { useCallback, useMemo, useState, type ReactNode } from "react";
import {
  DndContext,
  DragOverlay,
  MeasuringStrategy,
  MouseSensor,
  TouchSensor,
  pointerWithin,
  useDroppable,
  useSensor,
  useSensors,
  type CollisionDetection,
  type DragEndEvent,
  type DragOverEvent,
  type DragStartEvent,
} from "@dnd-kit/core";

import type { DockLocation } from "../lib/panes";
import type { PaneDisplay } from "./Dock";
import {
  PaneDndStateContext,
  pointerInsertsAfter,
  resolvePlacement,
  shouldApplyPlacement,
  type DockDropData,
  type DragSource,
  type DropTarget,
  type EmptyDockDropData,
  type PaneDndState,
  type PaneTabData,
  type RenderGroup,
} from "./paneDnd";

// Prefer the droppable the pointer is actually inside, and only among pane
// droppables, so a drag in the right column never magnetically snaps to a
// distant bottom target the way a global closestCenter would. A tab hit wins
// over everything (reorder), then a split-half hit (split into a new group),
// then group/empty-dock bodies. Mirrors the filtered-collision approach in
// WorkspaceSidebar (#1644).
const panesCollision: CollisionDetection = (args) => {
  const paneContainers = args.droppableContainers.filter((c) => {
    const t = c.data.current?.type;
    return t === "pane-tab" || t === "pane-group" || t === "pane-split" || t === "pane-empty-dock";
  });
  const hits = pointerWithin({ ...args, droppableContainers: paneContainers });
  const typeOf = (h: (typeof hits)[number]) => h.data?.droppableContainer?.data.current?.type;
  const tabHits = hits.filter((h) => typeOf(h) === "pane-tab");
  if (tabHits.length > 0) return tabHits;
  const splitHits = hits.filter((h) => typeOf(h) === "pane-split");
  return splitHits.length > 0 ? splitHits : hits;
};

interface Props {
  /** The rendered groups per dock (visible tabs, persisted group index), so
   *  drop targets line up with what the docks actually show. */
  groupsByDock: Record<DockLocation, RenderGroup[]>;
  descriptorFor: (id: string) => PaneDisplay;
  /** Reorder, move across docks/groups, or split into a new group. */
  onPlaceTab: (tabId: string, target: DropTarget) => void;
  children: ReactNode;
}

/** Owns the single DndContext spanning both docks: sensors, the pane-aware
 *  collision policy, the live drop target, a DragOverlay replica that follows
 *  the cursor across the distant docks, and the empty-dock landing zones. Docks
 *  stay presentational and read the drop state through usePaneDnd. */
export function PaneDndController({ groupsByDock, descriptorFor, onPlaceTab, children }: Props) {
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [source, setSource] = useState<DragSource | null>(null);
  const [dropTarget, setDropTarget] = useState<DropTarget | null>(null);

  const sensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 8 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 150, tolerance: 8 } }),
  );

  // Resolve the destination dock + group + insertion index from the hovered
  // droppable. Returns null when the pointer is not over a pane target.
  const resolveTarget = useCallback(
    (e: DragOverEvent | DragEndEvent): DropTarget | null => {
      const data = e.over?.data.current as PaneTabData | DockDropData | undefined;
      if (!data) return null;
      const group = "group" in data ? data.group : 0;
      const side = data.type === "pane-split" ? data.side : undefined;
      return resolvePlacement(
        {
          type: data.type,
          dock: data.dock,
          group,
          side,
          tabId: String(e.over!.id),
          after: pointerInsertsAfter(e.active.rect.current.translated, e.over!.rect),
        },
        String(e.active.id),
        groupsByDock,
      );
    },
    [groupsByDock],
  );

  const onDragStart = useCallback((e: DragStartEvent) => {
    const data = e.active.data.current as PaneTabData | undefined;
    setActiveTab(String(e.active.id));
    setSource(data ? { dock: data.dock, group: data.group } : null);
    setDropTarget(null);
  }, []);

  const onDragOver = useCallback((e: DragOverEvent) => setDropTarget(resolveTarget(e)), [resolveTarget]);

  const reset = useCallback(() => {
    setActiveTab(null);
    setSource(null);
    setDropTarget(null);
  }, []);

  const onDragEnd = useCallback(
    (e: DragEndEvent) => {
      const tabId = String(e.active.id);
      const target = resolveTarget(e);
      if (target && shouldApplyPlacement(groupsByDock, tabId, target, source)) {
        onPlaceTab(tabId, target);
      }
      reset();
    },
    [resolveTarget, source, groupsByDock, onPlaceTab, reset],
  );

  const state = useMemo<PaneDndState>(() => ({ activeTab, source, dropTarget }), [activeTab, source, dropTarget]);

  const overlayDesc = activeTab ? descriptorFor(activeTab) : null;

  return (
    <DndContext
      sensors={sensors}
      collisionDetection={panesCollision}
      measuring={{ droppable: { strategy: MeasuringStrategy.Always } }}
      onDragStart={onDragStart}
      onDragOver={onDragOver}
      onDragEnd={onDragEnd}
      onDragCancel={reset}
    >
      <PaneDndStateContext.Provider value={state}>
        <div className="relative flex flex-col min-h-0 flex-1">
          {children}
          {activeTab &&
            (["right", "bottom"] as DockLocation[])
              .filter((d) => groupsByDock[d].length === 0)
              .map((d) => <EmptyDockDropZone key={d} location={d} />)}
        </div>
        <DragOverlay dropAnimation={null}>
          {overlayDesc && (
            <div className="flex items-center gap-1 h-7 px-2 bg-surface-800 text-text-secondary border border-brand-600/60 rounded shadow-lg">
              <overlayDesc.icon className="size-3.5 shrink-0" aria-hidden />
              <span className="text-[11px] font-medium truncate max-w-[10rem]">{overlayDesc.title}</span>
            </div>
          )}
        </DragOverlay>
      </PaneDndStateContext.Provider>
    </DndContext>
  );
}

/** A landing zone for a dock that currently has no groups (so no Dock is in the
 *  DOM to drop onto). Pinned to the dock's screen edge, shown only while a pane
 *  tab is dragged. MeasuringStrategy.Always on the context measures it even
 *  though it mounts on drag start. */
function EmptyDockDropZone({ location }: { location: DockLocation }) {
  const { setNodeRef, isOver } = useDroppable({
    id: `empty-dock:${location}`,
    data: { type: "pane-empty-dock", dock: location } satisfies EmptyDockDropData,
  });
  const edge = location === "right" ? "top-0 right-0 h-full w-24 border-l" : "bottom-0 left-0 w-full h-24 border-t";
  return (
    <div
      ref={setNodeRef}
      data-testid={`empty-dock-dropzone-${location}`}
      className={`absolute z-30 flex items-center justify-center border-dashed transition-colors ${edge} ${
        isOver ? "border-brand-500 bg-brand-600/20" : "border-brand-600/40 bg-surface-900/40"
      }`}
    >
      <span className="text-xs font-medium text-text-dim uppercase tracking-wide">Dock here</span>
    </div>
  );
}
