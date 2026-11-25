import type { PaneDisplay } from "./Dock";
import { PaneIcon } from "./PaneIcon";
import { PanelRight } from "lucide-react";

import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";

interface Props {
  paneIds: string[];
  descriptorFor: (id: string) => PaneDisplay;
  isOpen: (id: string) => boolean;
  onToggle: (id: string) => void;
}

/** Desktop header toggles for panes that have no other control (the terminal); clicking one opens or closes it. */
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
            <PaneIcon icon={desc.icon} iconAssetUrl={desc.iconAssetUrl} className="size-4" />
          </button>
        );
      })}
    </div>
  );
}

const badgeClass =
  "absolute -top-1.5 -right-2.5 min-w-[1rem] rounded-full border border-surface-600 bg-surface-800 px-1 text-[10px] font-semibold leading-[0.875rem] tabular-nums text-text-secondary text-center";

/** Shows or hides the whole right dock, the Session's review panel. While it is closed, `badge` counts
 *  changed files. */
export function RightDockToggle({ open, onToggle, badge }: { open: boolean; onToggle: () => void; badge: number }) {
  const shown = open ? 0 : badge;
  return (
    <button
      onClick={onToggle}
      aria-pressed={open}
      data-testid="right-dock-toggle"
      {...tourAnchor(TOUR_ANCHORS.diffToggle)}
      className={`hidden md:flex w-8 h-8 items-center justify-center cursor-pointer rounded-md transition-colors hover:bg-surface-700/50 ${
        open ? "text-text-primary bg-surface-700/40" : "text-text-dim hover:text-text-secondary"
      }`}
      title={`${open ? "Hide" : "Show"} review panel`}
      aria-label={`Toggle review panel${shown > 0 ? `, ${shown} changed` : ""}`}
    >
      <span className="relative">
        <PanelRight className="size-4" aria-hidden />
        {shown > 0 && (
          <span aria-hidden="true" data-testid="right-dock-badge" className={badgeClass}>
            {shown > 99 ? "99+" : shown}
          </span>
        )}
      </span>
    </button>
  );
}
