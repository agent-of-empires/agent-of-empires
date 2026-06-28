import { useMemo } from "react";

const IS_MAC = typeof navigator !== "undefined" && /Mac|iPhone|iPad|iPod/.test(navigator.platform);
import type { SessionResponse } from "../lib/types";
import type { ConversationSearchHit } from "../lib/api";
import type { CommandAction } from "../components/command-palette/types";

// A conversation-search palette row minus its `perform` handler. The caller
// attaches `perform` in a closure (so the select callback is never passed
// into this render-time builder, which the react-hooks lint reads as a
// possible ref access during render).
export type ConversationActionData = Omit<CommandAction, "perform"> & { sessionId: string };

// Map conversation-content search hits (#2515) to palette row data. The hit
// carries only the session id; title and state come from the client's
// session list. Skips the active session (already in view) and any hit whose
// session is no longer in the list. A sunk-state label and a match-count
// suffix annotate the row so an archived/trashed hit is not mistaken for a
// live one.
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
        keywords: ["scratch", "temp", "temporary", "ephemeral", "throwaway", "create"],
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
        title: "Toggle diff pane",
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
