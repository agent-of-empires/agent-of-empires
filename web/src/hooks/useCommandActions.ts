import { useMemo } from "react";

const IS_MAC =
  typeof navigator !== "undefined" &&
  /Mac|iPhone|iPad|iPod/.test(navigator.platform);
import type { SessionResponse } from "../lib/types";
import type { CommandAction } from "../components/command-palette/types";

interface Args {
  sessions: SessionResponse[];
  activeSessionId: string | null;
  loginRequired: boolean;
  hasActiveSession: boolean;
  readOnly: boolean;
  onNewSession: () => void;
  onNewScratch: () => void;
  onSelectSession: (sessionId: string) => void;
  onToggleDiff: () => void;
  onOpenSettings: () => void;
  onOpenHelp: () => void;
  onOpenAbout: () => void;
  onGoDashboard: () => void;
  onToggleSidebar: () => void;
  onLogout: () => void;
}

export function useCommandActions({
  sessions,
  activeSessionId,
  loginRequired,
  hasActiveSession,
  readOnly,
  onNewSession,
  onNewScratch,
  onSelectSession,
  onToggleDiff,
  onOpenSettings,
  onOpenHelp,
  onOpenAbout,
  onGoDashboard,
  onToggleSidebar,
  onLogout,
}: Args): CommandAction[] {
  return useMemo(() => {
    const actions: CommandAction[] = [];

    // Creation commands are mutation UI. In read-only mode the sidebar and
    // dashboard already hide their "new session" buttons, so the palette must
    // omit these too rather than offer a command that opens a wizard the
    // server 403s on submit. The keyboard-shortcut path stays a guarded no-op
    // (a key can't be hidden); these visible entries are dropped instead.
    if (!readOnly) {
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
        keywords: [
          "scratch",
          "temp",
          "temporary",
          "ephemeral",
          "throwaway",
          "create",
        ],
        shortcut: IS_MAC ? "⌘⇧N" : "Ctrl+Shift+N",
        perform: onNewScratch,
      });
    }

    actions.push({
      id: "action:go-dashboard",
      title: "Go to dashboard",
      group: "Actions",
      keywords: ["home", "overview"],
      perform: onGoDashboard,
    });

    if (hasActiveSession) {
      actions.push({
        id: "action:toggle-diff",
        title: "Toggle diff panel",
        group: "Actions",
        keywords: ["changes", "files", "review"],
        shortcut: "D",
        perform: onToggleDiff,
      });
    }

    actions.push({
      id: "action:toggle-sidebar",
      title: "Toggle sidebar",
      group: "Actions",
      keywords: ["hide", "show", "nav"],
      shortcut: IS_MAC ? "⌘B" : "Ctrl+B",
      perform: onToggleSidebar,
    });

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

    for (const s of sessions) {
      if (s.id === activeSessionId) continue;
      const repo =
        (s.main_repo_path || s.project_path).split("/").filter(Boolean).pop() ??
        "";
      const subtitleParts = [repo, s.branch, s.tool].filter(
        Boolean,
      ) as string[];
      actions.push({
        id: `session:${s.id}`,
        title: s.title || s.branch || "(untitled)",
        subtitle: subtitleParts.join(" · "),
        group: "Sessions",
        keywords: [s.tool, s.status, s.branch ?? "", repo, s.group_path].filter(
          Boolean,
        ) as string[],
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
    loginRequired,
    hasActiveSession,
    readOnly,
    onNewSession,
    onNewScratch,
    onSelectSession,
    onToggleDiff,
    onOpenSettings,
    onOpenHelp,
    onOpenAbout,
    onGoDashboard,
    onToggleSidebar,
    onLogout,
  ]);
}
