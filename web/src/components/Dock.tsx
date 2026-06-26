import { createElement, type ReactNode } from "react";
import { PanelBottom, PanelRight, Plus, X, type LucideIcon } from "lucide-react";

import type { DockLocation } from "../lib/panes";

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

/** Renders one dock location as a tabbed group: a tab strip plus the active
 *  tab's body. Each tab carries its pane's icon, title, and a close control;
 *  the strip also offers move-to-other-dock and a new-terminal button. Only the
 *  active body is mounted; the terminal/diff state it shows is server-side
 *  (tmux session, diff API), so re-mounting on a tab switch is cheap. The
 *  parent hides the dock entirely when it has no tabs.
 *
 *  ponytail: a single tab group per dock. Multiple split groups within a dock
 *  arrive with the drag-and-drop follow-up; the layout storage is already
 *  group-shaped for it. */
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
  if (tabs.length === 0) return null;
  const activeId = active && tabs.includes(active) ? active : tabs[0]!;
  const target: DockLocation = location === "right" ? "bottom" : "right";
  const MoveIcon = location === "right" ? PanelBottom : PanelRight;
  const activeName = descriptorFor(activeId).title.toLowerCase();

  return (
    <section className="flex flex-col min-h-0 flex-1 overflow-hidden" data-pane-dock={location}>
      <div role="tablist" className="flex items-stretch h-7 shrink-0 bg-surface-900 border-b border-surface-700/20">
        <div className="flex items-stretch min-w-0 overflow-x-auto">
          {tabs.map((id) => {
            const desc = descriptorFor(id);
            const isActive = id === activeId;
            const name = desc.title.toLowerCase();
            return (
              <div
                key={id}
                className={`group flex items-center border-r border-surface-700/20 transition-colors ${
                  isActive
                    ? "bg-surface-800 text-text-secondary"
                    : "text-text-dim hover:text-text-secondary hover:bg-surface-800/40"
                }`}
              >
                {/* A real button so the tab is keyboard-reachable and Enter /
                    Space activate it, not a click-only div. */}
                <button
                  type="button"
                  role="tab"
                  aria-selected={isActive}
                  data-testid={`pane-tab-${id}`}
                  onClick={() => onActivate(id)}
                  className="flex items-center gap-1 pl-2 pr-1 cursor-pointer min-w-0"
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
          })}
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
