import { describe, expect, it } from "vitest";

import { computeSessionRowTag, parseSessionRowTagMode } from "./sessionRowTag";
import type { SessionResponse, Workspace } from "./types";

function session(overrides: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "row title",
    project_path: "/repo",
    artifact_dir: "/tmp/artifacts/s1",
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
    scratch: false,
    favorited: false,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    cleanup_defaults: {
      delete_worktree: false,
      delete_branch: false,
      delete_sandbox: false,
      delete_to_trash: true,
    },
    remote_owner: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: [],
    ...overrides,
  };
}

function workspace(overrides: Partial<Workspace> = {}, sessions = [session()]): Workspace {
  return {
    id: "w1",
    branch: null,
    projectPath: "/repo",
    displayName: "row title",
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
    ...overrides,
  };
}

describe("parseSessionRowTagMode", () => {
  it("accepts known session.row_tag values and defaults invalid input to branch", () => {
    expect(parseSessionRowTagMode({ session: { row_tag: "none" } })).toBe("none");
    expect(parseSessionRowTagMode({ session: { row_tag: "auto" } })).toBe("auto");
    expect(parseSessionRowTagMode({ session: { row_tag: "profile" } })).toBe("profile");
    expect(parseSessionRowTagMode({ session: { row_tag: "sandbox" } })).toBe("sandbox");
    expect(parseSessionRowTagMode({ session: { row_tag: "branch" } })).toBe("branch");
    expect(parseSessionRowTagMode({ session: { row_tag: "bogus" } })).toBe("branch");
    expect(parseSessionRowTagMode(null)).toBe("branch");
  });
});

describe("computeSessionRowTag", () => {
  it("uses the last branch path segment and truncates to the TUI branch width", () => {
    const tag = computeSessionRowTag(workspace({ branch: "feature/configurable-session-name-suffix" }), "branch");
    expect(tag?.content).toBe("configurable");
    expect(tag?.title).toBe("feature/configurable-session-name-suffix");
  });

  it("adds the workspace repo count to common multi-repo branch tags", () => {
    const ws = workspace({}, [
      session({
        workspace_repos: [
          { name: "api", source_path: "/repo/api", branch: "feature/web-tags" },
          { name: "web", source_path: "/repo/web", branch: "feature/web-tags" },
        ],
      }),
    ]);

    const tag = computeSessionRowTag(ws, "branch");
    expect(tag?.content).toBe("web-tags+2");
    expect(tag?.title).toBe("feature/web-tags across 2 repos");
  });

  it("uses the row's first session for profile and sandbox tags", () => {
    const ws = workspace({}, [session({ profile: "forit-backup", is_sandboxed: true })]);

    expect(computeSessionRowTag(ws, "profile")?.content).toBe("fb");
    expect(computeSessionRowTag(ws, "auto")?.content).toBe("fb");
    expect(computeSessionRowTag(ws, "sandbox")?.content).toBe("sb");
  });

  it("returns no tag for none, host sandbox mode, or mixed multi-repo branches", () => {
    expect(computeSessionRowTag(workspace(), "none")).toBeNull();
    expect(computeSessionRowTag(workspace(), "sandbox")).toBeNull();
    expect(
      computeSessionRowTag(
        workspace({}, [
          session({
            workspace_repos: [
              { name: "api", source_path: "/repo/api", branch: "feature/api" },
              { name: "web", source_path: "/repo/web", branch: "feature/web" },
            ],
          }),
        ]),
        "branch",
      ),
    ).toBeNull();
  });
});
