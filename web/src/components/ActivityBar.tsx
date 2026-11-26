import type { PaneDisplay } from "./Dock";
import { PaneIcon } from "./PaneIcon";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";

interface Props {
  paneIds: string[];
  descriptorFor: (id: string) => PaneDisplay;
  isOpen: (id: string) => boolean;
  onToggle: (id: string) => void;
  /** A count shown on a closed pane's icon, such as changed files on Diff. */
  closedBadge?: (id: string) => number;
}

/** Desktop icon strip (JetBrains-style tool-window bar): one icon per dockable pane (built-in or plugin-
 *  contributed), clicking toggles that pane open or closed. */
export function ActivityBar({ paneIds, descriptorFor, isOpen, onToggle, closedBadge }: Props) {
  if (paneIds.length === 0) return null;
  return (
    <div className="hidden md:flex items-center gap-0.5" data-testid="activity-bar">
      {paneIds.map((id) => {
        const desc = descriptorFor(id);
        const open = isOpen(id);
        const name = desc.title.toLowerCase();
        const badge = open ? 0 : (closedBadge?.(id) ?? 0);
        return (
          <button
            key={id}
            onClick={() => onToggle(id)}
            aria-pressed={open}
            data-testid={`pane-toggle-${id}`}
            {...(id === "diff" ? tourAnchor(TOUR_ANCHORS.diffToggle) : {})}
            className={`w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors hover:bg-surface-700/50 ${
              open ? "text-text-primary bg-surface-700/40" : "text-text-dim hover:text-text-secondary"
            }`}
            title={`${open ? "Hide" : "Show"} ${name} pane`}
            aria-label={`Toggle ${name} pane${badge > 0 ? `, ${badge} changed` : ""}`}
          >
            <span className="relative">
              <PaneIcon icon={desc.icon} iconAssetUrl={desc.iconAssetUrl} className="size-4" />
              {badge > 0 && (
                <span
                  aria-hidden="true"
                  data-testid={`pane-badge-${id}`}
                  className="absolute -top-1.5 -right-2.5 min-w-[1rem] rounded-full border border-surface-600 bg-surface-800 px-1 text-[10px] font-semibold leading-[0.875rem] tabular-nums text-text-secondary text-center"
                >
                  {badge > 99 ? "99+" : badge}
                </span>
              )}
            </span>
          </button>
        );
      })}
    </div>
  );
}
