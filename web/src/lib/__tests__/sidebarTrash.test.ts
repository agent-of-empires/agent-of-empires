// @vitest-environment jsdom

import { expect, it } from "vitest";
import type { SessionResponse, Workspace } from "../types";
import { workspaceIsSunk, workspaceIsTrashed, workspaceTrashedAtMs } from "../sidebarSort";

function session(over: Partial<SessionResponse>): SessionResponse {
  return { id: "s", title: "t", archived_at: null, snoozed_until: null, trashed_at: null, ...over } as SessionResponse;
}

function workspace(sessions: SessionResponse[]): Workspace {
  return { id: "w", displayName: "w", sessions } as unknown as Workspace;
}

it("workspaceIsTrashed only when every session is trashed (#2489)", () => {
  expect(workspaceIsTrashed(workspace([]))).toBe(false);
  expect(workspaceIsTrashed(workspace([session({ trashed_at: "x" })]))).toBe(true);
  expect(workspaceIsTrashed(workspace([session({ trashed_at: "x" }), session({ trashed_at: null })]))).toBe(false);
  expect(workspaceIsTrashed(workspace([session({ archived_at: "x" })]))).toBe(false);
});

it("workspaceIsSunk counts trash alongside archived and snoozed (#2489)", () => {
  expect(workspaceIsSunk(workspace([session({ trashed_at: "x" })]))).toBe(true);
  expect(
    workspaceIsSunk(
      workspace([session({ trashed_at: "x" }), session({ archived_at: "y" }), session({ snoozed_until: "z" })]),
    ),
  ).toBe(true);
  expect(workspaceIsSunk(workspace([session({ trashed_at: "x" }), session({})]))).toBe(false);
});

it("workspaceTrashedAtMs takes the newest trashed_at, else 0, so Trash sorts newest-first", () => {
  const older = workspace([session({ trashed_at: "2026-01-01T00:00:00Z" })]);
  const newer = workspace([
    session({ trashed_at: "2026-06-01T00:00:00Z" }),
    session({ trashed_at: "2026-01-01T00:00:00Z" }),
  ]);
  expect(workspaceTrashedAtMs(newer)).toBe(new Date("2026-06-01T00:00:00Z").getTime());
  expect(workspaceTrashedAtMs(workspace([session({})]))).toBe(0);
  expect(workspaceTrashedAtMs(workspace([session({ archived_at: "x" })]))).toBe(0);
  expect([older, newer].sort((a, b) => workspaceTrashedAtMs(b) - workspaceTrashedAtMs(a))).toEqual([newer, older]);
});
