import { Fragment, type ReactNode } from "react";
import { Maximize2, Minimize2, PanelBottom, PanelRight, Plus, X, type LucideIcon } from "lucide-react";
import { useDroppable } from "@dnd-kit/core";
import { SortableContext, horizontalListSortingStrategy, useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";

import { FileEditorPaneContext } from "../lib/fileEditGuard";
import { isTerminalTabId, type DockLocation } from "../lib/panes";
import { usePaneDnd, type PaneTabData, type SplitDropData } from "./paneDnd";
import { PaneIcon } from "./PaneIcon";

export interface PaneDisplay {
  title: string;
  icon: LucideIcon;
  /** A plugin's manifest `icon_asset`, resolved to a fetchable URL. See
   *  `PaneIcon` for the precedence over `icon`. */
  iconAssetUrl?: string;
  /** Short mono suffix after the title, e.g. a terminal's Container mode. */
  badge?: string;
}

/** Terminal controls each group of a dock shows. */
export interface DockControls {
  /** Omitted on read-only servers, where the terminal ensure route is
   *  rejected, so the new-terminal control is hidden rather than dead-ended. */
  onNewTerminal?: () => void;
  /** Names what the new-terminal control opens. */
  newTerminalLabel?: string;
  /** Controls for the active tab, shown before the dock controls. */
  tabActions?: (id: string) => ReactNode;
  /** The terminal switcher trigger, shown beside the new-terminal control. */
  terminalSwitcher?: ReactNode;
  /** Lets the dock take the whole main area and give it back. */
  maximize?: { maximized: boolean; onToggle: () => void };
}

interface Props extends DockControls {
  location: DockLocation;
  /** Persisted index of this group within its dock; carried on the drop
   *  payloads so a drop addresses the right group. */
  groupIndex: number;
  /** Visible tab ids in strip order (the parent pre-filters availability). */
  tabs: string[];
  /** Active tab id; falls back to the first tab if stale/missing. */
  active: string | null;
  /** Title + icon for a tab id (built-in from the registry, or a plugin pane). */
  descriptorFor: (id: string) => PaneDisplay;
  renderBody: (id: string) => ReactNode;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  onMove: (id: string, dock: DockLocation) => void;
}

const btn =
  "w-5 h-5 flex items-center justify-center shrink-0 rounded text-text-dim hover:text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors";

/** A vertical bar marking where a dragged tab would land. */
function InsertionMarker() {
  return <div data-testid="pane-insertion-marker" className="w-0.5 self-stretch bg-brand-500 shrink-0" />;
}

interface SortableTabProps {
  id: string;
  location: DockLocation;
  groupIndex: number;
  isActive: boolean;
  desc: PaneDisplay;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
}

/** One draggable tab. */
function SortableTab({ id, location, groupIndex, isActive, desc, onActivate, onClose }: SortableTabProps) {
  const { setNodeRef, listeners, transform, transition, isDragging } = useSortable({
    id,
    data: { type: "pane-tab", dock: location, group: groupIndex } satisfies PaneTabData,
  });
  const name = desc.title.toLowerCase();
  const style = { transform: CSS.Transform.toString(transform), transition };
  return (
    <div
      ref={setNodeRef}
      style={style}
      data-tab-active={isActive || undefined}
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
        <PaneIcon icon={desc.icon} iconAssetUrl={desc.iconAssetUrl} className="size-3.5 shrink-0" />
        <span className="text-[11px] font-medium truncate max-w-[10rem]">{desc.title}</span>
        {desc.badge && <span className="font-mono text-[10px] text-text-dim shrink-0">{desc.badge}</span>}
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

/** A half of a group body that, while a tab is dragged, lifts the drop into a fresh group before or after this
 *  one. */
function SplitDropZone({
  location,
  groupIndex,
  side,
}: {
  location: DockLocation;
  groupIndex: number;
  side: "before" | "after";
}) {
  const { setNodeRef, isOver } = useDroppable({
    id: `pane-split:${location}:${groupIndex}:${side}`,
    data: { type: "pane-split", dock: location, group: groupIndex, side } satisfies SplitDropData,
  });
  const half =
    location === "right"
      ? side === "before"
        ? "top-0 left-0 w-full h-1/2"
        : "bottom-0 left-0 w-full h-1/2"
      : side === "before"
        ? "top-0 left-0 h-full w-1/2"
        : "top-0 right-0 h-full w-1/2";
  return (
    <div
      ref={setNodeRef}
      data-testid={`pane-split-${location}-${groupIndex}-${side}`}
      className={`absolute z-20 ${half} ${isOver ? "bg-brand-600/25 ring-2 ring-inset ring-brand-500/70" : ""}`}
    />
  );
}

/** Renders one dock group: a tab strip plus the active tab's body. */
export function Dock({
  location,
  groupIndex,
  tabs,
  active,
  descriptorFor,
  renderBody,
  onActivate,
  onClose,
  onMove,
  onNewTerminal,
  newTerminalLabel = "New terminal",
  tabActions,
  terminalSwitcher,
  maximize,
}: Props) {
  const { activeTab, source, dropTarget } = usePaneDnd();
  const { setNodeRef: setGroupRef } = useDroppable({
    id: `pane-group:${location}:${groupIndex}`,
    data: { type: "pane-group", dock: location, group: groupIndex },
  });
  if (tabs.length === 0) return null;
  const activeId = active && tabs.includes(active) ? active : tabs[0]!;
  const target: DockLocation = location === "right" ? "bottom" : "right";
  const MoveIcon = location === "right" ? PanelBottom : PanelRight;
  const activeName = descriptorFor(activeId).title.toLowerCase();
  // Highlight + mark only a drop into a *different* group; a within-group reorder reads from the tabs sliding
  // apart, so a marker there would double up with the gap.
  const isSourceGroup = source?.dock === location && source.group === groupIndex;
  const isDropTarget =
    !!dropTarget &&
    !dropTarget.newGroup &&
    dropTarget.dock === location &&
    dropTarget.group === groupIndex &&
    !isSourceGroup;
  const markerIndex = isDropTarget ? (dropTarget!.index ?? tabs.length) : null;
  // Terminal controls belong to a group of terminals, and the dock-wide maximize to its first group.
  const holdsTerminals = tabs.some(isTerminalTabId);
  const showMaximize = maximize && groupIndex === 0;

  return (
    <section
      className={`flex flex-col min-h-0 min-w-0 flex-1 overflow-hidden ${
        isDropTarget ? "ring-2 ring-inset ring-brand-500/70" : ""
      }`}
      data-pane-dock={location}
      data-pane-group={groupIndex}
    >
      <div
        ref={setGroupRef}
        role="tablist"
        className="flex items-stretch h-7 shrink-0 bg-surface-900 border-b border-surface-700/20"
      >
        <div className="flex items-stretch min-w-0 overflow-x-auto">
          <SortableContext items={tabs} strategy={horizontalListSortingStrategy}>
            {tabs.map((id, i) => (
              <Fragment key={id}>
                {markerIndex === i && <InsertionMarker />}
                <SortableTab
                  id={id}
                  location={location}
                  groupIndex={groupIndex}
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
          {tabActions?.(activeId)}
          <button
            onClick={() => onMove(activeId, target)}
            className={btn}
            title={`Move ${activeName} to ${target} dock`}
            aria-label={`Move ${activeName} to ${target} dock`}
          >
            <MoveIcon className="size-3.5" aria-hidden />
          </button>
          {showMaximize && (
            <button
              type="button"
              onClick={maximize.onToggle}
              aria-pressed={maximize.maximized}
              className={btn}
              title={maximize.maximized ? "Restore panel" : "Maximize panel"}
              aria-label={maximize.maximized ? "Restore panel" : "Maximize panel"}
              data-testid="dock-maximize"
            >
              {maximize.maximized ? (
                <Minimize2 className="size-3.5" aria-hidden />
              ) : (
                <Maximize2 className="size-3.5" aria-hidden />
              )}
            </button>
          )}
          {holdsTerminals && terminalSwitcher}
          {holdsTerminals && onNewTerminal && (
            <button onClick={onNewTerminal} className={btn} title={newTerminalLabel} aria-label={newTerminalLabel}>
              <Plus className="size-3.5" aria-hidden />
            </button>
          )}
        </div>
      </div>
      <div className="relative flex-1 flex flex-col min-h-0 overflow-hidden">
        <FileEditorPaneContext.Provider value={activeId}>{renderBody(activeId)}</FileEditorPaneContext.Provider>
        {activeTab && (
          <>
            <SplitDropZone location={location} groupIndex={groupIndex} side="before" />
            <SplitDropZone location={location} groupIndex={groupIndex} side="after" />
          </>
        )}
      </div>
    </section>
  );
}
