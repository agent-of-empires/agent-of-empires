import { useMemo } from "react";

const IS_MAC = typeof navigator !== "undefined" && /Mac|iPhone|iPad|iPod/.test(navigator.platform);
import type { SessionResponse } from "../lib/types";
import type { ConversationSearchHit } from "../lib/api";
import type { CommandAction } from "../components/command-palette/types";

export type ConversationActionData = Omit<CommandAction, "perform"> & { sessionId: string };

export function buildConversationActions(
  hits: ConversationSearchHit[],
  sessions: SessionResponse[],
  activeSessionId: string | null,
): ConversationActionData[] {
  return hits.flatMap((hit) => {
    const session = sessions.find((s) => s.id === hit.session_id);
    if (!session || session.id === activeSessionId) return [];
    const state = session.trashed_at
      ? "trashed"
      : session.archived_at
        ? "archived"
        : session.snoozed_until
          ? "snoozed"
          : null;
    const title = session.title || session.branch || "(untitled)";
    const count = hit.match_count > 1 ? ` (${hit.match_count} matches)` : "";
    return [
      {
        id: `conversation:${session.id}`,
        sessionId: session.id,
        title: state ? `${title} · ${state}` : title,
        subtitle: `${hit.snippet}${count}`,
        group: "Conversations" as const,
        status: session.status,
        statusCreatedAt: session.created_at,
      },
    ];
  });
}

export type SessionStateAction =
  | "pin"
  | "unpin"
  | "archive"
  | "unarchive"
  | "snooze"
  | "unsnooze"
  | "trash"
  | "untrash";

interface Args {
  sessions: SessionResponse[];
  activeSessionId: string | null;
  activeSession: SessionResponse | null;
  loginRequired: boolean;
  hasActiveSession: boolean;
  readOnly: boolean;
  onNewSession: () => void;
  onNewScratch: () => void;
  onSelectSession: (sessionId: string) => void;
  onJumpToAttention: () => void;
  hasAttentionSession: boolean;
  onSessionStateAction: (sessionId: string, action: SessionStateAction) => void;
  onToggleDiff: () => void;
  onOpenSettings: () => void;
  onOpenHelp: () => void;
  onOpenAbout: () => void;
  onGoDashboard: () => void;
  onToggleSidebar: () => void;
  onLogout: () => void;
  /** This window is limited to the active session: switching, creation and sidebar commands are left out. */
  focusMode: boolean;
  /** Turns this window into a focus window on the given session. */
  onEnterFocusMode: (sessionId: string) => void;
  /** Opens the given session in a new focus-mode window; absent where a second window cannot open (phones). */
  onOpenFocusWindow?: (sessionId: string) => void;
  onExitFocusMode: () => void;
}

