// @vitest-environment jsdom

// Vitest coverage for the trash sidebar predicates (#2489): a workspace is
// "trashed" only when every session carries trashed_at, and "sunk" counts
// trashed alongside archived/snoozed.

import { describe, expect, it } from "vitest";
import type { SessionResponse, Workspace } from "../types";
import { workspaceIsSunk, workspaceIsTrashed } from "../sidebarSort";

function session(over: Partial<SessionResponse>): SessionResponse {
  return { id: "s", title: "t", archived_at: null, snoozed_until: null, trashed_at: null, ...over } as SessionResponse;
}

function workspace(sessions: SessionResponse[]): Workspace {
  return { id: "w", displayName: "w", sessions } as unknown as Workspace;
}

describe("workspaceIsTrashed (#2489)", () => {
  it("false for an empty workspace", () => {
    expect(workspaceIsTrashed(workspace([]))).toBe(false);
  });

  it("true only when every session is trashed", () => {
    expect(workspaceIsTrashed(workspace([session({ trashed_at: "x" })]))).toBe(true);
    expect(workspaceIsTrashed(workspace([session({ trashed_at: "x" }), session({ trashed_at: null })]))).toBe(false);
  });

  it("false when a session is merely archived, not trashed", () => {
    expect(workspaceIsTrashed(workspace([session({ archived_at: "x" })]))).toBe(false);
  });
});

describe("workspaceIsSunk counts trash (#2489)", () => {
  it("true when the only session is trashed", () => {
    expect(workspaceIsSunk(workspace([session({ trashed_at: "x" })]))).toBe(true);
  });

  it("true when sessions mix trashed and archived/snoozed", () => {
    expect(
      workspaceIsSunk(
        workspace([session({ trashed_at: "x" }), session({ archived_at: "y" }), session({ snoozed_until: "z" })]),
      ),
    ).toBe(true);
  });

  it("false when one session is still live", () => {
    expect(workspaceIsSunk(workspace([session({ trashed_at: "x" }), session({})]))).toBe(false);
  });
});
