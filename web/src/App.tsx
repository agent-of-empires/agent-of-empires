import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useMatch, useNavigate, useSearchParams } from "react-router-dom";
import { IDLE_DECAY_WINDOW_MS, isSessionActive } from "./lib/session";
import { useSessions } from "./hooks/useSessions";
import { clearCockpitCache } from "./hooks/useCockpit";
import { clearDraft, sweepOrphanDrafts } from "./lib/cockpitDrafts";
import { CockpitPrefsProvider } from "./lib/cockpitPrefs";
import { safeGetItem, safeSetItem } from "./lib/safeStorage";
import { useWorkspaces } from "./hooks/useWorkspaces";
import { useRepoGroups } from "./hooks/useRepoGroups";
import { useSessionGroups } from "./hooks/useSessionGroups";
import { useSidebarSortMode } from "./hooks/useSidebarSortMode";
import { useSidebarAxis } from "./hooks/useSidebarAxis";
import { repoGroupToSidebarGroup } from "./lib/sidebarGroups";
import { useKeyboardShortcuts } from "./hooks/useKeyboardShortcuts";
import { useResolvedTheme } from "./hooks/useResolvedTheme";
import { useWebSettings } from "./hooks/useWebSettings";
import { useDiffFiles } from "./hooks/useDiffFiles";
import { useDiffComments } from "./hooks/useDiffComments";
import { SendCommentsDialog } from "./components/diff/comments/SendCommentsDialog";
import { useCommandActions } from "./hooks/useCommandActions";
import { useEdgeSwipe } from "./hooks/useEdgeSwipe";
import { useMobileKeyboard } from "./hooks/useMobileKeyboard";
import { useIsCoarsePointer } from "./hooks/useIsCoarsePointer";
import { useIsWideViewport } from "./hooks/useIsWideViewport";
import type { RightPanelView } from "./lib/rightPanelView";
import {
  loginStatus,
  logout,
  deleteSession,
  fetchAbout,
  fetchSettings,
  fetchTelemetryStatus,
  setTelemetryConsent,
  reportTelemetrySeen,
  isDebugBuild,
  updateWorkspaceOrdering,
} from "./lib/api";
import type { DeleteSessionOptions, ServerAbout } from "./lib/api";
import {
  IdleDecayWindowContext,
  parseIdleDecayWindowMs,
  useIdleDecayWindowMs,
} from "./lib/idleDecay";
import { toastBus } from "./lib/toastBus";
import { OPEN_SESSION_EVENT } from "./lib/sessionRoute";
import {
  dispatchFocusTerminal,
  requestSessionInputFocus,
  setPendingTerminalFocus,
} from "./lib/terminalFocus";
import { WorkspaceSidebar } from "./components/WorkspaceSidebar";
import { DeleteSessionDialog } from "./components/DeleteSessionDialog";
import { TopBar } from "./components/TopBar";
import { ContentSplit } from "./components/ContentSplit";
import { TerminalSessionStack } from "./components/TerminalSessionStack";
// Lazy-load the cockpit surface so non-cockpit users never download
// the @assistant-ui/react, shiki, and in-house StringDiff/DiffLine
// dependency tree. Cuts ~hundreds of KB off the cold-start bundle
// for the (currently default) tmux-only flow. The Suspense fallback
// below covers the brief load while the chunk arrives.
const CockpitView = lazy(() =>
  import("./components/cockpit/CockpitView").then((m) => ({
    default: m.CockpitView,
  })),
);
import { RightPanel } from "./components/RightPanel";
import { MobileRightPanelPicker } from "./components/MobileRightPanelPicker";
import { MobileMainPane } from "./components/MobileMainPane";
import { DiffFileViewer } from "./components/diff/DiffFileViewer";
import { SettingsView } from "./components/SettingsView";
import { ProjectsView } from "./components/ProjectsView";
import { ProfilesPage } from "./components/profiles/ProfilesPage";
import { HelpOverlay } from "./components/HelpOverlay";
import { useTour } from "./hooks/useTour";
import type { TourScope } from "./lib/tourSteps";
import { SessionWizard } from "./components/session-wizard/SessionWizard";
import type { WizardPrefill } from "./components/session-wizard/SessionWizard";
import type { SessionResponse } from "./lib/types";
import { Dashboard } from "./components/Dashboard";
import { LoginPage } from "./components/LoginPage";
import { TokenEntryPage } from "./components/TokenEntryPage";
import {
  LOGIN_REQUIRED_EVENT,
  TOKEN_EXPIRED_EVENT,
  resetTokenExpired,
} from "./lib/fetchInterceptor";
import { AboutModal } from "./components/AboutModal";
import { TelemetryConsentModal } from "./components/TelemetryConsentModal";
import { CommandPalette } from "./components/command-palette/CommandPalette";
import { DisconnectBanner } from "./components/DisconnectBanner";
import { ElevationPrompt } from "./components/ElevationPrompt";
import { UpdateBanner } from "./components/UpdateBanner";

