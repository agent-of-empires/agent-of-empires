import { useMemo } from "react";
import type { SessionResponse, Workspace } from "../lib/types";
import { PaletteTriggerPill } from "./PaletteTriggerPill";
import { OverflowMenu, type OverflowItem } from "./OverflowMenu";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import { PluginStatusBarSegments } from "./plugin/PluginSlots";
import { ActivityBar } from "./ActivityBar";
import type { PaneDisplay } from "./Dock";

interface Props {
  activeWorkspace: Workspace | undefined;
  activeSession: SessionResponse | null;
  onToggleSidebar: () => void;
  onOpenPalette: () => void;
  /** Mobile (below md): opens the view picker. The desktop activity bar uses
   *  `onTogglePane` instead. */
  onToggleDiff: () => void;
  /** All dockable pane ids (built-in + plugin) for the active session. */
  paneIds: string[];
  paneDescriptor: (id: string) => PaneDisplay;
  isPaneOpen: (id: string) => boolean;
  onTogglePane: (id: string) => void;
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
  /** Opens the tip-of-the-day modal; wired into the overflow menu so tips are
   *  re-readable any time, like GIMP/DBeaver's Help menu entry. */
  onOpenTips: () => void;
  onGoDashboard: () => void;
  /** When true (desktop, sidebar open, not in a full-width settings/projects
   *  view), the header's left zone widens to match the sidebar column and the
   *  divider runs vertically through the header instead of a bottom border, so
   *  the top-left of the header reads as part of the sidebar. */
  sidebarColumnVisible: boolean;
  /** Mirror of `sidebarColumnVisible` for the right side: when the right panel
   *  column is showing (desktop, active session, not collapsed), the header's
   *  right zone widens to match it and the divider runs up through the header. */
  rightColumnVisible: boolean;
}

export function TopBar({
  activeWorkspace,
  activeSession,
  onToggleSidebar,
  onOpenPalette,
  onToggleDiff,
  paneIds,
  paneDescriptor,
  isPaneOpen,
  onTogglePane,
  onOpenHelp,
  onOpenAbout,
  onStartTutorial,
  onLogout,
  loginRequired,
  isOffline,
  isDevBuild,
  onOpenTips,
  onGoDashboard,
  sidebarColumnVisible,
  rightColumnVisible,
}: Props) {
  const overflowItems = useMemo<OverflowItem[]>(() => {
    const items: OverflowItem[] = [
      { label: "Help", onClick: onOpenHelp },
      { label: "Show tutorial", onClick: onStartTutorial },
      { label: "Tips", onClick: onOpenTips },
      { label: "About", onClick: onOpenAbout },
    ];
    if (loginRequired) items.push({ label: "Sign out", onClick: onLogout });
    return items;
  }, [onOpenHelp, onStartTutorial, onOpenTips, onOpenAbout, onLogout, loginRequired]);

  return (
    <header {...tourAnchor(TOUR_ANCHORS.topbar)} className="h-12 bg-surface-850 flex items-stretch shrink-0">
      {/* LEFT ZONE — widens to the sidebar column when it's visible so the
          divider runs vertically through the header instead of cutting across
          it; otherwise it keeps the shared bottom border like the rest. */}
      <div
        className={`flex items-center gap-2 px-3 min-w-0 shrink-0 border-b border-surface-700/60 ${
          sidebarColumnVisible ? "md:w-[var(--aoe-sidebar-width)] md:bg-surface-800 md:border-b-0 md:border-r" : ""
        }`}
      >
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

      {/* CENTER ZONE — palette trigger; carries the bottom border across the
          middle, between the two column-aligned zones. */}
      <div className="flex-1 flex items-center px-3 min-w-0 border-b border-surface-700/60">
        <div className="flex-1 flex justify-center px-2">
          <PaletteTriggerPill onClick={onOpenPalette} />
        </div>
      </div>

      {/* RIGHT ZONE — widens to the right-panel column when it's visible so the
          divider runs vertically through the header instead of cutting across
          it; otherwise it keeps the shared bottom border like the rest. */}
      <div
        className={`flex items-center justify-end gap-1.5 px-3 shrink-0 border-b border-surface-700/60 ${
          rightColumnVisible ? "md:w-[var(--aoe-right-panel-width)] md:border-b-0 md:border-l" : ""
        }`}
      >
        <PluginStatusBarSegments />
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
          <>
            {/* Desktop: per-pane toggles. Mobile: one button that opens the
                full-viewport view picker (#1452); there is no side dock to
                toggle pane-by-pane below md. */}
            <ActivityBar paneIds={paneIds} descriptorFor={paneDescriptor} isOpen={isPaneOpen} onToggle={onTogglePane} />
            <button
              onClick={onToggleDiff}
              className="md:hidden w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors text-text-secondary hover:text-text-primary hover:bg-surface-700/50"
              title="Toggle panels"
              aria-label="Toggle panels"
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
          </>
        )}

        <OverflowMenu items={overflowItems} triggerDataTour={TOUR_ANCHORS.topbarMore} />
      </div>
    </header>
  );
}
