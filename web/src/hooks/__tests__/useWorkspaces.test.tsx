// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { renderHook } from "@testing-library/react";

import { useWorkspaces } from "../useWorkspaces";
import type { SessionResponse } from "../../lib/types";
import { session } from "./fixtures";

const workspacesOf = (sessions: SessionResponse[]) => renderHook(() => useWorkspaces(sessions)).result.current;

describe("useWorkspaces", () => {
  it("returns an empty list for no sessions", () => {
    expect(workspacesOf([])).toEqual([]);
  });

  it("collapses sessions on the same repo+branch into one workspace", () => {
    const [ws, ...rest] = workspacesOf([
      session({ id: "a", branch: "feat", main_repo_path: "/repo/" }),
      session({ id: "b", branch: "feat", main_repo_path: "/repo", tool: "codex" }),
    ]);
    expect(rest).toHaveLength(0);
    expect(ws!.sessions.map((s) => s.id).sort()).toEqual(["a", "b"]);
    expect(ws).toMatchObject({
      projectPath: "/repo",
      branch: "feat",
      agents: ["claude", "codex"],
      primaryAgent: "claude",
      displayName: "feat",
    });
  });

  it("gives each branch-less session its own workspace (#956)", () => {
    const sessions = ["a", "b"].map((id) => session({ id, main_repo_path: "/repo" }));
    expect(workspacesOf(sessions)).toHaveLength(2);
  });

  it.each([
    ["  My Task  ", "My Task"],
    ["   ", "myrepo"],
  ])("names a single-session workspace from title %j as %j", (title, expected) => {
    expect(workspacesOf([session({ title, project_path: "/x/myrepo" })])[0]!.displayName).toBe(expected);
  });

  it.each([
    [["Idle", "Running"], "active"],
    [["Idle"], "idle"],
  ] as const)("rolls statuses %j up to %s", (statuses, expected) => {
    const sessions = statuses.map((status, i) => session({ id: `s${i}`, branch: "b", status }));
    expect(workspacesOf(sessions)[0]!.status).toBe(expected);
  });
});