const RIGHT_PANEL_COLLAPSED_KEY = "aoe-right-collapsed";

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
    return () =>
      window.removeEventListener(LOGIN_REQUIRED_EVENT, onLoginRequired);
  }, []);

  useEffect(() => {
    loginStatus().then(({ required, authenticated }) => {
      setLoginRequired(required);
      setLoginAuthenticated(authenticated);
    });
  }, []);

  useEffect(() => {
    fetchSettings().then((settings) => {
      setIdleDecayWindowMs(parseIdleDecayWindowMs(settings));
    });
  }, []);

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
      <AppContent loginRequired={loginRequired} onLogout={handleLogout} />
      <ElevationPrompt />
    </IdleDecayWindowContext.Provider>
  );
}

/** Walk from the event target up to the document root looking for any
 *  text-input surface, so global hotkeys don't fire when the user is
 *  typing in an `<input>`, `<textarea>`, or contenteditable element
 *  (or any contenteditable ancestor of a deeper rich-text widget). */
function isInsideEditable(target: EventTarget | null): boolean {
  let el: HTMLElement | null =
    target instanceof HTMLElement ? target : null;
  while (el) {
    const tag = el.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || el.isContentEditable) {
      return true;
    }
    el = el.parentElement;
  }
  return false;
}

function AppContent({ loginRequired, onLogout }: { loginRequired: boolean; onLogout: () => void }) {
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const { settings: webSettings } = useWebSettings();
  const sessionMatch = useMatch("/session/:sessionId");
  const settingsRootMatch = useMatch("/settings");
  const settingsTabMatch = useMatch("/settings/:tab");
  const projectsMatch = useMatch("/projects");
  const profilesMatch = useMatch("/profiles");
  const activeSessionId = sessionMatch?.params.sessionId ?? null;
  const showSettings = settingsRootMatch !== null || settingsTabMatch !== null;
  const showProjects = projectsMatch !== null;
  const showProfiles = profilesMatch !== null;
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
  } = useSessions();
  const workspaces = useWorkspaces(sessions);

  // One-shot orphan-draft sweep once useSessions has settled its first
  // fetch (success or null). Catches cockpit:draft:<id> keys left behind
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

  const [sidebarSortMode, setSidebarSortMode] = useSidebarSortMode();
  const [sidebarAxis, setSidebarAxis] = useSidebarAxis();

  const {
    groups: repoGroups,
    toggleRepoCollapsed,
    updateRepoAppearance,
    reorderRepoGroups,
  } = useRepoGroups(workspaces, workspaceOrdering, sidebarSortMode);
  const { groups: sessionGroups, toggleGroupCollapsed } =
    useSessionGroups(workspaces);

  // The sidebar render path consumes one honest model (SidebarGroup): the
  // repo axis maps in via an adapter, the user-group axis is already in
  // that shape. Collapse routing follows the active axis so the two
  // axes keep independent collapse state. See #1234.
  const sidebarGroups = useMemo(
    () =>
      sidebarAxis === "group"
        ? sessionGroups
        : repoGroups.map(repoGroupToSidebarGroup),
    [sidebarAxis, sessionGroups, repoGroups],
  );
  const toggleSidebarGroup =
    sidebarAxis === "group" ? toggleGroupCollapsed : toggleRepoCollapsed;

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
  } | null>(null);
  const selectedFilePath = selectedFile?.path ?? null;
  const selectedRepoName = selectedFile?.repoName;
  const [diffCollapsed, setDiffCollapsed] = useState(() => {
    const stored = safeGetItem(RIGHT_PANEL_COLLAPSED_KEY);
    if (stored === "1") return true;
    if (stored === "0") return false;
    return window.innerWidth < 768;
  });
  useEffect(() => {
    safeSetItem(RIGHT_PANEL_COLLAPSED_KEY, diffCollapsed ? "1" : "0");
  }, [diffCollapsed]);
  // Layout topology is width-driven so it stays aligned with the `md:`
  // Tailwind classes the rest of the layout uses. At md and up the
  // side-by-side ContentSplit renders; below md a single full-viewport
  // pane shows one of agent / diff / paired, chosen via the picker (#1452).
  const isMdUp = useIsWideViewport();
  const singlePane = !isMdUp;
  const [rightPanelView, setRightPanelView] =
    useState<RightPanelView>("agent");
  const [pickerOpen, setPickerOpen] = useState(false);
  // The paired shell mounts lazily on first activation, then stays mounted
  // (kept alive but hidden) so its PTY, scrollback, and focus survive view
  // switches. Mounting it eagerly would spawn a shell for every mobile
  // session the user never opens the shell on.
  const [pairedMounted, setPairedMounted] = useState(false);
  const [showSessionWizard, setShowSessionWizard] = useState(false);
  const [showHelp, setShowHelp] = useState(false);
  const [showPalette, setShowPalette] = useState(false);
  const [showAbout, setShowAbout] = useState(false);
  const [telemetryConsentNeeded, setTelemetryConsentNeeded] = useState(false);
  const [sidebarOpen, setSidebarOpen] = useState(
    () => window.innerWidth >= 768,
  );
  const keyboardProxyRef = useRef<HTMLTextAreaElement>(null);

  const activeWorkspace = useMemo(() => {
    if (!activeSessionId) return undefined;
    return workspaces.find((w) =>
      w.sessions.some((s) => s.id === activeSessionId),
    );
  }, [workspaces, activeSessionId]);
  const activeSession = activeWorkspace?.sessions.find(
    (s) => s.id === activeSessionId,
  );

  // Fetch the diff when the panel is actually showing: on desktop when the
  // split is expanded, on mobile when the diff view is the active pane.
  const diffPanelActive = isMdUp ? !diffCollapsed : rightPanelView === "diff";
  const {
    files: diffFiles,
    perRepoBases,
    warning,
    loading: diffFilesLoading,
    revision,
    refresh: refreshDiffFiles,
  } = useDiffFiles(activeSessionId, diffPanelActive);

  // Diff-viewer comments (#928). Cockpit-only and session-scoped. The
  // banner lives in RightPanel while the inline UI lives inside
  // DiffFileViewer, so the store is lifted here and threaded to both.
  const diffComments = useDiffComments(activeSessionId);
  const commentsEnabled = !!activeSession?.cockpit_mode;
  const commentSendEnabled =
    commentsEnabled && activeSession?.cockpit_worker_state === "running";
  const commentSendDisabledReason = !commentsEnabled
    ? "Diff comments require a cockpit session"
    : "Cockpit worker is not running";
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

  useEffect(() => {
    if (!activeSessionId) {
      setSelectedFile(null);
      return;
    }
    if (
      selectedFilePath &&
      !diffFilesLoading &&
      !diffFiles.some((f) => f.path === selectedFilePath)
    ) {
      setSelectedFile(null);
    }
  }, [activeSessionId, diffFiles, diffFilesLoading, selectedFilePath]);

  // Reset the mobile single-pane view to the agent terminal whenever the
  // active session changes, and close the picker. Landing a freshly opened
  // session on a stale "paired"/"diff" view would strand the user on a
  // shell or empty file list before the new session's terminal is ready.
  useEffect(() => {
    setRightPanelView("agent");
    setPickerOpen(false);
    setPairedMounted(false);
  }, [activeSessionId]);

  // Mount the paired shell on first activation and keep it mounted after.
  useEffect(() => {
    if (rightPanelView === "paired") setPairedMounted(true);
  }, [rightPanelView]);

  // Refit the newly active terminal after a single-pane view switch: the
  // layers keep their geometry while hidden (visibility, not display:none),
  // but a resize nudge re-runs the xterm fit so the grid matches exactly.
  useEffect(() => {
    if (!singlePane) return;
    const id = requestAnimationFrame(() =>
      window.dispatchEvent(new Event("resize")),
    );
    return () => cancelAnimationFrame(id);
  }, [singlePane, rightPanelView]);

  useEffect(() => {
    setSelectedFile(null);
  }, [activeSessionId]);

  const focusKeyboardProxy = () => {
    if (window.innerWidth < 768 && navigator.maxTouchPoints > 0) {
      keyboardProxyRef.current?.focus();
    }
  };

  // Selecting a session in the sidebar should land focus on its canonical
  // "type here" target so the user can start typing without a second click:
  // the cockpit composer in cockpit mode, the xterm textarea otherwise. See
  // requestSessionInputFocus for the dispatch/latch and coarse-pointer rules.
  const isCoarse = useIsCoarsePointer();
  const focusAgentInput = useCallback(
    (session: SessionResponse | undefined) =>
      requestSessionInputFocus(session, isCoarse),
    [isCoarse],
  );

  const handleSelectSession = useCallback((sessionId: string) => {
    const ws = workspaces.find((w) => w.sessions.some((s) => s.id === sessionId));
    if (ws) {
      navigate(`/session/${encodeURIComponent(sessionId)}`);
      // The proxy is a real textarea; focusing it inside the click gesture
      // would pop the soft keyboard on touch devices, so skip it on coarse
      // pointers (#1178), matching the focusAgentInput suppression.
      if (!isCoarse) focusKeyboardProxy();
      focusAgentInput(ws.sessions.find((s) => s.id === sessionId));
      if (window.innerWidth < 768) setSidebarOpen(false);
    }
  }, [navigate, workspaces, focusAgentInput, isCoarse]);

  const handleSelectWorkspace = (workspaceId: string) => {
    const ws = workspaces.find((w) => w.id === workspaceId);
    if (ws) {
      const running = ws.sessions.find((s) =>
        isSessionActive(s, idleDecayWindowMs),
      );
      const picked = running ?? ws.sessions[0] ?? null;
      if (picked) {
        navigate(`/session/${encodeURIComponent(picked.id)}`);
        focusAgentInput(picked);
      } else {
        navigate("/");
      }
    }
    if (!isCoarse) focusKeyboardProxy();
    if (window.innerWidth < 768) {
      setSidebarOpen(false);
    }
  };

  // In-app toast forwarded from the service worker sets this event when
  // the user taps it; navigate to the session that triggered the push.
  useEffect(() => {
    const onOpen = (e: Event) => {
      const detail = (e as CustomEvent).detail as
        | { sessionId?: string }
        | undefined;
      if (detail?.sessionId) {
        handleSelectSession(detail.sessionId);
      }
    };
    window.addEventListener(OPEN_SESSION_EVENT, onOpen);
    return () => window.removeEventListener(OPEN_SESSION_EVENT, onOpen);
  }, [handleSelectSession]);

  const [wizardPrefill, setWizardPrefill] = useState<WizardPrefill | undefined>(undefined);
  const [deletingWorkspaceId, setDeletingWorkspaceId] = useState<string | null>(null);
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

  useEffect(() => {
    refreshServerAbout();
  }, [refreshServerAbout]);

  // Telemetry: once authenticated and on a writable server, report that the
  // web dashboard was opened (folded into the daemon's next opt-in snapshot)
  // and, if the user has not yet answered the opt-in prompt, show the consent
  // modal. The browser never posts to the telemetry backend; it only talks to
  // the local daemon. Read-only servers can't persist a choice, so skip.
  useEffect(() => {
    // AppContent only renders past the login gate, so reaching here means the
    // session is usable. Read-only servers can't persist a choice, so skip.
    if (!serverAboutLoaded || serverAbout?.read_only) return;
    reportTelemetrySeen("web");
    let active = true;
    void fetchTelemetryStatus().then((status) => {
      if (!active || !status) return;
      if (!status.responded && !status.do_not_track) {
        setTelemetryConsentNeeded(true);
      }
    });
    return () => {
      active = false;
    };
  }, [serverAboutLoaded, serverAbout?.read_only]);

  const handleTelemetryConsent = useCallback((enabled: boolean) => {
    setTelemetryConsentNeeded(false);
    void setTelemetryConsent(enabled);
  }, []);

  const deletingWorkspace = deletingWorkspaceId
    ? workspaces.find((w) => w.id === deletingWorkspaceId)
    : null;
  const deletingSession = deletingWorkspace?.sessions[0] ?? null;

  const handleDeleteSession = useCallback((workspaceId: string) => {
    setDeletingWorkspaceId(workspaceId);
  }, []);

  const handleConfirmDelete = useCallback(async (options: DeleteSessionOptions) => {
    if (!deletingSession) return;
    const sessionId = deletingSession.id;
    const wasActive = sessionId === activeSessionId;

    // Close dialog and show "Deleting" status immediately
    setDeletingWorkspaceId(null);
    setSessionStatus(sessionId, "Deleting");

    if (wasActive) {
      navigate("/");
    }

    const result = await deleteSession(sessionId, options);
    if (!result.ok) {
      // Revert status on failure
      setSessionStatus(sessionId, "Error");
      toastBus.handler?.error(result.error || "Failed to delete session");
      return;
    }

    // Drop the per-session cockpit cache so a recreated session with
    // the same id doesn't briefly show the prior transcript on
    // remount before fetchReplay clears it.
    clearCockpitCache(sessionId);
    // Drop the persisted composer draft for the deleted session so its
    // localStorage key doesn't linger (#1358). Cross-tab / cross-device
    // deletes go through the startup sweep instead.
    clearDraft(sessionId);

    // Server returns `messages` from `perform_deletion` when there's something
    // user-facing to report (e.g. "Scratch directory kept at: <path>" when
    // `keep_scratch` is set). Surface the first one so the kept-path is visible.
    const toast = result.messages?.[0] ?? "Session deleted";
    toastBus.handler?.info(toast);
  }, [deletingSession, activeSessionId, setSessionStatus, navigate]);

  const handleCreateSession = useCallback((repoPath: string) => {
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
      skipToReview: true,
    });
    setShowSessionWizard(true);
  }, [sessions]);

  // The right-panel control toggles the desktop split, but on mobile there
  // is no split to collapse: it opens the view picker instead (#1452).
  const toggleDiff = useCallback(() => {
    if (isMdUp) {
      setDiffCollapsed((c) => !c);
    } else {
      setPickerOpen((o) => !o);
    }
  }, [isMdUp]);

  const handlePickView = useCallback((view: RightPanelView) => {
    setRightPanelView(view);
    setPickerOpen(false);
  }, []);

  const handleSelectFile = useCallback(
    (path: string, repoName?: string) => {
      setSelectedFile({ path, repoName });
    },
    [],
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

  const handleOpenProjects = useCallback(() => {
    navigate("/projects");
    if (window.innerWidth < 768) setSidebarOpen(false);
  }, [navigate]);

  const handleCloseProjects = useCallback(() => {
    if (activeSessionId) {
      navigate(`/session/${encodeURIComponent(activeSessionId)}`);
    } else {
      navigate("/");
    }
  }, [navigate, activeSessionId]);

  const handleOpenProfiles = useCallback(() => {
    navigate("/profiles");
    if (window.innerWidth < 768) setSidebarOpen(false);
  }, [navigate]);

  const handleCloseProfiles = useCallback(() => {
    if (activeSessionId) {
      navigate(`/session/${encodeURIComponent(activeSessionId)}`);
    } else {
      navigate("/");
    }
  }, [navigate, activeSessionId]);

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
      setDiffCollapsed(false);
    } else {
      setPickerOpen(true);
    }
  }, [isMdUp]);
  useEdgeSwipe({
    edge: "left",
    enabled: !sidebarOpen,
    onSwipe: openSidebar,
    blurOnSwipe: true,
  });
  useEdgeSwipe({
    edge: "right",
    enabled: diffCollapsed && !!activeSessionId,
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
    setWizardPrefill({ scratch: true, skipToReview: true });
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
    const pairedPanels = document.querySelectorAll<HTMLElement>(
      '[data-term="paired"]',
    );
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

    if (target === "paired" && diffCollapsed) {
      // Right panel is collapsed; paired terminal is unmounted. Set the
      // pending intent so PairedTerminal grabs focus once it mounts and
      // its PTY is ready, then expand the panel.
      setPendingTerminalFocus("paired");
      setDiffCollapsed(false);
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
  }, [activeSessionId, singlePane, diffCollapsed, selectedFilePath]);

  useKeyboardShortcuts(
    useCallback(
      () => ({
        onNew: handleNewSession,
        onNewScratch: handleNewScratch,
        onDiff: () => toggleDiff(),
        // Escape closes local UI surfaces only (dialogs, palette,
        // wizard, settings, help, file viewer). Never wire this to
        // cockpit.cancelPrompt; Claude Code CLI does that and stray
        // Escape presses kill in-flight turns the user didn't mean to
        // abort. Cancel/stop must stay behind an explicit gesture
        // (the assistant-ui Stop button in the composer).
        onEscape: () => {
          if (deletingWorkspaceId) {
            setDeletingWorkspaceId(null);
            return;
          }
          if (showPalette) {
            setShowPalette(false);
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
        onPalette: () => setShowPalette((p) => !p),
        onToggleSidebar: () => setSidebarOpen((o) => !o),
        onToggleRightPanel: () => toggleDiff(),
        onToggleTerminalFocus: handleToggleTerminalFocus,
      }),
      [
        toggleDiff,
        showPalette,
        deletingWorkspaceId,
        showSettings,
        handleCloseSettings,
        navigate,
        handleToggleTerminalFocus,
        handleNewSession,
        handleNewScratch,
      ],
    ),
  );

  const commandActions = useCommandActions({
    sessions,
    activeSessionId,
    loginRequired,
    hasActiveSession: !!activeSession,
    readOnly: !!serverAbout?.read_only,
    onNewSession: handleNewSession,
    onNewScratch: handleNewScratch,
    onSelectSession: handleSelectSession,
    onToggleDiff: toggleDiff,
    onOpenSettings: handleOpenSettings,
    onOpenHelp: handleOpenHelp,
    onOpenAbout: handleOpenAbout,
    onGoDashboard: handleGoDashboard,
    onToggleSidebar: handleToggleSidebar,
    onLogout,
  });

  const renderContent = () => {
    if (showSettings) {
      return (
        <SettingsView
          tab={settingsTab}
          onClose={handleCloseSettings}
          onSelectTab={(t) => {
            const p = searchParams.get("profile");
            navigate(
              `/settings/${t}${p ? `?profile=${encodeURIComponent(p)}` : ""}`,
            );
          }}
          serverAbout={serverAbout}
          onServerAboutRefresh={refreshServerAbout}
          profile={searchParams.get("profile")}
          onSelectProfile={(p) => {
            const next = new URLSearchParams(searchParams);
            next.set("profile", p);
            setSearchParams(next, { replace: true });
          }}
        />
      );
    }

    if (showProjects) {
      return (
        <ProjectsView
          onClose={handleCloseProjects}
          readOnly={serverAbout?.read_only}
        />
      );
    }

    if (showProfiles) {
      return (
        <ProfilesPage
          onClose={handleCloseProfiles}
          readOnly={serverAbout?.read_only}
        />
      );
    }

    // Refresh on `/session/<id>` paints once with `sessions === []` before
    // the first poll resolves. Without this guard the lookup misses, the
    // dashboard fallback renders, and the cockpit/terminal view only
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
          onBackToAgent={() => setRightPanelView("agent")}
          pairedMounted={pairedMounted}
          activeSession={activeSession ?? null}
          activeSessionId={activeSessionId}
          sessions={sessions}
          serverAbout={serverAbout}
          webSettings={webSettings}
          selectedFilePath={selectedFilePath}
          selectedRepoName={selectedRepoName}
          revision={revision}
          diffFiles={diffFiles}
          perRepoBases={perRepoBases}
          warning={warning}
          diffFilesLoading={diffFilesLoading}
          onSelectFile={handleSelectFile}
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

    return (
      <div className="flex-1 flex flex-col min-h-0">
        <ContentSplit
          collapsed={diffCollapsed}
          onToggleCollapse={toggleDiff}
          left={
            <div className="flex-1 flex flex-col min-h-0 overflow-hidden relative">
              <div
                className={
                  selectedFilePath
                    ? "hidden"
                    : "flex-1 flex flex-col min-h-0 overflow-hidden"
                }
              >
                {activeSession?.cockpit_mode ? (
                  <Suspense fallback={<CockpitLoadingFallback />}>
                    <CockpitView
                      key={activeSessionId}
                      sessionId={activeSessionId!}
                      cockpitWorkerState={activeSession.cockpit_worker_state ?? "absent"}
                      tool={activeSession.tool}
                      archivedAt={activeSession.archived_at ?? null}
                      snoozedUntil={activeSession.snoozed_until ?? null}
                    />
                  </Suspense>
                ) : (
                  <TerminalSessionStack
                    activeSessionId={activeSessionId!}
                    sessions={sessions.filter((session) => !session.cockpit_mode)}
                    cockpitMasterEnabled={
                      !!serverAbout?.cockpit_master_enabled
                    }
                    persistent={webSettings.persistentTerminals}
                    maxPersistentTerminals={
                      webSettings.maxPersistentTerminals
                    }
                  />
                )}
              </div>

              {selectedFilePath && activeSessionId && (
                <DiffFileViewer
                  sessionId={activeSessionId}
                  filePath={selectedFilePath}
                  repoName={selectedRepoName}
                  revision={revision}
                  onClose={handleCloseFile}
                  commentsEnabled={commentsEnabled}
                  commentsStore={diffComments}
                />
              )}
            </div>
          }
          right={
            <RightPanel
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
          }
        />
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
              // Close the diff viewer so the cockpit transcript is in
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

  // Lock the root height to the latched max innerHeight on mobile. Without
  // this, iOS PWA / iOS 26 Safari / Android Chrome shrink innerHeight
  // (and therefore 100dvh) when the soft keyboard opens, which would move
  // the terminal pane by the full keyboard height on its own. Pinning the
  // root to the no-keyboard height makes occlusion padding in TerminalView
  // the single thing that resizes the terminal, so the keyboard behaves the
  // same way on every platform (and not double-shrink on the shrinking ones).
  //
  // Cockpit substrate doesn't host xterm.js, so the SIGWINCH concern
  // doesn't apply; leaving the pin on for cockpit traps the composer
  // below the keyboard on Android Chrome PWA (#1177). Drop the pin when
  // the active session is cockpit so `h-dvh` plus the viewport meta's
  // `interactive-widget=resizes-content` shrink the container with the
  // keyboard and lift the composer back into view.
  //
  // Exception: when the single-pane paired shell is the active mobile view,
  // an xterm.js terminal owns the viewport even on a cockpit session, so it
  // needs the pin (plus the reservation in PairedTerminal) for the same
  // reason the agent terminal does (#1452).
  const { isMobile, stableViewportHeight } = useMobileKeyboard();
  const pairedFullViewport = singlePane && rightPanelView === "paired";
  const pinRootHeight =
    isMobile &&
    stableViewportHeight > 0 &&
    (!activeSession?.cockpit_mode || pairedFullViewport);
  const rootStyle = pinRootHeight
    ? { height: `${stableViewportHeight}px` }
    : undefined;

  const cockpitPrefs = useMemo(
    () => ({
      showToolDurations: serverAbout?.cockpit_show_tool_durations ?? true,
      queueDrainMode: serverAbout?.cockpit_queue_drain_mode ?? "combined",
      forceEndTurnThresholdSecs:
        serverAbout?.cockpit_force_end_turn_threshold_secs ?? 30,
      replayEvents: serverAbout?.cockpit_replay_events ?? 0,
    }),
    [
      serverAbout?.cockpit_show_tool_durations,
      serverAbout?.cockpit_queue_drain_mode,
      serverAbout?.cockpit_force_end_turn_threshold_secs,
      serverAbout?.cockpit_replay_events,
    ],
  );

  const tourScope: TourScope =
    !activeWorkspace || !activeSession
      ? "dashboard"
      : activeSession.cockpit_mode
        ? "cockpit"
        : "session";
  // Only auto-launch on a settled, unobstructed dashboard. Any open overlay or
  // an in-flight session route defers it (the flag stays unset until then).
  const tourAutoLaunchReady =
    serverAboutLoaded &&
    sessionsLoaded &&
    !activeSessionId &&
    !showSettings &&
    !showProjects &&
    !showProfiles &&
    !showSessionWizard &&
    !showHelp &&
    !showAbout &&
    !showPalette;
  const tour = useTour({
    scope: tourScope,
    readOnly: !!serverAbout?.read_only,
    isDesktop: !isCoarse,
    autoLaunchReady: tourAutoLaunchReady,
  });

  return (
    <CockpitPrefsProvider value={cockpitPrefs}>
    <div
      className="h-dvh flex flex-col bg-surface-900 text-text-primary overflow-hidden safe-area-inset"
      style={rootStyle}
    >
      <TopBar
        activeWorkspace={activeWorkspace}
        activeSession={activeSession ?? null}
        onToggleSidebar={handleToggleSidebar}
        onOpenPalette={() => setShowPalette(true)}
        onToggleDiff={toggleDiff}
        diffCollapsed={diffCollapsed}
        onOpenHelp={handleOpenHelp}
        onOpenAbout={handleOpenAbout}
        onStartTutorial={tour.startTour}
        onLogout={onLogout}
        loginRequired={loginRequired}
        isOffline={!!error}
        isDevBuild={isDebugBuild(serverAbout)}
        onGoDashboard={handleGoDashboard}
      />

      <DisconnectBanner />
      <UpdateBanner />

      <div className="flex flex-1 min-h-0">
        {!showSettings && !showProjects && (
          <WorkspaceSidebar
            groups={sidebarGroups}
            onReorderWorkspaces={handleReorderWorkspaces}
            onReorderGroups={reorderRepoGroups}
            activeId={activeWorkspace?.id ?? null}
            open={sidebarOpen}
            onToggle={() => setSidebarOpen(false)}
            onSelect={handleSelectWorkspace}
            onToggleGroup={toggleSidebarGroup}
            onUpdateRepoAppearance={updateRepoAppearance}
            onNew={() => { setWizardPrefill(undefined); setShowSessionWizard(true); }}
            onCreateSession={handleCreateSession}
            onSettings={handleOpenSettings}
            onProjects={handleOpenProjects}
            onProfiles={handleOpenProfiles}
            onDeleteSession={handleDeleteSession}
            readOnly={serverAbout?.read_only}
            sortMode={sidebarSortMode}
            onSortModeChange={setSidebarSortMode}
            axis={sidebarAxis}
            onAxisChange={setSidebarAxis}
          />
        )}

        <div className="flex-1 flex flex-col min-h-0 min-w-0">
          {renderContent()}
        </div>
      </div>

      {showSessionWizard && (
        <SessionWizard
          onClose={() => { setShowSessionWizard(false); setWizardPrefill(undefined); }}
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
          cockpitMasterEnabled={
            !!serverAbout?.cockpit_master_enabled
          }
        />
      )}

      {tour.tourElement}

      {showHelp && <HelpOverlay onClose={() => setShowHelp(false)} />}

      {showAbout && <AboutModal onClose={() => setShowAbout(false)} />}
      {telemetryConsentNeeded && (
        <TelemetryConsentModal onChoose={handleTelemetryConsent} />
      )}

      {deletingSession && (
        <DeleteSessionDialog
          sessionTitle={deletingSession.title}
          branchName={deletingSession.branch}
          hasManagedWorktree={deletingSession.has_managed_worktree}
          isSandboxed={deletingSession.is_sandboxed}
          isScratch={deletingSession.scratch}
          cleanupDefaults={deletingSession.cleanup_defaults}
          onConfirm={handleConfirmDelete}
          onCancel={() => setDeletingWorkspaceId(null)}
        />
      )}

      <CommandPalette
        open={showPalette}
        onClose={() => setShowPalette(false)}
        actions={commandActions}
      />

      {activeWorkspace && activeSession && (
        <MobileRightPanelPicker
          open={pickerOpen && singlePane}
          active={rightPanelView}
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
    </CockpitPrefsProvider>
  );
}

function CockpitLoadingFallback() {
  return (
    <div className="flex h-full items-center justify-center bg-surface-900 text-text-dim">
      <div className="text-xs font-mono uppercase tracking-wide">
        Loading cockpit…
      </div>
    </div>
  );
}
