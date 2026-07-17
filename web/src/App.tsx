import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Puzzle } from "lucide-react";
import { useMatch, useNavigate, useSearchParams } from "react-router-dom";
import { IDLE_DECAY_WINDOW_MS, isSessionActive } from "./lib/session";
import { diffSelectionStale } from "./lib/diffSelection";
import { useSessions } from "./hooks/useSessions";
import { clearAcpCache } from "./hooks/useAcpSession";
import { clearDraft, sweepOrphanDrafts } from "./lib/acpDrafts";
import { AcpPrefsProvider } from "./lib/acpPrefs";
import { safeGetItem, safeRemoveItem } from "./lib/safeStorage";
import { isAutomatedSession } from "./lib/onboarding";
import { useWorkspaces } from "./hooks/useWorkspaces";
import { useLastSessionRestore } from "./hooks/useLastSessionRestore";
import { useRepoGroups } from "./hooks/useRepoGroups";
import { useSessionGroups } from "./hooks/useSessionGroups";
import { useNestedSidebarGroups } from "./hooks/useNestedSidebarGroups";
import { PluginUiProvider, usePluginUiEntries } from "./lib/pluginUiContext";
import { buildSortValueMap, pluginSortSpecs } from "./lib/pluginUi";
import type { PluginSortContext, SidebarSortMode } from "./lib/sidebarSort";
import { workspaceIsTrashed } from "./lib/sidebarSort";
import { useSidebarSortMode } from "./hooks/useSidebarSortMode";
import { useSidebarAxis } from "./hooks/useSidebarAxis";
import { repoGroupToSidebarGroup, type SidebarGroup } from "./lib/sidebarGroups";
import { useProjects } from "./hooks/useProjects";
import { useKeyboardShortcuts } from "./hooks/useKeyboardShortcuts";
import { useResolvedTheme } from "./hooks/useResolvedTheme";
import { useWebSettings } from "./hooks/useWebSettings";
import { useDiffFiles } from "./hooks/useDiffFiles";
import { useDiffComments } from "./hooks/useDiffComments";
import { clearStoredComments, sweepOrphanComments } from "./components/diff/comments/storage";
import { SendCommentsDialog } from "./components/diff/comments/SendCommentsDialog";
import { useCommandActions, buildConversationActions, type SessionStateAction } from "./hooks/useCommandActions";
import { usePluginCommands } from "./hooks/usePluginCommands";
import { useSettingsCommands } from "./hooks/useSettingsCommands";
import { useEdgeSwipe } from "./hooks/useEdgeSwipe";
import { useIsCoarsePointer } from "./hooks/useIsCoarsePointer";
import { useIsWideViewport } from "./hooks/useIsWideViewport";
import type { RightPanelView } from "./lib/rightPanelView";
import { usePaneLayout, dockTabs, dockGroups, dockOf, isActiveTab, isDockCollapsed } from "./lib/paneLayout";
import { isPluginPaneId, resolvePaneIcon, usePluginPanes, type PluginPane } from "./lib/pluginPanes";
import { PluginPaneBody } from "./components/plugin/PluginSlots";
import { TOUR_ANCHORS, tourAnchor } from "./lib/tourSteps";
import {
  deleteWorkspaceSessions,
  restoreSessions,
  trashedWorkspaceRestoreIds,
  trashSessions,
} from "./lib/trashActions";
import {
  loginStatus,
  logout,
  stopSession,
  startSession,
  fetchAbout,
  fetchSettings,
  fetchTelemetryStatus,
  setTelemetryConsent,
  reportTelemetrySeen,
  isDebugBuild,
  markWebTourSeen,
  updateWorkspaceOrdering,
  createProject,
  setProjectPinned,
  deleteProject,
  setSessionUnread,
  killTerminal,
  setSessionPin,
  setSessionArchive,
  setSessionSnooze,
  trashSession,
  restoreSession,
  fetchPlugins,
} from "./lib/api";
import type { DeleteSessionOptions, ServerAbout } from "./lib/api";
import { normalizeProjectPathKey } from "./lib/registeredProjects";
import { IdleDecayWindowContext, parseIdleDecayWindowMs, useIdleDecayWindowMs } from "./lib/idleDecay";
import { parseUnreadIndicatorEnabled, UnreadIndicatorContext, useUnreadIndicatorEnabled } from "./lib/unreadIndicator";
import { parseSessionRowTagMode, SessionRowTagContext, type SessionRowTagMode } from "./lib/sessionRowTag";
import { toastBus, reportError } from "./lib/toastBus";
import { resolveToRepoRelative, type FileRef } from "./lib/fileRef";
import { OPEN_SESSION_EVENT } from "./lib/sessionRoute";
import { dispatchFocusTerminal, requestSessionInputFocus, setPendingTerminalFocus } from "./lib/terminalFocus";
import { hydrateWebUiStateFromServer, initWebUiSync } from "./lib/webUiSync";
import { WorkspaceSidebar, SnoozeModal } from "./components/WorkspaceSidebar";
import { DeleteSessionDialog } from "./components/DeleteSessionDialog";
import { StopSessionDialog } from "./components/StopSessionDialog";
import { TopBar } from "./components/TopBar";
import { ContentSplit } from "./components/ContentSplit";
import { TerminalSessionStack } from "./components/TerminalSessionStack";
// Lazy-load the acp surface so non-acp users never download
// the @assistant-ui/react, shiki, and in-house StringDiff/DiffLine
// dependency tree. Cuts ~hundreds of KB off the cold-start bundle
// for the (currently default) tmux-only flow. The Suspense fallback
// below covers the brief load while the chunk arrives.
const StructuredView = lazy(() =>
  import("./components/acp/StructuredView").then((m) => ({
    default: m.StructuredView,
  })),
);
import { type PaneDisplay } from "./components/Dock";
import { DockGroups, type DockGroupView } from "./components/DockGroups";
import { BottomDock } from "./components/BottomDock";
import { PaneDndController } from "./components/PaneDndController";
import { visibleToFullIndex, type DropTarget } from "./components/paneDnd";
import { BackgroundAgentsPanel } from "./components/acp/BackgroundAgentsPanel";
import { DiffPane } from "./components/DiffPane";
import { PairedShellPane } from "./components/PairedTerminal";
import { BUILTIN_PANES, isTerminalTabId, terminalIndexOf, terminalTabId, type DockLocation } from "./lib/panes";
import { MobileRightPanelPicker } from "./components/MobileRightPanelPicker";
import { MobileMainPane } from "./components/MobileMainPane";
import { DiffFileViewer } from "./components/diff/DiffFileViewer";
import { SettingsView } from "./components/SettingsView";
import { ProjectFormModal } from "./components/ProjectFormModal";
import { HelpOverlay } from "./components/HelpOverlay";
import { useTour } from "./hooks/useTour";
import { useWelcomePhase } from "./hooks/useWelcomePhase";
import { ThemeIntro } from "./components/onboarding/ThemeIntro";
import type { TourScope } from "./lib/tourSteps";
import { SessionWizard } from "./components/session-wizard/SessionWizard";
import type { WizardPrefill } from "./components/session-wizard/SessionWizard";
import type { ProjectInfo, RepoGroup, SessionResponse } from "./lib/types";
import { Dashboard } from "./components/Dashboard";
import { LoginPage } from "./components/LoginPage";
import { TokenEntryPage } from "./components/TokenEntryPage";
import { LOGIN_REQUIRED_EVENT, TOKEN_EXPIRED_EVENT, resetTokenExpired } from "./lib/fetchInterceptor";
import { AboutModal } from "./components/AboutModal";
import { TelemetryConsentModal } from "./components/TelemetryConsentModal";
import { TipsModal } from "./components/TipsModal";
import { useTips, shouldAutoPopTips } from "./hooks/useTips";
import { CommandPalette } from "./components/command-palette/CommandPalette";
import { useConversationSearch } from "./hooks/useConversationSearch";
import { DisconnectBanner } from "./components/DisconnectBanner";
import { ElevationPrompt } from "./components/ElevationPrompt";
import { UpdateBanner } from "./components/UpdateBanner";
import { DashboardUpdateBanner } from "./components/DashboardUpdateBanner";

// Pre-#1832 per-browser tour-seen flag. Read once on load to migrate users who
// already dismissed the tour to the backend; no longer written.
const LEGACY_TOUR_SEEN_KEY = "aoe-tour-seen";

export default function App() {
  // Apply the user-selected theme as CSS custom properties on the root
  // element. Runs once on mount + on settings-driven theme changes.
  // The pre-React /theme-bootstrap.js (referenced from index.html)
  // paints the cached theme before hydration; this hook keeps it in
  // sync with the server's view.
  useResolvedTheme();
  const [loginRequired, setLoginRequired] = useState<boolean | null>(null);
  const [loginAuthenticated, setLoginAuthenticated] = useState(true);
  const [tokenExpired, setTokenExpired] = useState(false);
  const [idleDecayWindowMs, setIdleDecayWindowMs] = useState(IDLE_DECAY_WINDOW_MS);
  const [unreadIndicatorEnabled, setUnreadIndicatorEnabled] = useState(true);
  const [sessionRowTagMode, setSessionRowTagMode] = useState<SessionRowTagMode>("branch");

  const applyAppSettings = useCallback((settings: Record<string, unknown> | null | undefined) => {
    setIdleDecayWindowMs(parseIdleDecayWindowMs(settings));
    setUnreadIndicatorEnabled(parseUnreadIndicatorEnabled(settings));
    setSessionRowTagMode(parseSessionRowTagMode(settings));
  }, []);

  const refreshAppSettings = useCallback(async () => {
    applyAppSettings(await fetchSettings());
  }, [applyAppSettings]);

  useEffect(() => {
    const onTokenExpired = () => setTokenExpired(true);
    window.addEventListener(TOKEN_EXPIRED_EVENT, onTokenExpired);
    return () => window.removeEventListener(TOKEN_EXPIRED_EVENT, onTokenExpired);
  }, []);

  // Clearing tokenExpired here matters: the render order below shows
  // TokenEntryPage above LoginPage, so without the reset a token that's
  // actually fine would keep getting shown the wrong screen.
  useEffect(() => {
    const onLoginRequired = () => {
      setTokenExpired(false);
      setLoginRequired(true);
      setLoginAuthenticated(false);
    };
    window.addEventListener(LOGIN_REQUIRED_EVENT, onLoginRequired);
    return () => window.removeEventListener(LOGIN_REQUIRED_EVENT, onLoginRequired);
  }, []);

  useEffect(() => {
    loginStatus().then(({ required, authenticated }) => {
      setLoginRequired(required);
      setLoginAuthenticated(authenticated);
    });
  }, []);

  useEffect(() => {
    fetchSettings().then(applyAppSettings);
  }, [applyAppSettings]);

  const handleTokenSuccess = () => {
    setTokenExpired(false);
    // Re-check login status now that token auth works
    loginStatus().then(({ required, authenticated }) => {
      setLoginRequired(required);
      setLoginAuthenticated(authenticated);
    });
  };

  const handleLoginSuccess = () => {
    setLoginAuthenticated(true);
    // Reset dedup flags so a future session expiry can re-fire the event.
    resetTokenExpired();
  };

  const handleLogout = async () => {
    await logout();
    setLoginAuthenticated(false);
  };

  // Only hydrate once the user is past every auth gate, so the request runs as
  // the authenticated user (and never against the login/token screens).
  // Token auth is the first factor; show token entry before anything else
  if (tokenExpired) {
    return <TokenEntryPage onSuccess={handleTokenSuccess} />;
  }

  if (loginRequired && !loginAuthenticated) {
    return <LoginPage onSuccess={handleLoginSuccess} />;
  }

  if (loginRequired === null) {
    return <div className="h-dvh bg-surface-900 safe-area-inset" />;
  }

  return (
    <IdleDecayWindowContext.Provider value={idleDecayWindowMs}>
      <UnreadIndicatorContext.Provider value={unreadIndicatorEnabled}>
        <SessionRowTagContext.Provider value={sessionRowTagMode}>
          {/* PluginUiProvider must sit above AppContent: AppContent itself reads
              the plugin UI snapshot (usePluginPanes), so the provider can't live
              inside its own return. */}
          <PluginUiProvider>
            <AppContent loginRequired={loginRequired} onLogout={handleLogout} onSettingsRefresh={refreshAppSettings} />
          </PluginUiProvider>
          <ElevationPrompt />
        </SessionRowTagContext.Provider>
      </UnreadIndicatorContext.Provider>
    </IdleDecayWindowContext.Provider>
  );
}

