import { createElement } from "react";

import { BUILTIN_PANES, type BuiltinPaneId } from "../lib/panes";
import type { PaneLayout } from "../lib/paneLayout";

interface Props {
  layout: PaneLayout;
  onToggle: (id: BuiltinPaneId) => void;
}

/** Desktop icon strip (JetBrains-style tool-window bar): one icon per
 *  dockable pane, clicking toggles that pane open/closed. Replaces the single
 *  "toggle diff panel" button, which mislabeled a column that now holds diff,
 *  terminal, and (later) plugin panes. */
export function ActivityBar({ layout, onToggle }: Props) {
  return (
    <div className="hidden md:flex items-center gap-0.5" data-testid="activity-bar">
      {BUILTIN_PANES.map((pane) => {
        const open = layout[pane.id].open;
        const name = pane.title.toLowerCase();
        return (
          <button
            key={pane.id}
            onClick={() => onToggle(pane.id)}
            aria-pressed={open}
            data-testid={`pane-toggle-${pane.id}`}
            className={`w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors hover:bg-surface-700/50 ${
              open ? "text-text-primary bg-surface-700/40" : "text-text-dim hover:text-text-secondary"
            }`}
            title={`${open ? "Hide" : "Show"} ${name} pane`}
            aria-label={`Toggle ${name} pane`}
          >
            {createElement(pane.icon, { className: "size-4", "aria-hidden": true })}
          </button>
        );
      })}
    </div>
  );
}
