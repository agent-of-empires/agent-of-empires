import { createElement, Fragment, type ReactNode } from "react";
import { PanelBottom, PanelRight, Plus, X, type LucideIcon } from "lucide-react";
import { useDroppable } from "@dnd-kit/core";
import { SortableContext, horizontalListSortingStrategy, useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";

import type { DockLocation } from "../lib/panes";
import { usePaneDnd, type PaneTabData } from "./paneDnd";

export interface PaneDisplay {
  title: string;
  icon: LucideIcon;
}

interface Props {
  location: DockLocation;
  /** Visible tab ids in strip order (the parent pre-filters availability). */
  tabs: string[];
  /** Active tab id; falls back to the first tab if stale/missing. */
  active: string | null;
  /** Title + icon for a tab id (built-in from the registry, or a plugin pane).
   *  A callback rather than an array prop so the icon component is resolved
   *  inside the dock, keeping the parent's render free of element arrays. */
  descriptorFor: (id: string) => PaneDisplay;
  renderBody: (id: string) => ReactNode;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  onMove: (id: string, dock: DockLocation) => void;
  /** Omitted on read-only servers, where the terminal ensure route is
   *  rejected, so the new-terminal control is hidden rather than dead-ended. */
  onNewTerminal?: () => void;
}

const btn =
  "w-5 h-5 flex items-center justify-center shrink-0 rounded text-text-dim hover:text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors";

/** A vertical bar marking where a dragged tab would land. Rendered inline
 *  between tabs (cross-dock drops only, see Dock), so it costs 2px of strip
 *  width and no reflow of the tab bodies. */
function InsertionMarker() {
  return <div data-testid="pane-insertion-marker" className="w-0.5 self-stretch bg-brand-500 shrink-0" />;
}

interface SortableTabProps {
  id: string;
  location: DockLocation;
  isActive: boolean;
  desc: PaneDisplay;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
}

/** One draggable tab. The drag listeners sit on the activation button (not the
 *  close button), and the MouseSensor's 8px distance means a stationary click
 *  still activates rather than starting a drag. Only `listeners` are spread,
 *  not dnd-kit's `attributes`, which would inject a conflicting role/aria onto
 *  the role="tab" button. */
function SortableTab({ id, location, isActive, desc, onActivate, onClose }: SortableTabProps) {
  const { setNodeRef, listeners, transform, transition, isDragging } = useSortable({
    id,
    data: { type: "pane-tab", dock: location } satisfies PaneTabData,
  });
  const name = desc.title.toLowerCase();
  const style = { transform: CSS.Transform.toString(transform), transition };
  return (
    <div
      ref={setNodeRef}
      style={style}
      className={`group flex items-center border-r border-surface-700/20 transition-colors ${
        isDragging ? "opacity-40" : ""
      } ${
        isActive
          ? "bg-surface-800 text-text-secondary"
          : "text-text-dim hover:text-text-secondary hover:bg-surface-800/40"
      }`}
    >
      {/* A real button so the tab is keyboard-reachable and Enter / Space
          activate it, not a click-only div. */}
      <button
        type="button"
        role="tab"
        aria-selected={isActive}
        data-testid={`pane-tab-${id}`}
        onClick={() => onActivate(id)}
        {...listeners}
        className="flex items-center gap-1 pl-2 pr-1 cursor-pointer min-w-0 touch-none"
      >
        {createElement(desc.icon, { className: "size-3.5 shrink-0", "aria-hidden": true })}
        <span className="text-[11px] font-medium truncate max-w-[10rem]">{desc.title}</span>
      </button>
      <button
        type="button"
        onClick={() => onClose(id)}
        className={`${btn} mr-1`}
        title={`Close ${name}`}
        aria-label={`Close ${name}`}
      >
        <X className="size-3" aria-hidden />
      </button>
    </div>
  );
}

/** Renders one dock location as a tabbed group: a tab strip plus the active
 *  tab's body. Each tab carries its pane's icon, title, and a close control;
 *  the strip also offers move-to-other-dock and a new-terminal button. Only the
 *  active body is mounted; the terminal/diff state it shows is server-side
 *  (tmux session, diff API), so re-mounting on a tab switch is cheap. The
 *  parent hides the dock entirely when it has no tabs.
 *
 *  Tabs reorder within the dock and move across docks via drag-and-drop (the
 *  shared DndContext lives in PaneDndController); the move button stays as the
 *  keyboard/click affordance. A within-dock reorder is shown by the tabs
 *  sliding apart; a cross-dock drop adds a destination ring plus an insertion
 *  marker, since the destination strip does not shift to preview the insert. */
export function Dock({
  location,
  tabs,
  active,
  descriptorFor,
  renderBody,
  onActivate,
  onClose,
  onMove,
  onNewTerminal,
}: Props) {
  const { sourceDock, dropTarget } = usePaneDnd();
  const { setNodeRef: setDockRef } = useDroppable({
    id: `dock-body:${location}`,
    data: { type: "pane-dock", dock: location },
  });
  if (tabs.length === 0) return null;
  const activeId = active && tabs.includes(active) ? active : tabs[0]!;
  const target: DockLocation = location === "right" ? "bottom" : "right";
  const MoveIcon = location === "right" ? PanelBottom : PanelRight;
  const activeName = descriptorFor(activeId).title.toLowerCase();
  // Highlight + mark only a cross-dock drop: a within-dock reorder reads from
  // the tabs sliding apart, so a marker there would double up with the gap.
  const isDropTarget = dropTarget?.dock === location && sourceDock !== location;
  const markerIndex = isDropTarget ? dropTarget!.index : null;

  return (
    <section
      ref={setDockRef}
      className={`flex flex-col min-h-0 flex-1 overflow-hidden ${
        isDropTarget ? "ring-2 ring-inset ring-brand-500/70" : ""
      }`}
      data-pane-dock={location}
    >
      <div role="tablist" className="flex items-stretch h-7 shrink-0 bg-surface-900 border-b border-surface-700/20">
        <div className="flex items-stretch min-w-0 overflow-x-auto">
          <SortableContext items={tabs} strategy={horizontalListSortingStrategy}>
            {tabs.map((id, i) => (
              <Fragment key={id}>
                {markerIndex === i && <InsertionMarker />}
                <SortableTab
                  id={id}
                  location={location}
                  isActive={id === activeId}
                  desc={descriptorFor(id)}
                  onActivate={onActivate}
                  onClose={onClose}
                />
              </Fragment>
            ))}
            {markerIndex === tabs.length && <InsertionMarker />}
          </SortableContext>
        </div>
        <div className="flex items-center ml-auto px-1 gap-0.5 shrink-0">
          <button
            onClick={() => onMove(activeId, target)}
            className={btn}
            title={`Move ${activeName} to ${target} dock`}
            aria-label={`Move ${activeName} to ${target} dock`}
          >
            <MoveIcon className="size-3.5" aria-hidden />
          </button>
          {onNewTerminal && (
            <button onClick={onNewTerminal} className={btn} title="New terminal" aria-label="New terminal">
              <Plus className="size-3.5" aria-hidden />
            </button>
          )}
        </div>
      </div>
      <div className="flex-1 flex flex-col min-h-0 overflow-hidden">{renderBody(activeId)}</div>
    </section>
  );
}