/** Walk from the event target up to the document root looking for any
 *  text-input surface, so global hotkeys don't fire when the user is
 *  typing in an `<input>`, `<textarea>`, or contenteditable element
 *  (or any contenteditable ancestor of a deeper rich-text widget). */
function isInsideEditable(target: EventTarget | null): boolean {
  let el: HTMLElement | null = target instanceof HTMLElement ? target : null;
  while (el) {
    const tag = el.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || el.isContentEditable) {
      return true;
    }
    el = el.parentElement;
  }
  return false;
}

function AppContent({
  loginRequired,
  onLogout,
  onSettingsRefresh,
}: {
  loginRequired: boolean;
  onLogout: () => void;
  onSettingsRefresh: () => Promise<void> | void;
}) {
  // Wire the localStorage write chokepoint and pull the server-side UI-state
  // blob into localStorage. AppContent only mounts past auth, so this runs as
  // the authenticated user. Background (does NOT gate render): blocking first
  // paint on this fetch raced immediate interactions and could flash a blank
  // screen if the endpoint were slow. A brand-new browser paints local defaults
  // for the first session; hydration writes the synced values for the next
  // mount/reload. Same-device loads (populated cache) are unaffected.
  useEffect(() => {
    initWebUiSync();
    void hydrateWebUiStateFromServer();
  }, []);

  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const { settings: webSettings } = useWebSettings();
  const sessionMatch = useMatch("/session/:sessionId");
  const settingsRootMatch = useMatch("/settings");
  const settingsTabMatch = useMatch("/settings/:tab");
  const profilesMatch = useMatch("/profiles");
  const activeSessionId = sessionMatch?.params.sessionId ?? null;
  const showSettings = settingsRootMatch !== null || settingsTabMatch !== null;
  const settingsTab = settingsTabMatch?.params.tab ?? null;

  const {
    sessions,
    workspaceOrdering,
    setWorkspaceOrdering,
    markLocalOrderingUpdate,
    error,
    loaded: sessionsLoaded,
    injectSession,
    setSessionStatus,
    applySession,
  } = useSessions();
  const workspaces = useWorkspaces(sessions);
  // Trash is a whole-workspace concern, so it is derived here from the
  // authoritative unsliced workspace list rather than reconstructed from the
  // sidebar's per-`group_path` slice views. A workspace is in Trash only when
  // every one of its sessions is trashed, and Restore/Delete then cover all of
  // them. See #2533.
  const trashedWorkspaces = useMemo(() => workspaces.filter(workspaceIsTrashed), [workspaces]);

  // Remember the active session and restore it on a PWA relaunch (#2103).
  useLastSessionRestore({ activeSessionId, sessions, sessionsLoaded });

  // One-shot orphan-draft sweep once useSessions has settled its first
  // fetch (success or null). Catches acp:draft:<id> keys left behind
  // by deletions that happened in another tab or on another device since
  // the last load (#1358). The local-tab delete path calls clearDraft
  // directly so it does not need to wait for this. Gating on
  // `sessionsLoaded` rather than `sessions.length > 0` covers the
  // legitimate empty-server case: a brand-new user with zero sessions
  // must still get prior orphan drafts swept. Bounded by localStorage
  // entry count; cheap.
  const sweptDraftsRef = useRef(false);
  useEffect(() => {
    if (sweptDraftsRef.current) return;
    if (!sessionsLoaded) return;
    sweptDraftsRef.current = true;
    sweepOrphanDrafts(new Set(sessions.map((s) => s.id)));
  }, [sessionsLoaded, sessions]);

  // Same once-on-mount sweep for diff-comments keys (#1842). Clears keys for
  // deleted sessions and retroactively removes empty keys written before the
  // empty-removal fix. Mirrors the draft sweep above.
  const sweptCommentsRef = useRef(false);
  useEffect(() => {
    if (sweptCommentsRef.current) return;
    if (!sessionsLoaded) return;
    sweptCommentsRef.current = true;
    sweepOrphanComments(new Set(sessions.map((s) => s.id)));
  }, [sessionsLoaded, sessions]);

  const [sidebarSortMode, setSidebarSortMode] = useSidebarSortMode();
  const [sidebarAxis, setSidebarAxis] = useSidebarAxis();

  // Active plugin sort (#2401): an ephemeral selection of a live `sort-key`
  // entry. Not persisted (plugin entries die with the daemon). The ref is only
  // ever read by resolving it against the live snapshot, so a stale ref (entry
  // gone after a daemon restart) is inert and the sidebar falls back to the
  // built-in sort; if the entry reappears on a later poll the selection
  // resumes. Selecting a built-in mode clears it via `selectSidebarSortMode`.
  const pluginUiEntries = usePluginUiEntries();
  const [pluginSortRef, setPluginSortRef] = useState<{ pluginId: string; entryId: string } | null>(null);
  const activePluginSort = useMemo(() => {
    if (!pluginSortRef) return null;
    return (
      pluginSortSpecs(pluginUiEntries).find(
        (s) => s.pluginId === pluginSortRef.pluginId && s.entryId === pluginSortRef.entryId,
      ) ?? null
    );
  }, [pluginUiEntries, pluginSortRef]);
  const pluginSort = useMemo<PluginSortContext | undefined>(
    () =>
      activePluginSort
        ? {
            direction: activePluginSort.direction,
            values: buildSortValueMap(pluginUiEntries, activePluginSort.pluginId, activePluginSort.column),
          }
        : undefined,
    [activePluginSort, pluginUiEntries],
  );
  const selectSidebarSortMode = useCallback(
    (mode: SidebarSortMode) => {
      setPluginSortRef(null);
      setSidebarSortMode(mode);
    },
    [setSidebarSortMode],
  );

  const { projects, refresh: refreshProjects } = useProjects();
  const {
    groups: repoGroups,
    savedProjects,
    toggleRepoCollapsed,
    updateRepoAppearance,
    reorderRepoGroups,
  } = useRepoGroups(workspaces, workspaceOrdering, sidebarSortMode, projects, pluginSort);
  const { groups: sessionGroups, toggleGroupCollapsed } = useSessionGroups(workspaces, sidebarSortMode, pluginSort);
  // The nested `repo+group` axis reuses the already-built repo groups for
  // its top level (so repo collapse, appearance, and ordering are shared
  // with the repo axis) and splits each repo by `group_path` underneath.
  // See #1720.
  const { groups: nestedGroups, toggleSubgroupCollapsed } = useNestedSidebarGroups(
    repoGroups,
    sidebarSortMode,
    pluginSort,
  );

  // The sidebar render path consumes one honest model (SidebarGroup): the
  // repo axis maps in via an adapter, the user-group axis is already in
  // that shape. Collapse routing follows the active axis so the two
  // axes keep independent collapse state. See #1234.
  const sidebarGroups = useMemo(
    () => (sidebarAxis === "group" ? sessionGroups : repoGroups.map(repoGroupToSidebarGroup)),
    [sidebarAxis, sessionGroups, repoGroups],
  );
  const toggleSidebarGroup = sidebarAxis === "group" ? toggleGroupCollapsed : toggleRepoCollapsed;

  // Drag-end handler for the sidebar. Optimistically applies the new
  // order locally so the row snaps into place, then persists to the
  // server. `markLocalOrderingUpdate` opens a short window during
  // which polled responses do not clobber our just-applied state, so a
  // poll firing mid-PUT can't revert the drag.
  const handleReorderWorkspaces = useCallback(
    (newOrder: string[]) => {
      setWorkspaceOrdering(newOrder);
      markLocalOrderingUpdate();
      void updateWorkspaceOrdering(newOrder);
    },
    [setWorkspaceOrdering, markLocalOrderingUpdate],
  );

  // Selected diff-file identity. `repoName` is undefined for single-repo
  // sessions and the workspace member name for multi-repo workspaces.
  // Kept as one state so the path + repo always update together; with
  // two parallel states we'd briefly fetch the wrong repo when only
  // one side changed (workspace path collisions across repos make this
  // a real bug, not theoretical). See #1047.
  const [selectedFile, setSelectedFile] = useState<{
    path: string;
    repoName?: string;
    /** 1-based source line to scroll into view, when the file was opened from a
     *  transcript `path:line` link. Undefined for plain file-list clicks. */
    line?: number;
    /** Opened from a transcript file-ref rather than the diff list. Such a
     *  file may have no diff against the base (full-file fallback, #1810), so
     *  it must not be auto-cleared for being absent from the diff list. */
    cited?: boolean;
  } | null>(null);
  const selectedFilePath = selectedFile?.path ?? null;
  const selectedRepoName = selectedFile?.repoName;
  const selectedFileLine = selectedFile?.line;
  // Dock panes render as tabbed groups (#2437): each dock holds an ordered set
  // of tabs (diff, one-or-more terminals, plugin panes) with one active body.
  // The tab membership + active tab + terminal count are persisted per session;
  // dock sizes stay global (Dock/BottomDock own those localStorage keys).
  const {
    layout: paneLayout,
    openTab,
    addTerminal,
    closeTab,
    activateTab,
    moveTab,
    placeTab,
    toggleKind,
    togglePlugin,
    syncPlugins,
    setDockCollapsed,
  } = usePaneLayout(activeSessionId);
  const pluginPanes = usePluginPanes(activeSessionId);
  const pluginPaneById = useMemo(() => {
    const m = new Map<string, PluginPane>();
    for (const p of pluginPanes) m.set(p.id, p);
    return m;
  }, [pluginPanes]);

  // Auto-add newly available plugin panes as tabs in their default dock; the
  // layout suppresses any the user explicitly closed.
  useEffect(() => {
    syncPlugins(pluginPanes.map((p) => ({ id: p.id, defaultDock: p.defaultDock })));
  }, [pluginPanes, syncPlugins]);

  // One-shot lookup from plugin id to its manifest identity (icon name +
  // icon_asset URL), so a pane gets a real identity glyph, up to the plugin's
  // actual logo, even when it doesn't set its own per-pane icon. Fetched once
  // on mount rather than polled: the installed-plugin list itself has no live
  // sync anywhere else in the app today (Settings > Plugins only reloads on
  // mount and after its own mutations), so this matches existing staleness
  // tolerance rather than introducing a new one.
  const [pluginIdentityById, setPluginIdentityById] = useState<
    Record<string, { icon?: string; iconAssetUrl?: string }>
  >({});
  useEffect(() => {
    void fetchPlugins().then((res) => {
      if (!res) return;
      setPluginIdentityById(
        Object.fromEntries(
          res.plugins.map((p) => [p.id, { icon: p.icon ?? undefined, iconAssetUrl: p.icon_asset_url ?? undefined }]),
        ),
      );
    });
  }, []);

  const paneDescriptor = useCallback(
    (id: string): PaneDisplay => {
      const plugin = pluginPaneById.get(id);
      if (plugin) {
        const identity = pluginIdentityById[plugin.entry.plugin_id];
        const icon = resolvePaneIcon(plugin.icon, identity?.icon) ?? Puzzle;
        // A plugin's real logo outranks any lucide glyph, including a
        // per-pane runtime icon a worker chose before icon_asset existed.
        return { title: plugin.title, icon, iconAssetUrl: identity?.iconAssetUrl };
      }
      if (isTerminalTabId(id)) {
        const idx = terminalIndexOf(id);
        const term = BUILTIN_PANES.find((p) => p.id === "terminal")!;
        return { title: idx === 0 ? term.title : `${term.title} ${idx + 1}`, icon: term.icon };
      }
      const d = BUILTIN_PANES.find((p) => p.id === id)!;
      return { title: d.title, icon: d.icon };
    },
    [pluginPaneById, pluginIdentityById],
  );

  // A persisted tab is visible only if its backing pane currently exists: diff
  // and terminals always do; a plugin tab does only while its plugin is loaded.
  const tabAvailable = useCallback(
    (id: string) => !id.startsWith("plugin:") || pluginPaneById.has(id),
    [pluginPaneById],
  );
  // A dock's groups reduced to what's actually shown: each surviving group keeps
  // its persisted index (so a drop addresses the right group) and a valid active
  // tab. Groups with no visible tab (only unloaded plugins) are not rendered.
  const renderGroups = useCallback(
    (dock: DockLocation): DockGroupView[] =>
      dockGroups(paneLayout, dock)
        .map((g, group) => {
          const tabs = g.tabs.filter(tabAvailable);
          const active = g.active && tabs.includes(g.active) ? g.active : (tabs[0] ?? null);
          return { group, tabs, active };
        })
        .filter((g) => g.tabs.length > 0),
    [paneLayout, tabAvailable],
  );

  const availableRightGroups = useMemo(() => renderGroups("right"), [renderGroups]);
  const rightDockExplicitlyCollapsed = isDockCollapsed(paneLayout, "right");
  const rightDockCollapsed = rightDockExplicitlyCollapsed || availableRightGroups.length === 0;
  const rightGroups = useMemo(
    () => (rightDockExplicitlyCollapsed ? [] : availableRightGroups),
    [rightDockExplicitlyCollapsed, availableRightGroups],
  );
  const bottomGroups = useMemo(() => renderGroups("bottom"), [renderGroups]);
  const groupsByDock = useMemo(
    () => ({
      right: rightGroups.map((g) => ({ group: g.group, tabs: g.tabs })),
      bottom: bottomGroups.map((g) => ({ group: g.group, tabs: g.tabs })),
    }),
    [rightGroups, bottomGroups],
  );
  const terminalOpen = (["right", "bottom"] as DockLocation[]).some(
    (d) => !isDockCollapsed(paneLayout, d) && dockTabs(paneLayout, d).some(isTerminalTabId),
  );

  // Activity-bar entries are pane KINDS (diff, terminal, each plugin), not
  // individual tabs; the strip's +/x manage terminal instances.
  const isPaneOpen = (kind: string): boolean => {
    if (kind === "terminal") return terminalOpen;
    const dock = dockOf(paneLayout, kind);
    return dock !== null && !isDockCollapsed(paneLayout, dock);
  };
  const togglePaneAny = useCallback(
    (kind: string) => {
      const defaultDock: DockLocation =
        pluginPaneById.get(kind)?.defaultDock ?? BUILTIN_PANES.find((p) => p.id === kind)?.defaultDock ?? "right";
      if (isPluginPaneId(kind)) togglePlugin(kind, defaultDock);
      else toggleKind(kind as "diff" | "terminal" | "agents", defaultDock);
    },
    [toggleKind, togglePlugin, pluginPaneById],
  );
  // Open (or focus) the Sub agents pane. Used by an inline async
  // sub-agent card to jump to its panel entry.
  const openAgentsPane = useCallback(() => {
    const dock = dockOf(paneLayout, "agents");
    if (dock) activateTab(dock, "agents");
    else toggleKind("agents", "right");
  }, [paneLayout, activateTab, toggleKind]);
  const closePaneAny = useCallback(
    (id: string) => {
      // Closing an extra terminal tab kills its tmux shell so it does not leak;
      // terminal 0 (shared with the native TUI) only hides. Diff/plugin tabs
      // have no backend shell to reap.
      if (isTerminalTabId(id)) {
        const idx = terminalIndexOf(id);
        if (idx >= 1) {
          // Remove the tab only once the shell is actually killed; if the
          // DELETE fails, keep the tab so the user can retry instead of
          // silently leaking the shell with no way to close it.
          if (activeSessionId) {
            void killTerminal(activeSessionId, idx).then((ok) => {
              if (ok) closeTab(id);
            });
          }
          return;
        }
      }
      closeTab(id);
    },
    [closeTab, activeSessionId],
  );
  const movePaneAny = useCallback((id: string, dock: DockLocation) => moveTab(id, dock), [moveTab]);
  // The dnd controller works in visible-tab space; map a drop index back to the
  // target group's full persisted tab list, since a hidden (unloaded) plugin tab
  // still holds a slot the visible index does not count. A split target carries
  // no index (the new group starts with just the dragged tab).
  const placeVisibleTab = useCallback(
    (id: string, target: DropTarget) => {
      if (target.newGroup) {
        placeTab(id, { dock: target.dock, group: target.group, newGroup: true });
        return;
      }
      const fullBase = (dockGroups(paneLayout, target.dock)[target.group]?.tabs ?? []).filter((tab) => tab !== id);
      const index = visibleToFullIndex(fullBase, target.index ?? fullBase.length, tabAvailable);
      placeTab(id, { dock: target.dock, group: target.group, index });
    },
    [paneLayout, placeTab, tabAvailable],
  );
  // Layout topology is width-driven so it stays aligned with the `md:`
  // Tailwind classes the rest of the layout uses. At md and up the
  // side-by-side ContentSplit renders; below md a single full-viewport
  // pane shows one of agent / diff / paired, chosen via the picker (#1452).
  const isMdUp = useIsWideViewport();
  const singlePane = !isMdUp;
  const [rightPanelView, setRightPanelView] = useState<RightPanelView>("agent");
  const [pickerOpen, setPickerOpen] = useState(false);
  // The paired shell mounts lazily on first activation, then stays mounted
  // (kept alive but hidden) so its PTY, scrollback, and focus survive view
  // switches. Mounting it eagerly would spawn a shell for every mobile
  // session the user never opens the shell on.
  const [pairedMounted, setPairedMounted] = useState(false);
  const [showSessionWizard, setShowSessionWizard] = useState(false);
  const [showHelp, setShowHelp] = useState(false);
  const tipsAutoPoppedRef = useRef(false);
  // Whether the tour was already seen when this page loaded (set in the settings
  // fetch below). Auto-pop keys off this, not the live tourSeen, so finishing
  // the tour this session does not then pop tips on top of the first-run flow.
  const tourSeenAtLoadRef = useRef<boolean | null>(null);
  // All tips orchestration (open state, mark-seen, the show toggle, the auto-pop
  // decision) lives in the hook / lib so it stays out of this component and is
  // unit-tested directly.
  const tips = useTips();
  const [showPalette, setShowPalette] = useState(false);
  // Palette content-search query (#2515); declared here so the keyboard
  // handlers below can clear it on close/toggle. Consumed lower down by
  // useConversationSearch.
  const [paletteQuery, setPaletteQuery] = useState("");
  // Session id awaiting a snooze duration from the palette's "Snooze…" action;
  // drives the shared SnoozeModal. Null when no snooze is in flight.
  const [snoozeTargetId, setSnoozeTargetId] = useState<string | null>(null);
  const [showAbout, setShowAbout] = useState(false);
  const [telemetryConsentNeeded, setTelemetryConsentNeeded] = useState(false);
  // Whether the telemetry status fetch has settled. `telemetryConsentNeeded`
  // starts false, so before this is true "no consent needed" and "not resolved
  // yet" look the same; the tips auto-pop waits on this so it can't slip in
  // before a pending consent modal.
  const [telemetryConsentKnown, setTelemetryConsentKnown] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(() => window.innerWidth >= 768);
  const keyboardProxyRef = useRef<HTMLTextAreaElement>(null);

  const activeWorkspace = useMemo(() => {
    if (!activeSessionId) return undefined;
    return workspaces.find((w) => w.sessions.some((s) => s.id === activeSessionId));
  }, [workspaces, activeSessionId]);
  const activeSession = activeWorkspace?.sessions.find((s) => s.id === activeSessionId);
  const allPaneIds: string[] = [
    "diff",
    "terminal",
    // The background-agents panel only applies to structured-view (ACP)
    // sessions; a plain terminal session never launches sub-agents.
    ...(activeSession?.view === "structured" ? ["agents"] : []),
    ...pluginPanes.map((p) => p.id),
  ];

  // Fetch the diff when the panel is actually showing: on desktop when the
  // split is expanded, on mobile when the diff view is the active pane.
  const diffPanelActive = isMdUp ? dockOf(paneLayout, "diff") !== null : rightPanelView === "diff";
  const {
    files: diffFiles,
    perRepoBases,
    warning,
    loading: diffFilesLoading,
    revision,
    refresh: refreshDiffFiles,
  } = useDiffFiles(activeSessionId, diffPanelActive);

  // Diff-viewer comments (#928). Acp-only and session-scoped. The
  // banner lives in the diff pane while the inline UI lives inside
  // DiffFileViewer, so the store is lifted here and threaded to both.
  const diffComments = useDiffComments(activeSessionId);
  const commentsEnabled = activeSession?.view === "structured";
  const commentSendEnabled = commentsEnabled && activeSession?.acp_worker_state === "running";
  const commentSendDisabledReason = !commentsEnabled
    ? "Diff comments require an acp session"
    : "Acp worker is not running";
  const commentsIsMultiRepo = (activeSession?.workspace_repos.length ?? 0) > 0;
  const [sendDialogOpen, setSendDialogOpen] = useState(false);

  useEffect(() => {
    if (!commentSendEnabled) return;
    const onKey = (e: KeyboardEvent) => {
      if (!((e.metaKey || e.ctrlKey) && e.shiftKey && e.key.toLowerCase() === "s")) {
        return;
      }
      if (isInsideEditable(e.target)) return;
      if (diffComments.count === 0) return;
      e.preventDefault();
      setSendDialogOpen(true);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [commentSendEnabled, diffComments.count]);

  // Clear-on-view: opening a session (or having it open when its turn
  // finishes) reads it, clearing the unread marker. Mirrors the TUI, where
  // engaging with a session (open / live-send / dwell) clears it. The sidebar
  // separately hides the chip for the active row, so there's no flash in the
  // ~poll window before this lands.
  const unreadIndicatorEnabled = useUnreadIndicatorEnabled();
  useEffect(() => {
    if (unreadIndicatorEnabled && activeSessionId && activeSession?.unread) {
      void setSessionUnread(activeSessionId, false);
    }
  }, [unreadIndicatorEnabled, activeSessionId, activeSession?.unread]);

  // Derive selectedFile/rightPanelView/pickerOpen/pairedMounted resets
  // during render to satisfy
  // react-you-might-not-need-an-effect/no-adjust-state-on-prop-change
  // and react-hooks/set-state-in-effect.
  const prevActiveSessionIdRef = useRef(activeSessionId);
  if (activeSessionId !== prevActiveSessionIdRef.current) {
    prevActiveSessionIdRef.current = activeSessionId;
    setRightPanelView("agent");
    setPickerOpen(false);
    setPairedMounted(false);
    setSelectedFile(null);
  }

  // Inline derivation for diffFiles validation: clear a stale diff-list
  // selection. The staleness rule (cited exemption, path+repo match) lives in
  // diffSelectionStale so it can be unit-tested. See #1810.
  if (activeSessionId && diffSelectionStale(selectedFile, diffFilesLoading, diffFiles)) {
    setSelectedFile(null);
  }

  // Mount the paired shell on first activation and keep it mounted after.
  if (rightPanelView === "paired" && !pairedMounted) {
    setPairedMounted(true);
  }

  // A plugin pane promoted into the mobile main pane can vanish (plugin
  // unloaded, or the new session has no such pane). Fall back to the agent
  // view so the user is never stranded on a blank pane. Mirrors the diff /
  // paired guards above; render-phase derivation per the block at the top.
  if (isPluginPaneId(rightPanelView) && !pluginPanes.some((p) => p.id === rightPanelView)) {
    setRightPanelView("agent");
  }

  // Refit the newly active terminal after a single-pane view switch: the
  // layers keep their geometry while hidden (visibility, not display:none),
  // but a resize nudge re-runs the xterm fit so the grid matches exactly.
  useEffect(() => {
    if (!singlePane) return;
    const id = requestAnimationFrame(() => window.dispatchEvent(new Event("resize")));
    return () => cancelAnimationFrame(id);
  }, [singlePane, rightPanelView]);

  const focusKeyboardProxy = () => {
    if (window.innerWidth < 768 && navigator.maxTouchPoints > 0) {
      keyboardProxyRef.current?.focus();
    }
  };

  // Selecting a session in the sidebar should land focus on its canonical
  // "type here" target so the user can start typing without a second click:
  // the acp composer in acp mode, the xterm textarea otherwise. See
  // requestSessionInputFocus for the dispatch/latch and coarse-pointer rules.
  const isCoarse = useIsCoarsePointer();
  const focusAgentInput = useCallback(
    (session: SessionResponse | undefined) => requestSessionInputFocus(session, isCoarse),
    [isCoarse],
  );

  const handleSelectSession = useCallback(
    (sessionId: string) => {
      const ws = workspaces.find((w) => w.sessions.some((s) => s.id === sessionId));
      if (ws) {
        const picked = ws.sessions.find((s) => s.id === sessionId);
        navigate(`/session/${encodeURIComponent(sessionId)}`);
        // On touch devices, raise the soft keyboard within the tap gesture and
        // latch the terminal/composer to take focus once it mounts (keeping the
        // keyboard up) — but only when the user opted into auto-open keyboard.
        // On desktop the proxy is a no-op and we focus the real input directly.
        if (isCoarse) {
          if (webSettings.autoOpenKeyboard) {
            focusKeyboardProxy();
            setPendingTerminalFocus(picked?.view === "structured" ? "composer" : "agent");
          }
        } else {
          focusKeyboardProxy();
          focusAgentInput(picked);
        }
        if (window.innerWidth < 768) setSidebarOpen(false);
      }
    },
    [navigate, workspaces, focusAgentInput, isCoarse, webSettings.autoOpenKeyboard],
  );

  const handleSelectWorkspace = (workspaceId: string) => {
    const ws = workspaces.find((w) => w.id === workspaceId);
    if (ws) {
      const running = ws.sessions.find((s) => isSessionActive(s, idleDecayWindowMs));
      const picked = running ?? ws.sessions[0] ?? null;
      if (picked) {
        navigate(`/session/${encodeURIComponent(picked.id)}`);
        // Mirror handleSelectSession: on touch, raise the keyboard + latch focus
        // only when auto-open keyboard is enabled; on desktop focus directly.
        if (isCoarse) {
          if (webSettings.autoOpenKeyboard) {
            focusKeyboardProxy();
            setPendingTerminalFocus(picked.view === "structured" ? "composer" : "agent");
          }
        } else {
          focusKeyboardProxy();
          focusAgentInput(picked);
        }
      } else {
        navigate("/");
      }
    }
    if (window.innerWidth < 768) {
      setSidebarOpen(false);
    }
  };

  // In-app toast forwarded from the service worker sets this event when
  // the user taps it; navigate to the session that triggered the push.
  useEffect(() => {
    const onOpen = (e: Event) => {
      const detail = (e as CustomEvent).detail as { sessionId?: string } | undefined;
      if (detail?.sessionId) {
        handleSelectSession(detail.sessionId);
      }
    };
    window.addEventListener(OPEN_SESSION_EVENT, onOpen);
    return () => window.removeEventListener(OPEN_SESSION_EVENT, onOpen);
  }, [handleSelectSession]);

  const [wizardPrefill, setWizardPrefill] = useState<WizardPrefill | undefined>(undefined);
  const [deletingWorkspaceId, setDeletingWorkspaceId] = useState<string | null>(null);
  const [stoppingWorkspaceId, setStoppingWorkspaceId] = useState<string | null>(null);
  const [serverAbout, setServerAbout] = useState<ServerAbout | null>(null);
  // `serverAbout === null` conflates "not fetched yet" with "fetch failed", so
  // the tour gates auto-launch on an explicit loaded flag instead.
  const [serverAboutLoaded, setServerAboutLoaded] = useState(false);

  const refreshServerAbout = useCallback(async () => {
    try {
      const about = await fetchAbout();
      if (about) setServerAbout(about);
    } finally {
      setServerAboutLoaded(true);
    }
  }, []);

  // Kick off the initial server-about fetch on mount. The effect body only
  // calls fetchAbout and schedules the telemetry consent check; neither runs
  // setState synchronously, so set-state-in-effect is not triggered.
  useEffect(() => {
    let active = true;
    void fetchAbout().then((about) => {
      if (!active) return;
      if (about) setServerAbout(about);
      setServerAboutLoaded(true);
      // Read-only servers can't persist an opt-in choice, so skip the ping.
      if (about && !about.read_only) reportTelemetrySeen("web");
    });
    void fetchTelemetryStatus()
      .then((status) => {
        if (!active || !status) return;
        if (!status.responded && !status.do_not_track) {
          setTelemetryConsentNeeded(true);
        }
      })
      .finally(() => {
        if (active) setTelemetryConsentKnown(true);
      });
    return () => {
      active = false;
    };
  }, []);

  // Telemetry: report that the acp web UI was opened, folded into the
  // daemon's next opt-in snapshot under the `usage_seen` map's `acp` key.
  // `activeSession` drives both the desktop and mobile acp mounts, so this
  // single effect covers both layouts. Same guard as the `"web"` ping above:
  // skip until `serverAbout` loads, skip read-only servers (which can't
  // persist). The backend folds repeated pings into a monotonic open-count
  // (decremented by exactly what each snapshot reported), so re-fires on
  // session switch are harmless. See #1882.
  useEffect(() => {
    if (!serverAboutLoaded || serverAbout?.read_only) return;
    if (activeSession?.view !== "structured") return;
    reportTelemetrySeen("structured_view");
  }, [serverAboutLoaded, serverAbout?.read_only, activeSession?.view]);

  const handleTelemetryConsent = useCallback((enabled: boolean) => {
    setTelemetryConsentNeeded(false);
    void setTelemetryConsent(enabled);
  }, []);

  const deletingWorkspace = deletingWorkspaceId ? workspaces.find((w) => w.id === deletingWorkspaceId) : null;
  const deletingSessions = deletingWorkspace?.sessions ?? [];
  const liveDeletingSessions = deletingSessions.filter((session) => !session.trashed_at);
  const deletingSession = deletingWorkspace?.sessions[0] ?? null;
  const deletingDefaultToTrash = liveDeletingSessions.some((session) => session.cleanup_defaults.delete_to_trash);
  const deletingCleanupDefaults = deletingSession
    ? {
        delete_to_trash: deletingDefaultToTrash,
        delete_worktree: deletingSessions.some(
          (session) => (session.has_cleanable_worktree ?? false) && session.cleanup_defaults.delete_worktree,
        ),
        delete_branch: deletingSessions.some(
          (session) => (session.has_cleanable_worktree ?? false) && session.cleanup_defaults.delete_branch,
        ),
        delete_sandbox: deletingSessions.some(
          (session) => session.is_sandboxed && session.cleanup_defaults.delete_sandbox,
        ),
      }
    : null;
  const deletingBranchName =
    deletingSessions.find((session) => session.branch)?.branch ?? deletingSession?.branch ?? null;

  const handleDeleteSession = useCallback((workspaceId: string) => {
    setDeletingWorkspaceId(workspaceId);
  }, []);

  const handleConfirmDelete = async (options: DeleteSessionOptions) => {
    if (!deletingWorkspace) return;
    const sessions = deletingWorkspace.sessions;
    // Close the dialog immediately; the loop, ordering, and toast logic live
    // in deleteWorkspaceSessions so they are unit-testable without the bundle.
    setDeletingWorkspaceId(null);
    await deleteWorkspaceSessions(sessions, options, activeSessionId, {
      setStatus: setSessionStatus,
      // Drop a deleted session's local-only state (#1358 acp cache + draft,
      // #1842 diff comments). Cross-tab / cross-device deletes fall to the
      // startup sweep.
      purgeLocal: (id) => {
        clearAcpCache(id);
        clearDraft(id);
        clearStoredComments(id);
      },
      navigateHome: () => navigate("/"),
      notify: toastBus.handler,
    });
  };

  // Move-to-trash path (#2489): the safe default. Unlike permanent delete it
  // deliberately KEEPS the per-session acp cache, draft, and stored comments
  // so a restore is faithful; only purge clears them. Trashes every session
  // in the workspace so a multi-session workspace sinks as a whole.
  const handleConfirmTrash = async () => {
    if (!deletingWorkspace) return;
    const ids = deletingWorkspace.sessions.map((s) => s.id);
    if (ids.length === 0) return;
    const wasActive = activeSessionId != null && ids.includes(activeSessionId);

    setDeletingWorkspaceId(null);
    for (const id of ids) setSessionStatus(id, "Stopped");
    if (wasActive) {
      navigate("/");
    }

    // The returned snapshot re-buckets each row into Trash immediately
    // instead of on the next poll. See trashSessions.
    await trashSessions(ids, {
      applySession,
      onError: (id) => setSessionStatus(id, "Error"),
      notify: toastBus.handler,
    });
  };

  // Restore a trashed workspace from the sidebar Trash section (#2489).
  // Restores every session in the workspace (a workspace only lands in Trash
  // when all of its sessions are trashed), not just the first.
  const handleRestoreSession = useCallback(
    (sessionIds: string[]) => restoreSessions(sessionIds, { applySession, notify: toastBus.handler }),
    [applySession],
  );

  const stoppingWorkspace = stoppingWorkspaceId ? workspaces.find((w) => w.id === stoppingWorkspaceId) : null;
  const stoppingSession = stoppingWorkspace?.sessions[0] ?? null;

  const handleStopSession = useCallback((workspaceId: string) => {
    setStoppingWorkspaceId(workspaceId);
  }, []);

  const handleConfirmStop = useCallback(async () => {
    if (!stoppingSession) return;
    const sessionId = stoppingSession.id;

    // Close the dialog and show "Stopped" immediately; the 2s status poller
    // reconciles the true state and corrects this if the request fails.
    setStoppingWorkspaceId(null);
    setSessionStatus(sessionId, "Stopped");

    const result = await stopSession(sessionId);
    if (!result) {
      setSessionStatus(sessionId, "Error");
      toastBus.handler?.error("Failed to stop session");
      return;
    }
    toastBus.handler?.info("Session stopped");
  }, [stoppingSession, setSessionStatus]);

  const handleStartSession = useCallback(
    async (workspaceId: string) => {
      const ws = workspaces.find((w) => w.id === workspaceId);
      const session = ws?.sessions[0];
      if (!session) return;

      // Optimistic Starting; the status poller reconciles to the real state.
      setSessionStatus(session.id, "Starting");
      const result = await startSession(session.id);
      if (!result) {
        setSessionStatus(session.id, "Error");
        toastBus.handler?.error("Failed to start session");
        return;
      }
      toastBus.handler?.info("Session started");
    },
    [workspaces, setSessionStatus],
  );

  const handleCreateSession = useCallback(
    (repoPath: string) => {
      const projectSessions = sessions
        .filter((s) => (s.main_repo_path || s.project_path) === repoPath)
        .sort((a, b) => (b.last_accessed_at ?? "").localeCompare(a.last_accessed_at ?? ""));
      const latest = projectSessions[0];

      setWizardPrefill({
        path: repoPath,
        tool: latest?.tool ?? "claude",
        yoloMode: latest?.yolo_mode ?? false,
        sandboxEnabled: latest?.is_sandboxed ?? false,
        profile: latest?.profile || undefined,
        group: latest?.group_path || undefined,
      });
      setShowSessionWizard(true);
    },
    [sessions],
  );

  // Pin a repo so its header persists with zero sessions. If the repo is
  // already a saved project, just set its pin flag (PATCH); otherwise register
  // it pinned (scope global, matching the TUI's global registry). Then refresh
  // so the diamond / empty header reflects it. See #2047, #2208.
  const handlePinProject = useCallback(
    async (repoPath: string) => {
      const key = normalizeProjectPathKey(repoPath);
      const existing = projects.filter((p) => normalizeProjectPathKey(p.path) === key);
      let failed: { error?: string } | undefined;
      if (existing.length > 0) {
        const results = await Promise.all(existing.map((p) => setProjectPinned(p.name, p.scope, true)));
        failed = results.find((r) => !r.ok);
      } else {
        const res = await createProject({ path: repoPath, scope: "global", pinned: true });
        if (!res.ok) failed = res;
      }
      if (failed) {
        toastBus.handler?.error(failed.error ?? "Failed to pin project");
        return;
      }
      await refreshProjects();
    },
    [projects, refreshProjects],
  );

  // Unpin a repo: clear the pin flag on every pinned registry entry for its
  // path (a path can be registered under both global and profile scope),
  // keeping the saved project so it stays in the Projects view and the wizard.
  // Only the Projects view's Remove deletes the entry. See #2208.
  const handleUnpinProject = useCallback(
    async (group: SidebarGroup) => {
      const pinned = group.registeredProjects.filter((p) => p.pinned);
      const results = await Promise.all(pinned.map((p) => setProjectPinned(p.name, p.scope, false)));
      const failed = results.find((r) => !r.ok);
      if (failed) {
        toastBus.handler?.error(failed.error ?? "Failed to unpin project");
      }
      await refreshProjects();
    },
    [refreshProjects],
  );

  // Add / edit a saved project from the sidebar Projects section. The modal is
  // open for `add` (no editProject) or `edit` (a specific registration); both
  // refresh the registry on save. See #2212.
  const [projectForm, setProjectForm] = useState<{ editProject: ProjectInfo | null } | null>(null);
  const handleAddProject = useCallback(() => setProjectForm({ editProject: null }), []);
  const handleEditProject = useCallback((project: ProjectInfo) => setProjectForm({ editProject: project }), []);

  // Remove a saved project: delete every registration for its path, then
  // refresh. Confirms first since it is not undoable. See #2212.
  const handleRemoveProject = useCallback(
    async (group: RepoGroup) => {
      if (!confirm(`Remove project '${group.displayName}' from the sidebar?`)) return;
      const results = await Promise.all(group.registeredProjects.map((p) => deleteProject(p.name, p.scope)));
      const failed = results.find((r) => !r.ok);
      if (failed) {
        toastBus.handler?.error(failed.error ?? "Failed to remove project");
      }
      await refreshProjects();
    },
    [refreshProjects],
  );

  // The right-panel control toggles the desktop split, but on mobile there
  // is no split to collapse: it opens the view picker instead (#1452).
  const toggleDiff = useCallback(() => {
    if (isMdUp) {
      toggleKind("diff", "right");
    } else {
      setPickerOpen((o) => !o);
    }
  }, [isMdUp, toggleKind]);

  // Collapse or restore the whole right dock (the "toggle right panel"
  // shortcut). Collapse hides the dock without removing its tabs so expanding
  // restores the active session's previous pane set.
  const toggleRightDock = useCallback(() => {
    if (!isMdUp) {
      setPickerOpen((o) => !o);
      return;
    }
    if (rightDockCollapsed) {
      setDockCollapsed("right", false);
      if (availableRightGroups.length === 0) {
        openTab("diff", "right");
        openTab(terminalTabId(0), "right");
      }
    } else {
      setDockCollapsed("right", true);
    }
  }, [isMdUp, rightDockCollapsed, setDockCollapsed, availableRightGroups.length, openTab]);

  const handlePickView = useCallback((view: RightPanelView) => {
    setRightPanelView(view);
    setPickerOpen(false);
  }, []);

  const handleSelectFile = useCallback((path: string, repoName?: string, line?: number) => {
    setSelectedFile({ path, repoName, line });
  }, []);

  // Open a local file reference cited in an acp transcript (Codex
  // `path:line` markdown links). Resolve the absolute path back to a
  // repo-relative path for the active session and open it in the in-app
  // diff/file viewer, keeping the current session route. A path outside
  // the session's known repo roots surfaces a non-destructive toast
  // rather than navigating away. The parsed line is threaded through so the
  // viewer scrolls it into view. See #1718, #1809.
  const handleOpenFileRef = useCallback(
    (ref: FileRef) => {
      if (!activeSession) return;
      const resolved = resolveToRepoRelative(ref.path, activeSession);
      if (!resolved) {
        toastBus.handler?.error(`Could not open ${ref.path}: not inside this session's repo`);
        return;
      }
      setSelectedFile({
        path: resolved.relativePath,
        repoName: resolved.repoName,
        line: ref.line,
        cited: true,
      });
    },
    [activeSession],
  );

  const handleCloseFile = useCallback(() => {
    setSelectedFile(null);
  }, []);

  const handleGoDashboard = useCallback(() => {
    navigate("/");
    setSelectedFile(null);
  }, [navigate]);

  const handleOpenSettings = useCallback(() => {
    navigate("/settings");
    if (window.innerWidth < 768) setSidebarOpen(false);
  }, [navigate]);

  // Profiles moved into Settings as a tab; redirect the retired standalone
  // route so old bookmarks and links still land somewhere valid.
  useEffect(() => {
    if (profilesMatch) navigate(`/settings/profiles${window.location.search}`, { replace: true });
  }, [profilesMatch, navigate]);

  const handleCloseSettings = useCallback(() => {
    if (activeSessionId) {
      navigate(`/session/${encodeURIComponent(activeSessionId)}`);
    } else {
      navigate("/");
    }
  }, [navigate, activeSessionId]);

  const handleOpenHelp = useCallback(() => {
    setShowHelp(true);
  }, []);

  const handleOpenAbout = useCallback(() => {
    setShowAbout(true);
  }, []);

  const handleToggleSidebar = useCallback(() => {
    setSidebarOpen((o) => !o);
  }, []);

  const openSidebar = useCallback(() => setSidebarOpen(true), []);
  const openDiff = useCallback(() => {
    if (isMdUp) {
      openTab("diff", "right");
    } else {
      setPickerOpen(true);
    }
  }, [isMdUp, openTab]);
  useEdgeSwipe({
    edge: "left",
    // The swipe-right-to-open gesture only makes sense for a left-anchored
    // drawer; with the sidebar on the right edge it would slide in from the
    // opposite side of the drag, so disable it there (#2244).
    enabled: !sidebarOpen && webSettings.sidebarSide !== "right",
    onSwipe: openSidebar,
    blurOnSwipe: true,
    // A swipe-right anywhere on screen opens the sidebar, not just from the
    // left edge. The right-edge (diff) swipe stays edge-only below.
    anywhere: true,
  });
  useEdgeSwipe({
    edge: "right",
    enabled: rightDockCollapsed && !!activeSessionId,
    onSwipe: openDiff,
  });

  // Read-only mode hides mutation UI. Guard creation at the handler so every
  // caller (keyboard shortcut, command palette) is a no-op rather than opening
  // a wizard that 403s on submit. Caught by the live read-only-mode spec.
  const handleNewSession = useCallback(() => {
    if (serverAbout?.read_only) return;
    setWizardPrefill(undefined);
    setShowSessionWizard(true);
  }, [serverAbout?.read_only]);

  const handleNewScratch = useCallback(() => {
    if (serverAbout?.read_only) return;
    setWizardPrefill({ scratch: true });
    setShowSessionWizard(true);
  }, [serverAbout?.read_only]);

  const handleCloneFromUrl = useCallback(() => {
    setWizardPrefill({ initialTab: "clone" });
    setShowSessionWizard(true);
  }, []);

  const handleToggleTerminalFocus = useCallback(() => {
    if (!activeSessionId) return;
    // Probe by data-term attribute rather than a component ref: it is
    // robust against panel reorderings and against the paired terminal
    // living in either the desktop split or the mobile single pane.
    //
    // Semantic: VSCode-like "Cmd+` opens/focuses the terminal." So if the
    // user is NOT in the paired terminal, send them there; only flip back
    // to agent when they're already in paired.
    const active = document.activeElement;
    const pairedPanels = document.querySelectorAll<HTMLElement>('[data-term="paired"]');
    let inPaired = false;
    if (active) {
      for (const p of pairedPanels) {
        if (p.contains(active)) {
          inPaired = true;
          break;
        }
      }
    }
    const target = inPaired ? "agent" : "paired";

    if (singlePane) {
      // Below md there is one full-viewport pane. Promote the target view,
      // then dispatch focus on the next frame: the inactive layer is inert
      // until React commits the switch, and focus() on an inert subtree is
      // a no-op. The paired shell mounts lazily on first activation, so its
      // PTY may not be ready when the dispatch fires; latch the intent too,
      // and PairedTerminal grabs focus once ready.
      setRightPanelView(target);
      if (target === "paired") setPendingTerminalFocus("paired");
      requestAnimationFrame(() => dispatchFocusTerminal(target));
      return;
    }

    if (target === "paired") {
      // The paired shell only mounts when a terminal tab is the active tab of
      // its group. Prefer a terminal that is already active (and thus mounted)
      // over the first one, so multi-group layouts focus the live terminal
      // instead of switching another group's tab.
      const terminalTabs = (["right", "bottom"] as DockLocation[])
        .flatMap((d) => dockTabs(paneLayout, d))
        .filter(isTerminalTabId);
      const termTab = terminalTabs.find((id) => isActiveTab(paneLayout, id)) ?? terminalTabs[0] ?? terminalTabId(0);
      const termDock = dockOf(paneLayout, termTab);
      if (termDock && isActiveTab(paneLayout, termTab)) {
        // Already the active tab (mounted): move focus synchronously so rapid
        // agent<->paired toggles stay deterministic.
        dispatchFocusTerminal("paired");
        return;
      }
      // Not mounted yet: latch the intent and activate/open its tab; the paired
      // panel grabs focus once its PTY is ready.
      setPendingTerminalFocus("paired");
      if (termDock) activateTab(termDock, termTab);
      else openTab(termTab, "right");
      return;
    }
    if (target === "agent" && selectedFilePath) {
      // Agent terminal is hidden under the diff viewer; close the diff first
      // so the wrapper un-hides, then dispatch on the next frame because
      // focus() on a display:none element is a no-op.
      setSelectedFile(null);
      requestAnimationFrame(() => dispatchFocusTerminal("agent"));
      return;
    }
    dispatchFocusTerminal(target);
  }, [activeSessionId, singlePane, paneLayout, openTab, activateTab, selectedFilePath]);

  useKeyboardShortcuts(
    useCallback(
      () => ({
        onNew: handleNewSession,
        onNewScratch: handleNewScratch,
        onDiff: () => toggleDiff(),
        // Escape closes local UI surfaces only (dialogs, palette,
        // wizard, settings, help, file viewer). Never wire this to
        // acp.cancelPrompt; Claude Code CLI does that and stray
        // Escape presses kill in-flight turns the user didn't mean to
        // abort. Cancel/stop must stay behind an explicit gesture
        // (the assistant-ui Stop button in the composer).
        onEscape: () => {
          if (deletingWorkspaceId) {
            setDeletingWorkspaceId(null);
            return;
          }
          if (stoppingWorkspaceId) {
            setStoppingWorkspaceId(null);
            return;
          }
          if (showPalette) {
            setShowPalette(false);
            setPaletteQuery("");
            return;
          }
          setShowSessionWizard(false);
          setShowHelp(false);
          if (showSettings) handleCloseSettings();
          setShowAbout(false);
          setSelectedFile(null);
        },
        onHelp: () => setShowHelp((h) => !h),
        onSettings: () => (showSettings ? handleCloseSettings() : navigate("/settings")),
        onPalette: () => {
          setPaletteQuery("");
          setShowPalette((p) => !p);
        },
        onToggleSidebar: () => setSidebarOpen((o) => !o),
        onToggleRightPanel: () => toggleRightDock(),
        onToggleTerminalFocus: handleToggleTerminalFocus,
      }),
      [
        toggleDiff,
        toggleRightDock,
        showPalette,
        deletingWorkspaceId,
        stoppingWorkspaceId,
        showSettings,
        handleCloseSettings,
        navigate,
        handleToggleTerminalFocus,
        handleNewSession,
        handleNewScratch,
      ],
    ),
  );

  // Palette triage toggles for the active session. "snooze" needs a duration,
  // so it opens the shared modal; the rest are argless server toggles that
  // apply the returned snapshot immediately (re-bucketing without waiting for
  // the poll) and surface a toast on failure.
  const handleSessionStateAction = useCallback(
    async (id: string, action: SessionStateAction) => {
      if (action === "snooze") {
        setSnoozeTargetId(id);
        return;
      }
      const run: Record<Exclude<SessionStateAction, "snooze">, () => Promise<SessionResponse | null>> = {
        pin: () => setSessionPin(id, true),
        unpin: () => setSessionPin(id, false),
        archive: () => setSessionArchive(id, true),
        unarchive: () => setSessionArchive(id, false),
        unsnooze: () => setSessionSnooze(id, null),
        trash: () => trashSession(id),
        untrash: () => restoreSession(id),
      };
      const result = await run[action]();
      if (result) applySession(result);
      else reportError(`Failed to ${action} session`);
    },
    [applySession],
  );

  const commandActions = useCommandActions({
    sessions,
    activeSessionId,
    activeSession: activeSession ?? null,
    loginRequired,
    hasActiveSession: !!activeSession,
    readOnly: !!serverAbout?.read_only,
    onNewSession: handleNewSession,
    onNewScratch: handleNewScratch,
    onSelectSession: handleSelectSession,
    onSessionStateAction: handleSessionStateAction,
    onToggleDiff: toggleDiff,
    onOpenSettings: handleOpenSettings,
    onOpenHelp: handleOpenHelp,
    onOpenAbout: handleOpenAbout,
    onGoDashboard: handleGoDashboard,
    onToggleSidebar: handleToggleSidebar,
    onLogout,
  });

  const openSettingsTab = useCallback((tab: string) => navigate(`/settings/${tab}`), [navigate]);
  const settingsCommands = useSettingsCommands({
    open: showPalette,
    readOnly: !!serverAbout?.read_only,
    onOpenSettingsTab: openSettingsTab,
  });
  const pluginCommandActions = usePluginCommands(pluginUiEntries, activeSessionId);

  // Conversation-content search for the palette (#2515). paletteQuery is
  // declared above (near showPalette) so the keyboard handlers can clear it
  // on close/toggle; consumed here.
  const { results: conversationHits, loading: conversationSearching } = useConversationSearch(paletteQuery);
  const conversationActions = useMemo(
    () =>
      buildConversationActions(conversationHits, sessions, activeSessionId).map(({ sessionId, ...rest }) => ({
        ...rest,
        perform: () => handleSelectSession(sessionId),
      })),
    [conversationHits, sessions, activeSessionId, handleSelectSession],
  );

  const renderContent = () => {
    if (showSettings) {
      return (
        <SettingsView
          tab={settingsTab}
          onClose={handleCloseSettings}
          onSelectTab={(t) => {
            const p = searchParams.get("profile");
            navigate(`/settings/${t}${p ? `?profile=${encodeURIComponent(p)}` : ""}`);
          }}
          onServerAboutRefresh={refreshServerAbout}
          onSettingsRefresh={onSettingsRefresh}
          profile={searchParams.get("profile")}
          onSelectProfile={(p) => {
            const next = new URLSearchParams(searchParams);
            next.set("profile", p);
            setSearchParams(next, { replace: true });
          }}
          readOnly={serverAbout?.read_only}
        />
      );
    }

    // Refresh on `/session/<id>` paints once with `sessions === []` before
    // the first poll resolves. Without this guard the lookup misses, the
    // dashboard fallback renders, and the acp/terminal view only
    // reappears once the fetch lands. Hold the minimal pre-auth shell
    // until the first fetch settles, then let the real fallback decide.
    // See #1351.
    if (activeSessionId && !sessionsLoaded) {
      return <div className="h-dvh bg-surface-900 safe-area-inset" />;
    }

    if (!activeWorkspace || !activeSession) {
      return (
        <Dashboard
          sessions={sessions}
          onSelectSession={handleSelectSession}
          onNewSession={handleNewSession}
          onCloneFromUrl={handleCloneFromUrl}
          onToggleSidebar={handleToggleSidebar}
          readOnly={serverAbout?.read_only}
        />
      );
    }

    // Below the md breakpoint there is no room for the side-by-side split.
    // Render one full-viewport pane and let the picker choose which view
    // occupies it (#1452). The agent terminal (and the paired shell, once
    // first opened) stay mounted but hidden so their PTY, scrollback, and
    // focus survive view switches; the diff view has no xterm so it mounts
    // on demand. Inactive layers use visibility, never display:none, which
    // would collapse xterm's measured geometry to zero. The desktop branch
    // below is left exactly as it was; only this mobile branch is new.
    if (singlePane) {
      return (
        <MobileMainPane
          view={rightPanelView}
          pluginPanes={pluginPanes}
          onBackToAgent={() => setRightPanelView("agent")}
          pairedMounted={pairedMounted}
          activeSession={activeSession ?? null}
          activeSessionId={activeSessionId}
          sessions={sessions}
          webSettings={webSettings}
          selectedFilePath={selectedFilePath}
          selectedRepoName={selectedRepoName}
          selectedFileLine={selectedFileLine}
          revision={revision}
          diffFiles={diffFiles}
          perRepoBases={perRepoBases}
          warning={warning}
          diffFilesLoading={diffFilesLoading}
          onSelectFile={handleSelectFile}
          onOpenFileRef={handleOpenFileRef}
          onCloseFile={handleCloseFile}
          onDiffRefresh={refreshDiffFiles}
          commentsEnabled={commentsEnabled}
          commentSendEnabled={commentSendEnabled}
          commentSendDisabledReason={commentSendDisabledReason}
          diffComments={diffComments}
          commentsIsMultiRepo={commentsIsMultiRepo}
          sendDialogOpen={sendDialogOpen}
          onOpenSendDialog={() => setSendDialogOpen(true)}
          onCloseSendDialog={() => setSendDialogOpen(false)}
          onClearSelectedFile={() => setSelectedFile(null)}
        />
      );
    }

    // Render a pane body by id. Passed to the docks as a callback (rather than
    // building an array of {icon, body} objects here) so the per-session JSX is
    // constructed inside the dock, not threaded through a prop object.
    const renderPaneBody = (id: string): ReactNode => {
      const plugin = pluginPaneById.get(id);
      if (plugin) return <PluginPaneBody entry={plugin.entry} />;
      if (id === "agents") {
        return <BackgroundAgentsPanel sessionId={activeSessionId} />;
      }
      if (id === "diff") {
        return (
          <DiffPane
            session={activeSession ?? null}
            sessionId={activeSessionId}
            files={diffFiles}
            perRepoBases={perRepoBases}
            warning={warning}
            filesLoading={diffFilesLoading}
            selectedFilePath={selectedFilePath}
            selectedRepoName={selectedRepoName}
            onSelectFile={handleSelectFile}
            onDiffRefresh={refreshDiffFiles}
            commentsEnabled={commentsEnabled}
            commentsCount={diffComments.count}
            commentsSendEnabled={commentSendEnabled}
            commentsSendDisabledReason={commentSendDisabledReason}
            onOpenSendDialog={() => setSendDialogOpen(true)}
            onDiscardAllComments={diffComments.clearComments}
          />
        );
      }
      return (
        <PairedShellPane
          session={activeSession ?? null}
          sessionId={activeSessionId}
          terminalIndex={isTerminalTabId(id) ? terminalIndexOf(id) : 0}
        />
      );
    };
    return (
      <div className="flex-1 flex flex-col min-h-0">
        <PaneDndController groupsByDock={groupsByDock} descriptorFor={paneDescriptor} onPlaceTab={placeVisibleTab}>
          <ContentSplit
            collapsed={rightDockCollapsed}
            onToggleCollapse={toggleRightDock}
            left={
              <div className="flex-1 flex flex-col min-h-0 overflow-hidden relative">
                <div className={selectedFilePath ? "hidden" : "flex-1 flex flex-col min-h-0 overflow-hidden"}>
                  {activeSession?.view === "structured" ? (
                    <Suspense fallback={<AcpLoadingFallback />}>
                      <StructuredView
                        key={activeSessionId}
                        sessionId={activeSessionId!}
                        acpWorkerState={activeSession.acp_worker_state ?? "absent"}
                        tool={activeSession.tool}
                        acpAgent={activeSession.acp_agent ?? null}
                        archivedAt={activeSession.archived_at ?? null}
                        snoozedUntil={activeSession.snoozed_until ?? null}
                        trashedAt={activeSession.trashed_at ?? null}
                        onRestore={
                          activeSession.trashed_at
                            ? () => handleRestoreSession(trashedWorkspaceRestoreIds(workspaces, activeSessionId!))
                            : undefined
                        }
                        onOpenFileRef={handleOpenFileRef}
                        fileRefSession={activeSession}
                        onOpenAgentsPane={openAgentsPane}
                        isSandboxed={activeSession.is_sandboxed}
                      />
                    </Suspense>
                  ) : (
                    <TerminalSessionStack
                      activeSessionId={activeSessionId!}
                      sessions={sessions.filter((session) => session.view !== "structured")}
                      persistent={webSettings.persistentTerminals}
                      maxPersistentTerminals={webSettings.maxPersistentTerminals}
                    />
                  )}
                </div>

                {selectedFilePath && activeSessionId && (
                  <DiffFileViewer
                    sessionId={activeSessionId}
                    filePath={selectedFilePath}
                    repoName={selectedRepoName}
                    targetLine={selectedFileLine}
                    revision={revision}
                    onClose={handleCloseFile}
                    commentsEnabled={commentsEnabled}
                    commentsStore={diffComments}
                  />
                )}
              </div>
            }
            right={
              <div {...tourAnchor(TOUR_ANCHORS.rightPanel)} className="flex min-h-0 min-w-0 flex-1">
                <DockGroups
                  location="right"
                  groups={rightGroups}
                  descriptorFor={paneDescriptor}
                  renderBody={renderPaneBody}
                  onActivate={(id) => activateTab("right", id)}
                  onMove={movePaneAny}
                  onClose={closePaneAny}
                  onNewTerminal={serverAbout?.read_only ? undefined : () => addTerminal("right")}
                />
              </div>
            }
          />
          {bottomGroups.length > 0 && (
            <BottomDock
              groups={bottomGroups}
              descriptorFor={paneDescriptor}
              renderBody={renderPaneBody}
              onActivate={(id) => activateTab("bottom", id)}
              onMove={movePaneAny}
              onClose={closePaneAny}
              onNewTerminal={serverAbout?.read_only ? undefined : () => addTerminal("bottom")}
            />
          )}
        </PaneDndController>
        {sendDialogOpen && commentsEnabled && activeSessionId && (
          <SendCommentsDialog
            sessionId={activeSessionId}
            comments={diffComments.comments}
            isMultiRepo={commentsIsMultiRepo}
            sendEnabled={commentSendEnabled}
            sendDisabledReason={commentSendDisabledReason}
            introDraft={diffComments.introDraft}
            outroDraft={diffComments.outroDraft}
            clearAfterSend={diffComments.clearAfterSend}
            onChangeIntro={diffComments.setIntroDraft}
            onChangeOutro={diffComments.setOutroDraft}
            onChangeClearAfterSend={diffComments.setClearAfterSend}
            onClose={() => setSendDialogOpen(false)}
            onSent={() => {
              if (diffComments.clearAfterSend) {
                diffComments.clearComments();
                diffComments.setIntroDraft("");
                diffComments.setOutroDraft("");
              }
              setSendDialogOpen(false);
              // Close the diff viewer so the acp transcript is in
              // view: the user just dispatched feedback and wants to
              // see the agent's response. They can re-open any file
              // from the right-panel list afterwards.
              setSelectedFile(null);
              toastBus.handler?.info("Comments sent to agent");
            }}
          />
        )}
      </div>
    );
  };

  // No root-height pin remains: every mobile terminal surface (agent,
  // paired host, paired container) is the capture-snapshot live view
  // now, with no PTY to protect from keyboard-driven layout shrink. The
  // natural `100dvh` shrink keeps bottom-anchored UI above the keyboard
  // everywhere (#1177, #1452 are fully superseded).

  const acpPrefs = useMemo(
    () => ({
      showToolDurations: serverAbout?.acp_show_tool_durations ?? true,
      replayEvents: serverAbout?.acp_replay_events ?? 0,
    }),
    [serverAbout?.acp_show_tool_durations, serverAbout?.acp_replay_events],
  );

  const tourScope: TourScope =
    !activeWorkspace || !activeSession
      ? "dashboard"
      : activeSession.view === "structured"
        ? "structured-view"
        : "session";
  // First-run tour "seen" state, sourced from the backend (app_state) so it
  // follows the user across browsers and devices. `tourSeenKnown` stays false
  // until settings resolve, so the tour never flashes on a `false` default
  // while the request is in flight (and never auto-launches when the fetch
  // fails). Fetched here in AppContent (post-auth) so the request runs as the
  // authenticated user. `LEGACY_TOUR_SEEN_KEY` is the pre-#1832 per-browser
  // flag, read once to migrate existing users so they are not re-shown the tour.
  const [tourSeen, setTourSeen] = useState(false);
  const [tourSeenKnown, setTourSeenKnown] = useState(false);

  useEffect(() => {
    fetchSettings().then((settings) => {
      // Fetch failed: leave the seen state unknown so the tour does not
      // auto-launch over an error/recovery screen. The menu trigger still works.
      if (!settings) return;
      const backendSeen = settings.app_state?.has_seen_web_tour === true;
      const legacySeen = safeGetItem(LEGACY_TOUR_SEEN_KEY) === "1";
      // Treat the legacy local flag as a suppression hint while the migration
      // POST is in flight, so the tour cannot flash before the backend agrees.
      const seenAtLoad = backendSeen || legacySeen;
      setTourSeen(seenAtLoad);
      setTourSeenKnown(true);
      // Capture whether onboarding was already done at load so completing the
      // tour this session does not then pop the tip-of-the-day on top of it.
      tourSeenAtLoadRef.current = seenAtLoad;
      if (legacySeen && !backendSeen) {
        void markWebTourSeen().then((ok) => {
          if (ok) safeRemoveItem(LEGACY_TOUR_SEEN_KEY);
        });
      }
    });
  }, []);

  // Persist the seen flag when the user finishes or skips the tour. Optimistic:
  // flip local state immediately so a failed POST (e.g. read-only 403) cannot
  // re-auto-launch the tour for the rest of this page's lifetime.
  const handleTourSeen = useCallback(() => {
    setTourSeen(true);
    void markWebTourSeen();
  }, []);

  // Only auto-launch on a settled, unobstructed dashboard. Any open overlay or
  // an in-flight session route defers it (the flag stays unset until then).
  const tourAutoLaunchReady =
    serverAboutLoaded &&
    sessionsLoaded &&
    !activeSessionId &&
    !showSettings &&
    !showSessionWizard &&
    !showHelp &&
    !showAbout &&
    !showPalette &&
    !projectForm;
  // First-run theme choice is phase one of onboarding. It decides on the same
  // settled-dashboard gate as the tour, then the tour follows once the modal
  // resolves so the two never overlap on first load.
  const welcome = useWelcomePhase({
    scope: tourScope,
    readOnly: !!serverAbout?.read_only,
    autoLaunchReady: tourAutoLaunchReady,
    tourSeen,
    tourSeenKnown,
  });
  const tour = useTour({
    scope: tourScope,
    readOnly: !!serverAbout?.read_only,
    isDesktop: !isCoarse,
    autoLaunchReady: tourAutoLaunchReady && welcome.resolved,
    seen: tourSeen,
    seenKnown: tourSeenKnown,
    onSeen: handleTourSeen,
    onNavigate: (tab) => (tab ? navigate(`/settings/${tab}`) : handleCloseSettings()),
  });

  // Auto-pop the tip-of-the-day once per load, after onboarding settles, like
  // GIMP/DBeaver. Gated like the tour: only on a settled dashboard, only when a
  // tip is unseen and tips are enabled, never while the welcome/telemetry/tour
  // flows are up, and never in an automated browser session (so the modal can't
  // intercept the rest of the Playwright suite). Only for users who already
  // finished onboarding before this load: first-run users get the welcome and
  // tour, not a tips modal piled on top. Reopen any time from the menu.
  useEffect(() => {
    if (tipsAutoPoppedRef.current) return;
    const gate = shouldAutoPopTips({
      loaded: tips.loaded,
      hasUnseen: tips.hasUnseen,
      tourSeenAtLoad: tourSeenAtLoadRef.current,
      onboardingReady: tourAutoLaunchReady && welcome.resolved,
      // Treat "not resolved yet" as pending so tips can't pop ahead of a consent
      // modal that the in-flight status fetch is about to raise.
      telemetryPending: !telemetryConsentKnown || telemetryConsentNeeded,
      tourActive: tour.isTourActive,
      automated: isAutomatedSession(),
    });
    if (!gate) return;
    tipsAutoPoppedRef.current = true;
    // Defer one frame so the open happens off the effect body (mirrors the
    // tour's begin()), keeping the state change out of the effect.
    const id = requestAnimationFrame(() => tips.open());
    return () => cancelAnimationFrame(id);
  }, [
    tips,
    tourSeenKnown,
    tourAutoLaunchReady,
    welcome.resolved,
    telemetryConsentKnown,
    telemetryConsentNeeded,
    tour.isTourActive,
  ]);

  return (
    <AcpPrefsProvider value={acpPrefs}>
      <div className="h-dvh flex flex-col bg-surface-900 text-text-primary overflow-hidden safe-area-inset">
        <TopBar
          activeWorkspace={activeWorkspace}
          activeSession={activeSession ?? null}
          onToggleSidebar={handleToggleSidebar}
          onOpenPalette={() => setShowPalette(true)}
          onToggleDiff={toggleDiff}
          paneIds={allPaneIds}
          paneDescriptor={paneDescriptor}
          isPaneOpen={isPaneOpen}
          onTogglePane={togglePaneAny}
          onOpenHelp={handleOpenHelp}
          onOpenAbout={handleOpenAbout}
          onStartTutorial={tour.startTour}
          onLogout={onLogout}
          loginRequired={loginRequired}
          isOffline={!!error}
          isDevBuild={isDebugBuild(serverAbout)}
          onOpenTips={tips.open}
          onGoDashboard={handleGoDashboard}
          sidebarColumnVisible={!showSettings && sidebarOpen}
          rightColumnVisible={isMdUp && !showSettings && !!activeWorkspace && !!activeSession && !rightDockCollapsed}
        />

        <DisconnectBanner />
        <UpdateBanner />
        <DashboardUpdateBanner />

        <div className="flex flex-1 min-h-0">
          {!showSettings && (
            <WorkspaceSidebar
              groups={sidebarGroups}
              nestedGroups={nestedGroups}
              trashedWorkspaces={trashedWorkspaces}
              onToggleSubgroup={toggleSubgroupCollapsed}
              onReorderWorkspaces={handleReorderWorkspaces}
              onReorderGroups={reorderRepoGroups}
              activeId={activeWorkspace?.id ?? null}
              open={sidebarOpen}
              onToggle={() => setSidebarOpen(false)}
              onSelect={handleSelectWorkspace}
              onToggleGroup={toggleSidebarGroup}
              onUpdateRepoAppearance={updateRepoAppearance}
              onNew={() => {
                setWizardPrefill(undefined);
                setShowSessionWizard(true);
              }}
              onCreateSession={handleCreateSession}
              onPinProject={handlePinProject}
              onUnpinProject={handleUnpinProject}
              savedProjects={savedProjects}
              onAddProject={handleAddProject}
              onEditProject={handleEditProject}
              onRemoveProject={handleRemoveProject}
              onSettings={handleOpenSettings}
              onDeleteSession={handleDeleteSession}
              onRestoreSession={handleRestoreSession}
              onStopSession={handleStopSession}
              onStartSession={handleStartSession}
              readOnly={serverAbout?.read_only}
              sortMode={sidebarSortMode}
              onSortModeChange={selectSidebarSortMode}
              pluginSortRef={pluginSortRef}
              onPluginSortChange={setPluginSortRef}
              axis={sidebarAxis}
              onAxisChange={setSidebarAxis}
            />
          )}

          <div className="flex-1 flex flex-col min-h-0 min-w-0">{renderContent()}</div>
        </div>

        {showSessionWizard && (
          <SessionWizard
            onClose={() => {
              setShowSessionWizard(false);
              setWizardPrefill(undefined);
            }}
            onCreated={(session?: SessionResponse) => {
              if (session) {
                injectSession(session);
                navigate(`/session/${encodeURIComponent(session.id)}`);
                if (window.innerWidth < 768) setSidebarOpen(false);
              }
              setShowSessionWizard(false);
              setWizardPrefill(undefined);
            }}
            prefill={wizardPrefill}
          />
        )}

        {projectForm && (
          <ProjectFormModal
            initial={projectForm.editProject}
            onClose={() => setProjectForm(null)}
            onSaved={() => refreshProjects()}
          />
        )}

        {welcome.showWelcome && <ThemeIntro onDone={welcome.dismissWelcome} />}

        {tour.tourElement}

        {showHelp && <HelpOverlay onClose={() => setShowHelp(false)} />}

        {tips.isOpen && (
          <TipsModal
            tips={tips.tips}
            startIndex={tips.startIndex}
            enabled={tips.enabled}
            onMarkSeen={tips.markSeen}
            onSetEnabled={tips.setEnabled}
            onClose={tips.close}
          />
        )}

        {showAbout && <AboutModal onClose={() => setShowAbout(false)} sessionId={activeSessionId} />}
        {telemetryConsentNeeded && <TelemetryConsentModal onChoose={handleTelemetryConsent} />}

        {deletingSession && deletingCleanupDefaults && (
          <DeleteSessionDialog
            sessionTitle={deletingSession.title}
            branchName={deletingBranchName}
            hasManagedWorktree={deletingSessions.some((session) => session.has_cleanable_worktree ?? false)}
            isSandboxed={deletingSessions.some((session) => session.is_sandboxed)}
            isScratch={deletingSessions.some((session) => session.scratch)}
            cleanupDefaults={deletingCleanupDefaults}
            defaultToTrash={deletingDefaultToTrash}
            affectedSessions={deletingSessions.map((session) => ({
              id: session.id,
              title: session.title,
              isSandboxed: session.is_sandboxed,
            }))}
            onConfirm={handleConfirmDelete}
            onTrash={handleConfirmTrash}
            onCancel={() => setDeletingWorkspaceId(null)}
          />
        )}

        {stoppingSession && (
          <StopSessionDialog
            sessionTitle={stoppingSession.title}
            onConfirm={handleConfirmStop}
            onCancel={() => setStoppingWorkspaceId(null)}
          />
        )}

        <CommandPalette
          open={showPalette}
          onClose={() => {
            setShowPalette(false);
            setPaletteQuery("");
          }}
          actions={[...commandActions, ...conversationActions, ...settingsCommands, ...pluginCommandActions]}
          onSearchChange={setPaletteQuery}
          searching={conversationSearching}
        />

        {snoozeTargetId && (
          <SnoozeModal
            title="Snooze session"
            onCancel={() => setSnoozeTargetId(null)}
            onPick={(minutes) => {
              const id = snoozeTargetId;
              setSnoozeTargetId(null);
              void setSessionSnooze(id, minutes).then((result) => {
                if (result) applySession(result);
                else reportError("Failed to snooze session");
              });
            }}
          />
        )}

        {activeWorkspace && activeSession && (
          <MobileRightPanelPicker
            open={pickerOpen && singlePane}
            active={rightPanelView}
            pluginPanes={pluginPanes}
            onSelect={handlePickView}
            onClose={() => setPickerOpen(false)}
          />
        )}

        <textarea
          ref={keyboardProxyRef}
          aria-hidden="true"
          tabIndex={-1}
          className="fixed opacity-0 w-0 h-0 pointer-events-none"
          style={{ top: -9999, left: -9999 }}
        />
      </div>
    </AcpPrefsProvider>
  );
}

function AcpLoadingFallback() {
  return (
    <div className="flex h-full items-center justify-center bg-surface-900 text-text-dim">
      <div className="text-xs font-mono uppercase tracking-wide">Loading acp…</div>
    </div>
  );
}
