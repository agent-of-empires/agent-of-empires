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
import { arrayMove } from "@dnd-kit/sortable";

import type { DockLocation } from "../lib/panes";
import type { PaneDisplay } from "./Dock";
import {
  PaneDndStateContext,
  type DockDropData,
  type DropTarget,
  type PaneDndState,
  type PaneTabData,
} from "./paneDnd";

// Prefer the droppable the pointer is actually inside, and only among pane
// droppables, so a drag in the right column never magnetically snaps to a tab
// in the distant bottom strip the way a global closestCenter would. A tab hit
// wins over a dock-body hit so dropping onto a sibling tab reorders rather than
// appends. Mirrors the filtered-collision approach in WorkspaceSidebar (#1644).
const panesCollision: CollisionDetection = (args) => {
  const paneContainers = args.droppableContainers.filter((c) => {
    const t = c.data.current?.type;
    return t === "pane-tab" || t === "pane-dock" || t === "pane-empty-dock";
  });
  const hits = pointerWithin({ ...args, droppableContainers: paneContainers });
  const tabHits = hits.filter((h) => h.data?.droppableContainer?.data.current?.type === "pane-tab");
  return tabHits.length > 0 ? tabHits : hits;
};

function centerX(rect: { left: number; width: number } | null | undefined): number | null {
  return rect ? rect.left + rect.width / 2 : null;
}

interface Props {
  /** The rendered tab order per dock (already availability-filtered), so drop
   *  indices line up with what the docks actually show. */
  tabsByDock: Record<DockLocation, string[]>;
  descriptorFor: (id: string) => PaneDisplay;
  /** Reorder within a dock or move across docks, landing at `toIndex`. */
  onPlaceTab: (tabId: string, toDock: DockLocation, toIndex: number) => void;
  children: ReactNode;
}

/** Owns the single DndContext spanning both docks: sensors, the pane-aware
 *  collision policy, the live drop target, a DragOverlay replica that follows
 *  the cursor across the distant docks, and the empty-dock landing zones. Docks
 *  stay presentational and read the drop state through usePaneDnd. */
export function PaneDndController({ tabsByDock, descriptorFor, onPlaceTab, children }: Props) {
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [sourceDock, setSourceDock] = useState<DockLocation | null>(null);
  const [dropTarget, setDropTarget] = useState<DropTarget | null>(null);

  const sensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 8 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 150, tolerance: 8 } }),
  );

  // Resolve the destination dock + post-removal insertion index from the
  // hovered droppable. Returns null when the pointer is not over a pane target.
  const resolveTarget = useCallback(
    (e: DragOverEvent | DragEndEvent): DropTarget | null => {
      const overData = e.over?.data.current as PaneTabData | DockDropData | undefined;
      if (!overData) return null;
      const draggedId = String(e.active.id);
      const dock = overData.dock;
      // The destination's tabs without the dragged one: this is the index
      // space placeTab inserts into, and (cross-dock) it has no gap to map.
      const base = tabsByDock[dock].filter((id) => id !== draggedId);
      if (overData.type !== "pane-tab") return { dock, index: base.length };
      const overIndex = base.indexOf(String(e.over!.id));
      if (overIndex < 0) return { dock, index: base.length };
      const activeC = centerX(e.active.rect.current.translated);
      const overC = centerX(e.over!.rect);
      const after = activeC !== null && overC !== null && activeC > overC;
      return { dock, index: overIndex + (after ? 1 : 0) };
    },
    [tabsByDock],
  );

  const onDragStart = useCallback((e: DragStartEvent) => {
    const data = e.active.data.current as PaneTabData | undefined;
    setActiveTab(String(e.active.id));
    setSourceDock(data?.dock ?? null);
    setDropTarget(null);
  }, []);

  const onDragOver = useCallback((e: DragOverEvent) => setDropTarget(resolveTarget(e)), [resolveTarget]);

  const reset = useCallback(() => {
    setActiveTab(null);
    setSourceDock(null);
    setDropTarget(null);
  }, []);

  const onDragEnd = useCallback(
    (e: DragEndEvent) => {
      const tabId = String(e.active.id);
      const target = resolveTarget(e);
      if (target) {
        if (target.dock === sourceDock) {
          // Within-dock reorder: arrayMove gives the final order, and the
          // dragged tab's index in it is exactly placeTab's post-removal index.
          const arr = tabsByDock[target.dock];
          const from = arr.indexOf(tabId);
          const over = e.over ? arr.indexOf(String(e.over.id)) : -1;
          if (from >= 0 && over >= 0 && from !== over) {
            const finalIndex = arrayMove(arr, from, over).indexOf(tabId);
            onPlaceTab(tabId, target.dock, finalIndex);
          }
        } else {
          onPlaceTab(tabId, target.dock, target.index);
        }
      }
      reset();
    },
    [resolveTarget, sourceDock, tabsByDock, onPlaceTab, reset],
  );

  const state = useMemo<PaneDndState>(
    () => ({ activeTab, sourceDock, dropTarget }),
    [activeTab, sourceDock, dropTarget],
  );

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
              .filter((d) => tabsByDock[d].length === 0)
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

/** A landing zone for a dock that currently has no tabs (so its Dock is not in
 *  the DOM to drop onto). Pinned to the dock's screen edge, shown only while a
 *  pane tab is dragged. MeasuringStrategy.Always on the context measures it
 *  even though it mounts on drag start. */
function EmptyDockDropZone({ location }: { location: DockLocation }) {
  const { setNodeRef, isOver } = useDroppable({
    id: `empty-dock:${location}`,
    data: { type: "pane-empty-dock", dock: location } satisfies DockDropData,
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
