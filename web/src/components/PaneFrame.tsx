import { createElement } from "react";
import { PanelBottom, PanelRight, X, type LucideIcon } from "lucide-react";

import type { DockLocation } from "../lib/panes";

interface Props {
  title: string;
  icon: LucideIcon;
  dock: DockLocation;
  onMove: (dock: DockLocation) => void;
  onClose: () => void;
  children: React.ReactNode;
}

const btn =
  "w-6 h-6 flex items-center justify-center shrink-0 rounded text-text-dim hover:text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors";

/** Tool-window chrome around a docked pane: a title bar with the pane's icon,
 *  a move-to-other-dock button, and a close button. The pane body fills the
 *  rest. */
export function PaneFrame({ title, icon, dock, onMove, onClose, children }: Props) {
  const target: DockLocation = dock === "right" ? "bottom" : "right";
  const MoveIcon = dock === "right" ? PanelBottom : PanelRight;
  const lower = title.toLowerCase();
  return (
    <section className="flex flex-col min-h-0 flex-1 overflow-hidden" data-pane-dock={dock}>
      <div className="flex items-center gap-1 px-2 h-7 shrink-0 bg-surface-900 border-b border-surface-700/20">
        {createElement(icon, { className: "size-3.5 text-text-dim shrink-0", "aria-hidden": true })}
        <span className="text-[11px] font-medium text-text-secondary truncate flex-1 min-w-0">{title}</span>
        <button
          onClick={() => onMove(target)}
          className={btn}
          title={`Move ${lower} to ${target} dock`}
          aria-label={`Move ${lower} to ${target} dock`}
        >
          <MoveIcon className="size-3.5" aria-hidden />
        </button>
        <button onClick={onClose} className={btn} title={`Close ${lower}`} aria-label={`Close ${lower}`}>
          <X className="size-3.5" aria-hidden />
        </button>
      </div>
      <div className="flex-1 flex flex-col min-h-0 overflow-hidden">{children}</div>
    </section>
  );
}
