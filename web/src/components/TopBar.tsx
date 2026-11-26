import { useMemo } from "react";
import { ChevronLeft, Folder } from "lucide-react";
import type { SessionResponse, Workspace } from "../lib/types";
import { PaletteTriggerPill } from "./PaletteTriggerPill";
import { OverflowMenu, type OverflowItem } from "./OverflowMenu";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import { PluginHeaderStartSegments, PluginStatusBarSegments } from "./plugin/PluginSlots";
import { PluginPageButtons } from "./plugin/PluginPageButtons";
import { PanelControl, TerminalToggle, type PanelControlProps } from "./PanelControl";
import { useWebSettings } from "../hooks/useWebSettings";
import type { AttentionBadgeColors } from "../lib/attentionBadgeColors";
import { StrokeIcon } from "./icons";
import { Tooltip } from "./Tooltip";
import { LiveSlowNotice } from "./LiveSlowNotice";

interface Props {
  activeWorkspace: Workspace | undefined;
  activeSession: SessionResponse | null;
  onToggleSidebar: () => void;
  onOpenPalette: () => void;
  /** The terminal toggle; omitted where the Session cannot open one. */
  terminal?: { open: boolean; onToggle: () => void };
  /** The open Session's actions for the overflow menu (a phone's only way to them); omitted on desktop. */
  sessionItems?: OverflowItem[];
  /** The review panel toggle and its pane menu; omitted where the Session has no review panel. */
  panels?: PanelControlProps;
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
  /** Set in a focus-mode window: an "All Sessions" button replaces the sidebar toggle and the dashboard logo. */
  onExitFocusMode?: () => void;
  /** When true (desktop, sidebar open, not in a full-width settings/projects view), the header's left zone widens
   *  to match the sidebar column and the divider runs vertically through the header instead of a bottom border, so
   *  the top-left of the header reads as part of the sidebar. */
  sidebarColumnVisible: boolean;
  /** Mirror of `sidebarColumnVisible` for the right side: when the right panel
   *  column is showing (desktop, active session, not collapsed), the header's
   *  right zone widens to match it and the divider runs up through the header. */
  rightColumnVisible: boolean;
}

/** The last segment of a project path, as a sidebar group names it. */
const projectName = (path: string) => path.replace(/\/+$/, "").split("/").pop() || path;