export function useCommandActions({
  sessions,
  activeSessionId,
  activeSession,
  loginRequired,
  hasActiveSession,
  readOnly,
  onNewSession,
  onNewScratch,
  onSelectSession,
  onJumpToAttention,
  hasAttentionSession,
  onSessionStateAction,
  onToggleDiff,
  onOpenSettings,
  onOpenHelp,
  onOpenAbout,
  onGoDashboard,
  onToggleSidebar,
  onLogout,
  focusMode,
  onEnterFocusMode,
  onOpenFocusWindow,
  onExitFocusMode,
}: Args): CommandAction[] {
  return useMemo(() => {
    const actions: CommandAction[] = [];

    if (focusMode) {
      actions.push({
        id: "action:exit-focus-mode",
        title: "Show all Sessions",
        group: "Actions",
        keywords: ["focus", "exit", "sidebar", "sessions", "leave"],
        perform: onExitFocusMode,
      });
    } else if (activeSessionId) {
      actions.push({
        id: "action:enter-focus-mode",
        title: "Show only this Session",
        subtitle: "current session, in this window",
        group: "Actions",
        keywords: ["focus", "single", "hide", "sidebar", "distraction"],
        perform: () => onEnterFocusMode(activeSessionId),
      });
      if (onOpenFocusWindow) {
        actions.push({
          id: "action:open-focus-window",
          title: "Open in its own window",
          subtitle: "current session",
          group: "Actions",
          keywords: ["focus", "pop out", "popout", "window", "single"],
          perform: () => onOpenFocusWindow(activeSessionId),
        });
      }
    }

    if (!readOnly && !focusMode) {
      actions.push({
        id: "action:new-session",
        title: "New session",
        group: "Actions",
        keywords: ["create", "start", "agent", "worktree"],
        shortcut: "n",
        perform: onNewSession,
      });

      actions.push({
        id: "action:new-scratch-session",
        title: "New scratch session",
        group: "Actions",
        keywords: ["scratch", "temp", "temporary", "ephemeral", "throwaway", "create"],
        shortcut: IS_MAC ? "⌘⇧N" : "Ctrl+Shift+N",
        perform: onNewScratch,
      });
    }

    if (!focusMode) {
      actions.push({
        id: "action:go-dashboard",
        title: "Go to dashboard",
        group: "Actions",
        keywords: ["home", "overview"],
        perform: onGoDashboard,
      });
    }

    if (hasAttentionSession && !focusMode) {
      actions.push({
        id: "action:jump-attention",
        title: "Go to next attention session",
        group: "Actions",
        keywords: ["attention", "waiting", "error", "next", "needs", "input", "urgent"],
        shortcut: "a",
        perform: onJumpToAttention,
      });
    }

    if (hasActiveSession) {
      actions.push({
        id: "action:toggle-diff",
        title: "Toggle diff pane",
        group: "Actions",
        keywords: ["changes", "files", "review"],
        shortcut: "D",
        perform: onToggleDiff,
      });
    }

    if (!readOnly && activeSession) {
      const a = activeSession;
      const label = a.title || a.branch || "session";
      const toggles: { verb: string; action: SessionStateAction; keywords: string[] }[] = [
        a.pinned_at != null
          ? { verb: "Unpin", action: "unpin", keywords: ["pin", "favorite", "sidebar"] }
          : { verb: "Pin", action: "pin", keywords: ["pin", "favorite", "sidebar"] },
        a.archived_at != null
          ? { verb: "Unarchive", action: "unarchive", keywords: ["archive", "restore"] }
          : { verb: "Archive", action: "archive", keywords: ["archive"] },
        a.snoozed_until != null
          ? { verb: "Unsnooze", action: "unsnooze", keywords: ["snooze", "wake"] }
          : { verb: "Snooze…", action: "snooze", keywords: ["snooze", "later", "remind"] },
        a.trashed_at != null
          ? { verb: "Untrash", action: "untrash", keywords: ["trash", "restore", "delete"] }
          : { verb: "Trash", action: "trash", keywords: ["trash", "delete", "remove"] },
      ];
      for (const t of toggles) {
        actions.push({
          id: `session-state:${t.action}:${a.id}`,
          title: `${t.verb} ${label}`,
          subtitle: "current session",
          group: "Actions",
          keywords: [...t.keywords, label, "session"],
          perform: () => onSessionStateAction(a.id, t.action),
        });
      }
    }

    if (!focusMode) {
      actions.push({
        id: "action:toggle-sidebar",
        title: "Toggle sidebar",
        group: "Actions",
        keywords: ["hide", "show", "nav"],
        shortcut: IS_MAC ? "⌘B" : "Ctrl+B",
        perform: onToggleSidebar,
      });
    }

    actions.push({
      id: "action:help",
      title: "Show help",
      group: "Actions",
      keywords: ["help", "keys", "shortcuts", "gestures", "?"],
      shortcut: "?",
      perform: onOpenHelp,
    });

    actions.push({
      id: "action:about",
      title: "About Agent of Empires",
      group: "Actions",
      keywords: ["info", "version", "links", "github", "website"],
      perform: onOpenAbout,
    });

    if (loginRequired) {
      actions.push({
        id: "action:logout",
        title: "Sign out",
        group: "Actions",
        keywords: ["logout", "exit"],
        perform: onLogout,
      });
    }

    for (const s of focusMode ? [] : sessions) {
      if (s.id === activeSessionId) continue;
      const repo = (s.main_repo_path || s.project_path).split("/").filter(Boolean).pop() ?? "";
      const subtitleParts = [repo, s.branch, s.tool].filter(Boolean) as string[];
      actions.push({
        id: `session:${s.id}`,
        title: s.title || s.branch || "(untitled)",
        subtitle: subtitleParts.join(" · "),
        group: "Sessions",
        keywords: [s.tool, s.status, s.branch ?? "", repo, s.group_path].filter(Boolean) as string[],
        status: s.status,
        statusCreatedAt: s.created_at,
        perform: () => onSelectSession(s.id),
      });
    }

    actions.push({
      id: "settings:open",
      title: "Open settings",
      group: "Settings",
      keywords: ["preferences", "config"],
      shortcut: "s",
      perform: onOpenSettings,
    });

    return actions;
  }, [
    sessions,
    activeSessionId,
    activeSession,
    loginRequired,
    hasActiveSession,
    readOnly,
    onNewSession,
    onNewScratch,
    onSelectSession,
    onJumpToAttention,
    hasAttentionSession,
    onSessionStateAction,
    onToggleDiff,
    onOpenSettings,
    onOpenHelp,
    onOpenAbout,
    onGoDashboard,
    onToggleSidebar,
    onLogout,
    focusMode,
    onEnterFocusMode,
    onOpenFocusWindow,
    onExitFocusMode,
  ]);
}
