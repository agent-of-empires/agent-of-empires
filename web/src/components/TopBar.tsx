import { useMemo } from "react";
import type { SessionResponse, Workspace } from "../lib/types";
import { PaletteTriggerPill } from "./PaletteTriggerPill";
import { OverflowMenu, type OverflowItem } from "./OverflowMenu";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";

interface Props {
  activeWorkspace: Workspace | undefined;
  activeSession: SessionResponse | null;
  onToggleSidebar: () => void;
  onOpenPalette: () => void;
  onToggleDiff: () => void;
  diffCollapsed: boolean;
  onOpenHelp: () => void;
  onOpenAbout: () => void;
  onStartTutorial: () => void;
  onLogout: () => void;
  loginRequired: boolean;
  isOffline: boolean;
  /** When true, render a "DEV" badge (in the `status-waiting` amber)
   *  in the right-hand status zone so debug builds (port 8081 /
   *  `aoe_dev_` tmux / `~/.agent-of-empires-dev/`) are visually distinct
   *  from release builds at a glance, including in PWA installs where
   *  the port is not visible in the window chrome. Driven by
   *  `ServerAbout.build_flavor === "debug"`. See #1055. */
  isDevBuild: boolean;
  onGoDashboard: () => void;
}

export function TopBar({
  activeWorkspace,
  activeSession,
  onToggleSidebar,
  onOpenPalette,
  onToggleDiff,
  diffCollapsed,
  onOpenHelp,
  onOpenAbout,
  onStartTutorial,
  onLogout,
  loginRequired,
  isOffline,
  isDevBuild,
  onGoDashboard,
}: Props) {
  const overflowItems = useMemo<OverflowItem[]>(() => {
    const items: OverflowItem[] = [
      { label: "Help", onClick: onOpenHelp },
      { label: "Show tutorial", onClick: onStartTutorial },
      { label: "About", onClick: onOpenAbout },
    ];
    if (loginRequired) items.push({ label: "Sign out", onClick: onLogout });
    return items;
  }, [onOpenHelp, onStartTutorial, onOpenAbout, onLogout, loginRequired]);

  return (
    <header
      {...tourAnchor(TOUR_ANCHORS.topbar)}
      className="h-12 bg-surface-800 border-b border-surface-700/20 flex items-center px-3 shrink-0 gap-2"
    >
      {/* LEFT ZONE */}
      <div className="flex items-center gap-2 min-w-0 shrink-0">
        <button
          onClick={onToggleSidebar}
          className="w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors text-text-dim hover:text-text-secondary hover:bg-surface-700/50"
          title="Toggle sidebar"
          aria-label="Toggle sidebar"
        >
          <svg
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <rect x="3" y="3" width="18" height="18" rx="2" />
            <line x1="9" y1="3" x2="9" y2="21" />
          </svg>
        </button>

        <button
          onClick={onGoDashboard}
          className="flex items-center gap-1.5 text-text-muted hover:text-text-secondary transition-colors cursor-pointer"
          aria-label="Go to dashboard"
        >
          <img src="/icon-192.png" alt="" width="18" height="18" className="rounded-sm" />
          <span className="font-mono text-xs leading-none">aoe</span>
        </button>
      </div>

      {/* CENTER ZONE — palette trigger */}
      <div className="flex-1 flex justify-center px-2">
        <PaletteTriggerPill onClick={onOpenPalette} />
      </div>

      {/* RIGHT ZONE */}
      <div className="flex items-center gap-1.5 shrink-0">
        {isDevBuild && (
          <span
            className="font-mono text-[11px] px-1.5 py-0.5 rounded-full bg-status-waiting/15 text-status-waiting ring-1 ring-status-waiting/30"
            title="Debug build (cfg!(debug_assertions)); distinguishes the dev instance from a concurrent release build. See issue #1055."
            aria-label="Debug build"
          >
            DEV
          </span>
        )}
        {isOffline && (
          <span
            className="font-mono text-[11px] px-1.5 py-0.5 rounded-full bg-status-error/10 text-status-error flex items-center gap-1.5"
            title="Disconnected from backend"
          >
            <span className="w-1.5 h-1.5 rounded-full bg-status-error animate-pulse" />
            offline
          </span>
        )}

        {activeWorkspace && activeSession && (
          <button
            onClick={onToggleDiff}
            className={`w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors hover:bg-surface-700/50 ${
              diffCollapsed ? "text-text-dim hover:text-text-secondary" : "text-text-secondary hover:text-text-primary"
            }`}
            title="Toggle diff panel"
            aria-label="Toggle diff panel"
          >
            <svg
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <rect x="3" y="3" width="18" height="18" rx="2" />
              <line x1="15" y1="3" x2="15" y2="21" />
            </svg>
          </button>
        )}

        <OverflowMenu items={overflowItems} triggerDataTour={TOUR_ANCHORS.topbarMore} />
      </div>
    </header>
  );
}
