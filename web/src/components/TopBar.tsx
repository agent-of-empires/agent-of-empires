import { useMemo } from "react";
import type { SessionResponse, Workspace } from "../lib/types";
import { PaletteTriggerPill } from "./PaletteTriggerPill";
import { OverflowMenu, type OverflowItem } from "./OverflowMenu";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import { PluginStatusBarSegments } from "./plugin/PluginSlots";
import { ActivityBar } from "./ActivityBar";
import type { PaneDisplay } from "./Dock";
import { useWebSettings } from "../hooks/useWebSettings";
import type { AttentionBadgeColors } from "../lib/attentionBadgeColors";
import { StrokeIcon } from "./icons";
import { Tooltip } from "./Tooltip";

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
  /** Sessions with an unseen finished turn, across every workspace; shown as a badge on the sidebar toggle. */
  unreadCount: number;
  /** Sessions waiting for input, across every workspace; shown as a badge on the sidebar toggle. */
  waitingCount: number;
  /** Colors for the unread/waiting badges, derived from the app's single resolved-theme read (in `App`); computed
   *  here instead, it would call `useResolvedTheme()` a second time and race that one (see the function's doc). */
  attentionBadgeColors: AttentionBadgeColors;
  onLogout: () => void;
  loginRequired: boolean;
  isOffline: boolean;
  /** When true, render a "DEV" badge (in the `status-waiting` amber) in the right-hand status zone so debug builds
   *  (port 8081 / `aoe_dev_` tmux / `~/.agent-of-empires-dev/`) are visually distinct from release builds at a
   *  glance, including in PWA installs where the port is not visible in the window chrome. */
  isDevBuild: boolean;
  /** Opens the tip-of-the-day modal; wired into the overflow menu so tips are
   *  re-readable any time, like GIMP/DBeaver's Help menu entry. */
  onOpenTips: () => void;
  onGoDashboard: () => void;
  /** When true (desktop, sidebar open, not in a full-width settings/projects view), the header's left zone widens
   *  to match the sidebar column and the divider runs vertically through the header instead of a bottom border, so
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
  unreadCount,
  waitingCount,
  attentionBadgeColors,
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

  // The left zone only borrows the sidebar's width while that column is visible, so the compact rail only crowds
  // the wordmark in that combination.
  const { settings: webSettings } = useWebSettings();
  const hideWordmark = sidebarColumnVisible && webSettings.sidebarCompact;

  return (
    <header {...tourAnchor(TOUR_ANCHORS.topbar)} className="h-12 bg-surface-850 flex items-stretch shrink-0">
      {/* Left zone: widens to the sidebar column when it's visible so the divider runs vertically through the
         header instead of cutting across it; otherwise it keeps the shared bottom border like the rest. */}
      <div
        className={`flex items-center gap-2 px-3 min-w-0 shrink-0 border-b border-surface-700/60 ${
          sidebarColumnVisible ? "md:w-[var(--aoe-sidebar-width)] md:bg-surface-800 md:border-b-0 md:border-r" : ""
        }`}
      >
        <button
          onClick={onToggleSidebar}
          className="relative w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors text-text-dim hover:text-text-secondary hover:bg-surface-700/50"
          title="Toggle sidebar"
          aria-label={
            unreadCount > 0 || waitingCount > 0
              ? `Toggle sidebar, ${unreadCount} unread, ${waitingCount} waiting for your input`
              : "Toggle sidebar"
          }
        >
          <StrokeIcon size={16} strokeWidth="1.5">
            <rect x="3" y="3" width="18" height="18" rx="2" />
            <line x1="9" y1="3" x2="9" y2="21" />
          </StrokeIcon>
          {unreadCount > 0 && (
            // Positioned on this wrapper, not the pill inside Tooltip: Tooltip's own trigger span is an
            // in-flow inline-flex box, and leaving it in flow would inflate the button's own `relative` box.
            // aria-hidden: the button's own aria-label and the live region below carry the accessible counts.
            <span className="absolute -top-1 -right-1" aria-hidden="true">
              <Tooltip text={`${unreadCount} unread`}>
                {/* Solid fill in the theme's own resolved accent, with a foreground picked at render time (not a
                   fixed class) from that same resolved color: `waiting`/`unread` are user-configurable per theme
                   (resolved.rs), so no single hardcoded foreground clears WCAG AA against every possible accent. */}
                <span
                  className="block min-w-[1.1rem] rounded-full px-1 text-[10px] font-semibold leading-[1.1rem] tabular-nums text-center"
                  style={{ backgroundColor: attentionBadgeColors.unreadBg, color: attentionBadgeColors.unreadFg }}
                  data-testid="topbar-unread-badge"
                >
                  {unreadCount}
                </span>
              </Tooltip>
            </span>
          )}
          {waitingCount > 0 && (
            <span className="absolute -bottom-1 -right-1" aria-hidden="true">
              <Tooltip text={`${waitingCount} waiting for your input`}>
                <span
                  className="block min-w-[1.1rem] rounded-full px-1 text-[10px] font-semibold leading-[1.1rem] tabular-nums text-center"
                  style={{ backgroundColor: attentionBadgeColors.waitingBg, color: attentionBadgeColors.waitingFg }}
                  data-testid="topbar-waiting-badge"
                >
                  {waitingCount}
                </span>
              </Tooltip>
            </span>
          )}
        </button>
        {/* Persistent (not conditionally mounted) and always carrying the concrete counts, even at zero: emptying
           the text on the last-cleared transition relies on `aria-relevant`'s default (which excludes removals) to
           announce it, so screen readers could miss it. A "0 unread, 0 waiting" text change is a reliable text
           mutation instead. */}
        <span className="sr-only" aria-live="polite" aria-atomic="true" data-testid="topbar-attention-live-region">
          {`${unreadCount} unread, ${waitingCount} waiting for your input`}
        </span>

        <button
          onClick={onGoDashboard}
          className="flex items-center gap-1.5 min-w-0 text-text-muted hover:text-text-secondary transition-colors cursor-pointer"
          aria-label="Go to dashboard"
        >
          <img src="/icon-192.png" alt="" width="18" height="18" className="rounded-sm shrink-0" />
          {/* This zone matches the sidebar column, so a compact rail leaves no room for the wordmark next to the
             toggle and the logo: it would sit flush against the divider and read as clipped. */}
          <span className={`font-mono text-xs leading-none truncate ${hideWordmark ? "md:hidden" : ""}`}>aoe</span>
        </button>
      </div>

      {/* Center zone: palette trigger; carries the bottom border across the
          middle, between the two column-aligned zones. */}
      <div className="flex-1 flex items-center px-3 min-w-0 border-b border-surface-700/60">
        <div className="flex-1 flex justify-center px-2">
          <PaletteTriggerPill onClick={onOpenPalette} />
        </div>
      </div>

      {/* Right zone: widens to the right-panel column when it's visible so the divider runs vertically through
         the header instead of cutting across it; otherwise it keeps the shared bottom border like the rest. */}
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
            {/* Desktop: per-pane toggles. */}
            <ActivityBar paneIds={paneIds} descriptorFor={paneDescriptor} isOpen={isPaneOpen} onToggle={onTogglePane} />
            <button
              onClick={onToggleDiff}
              className="md:hidden w-8 h-8 flex items-center justify-center cursor-pointer rounded-md transition-colors text-text-secondary hover:text-text-primary hover:bg-surface-700/50"
              title="Toggle panels"
              aria-label="Toggle panels"
            >
              <StrokeIcon size={16} strokeWidth="1.5">
                <rect x="3" y="3" width="18" height="18" rx="2" />
                <line x1="15" y1="3" x2="15" y2="21" />
              </StrokeIcon>
            </button>
          </>
        )}

        <OverflowMenu items={overflowItems} triggerDataTour={TOUR_ANCHORS.topbarMore} />
      </div>
    </header>
  );
}
