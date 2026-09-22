import type { AgentInfo, SessionResponse } from "../../../lib/types";

export function mockSession(overrides: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "session",
    project_path: "/repo/alpha",
    group_path: "/repo/alpha",
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
    scratch: false,
    ...overrides,
  } as SessionResponse;
}

export function agent(name: string, overrides: Partial<AgentInfo> = {}): AgentInfo {
  return {
    kind: "builtin",
    name,
    binary: name,
    host_only: false,
    installed: true,
    install_hint: "",
    acp_capable: true,
    ...overrides,
  } as AgentInfo;
}
