// Session, workspace, and repo-group builders plus the matchMedia stub shared by the hook tests.

import { vi } from "vitest";
import type { RepoGroup, SessionResponse, Workspace } from "../../lib/types";

type Listener = () => void;

/** Replace `window.matchMedia` with a controllable stub for `media`. */
export function stubMatchMedia(initialMatches: boolean, media: string) {
  let matches = initialMatches;
  const listeners = new Set<Listener>();
  const mql = {
    get matches() {
      return matches;
    },
    media,
    addEventListener: (_: string, cb: Listener) => listeners.add(cb),
    removeEventListener: (_: string, cb: Listener) => listeners.delete(cb),
  };
  window.matchMedia = vi.fn().mockReturnValue(mql) as unknown as typeof window.matchMedia;
  return {
    setWithoutEvent(next: boolean) {
      matches = next;
    },
    set(next: boolean) {
      matches = next;
      listeners.forEach((cb) => cb());
    },
    listenerCount: () => listeners.size,
  };
}

export function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "t",
    project_path: "/repo",
    group_path: "/repo",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    favorited: false,
    scratch: false,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    cleanup_defaults: { delete_worktree: false, delete_branch: false, delete_sandbox: false },
    remote_owner: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: [],
    ...over,
  } as SessionResponse;
}

export function workspace(
  id: string,
  projectPath: string,
  sessions: SessionResponse[],
  over: Partial<Workspace> = {},
): Workspace {
  return {
    id,
    branch: null,
    projectPath,
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
    ...over,
  };
}

export function repoGroup(over: Partial<RepoGroup> = {}): RepoGroup {
  return {
    id: "repo-1",
    repoPath: "/repo",
    displayName: "repo",
    defaultDisplayName: "repo",
    alias: null,
    color: null,
    remoteOwner: null,
    workspaces: [],
    status: "idle",
    collapsed: false,
    registeredProjects: [],
    ...over,
  } as RepoGroup;
}
