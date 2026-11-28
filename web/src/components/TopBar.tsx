import { useMemo } from "react";
import { setHeaderPageSlot } from "../lib/headerSlot";
import { ChevronLeft, Folder, LayoutGrid, Menu, type LucideIcon } from "lucide-react";
import { getStatusDotClass } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import type { RightPanelView } from "../lib/rightPanelView";
import type { SessionResponse, Workspace } from "../lib/types";
import { PaletteTriggerPill } from "./PaletteTriggerPill";
import { OPEN_HEADER_MENU_EVENT, OverflowMenu, type OverflowItem } from "./OverflowMenu";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";
import { PluginHeaderStartSegments, PluginStatusBarSegments } from "./plugin/PluginSlots";
import { PluginPageButtons } from "./plugin/PluginPageButtons";
import { PanelControl, TerminalToggle, type PanelControlProps } from "./PanelControl";
import { useWebSettings } from "../hooks/useWebSettings";
import { StrokeIcon } from "./icons";
import { Tooltip } from "./Tooltip";
import { LiveSlowNotice } from "./LiveSlowNotice";

export interface PhoneViewChoice {
  id: RightPanelView;
  label: string;
  icon: LucideIcon;
}

interface Props {
  activeWorkspace: Workspace | undefined;
  activeSession: SessionResponse | null;
  onToggleSidebar: () => void;
  onOpenPalette: () => void;
  /** The terminal toggle; omitted where the Session cannot open one. */
  terminal?: { open: boolean; onToggle: () => void };
  /** The open Session's actions for the overflow menu (a phone's only way to them); omitted on desktop. */
  sessionItems?: OverflowItem[];
  /** Opens the Session's actions, as a long press on its row does; a phone's title pill calls it when tapped. */
  onOpenSessionMenu?: (x: number, y: number) => void;
  /** A phone page with the session drawer: the header floats over the page with ☰ for the drawer. */
  phoneFloating?: boolean;
  /** A wide screen's full page such as Settings: Back and its title replace the sidebar toggle, and the page fills
   *  the header's middle through `useHeaderPageSlot`. */
  page?: { title: string; onBack: () => void };
  /** A phone's views of the open Session, chosen from the ⋯; away from the agent, the pill names the view and
   *  back returns to the agent. */
  phoneViews?: {
    choices: PhoneViewChoice[];
    current: RightPanelView;
    label: string | undefined;
    hasMore: boolean;
    moreDetail: string | undefined;
    onPick: (view: RightPanelView) => void;
    onMore: () => void;
  };
  /** The review panel toggle and its pane menu; omitted where the Session has no review panel. */
  panels?: PanelControlProps;
  /** Sessions that need you, counted as the sidebar headers count them; the sidebar toggle shows it while the
   *  sidebar is hidden. */
  needsYouCount: number;
  /** When true, render a "DEV" badge (in the `status-waiting` amber) in the right-hand status zone so debug builds
   *  (port 8081 / `aoe_dev_` tmux / `~/.agent-of-empires-dev/`) are visually distinct from release builds at a
   *  glance, including in PWA installs where the port is not visible in the window chrome. */
  isDevBuild: boolean;
  /** The app's own items, ending the ⋯ menu: Help, the symbol legend, the tutorial, Tips, About, Sign out. */
  appMenu: OverflowItem[];
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
  onOpenSessionMenu,
  phoneFloating = false,
  page,
  phoneViews,
  appMenu,
  needsYouCount,
  isDevBuild,
  onGoDashboard,
  onExitFocusMode,
  sidebarColumnVisible,
  rightColumnVisible,
}: Props) {
  // Its folder, left out where it repeats the title, as a worktree named after its Session does.
  const project = activeSession ? projectName(activeSession.project_path) : "";
  // A phone's menu: the Session's views, then its actions, then the app's items.
  const menuItems = useMemo<OverflowItem[]>(() => {
    const views: OverflowItem[] = (phoneViews?.choices ?? []).map(({ id, label, icon }) => ({
      label,
      icon,
      checked: phoneViews?.current === id,
      onClick: () => phoneViews?.onPick(id),
    }));
    if (phoneViews?.hasMore) {
      views.push({
        label: "More panes…",
        icon: LayoutGrid,
        detail: phoneViews.moreDetail,
        checked: phoneViews.moreDetail !== undefined,
        onClick: phoneViews.onMore,
      });
    }
    const groups = [views, sessionItems ?? [], appMenu].filter((g) => g.length > 0);
    return groups.flatMap((group, g) =>
      group.map((item, i) => (g > 0 && i === 0 ? { ...item, separated: true } : item)),
    );
  }, [phoneViews, sessionItems, appMenu]);
  const phoneFloat = phoneFloating && !onExitFocusMode;
  const idleDecayWindowMs = useIdleDecayWindowMs();

  // The left zone only borrows the sidebar's width while that column is visible, so the compact rail only crowds
  // the wordmark in that combination.
  const { settings: webSettings } = useWebSettings();
  const hideWordmark = sidebarColumnVisible && webSettings.sidebarCompact;
  const toggleLabel = onExitFocusMode ? "All Sessions" : "Toggle sidebar";
  // The headers show the count while the sidebar is open.
  const badge = sidebarColumnVisible ? 0 : needsYouCount;
  const needsYouText = `${needsYouCount} need${needsYouCount === 1 ? "s" : ""} you`;

  return (
    <header
      {...tourAnchor(TOUR_ANCHORS.topbar)}
      // On a phone the bar dissolves: over a Session its controls float on the content scrolling beneath, as in
      // Muse; elsewhere it stays a sticky, solid box so iOS 26 tints the status bar from it rather than blurring.
      className={`h-12 md:bg-transparent flex items-stretch shrink-0 ${
        // Over a Session it sits lower, clear of the faint band iOS still shades just under the status bar.
        phoneFloat
          ? "max-md:h-[68px] max-md:pt-3 max-md:bg-transparent"
          : "max-md:h-14 bg-surface-900 max-md:sticky max-md:top-0 max-md:z-30"
      }`}
    >
      {/* Left zone: widens to the sidebar column when it's visible, so the header lines up with the sidebar on the
         canvas. */}
      <div
        className={`flex items-center gap-2 px-3 min-w-0 shrink-0 ${
          sidebarColumnVisible ? "md:w-[var(--aoe-sidebar-width)]" : ""
        }`}
      >
        {phoneFloat && (
          // A phone opens the session list as a drawer from here, as Muse and Claude's app do, carrying its counts.
          <button
            onClick={onToggleSidebar}
            aria-label={needsYouCount > 0 ? `Show sessions, ${needsYouText}` : "Show sessions"}
            data-testid="topbar-menu"
            className="md:hidden relative w-10 h-10 flex items-center justify-center shrink-0 rounded-full float-control text-text-secondary cursor-pointer active:scale-95 transition-transform"
          >
            <Menu className="size-5" strokeWidth={1.75} aria-hidden />
            {needsYouCount > 0 && (
              <span
                aria-hidden="true"
                className="absolute -top-0.5 -right-0.5 min-w-[1.1rem] rounded-full bg-status-error px-1 text-[10px] font-semibold leading-[1.1rem] tabular-nums text-center text-text-on-status-error shadow"
              >
                {needsYouCount}
              </span>
            )}
          </button>
        )}
        <button
          hidden={!!page}
          onClick={onExitFocusMode ?? onToggleSidebar}
          className={`h-8 flex items-center shrink-0 cursor-pointer rounded-md transition-colors text-text-dim hover:text-text-secondary hover:bg-surface-700/50 ${
            onExitFocusMode ? "pr-2.5" : "max-md:hidden"
          }`}
          title={toggleLabel}
          data-testid={onExitFocusMode ? "topbar-exit-focus" : undefined}
          aria-label={badge > 0 ? `${toggleLabel}, ${needsYouText}` : toggleLabel}
        >
          {/* The badge is pinned to this icon box, not the button, so a labelled button keeps it on the icon. */}
          <span className="relative w-8 h-8 flex items-center justify-center shrink-0">
            <StrokeIcon size={16} strokeWidth="1.5">
              <rect x="3" y="3" width="18" height="18" rx="2" />
              <line x1="9" y1="3" x2="9" y2="21" />
            </StrokeIcon>
            {badge > 0 && (
              // On this wrapper, not inside Tooltip, whose in-flow trigger would inflate the button's box. The
              // button's label and the live region below carry the count.
              <span className="absolute -top-1 -right-1" aria-hidden="true">
                <Tooltip text={needsYouText}>
                  <span
                    className="block min-w-[1.1rem] rounded-full bg-status-error px-1 text-[10px] font-semibold leading-[1.1rem] tabular-nums text-center text-text-on-status-error"
                    data-testid="topbar-needs-you-badge"
                  >
                    {badge}
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
        {/* Persistent and carrying the count even at zero, so clearing the last one is a text change screen readers
           announce, not a removal they may skip. */}
        <span className="sr-only" aria-live="polite" aria-atomic="true" data-testid="topbar-attention-live-region">
          {needsYouText}
        </span>

        {!onExitFocusMode && (
          <button
            onClick={onGoDashboard}
            className={`flex items-center gap-1.5 shrink-0 text-text-muted hover:text-text-secondary transition-colors cursor-pointer ${
              phoneFloat ? "max-md:hidden" : ""
            }`}
            aria-label="Go to dashboard"
          >
            <img src="/icon-192.png" alt="" width="18" height="18" className="rounded-sm shrink-0" />
            {/* This zone matches the sidebar column, so a compact rail leaves no room for the wordmark next to the
               toggle and the logo: it would sit flush against the divider and read as clipped. */}
            <span className={`font-mono text-xs leading-none truncate ${hideWordmark ? "md:hidden" : ""}`}>aoe</span>
          </button>
        )}
        {page && (
          <>
            <button
              type="button"
              onClick={page.onBack}
              className="h-8 pl-1.5 pr-2.5 flex items-center gap-0.5 rounded-md text-[13px] text-text-secondary hover:text-text-primary hover:bg-surface-800/60 cursor-pointer transition-colors"
            >
              <ChevronLeft className="size-4" strokeWidth={1.75} aria-hidden />
              Back
            </button>
            <span className="text-[13px] font-medium text-text-primary truncate">{page.title}</span>
          </>
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
              {project !== activeSession.title && (
                <>
                  <span className="text-text-secondary truncate shrink">{project}</span>
                  <span className="text-text-dim" aria-hidden>
                    /
                  </span>
                </>
              )}
              <span className="text-text-primary font-medium truncate">{activeSession.title}</span>
            </div>
          )}
          {activeSession && (
            // A phone heads the Session with a floating pill, its title over its project, as Claude's app does;
            // it gives the slower-updates notice the room when that shows.
            <button
              type="button"
              data-testid="session-title-pill"
              title={activeSession.project_path}
              aria-label={`${activeSession.title} actions`}
              aria-haspopup="menu"
              disabled={!onOpenSessionMenu}
              onClick={(e) => {
                const r = e.currentTarget.getBoundingClientRect();
                onOpenSessionMenu?.(r.left + r.width / 2, r.bottom);
              }}
              className="md:hidden min-w-0 max-w-full h-10 px-4 flex flex-col items-center justify-center rounded-full float-control leading-tight cursor-pointer active:scale-[0.97] transition-transform disabled:cursor-default group-has-[[data-live-slow-notice]]/center:hidden focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand-500"
            >
              <span className="max-w-full truncate text-[13px] font-semibold text-text-primary">
                {activeSession.title}
              </span>
              {/* What the Session is doing, live, as clawbolt's header says what its assistant is doing; a view
                  other than the agent names itself instead. */}
              <span
                data-testid="session-title-status"
                className="max-w-full flex items-center gap-1.5 text-[11px] text-text-dim"
              >
                {!phoneViews?.label && (
                  <span
                    aria-hidden
                    className={`h-1.5 w-1.5 shrink-0 rounded-full ${getStatusDotClass(activeSession, idleDecayWindowMs)} ${
                      activeSession.status === "Running" ? "motion-safe:animate-pulse" : ""
                    }`}
                  />
                )}
                <span className="truncate">
                  {project === activeSession.title
                    ? (phoneViews?.label ?? activeSession.status)
                    : `${phoneViews?.label ?? activeSession.status} · ${project}`}
                </span>
              </span>
            </button>
          )}
          {/* Search lives in the sidebar on desktop with a Session open, and on the phone's session list. */}
          {page ? (
            <div ref={setHeaderPageSlot} className="flex-1 min-w-0 flex items-center justify-end gap-3" />
          ) : (
            <div className={activeSession ? "hidden" : "hidden md:contents"}>
              <PaletteTriggerPill onClick={onOpenPalette} />
            </div>
          )}
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
              phoneFloat ? "max-md:hidden" : ""
            }`}
            title="Debug build (cfg!(debug_assertions)); distinguishes the dev instance from a concurrent release build. See issue #1055."
            aria-label="Debug build"
          >
            DEV
          </span>
        )}

        {activeWorkspace && activeSession && (
          <>
            {/* Desktop: the terminal, then the review panel with its pane menu. */}
            {terminal && <TerminalToggle {...terminal} />}
            {panels && <PanelControl {...panels} />}
          </>
        )}

        <OverflowMenu
          items={menuItems}
          triggerDataTour={TOUR_ANCHORS.topbarMore}
          phoneFloating
          openOn={phoneFloat ? OPEN_HEADER_MENU_EVENT : undefined}
        />
      </div>
    </header>
  );
}