export function TopBar({
  activeWorkspace,
  activeSession,
  onToggleSidebar,
  onOpenPalette,
  terminal,
  panels,
  sessionItems,
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
  onExitFocusMode,
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
  // The open Session's own actions lead the menu, with the app's after them.
  const menuItems = useMemo<OverflowItem[]>(
    () =>
      sessionItems && sessionItems.length > 0
        ? [...sessionItems, ...overflowItems.map((item, i) => (i === 0 ? { ...item, separated: true } : item))]
        : overflowItems,
    [sessionItems, overflowItems],
  );
  const phoneSession = !!activeSession && !onExitFocusMode;

  // The left zone only borrows the sidebar's width while that column is visible, so the compact rail only crowds
  // the wordmark in that combination.
  const { settings: webSettings } = useWebSettings();
  const hideWordmark = sidebarColumnVisible && webSettings.sidebarCompact;
  const toggleLabel = onExitFocusMode ? "All Sessions" : "Toggle sidebar";

  return (
    <header
      {...tourAnchor(TOUR_ANCHORS.topbar)}
      // On a phone the bar dissolves into the page: its controls float, as in Claude's app.
      className="h-12 max-md:h-14 bg-surface-900 md:bg-transparent flex items-stretch shrink-0"
    >
      {/* Left zone: widens to the sidebar column when it's visible, so the header lines up with the sidebar on the
         canvas. */}
      <div
        className={`flex items-center gap-2 px-3 min-w-0 shrink-0 ${
          sidebarColumnVisible ? "md:w-[var(--aoe-sidebar-width)]" : ""
        }`}
      >
        {phoneSession && (
          // A Session opens over the list on a phone; back returns to it, carrying the list's attention counts.
          <button
            onClick={onGoDashboard}
            aria-label={
              waitingCount > 0 ? `Back to sessions, ${waitingCount} waiting for your input` : "Back to sessions"
            }
            data-testid="topbar-back"
            className="md:hidden relative w-10 h-10 flex items-center justify-center shrink-0 rounded-full float-control text-text-secondary cursor-pointer active:scale-95 transition-transform"
          >
            <ChevronLeft className="size-5" strokeWidth={1.75} aria-hidden />
            {waitingCount + unreadCount > 0 && (
              <span
                aria-hidden="true"
                className="absolute -top-0.5 -right-0.5 min-w-[1.1rem] rounded-full px-1 text-[10px] font-semibold leading-[1.1rem] tabular-nums text-center shadow"
                style={
                  waitingCount > 0
                    ? { backgroundColor: attentionBadgeColors.waitingBg, color: attentionBadgeColors.waitingFg }
                    : { backgroundColor: attentionBadgeColors.unreadBg, color: attentionBadgeColors.unreadFg }
                }
              >
                {waitingCount > 0 ? waitingCount : unreadCount}
              </span>
            )}
          </button>
        )}
        <button
          onClick={onExitFocusMode ?? onToggleSidebar}
          className={`h-8 flex items-center shrink-0 cursor-pointer rounded-md transition-colors text-text-dim hover:text-text-secondary hover:bg-surface-700/50 ${
            onExitFocusMode ? "pr-2.5" : "max-md:hidden"
          }`}
          title={toggleLabel}
          data-testid={onExitFocusMode ? "topbar-exit-focus" : undefined}
          aria-label={
            unreadCount > 0 || waitingCount > 0
              ? `${toggleLabel}, ${unreadCount} unread, ${waitingCount} waiting for your input`
              : toggleLabel
          }
        >
          {/* The badges are pinned to this icon box, not the button, so a labelled button keeps them on the icon. */}
          <span className="relative w-8 h-8 flex items-center justify-center shrink-0">
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
          </span>
          {onExitFocusMode && (
            <span className="text-xs leading-none whitespace-nowrap" aria-hidden="true">
              All Sessions
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

        {!onExitFocusMode && (
          <button
            onClick={onGoDashboard}
            className={`flex items-center gap-1.5 shrink-0 text-text-muted hover:text-text-secondary transition-colors cursor-pointer ${
              phoneSession ? "max-md:hidden" : ""
            }`}
            aria-label="Go to dashboard"
          >
            <img src="/icon-192.png" alt="" width="18" height="18" className="rounded-sm shrink-0" />
            {/* This zone matches the sidebar column, so a compact rail leaves no room for the wordmark next to the
               toggle and the logo: it would sit flush against the divider and read as clipped. */}
            <span className={`font-mono text-xs leading-none truncate ${hideWordmark ? "md:hidden" : ""}`}>aoe</span>
          </button>
        )}
        {/* The left zone is content-sized unless the sidebar column is showing, so the chips are capped and hidden
           below md instead of pushing the core controls off-screen. Also hidden with the wordmark: the compact rail
           leaves no room beside the logo. */}
        <div
          data-testid="topbar-header-start"
          className={`hidden ${hideWordmark ? "md:hidden" : "md:flex"} items-center gap-1.5 min-w-0 max-w-48 overflow-hidden`}
        >
          <PluginHeaderStartSegments />
        </div>
      </div>

      {/* Center zone: on desktop the open session's `project / title`, as Claude Code desktop heads its pane, with
          search in the sidebar; otherwise the palette trigger. Plugin page buttons follow. */}
      <div className="flex-1 flex items-center px-3 max-md:px-1 min-w-0">
        <div
          className={`group/center flex-1 flex justify-center items-center gap-1 px-2 min-w-0 ${activeSession ? "md:justify-start" : ""}`}
        >
          {activeSession && (
            <div
              data-testid="session-breadcrumb"
              className="hidden md:flex items-center gap-1.5 min-w-0 flex-1 text-[13px]"
              title={activeSession.project_path}
            >
              <Folder className="size-3.5 shrink-0 text-text-dim" strokeWidth={1.5} aria-hidden />
              <span className="text-text-secondary truncate shrink">{projectName(activeSession.project_path)}</span>
              <span className="text-text-dim" aria-hidden>
                /
              </span>
              <span className="text-text-primary font-medium truncate">{activeSession.title}</span>
            </div>
          )}
          {activeSession && (
            // A phone heads the Session with a floating pill, its title over its project, as Claude's app does;
            // it gives the slower-updates notice the room when that shows.
            <div
              data-testid="session-title-pill"
              title={activeSession.project_path}
              className="md:hidden min-w-0 max-w-full h-10 px-4 flex flex-col items-center justify-center rounded-full float-control leading-tight group-has-[[data-live-slow-notice]]/center:hidden"
            >
              <span className="max-w-full truncate text-[13px] font-semibold text-text-primary">
                {activeSession.title}
              </span>
              <span className="max-w-full truncate text-[11px] text-text-dim">
                {projectName(activeSession.project_path)}
              </span>
            </div>
          )}
          {/* Search lives in the sidebar on desktop with a Session open, and on the phone's session list. */}
          <div className={activeSession ? "hidden" : "hidden md:contents"}>
            <PaletteTriggerPill onClick={onOpenPalette} />
          </div>
          <PluginPageButtons />
          <LiveSlowNotice sessionId={activeSession?.id ?? null} />
        </div>
      </div>

      {/* Right zone: widens to the right-panel column when it's visible, so its controls sit over the panel. */}
      <div
        className={`flex items-center justify-end gap-1.5 px-3 shrink-0 ${
          rightColumnVisible ? "md:w-[var(--aoe-right-panel-width)]" : ""
        }`}
      >
        <PluginStatusBarSegments />
        {isDevBuild && (
          <span
            className={`font-mono text-[11px] px-1.5 py-0.5 rounded-full bg-status-waiting/15 text-status-waiting ring-1 ring-status-waiting/30 ${
              phoneSession ? "max-md:hidden" : ""
            }`}
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
            {/* Desktop: the terminal, then the review panel with its pane menu. */}
            {terminal && <TerminalToggle {...terminal} />}
            {panels && <PanelControl {...panels} />}
          </>
        )}

        <OverflowMenu items={menuItems} triggerDataTour={TOUR_ANCHORS.topbarMore} phoneFloating />
      </div>
    </header>
  );
}
