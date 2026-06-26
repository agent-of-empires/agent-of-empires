import { createElement } from "react";

import type { PaneDisplay } from "./Dock";

interface Props {
  paneIds: string[];
  descriptorFor: (id: string) => PaneDisplay;
  isOpen: (id: string) => boolean;
  onToggle: (id: string) => void;
}

/** Desktop icon strip (JetBrains-style tool-window bar): one icon per dockable
 *  pane (built-in or plugin-contributed), clicking toggles that pane open or
 *  closed. Replaces the single "toggle diff panel" button, which mislabeled a
 *  column that now holds diff, terminal, and plugin panes. */
export function ActivityBar({ paneIds, descriptorFor, isOpen, onToggle }: Props) {
  if (paneIds.length === 0) return null;
  return (
    <div className="hidden md:flex items-center gap-0.5" data-testid="activity-bar">
      {paneIds.map((id) => {
        const desc = descriptorFor(id);
        const open = isOpen(id);
        const name = desc.title.toLowerCase();
        return (
          <button
            key={id}
            onClick={() => onToggle(id)}
            aria-pressed={open}
            data-testid={`pane-toggle-${id}`}
            className={`w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors hover:bg-surface-700/50 ${
              open ? "text-text-primary bg-surface-700/40" : "text-text-dim hover:text-text-secondary"
            }`}
            title={`${open ? "Hide" : "Show"} ${name} pane`}
            aria-label={`Toggle ${name} pane`}
          >
            {createElement(desc.icon, { className: "size-4", "aria-hidden": true })}
          </button>
        );
      })}
    </div>
  );
}
